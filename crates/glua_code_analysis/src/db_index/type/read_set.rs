//! Which cached facts an inference actually consulted, recorded as it runs.
//!
//! The settled tail runs to a fixpoint, and re-deriving every candidate every
//! round costs the same as the first round however little moved. A candidate
//! only needs re-deriving when a cache it *read* has since changed, and the
//! read set is free to collect.
//!
//! Stored types, signature returns, and failed member lookups record here at
//! their shared read boundaries. A read that bypassed those boundaries would
//! leave a candidate looking unaffected by a fact it actually depends on, and
//! the fixpoint would settle on the stale answer. Over-recording is harmless —
//! it costs one re-derivation — so a new reader belongs at its shared boundary.
//!
//! Worker-local by construction — each file is re-derived on one thread, and
//! the collector is a thread local armed around that file's inference. The
//! armed flag is a plain `Cell<u32>` depth so a call outside the settled tail
//! (the walk asks for type caches constantly) pays one thread-local read.
//!
//! The depth makes arming nest: the fixpoint arms around each attempt while a
//! return derivation arms around itself inside that attempt. Only the outermost
//! `arm` clears, and only the outermost `disarm` takes — a nested `disarm`
//! hands back a snapshot and leaves the outer collection in place, so the
//! outer read set keeps what the inner scope read. The inner record then
//! over-approximates by the outer scope's earlier reads, which only costs a
//! re-derivation.

use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{Cell, RefCell};

use super::type_owner::LuaTypeOwner;
use crate::{
    DynamicFieldOwner, FileId, GlobalId, InFiled, LuaDeclId, LuaDefinitionId, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaOperatorOwner, LuaSignatureId, LuaTypeDeclId,
};
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

/// The slot one index mutation lands in, keyed the same way the read
/// recorder keys reads so a read/write overlap compares directly. The wave's
/// deferred writes record these, and every recorded read stamps its slot
/// here, so a read can be ordered against the push that writes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WriteKey {
    TypeOwner(LuaTypeOwner),
    MemberOwner(LuaMemberOwner),
    MemberKey(LuaMemberKey),
    MemberSlot(LuaMemberOwner, LuaMemberKey),
    Signature(LuaSignatureId),
    Param(LuaSignatureId),
    Module(FileId),
    SpecialCall(FileId, TextSize),
    Contrib(LuaSignatureId, usize),
    DynamicField(DynamicFieldOwner, Option<SmolStr>),
    /// A `---` type declaration object (`---@class`/`---@alias` and its
    /// supertypes/generic params), keyed by its decl id.
    TypeDecl(LuaTypeDeclId),
    /// A decl-anchored side entry: feature caches, decl write claims.
    DeclMisc(LuaDeclId),
    /// A definition-anchored inferred fact (see `bind_definition_fact`).
    DefinitionFact(LuaDefinitionId),
    /// A global-path member-metadata slot (definition sites, path provenance,
    /// class-candidate flags).
    GlobalPath(GlobalId),
    /// The operator table for one operand owner (`a + b`/`t.x` metamethods).
    OperatorOwner(LuaOperatorOwner),
    /// One setmetatable binding slot, keyed by the table's own site.
    Metatable(InFiled<TextRange>),
    /// A name-keyed gmod metadata entry (vgui panels, derma skins, net/system
    /// registrations).
    GmodMeta(SmolStr),
    /// A member object slot (`get_member`/`get_member_mut`), keyed by member
    /// id — covers rekeys, re-homes, and direct object pokes alike.
    MemberId(LuaMemberId),
    /// A whole-map scan of the `---` type declarations (`find_type_decls`)
    /// — stales on *any* `TypeDecl` write, which the merge checks by tag.
    TypeDeclScan,
    /// A table-literal → owning-path slot in `definition_site_owner`, keyed
    /// by the literal's own range.
    DefinitionSite(InFiled<TextRange>),
    /// An unattributed dynamic field slot, keyed by field name — these have
    /// no owner until a settled retry attributes them.
    UnattributedField(SmolStr),
    /// A slot inside a map keyed by `FileId` — inherently file-scoped, so it
    /// can never collide with another file's write; the key only feeds the
    /// boundary coverage report.
    FileScoped(FileId),
    /// One file's helper-scan cache slot on `DbIndex` (gmod net helpers).
    HelperCache(FileId),
    /// The workspace-wide gmod net-helper registry slot on `DbIndex`.
    HelperRegistry,
}

/// Per-key write epochs owned by one index: `note` stamps a key with the
/// next clock tick, `epoch` reads it back (`0` for never-written). The store
/// lives inside the index that owns the map, so a write only moves the epoch
/// of the instance it landed on — a fork's writes stay fork-local until the
/// merge replays them, and a neighbouring `DbIndex` (another workspace, or
/// another test's) cannot stale this instance's readers.
#[derive(Debug, Clone)]
pub(crate) struct KeyedEpochs<K> {
    clock: u64,
    epochs: FxHashMap<K, u64>,
}

impl<K> Default for KeyedEpochs<K> {
    fn default() -> Self {
        Self {
            clock: 0,
            epochs: FxHashMap::default(),
        }
    }
}

impl<K: Clone + Eq + std::hash::Hash> KeyedEpochs<K> {
    /// Stamps `key` with the next tick — call inside the owning index's
    /// mutators, so direct writes and op-log/deferred replays share the
    /// funnel.
    pub(crate) fn note(&mut self, key: K) {
        self.clock = self.clock.wrapping_add(1);
        self.epochs.insert(key, self.clock);
    }

    /// `key`'s last write tick, or `0` if the index never wrote it.
    pub(crate) fn epoch(&self, key: &K) -> u64 {
        self.epochs.get(key).copied().unwrap_or(0)
    }

    /// The store's own tick counter. A capture can compare it whole: an
    /// unchanged clock proves every key epoch inside is unchanged too (the
    /// per-key checks cannot see a write `note` never stamped), so the
    /// family's entries need no walk.
    pub(crate) fn clock(&self) -> u64 {
        self.clock
    }

    pub(crate) fn clear(&mut self) {
        self.clock = 0;
        self.epochs.clear();
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct InferenceReadSet {
    pub type_owners: FxHashSet<LuaTypeOwner>,
    pub member_maps: FxHashMap<LuaMemberOwner, (u64, u64, u64, usize)>,
    /// Key-indexed member reads (`get_current_members_for_key`): the reverse
    /// owner lookup draws narrowing candidates from the live member map, so a
    /// pass that read a key must re-derive once a member joins or leaves it.
    /// Versioned as `(canonical_revision, key_move_epoch, slot_len)` —
    /// see [`crate::LuaMemberIndex::key_read_version`].
    pub member_keys: FxHashMap<LuaMemberKey, (u64, u64, usize)>,
    pub signatures: FxHashSet<LuaSignatureId>,
    pub params: FxHashSet<LuaSignatureId>,
    pub modules: FxHashSet<FileId>,
    /// Special-call materializations read through the flow effects of one
    /// call-site, keyed by that site. See `LuaFlowIndex::special_writes`.
    pub special_calls: FxHashSet<(FileId, TextSize)>,
    /// Call-site-inferred parameter facts, keyed by the callee slot that
    /// holds them. See `CallSiteParamIndex::contrib_writes`.
    pub contribs: FxHashSet<(LuaSignatureId, usize)>,
    /// Dynamic-field definitions consulted, keyed by owner and field name
    /// (`None` is the wildcard bucket). See `DynamicFieldIndex::dynamic_writes`.
    pub dynamic_fields: FxHashSet<(DynamicFieldOwner, Option<SmolStr>)>,
    /// Member slots a lookup missed on, versioned by what the slot held when
    /// read: the member count, and for a `Name` key the dynamic-field
    /// definition count. A slot that was already non-empty (a realm-incompatible
    /// member, or a key a dynamic field already defines) is a definitive answer
    /// the settled pass reproduces, so it must not report stale every round —
    /// only the slot changing what it holds re-derives the read.
    pub missing_member_slots: FxHashMap<(LuaMemberOwner, LuaMemberKey), (usize, usize)>,
    /// Member objects read by id (`get_member`, `get_member_owner`,
    /// provenance and bookkeeping flags), stamped with the member's own
    /// move epoch — the per-id counterpart of the owner/key epochs, so a
    /// write to another member leaves the read fresh.
    pub member_ids: FxHashMap<LuaMemberId, u64>,
    /// `---` type declarations consulted (`find_type_decl`, super-type and
    /// generic-param lookups), stamped with the (decl, map family) write
    /// epoch on the type index — a write to another decl, or to a side map
    /// the read never consulted, leaves the read fresh.
    pub type_decls: FxHashMap<(LuaTypeDeclId, TypeDeclKind), u64>,
    /// `decls_bound_to_type` reads, stamped per (bound-to type, observed
    /// file) — the caller filters bound decls by the class's own files, so a
    /// bind in a file it never asked about cannot stale the read.
    pub bound_decls: FxHashMap<(LuaTypeDeclId, FileId), u64>,
    /// Vgui parent-chain lookups consulted (`get_vgui_panel_parent_chain`,
    /// `vgui_panel_parent_chain_is_complete`), stamped with the child decl's
    /// own write epoch on the gmod metadata index which owns those maps.
    pub vgui_type_decls: FxHashMap<LuaTypeDeclId, u64>,
    /// Decl objects and decl-anchored side entries consulted (`get_decl`,
    /// `get_decl_mut`), stamped with the decl's own write epoch — a write to
    /// another decl leaves the read fresh. The epochs live on the decl
    /// index.
    pub decl_miscs: FxHashMap<LuaDeclId, u64>,
    /// `decl_write_claim` arbitration reads, stamped with the claimed decl's
    /// own write epoch — claims are seeded mid-fixpoint, so only readers of
    /// the moved decl's claim re-derive; decl-object readers are unaffected.
    /// The epochs live on the type index, which owns the claims map.
    pub decl_claims: FxHashMap<LuaDeclId, u64>,
    /// `inferred_string_defaults` reads, stamped with the decl's own write
    /// epoch — the map lives on the property index, so its epochs do too.
    pub decl_defaults: FxHashMap<LuaDeclId, u64>,
    /// Definition-anchored inferred facts consulted, stamped with the
    /// definition's own write epoch on the type index.
    pub definition_facts: FxHashMap<LuaDefinitionId, u64>,
    /// Global-path member metadata consulted (definition sites, provenance),
    /// stamped with the path's own write epoch on the member index.
    pub global_paths: FxHashMap<GlobalId, u64>,
    /// Operator tables consulted, stamped with the operand owner's own
    /// write epoch on the operator index.
    pub operator_owners: FxHashMap<LuaOperatorOwner, u64>,
    /// Setmetatable binding slots consulted, stamped with the table site's
    /// own write epoch on the metatable index.
    pub metatable_sites: FxHashMap<InFiled<TextRange>, u64>,
    /// Table-literal → owning-path slots consulted, stamped with the
    /// literal's own write epoch on the member index
    /// (`definition_site_owner` / `canonical` lookups).
    pub definition_sites: FxHashMap<InFiled<TextRange>, u64>,
    /// Name-keyed gmod metadata consulted (vgui panels, derma skins,
    /// net/system registrations).
    pub gmod_metas: FxHashSet<SmolStr>,
    /// Reads of `FileId`-keyed side maps (namespaces, decl trees, per-file
    /// signature lists, accessor calls), versioned per (file, map family) —
    /// one epoch per file let a signature registration stale a decl-tree
    /// reader, so each family keeps its own slot.
    pub file_scopeds: FxHashMap<(FileId, FileScopedKind), u64>,
    /// The file enumerated the whole type-declaration map
    /// (`find_type_decls`), so any `TypeDecl` write stales it.
    pub type_decl_scan: bool,
    /// The sequence number of the most recent read of each slot, keyed the
    /// same way the wave's deferred writes key theirs. A slot read *after*
    /// its own write was queued saw pre-write state where the serial drain
    /// would have seen the write, which is what `self_conflicted` answers.
    /// Fixpoint staleness ignores this — presence in the sets above already
    /// covers it.
    pub read_seqs: FxHashMap<WriteKey, u64>,
}

/// Merges one epoch-keyed read family: the earliest recorded epoch wins —
/// a write after the first read still lands ahead of it.
fn merge_min_epochs<K: Eq + std::hash::Hash>(
    target: &mut FxHashMap<K, u64>,
    other: FxHashMap<K, u64>,
) {
    for (key, epoch) in other {
        target
            .entry(key)
            .and_modify(|held| *held = (*held).min(epoch))
            .or_insert(epoch);
    }
}

impl InferenceReadSet {
    /// Whether the armed scope that produced this set consulted no mutable
    /// state at all — `read_seqs` aside, an empty set has nothing
    /// `is_stale` could ever observe moving.
    pub(crate) fn is_empty(&self) -> bool {
        self.type_owners.is_empty()
            && self.member_maps.is_empty()
            && self.member_keys.is_empty()
            && self.signatures.is_empty()
            && self.params.is_empty()
            && self.modules.is_empty()
            && self.special_calls.is_empty()
            && self.contribs.is_empty()
            && self.dynamic_fields.is_empty()
            && self.missing_member_slots.is_empty()
            && self.member_ids.is_empty()
            && self.type_decls.is_empty()
            && self.bound_decls.is_empty()
            && self.vgui_type_decls.is_empty()
            && self.decl_miscs.is_empty()
            && self.decl_claims.is_empty()
            && self.decl_defaults.is_empty()
            && self.definition_facts.is_empty()
            && self.global_paths.is_empty()
            && self.operator_owners.is_empty()
            && self.metatable_sites.is_empty()
            && self.definition_sites.is_empty()
            && self.gmod_metas.is_empty()
            && self.file_scopeds.is_empty()
            && !self.type_decl_scan
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.type_owners.extend(other.type_owners);
        for (owner, version) in other.member_maps {
            self.member_maps.entry(owner).or_insert(version);
        }
        for (key, version) in other.member_keys {
            self.member_keys.entry(key).or_insert(version);
        }
        self.signatures.extend(other.signatures);
        self.params.extend(other.params);
        self.modules.extend(other.modules);
        self.special_calls.extend(other.special_calls);
        self.contribs.extend(other.contribs);
        self.dynamic_fields.extend(other.dynamic_fields);
        self.missing_member_slots.extend(other.missing_member_slots);
        merge_min_epochs(&mut self.member_ids, other.member_ids);
        merge_min_epochs(&mut self.type_decls, other.type_decls);
        merge_min_epochs(&mut self.bound_decls, other.bound_decls);
        merge_min_epochs(&mut self.vgui_type_decls, other.vgui_type_decls);
        merge_min_epochs(&mut self.decl_miscs, other.decl_miscs);
        merge_min_epochs(&mut self.decl_claims, other.decl_claims);
        merge_min_epochs(&mut self.decl_defaults, other.decl_defaults);
        merge_min_epochs(&mut self.definition_facts, other.definition_facts);
        merge_min_epochs(&mut self.global_paths, other.global_paths);
        merge_min_epochs(&mut self.operator_owners, other.operator_owners);
        merge_min_epochs(&mut self.metatable_sites, other.metatable_sites);
        merge_min_epochs(&mut self.definition_sites, other.definition_sites);
        self.gmod_metas.extend(other.gmod_metas);
        merge_min_epochs(&mut self.file_scopeds, other.file_scopeds);
        self.type_decl_scan |= other.type_decl_scan;
        for (key, seq) in other.read_seqs {
            let entry = self.read_seqs.entry(key).or_insert(seq);
            *entry = (*entry).max(seq);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InferredReturnReadRecord {
    pub reads: InferenceReadSet,
    pub type_epoch: u64,
    pub return_epoch: u64,
    pub param_epoch: u64,
    pub module_epoch: u64,
    pub special_epoch: u64,
    pub contrib_epoch: u64,
    pub dynamic_epoch: u64,
    pub untracked_epochs: [u64; UNTRACKED_DOMAIN_COUNT],
    pub decl_clock: u64,
    pub claim_clock: u64,
    pub default_clock: u64,
    pub file_clocks: [u64; FILE_SCOPED_KIND_COUNT],
    pub member_clock: u64,
    pub decl_key_clock: u64,
    pub bound_decl_clock: u64,
    pub vgui_decl_clock: u64,
    pub fact_clock: u64,
    pub global_path_clock: u64,
    pub site_clock: u64,
    pub operator_clock: u64,
    pub metatable_clock: u64,
}

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static TRACK_MEMBER_MAPS: Cell<bool> = const { Cell::new(false) };
    static MEMBER_MAPS: RefCell<FxHashMap<LuaMemberOwner, (u64, u64, u64, usize)>> =
        RefCell::new(FxHashMap::default());
    static MEMBER_KEYS: RefCell<FxHashMap<LuaMemberKey, (u64, u64, usize)>> =
        RefCell::new(FxHashMap::default());
    static READS: RefCell<FxHashSet<LuaTypeOwner>> = RefCell::new(FxHashSet::default());
    static SIGNATURES: RefCell<FxHashSet<LuaSignatureId>> = RefCell::new(FxHashSet::default());
    static PARAMS: RefCell<FxHashSet<LuaSignatureId>> = RefCell::new(FxHashSet::default());
    static MODULES: RefCell<FxHashSet<FileId>> = RefCell::new(FxHashSet::default());
    static SPECIAL_CALLS: RefCell<FxHashSet<(FileId, TextSize)>> = RefCell::new(FxHashSet::default());
    static CONTRIBS: RefCell<FxHashSet<(LuaSignatureId, usize)>> = RefCell::new(FxHashSet::default());
    static DYNAMIC_FIELDS: RefCell<FxHashSet<(DynamicFieldOwner, Option<SmolStr>)>> = RefCell::new(FxHashSet::default());
    static MISSING_MEMBER_SLOTS: RefCell<FxHashMap<(LuaMemberOwner, LuaMemberKey), (usize, usize)>> = RefCell::new(FxHashMap::default());
    static MEMBER_IDS: RefCell<FxHashMap<LuaMemberId, u64>> =
        RefCell::new(FxHashMap::default());
    static TYPE_DECLS: RefCell<FxHashMap<(LuaTypeDeclId, TypeDeclKind), u64>> =
        RefCell::new(FxHashMap::default());
    static BOUND_DECLS: RefCell<FxHashMap<(LuaTypeDeclId, FileId), u64>> =
        RefCell::new(FxHashMap::default());
    static VGUI_TYPE_DECLS: RefCell<FxHashMap<LuaTypeDeclId, u64>> =
        RefCell::new(FxHashMap::default());
    static DECL_MISCS: RefCell<FxHashMap<LuaDeclId, u64>> =
        RefCell::new(FxHashMap::default());
    static DECL_CLAIMS: RefCell<FxHashMap<LuaDeclId, u64>> =
        RefCell::new(FxHashMap::default());
    static DECL_DEFAULTS: RefCell<FxHashMap<LuaDeclId, u64>> =
        RefCell::new(FxHashMap::default());
    static DEFINITION_FACTS: RefCell<FxHashMap<LuaDefinitionId, u64>> =
        RefCell::new(FxHashMap::default());
    static GLOBAL_PATHS: RefCell<FxHashMap<GlobalId, u64>> =
        RefCell::new(FxHashMap::default());
    static OPERATOR_OWNERS: RefCell<FxHashMap<LuaOperatorOwner, u64>> =
        RefCell::new(FxHashMap::default());
    static METATABLE_SITES: RefCell<FxHashMap<InFiled<TextRange>, u64>> =
        RefCell::new(FxHashMap::default());
    static DEFINITION_SITES: RefCell<FxHashMap<InFiled<TextRange>, u64>> =
        RefCell::new(FxHashMap::default());
    static GMOD_METAS: RefCell<FxHashSet<SmolStr>> = RefCell::new(FxHashSet::default());
    static FILE_SCOPEDS: RefCell<FxHashMap<(FileId, FileScopedKind), u64>> =
        RefCell::new(FxHashMap::default());
    static TYPE_DECL_SCAN: Cell<bool> = const { Cell::new(false) };
    /// A monotonic event ordinal shared by read recording and the wave's
    /// deferred-write pushes. Stamping both on one clock lets the drain tell
    /// whether a slot read happened *after* the attempt queued its own write
    /// to that slot — a read that precedes the push sees the same pre-write
    /// state the serial attempt sees, while a later one diverged.
    static SEQ: Cell<u64> = const { Cell::new(0) };
    static READ_SEQS: RefCell<FxHashMap<WriteKey, u64>> =
        RefCell::new(FxHashMap::default());
    /// True once the armed scope queued its first deferred write. A read
    /// taken before that point precedes every push, so it can never have
    /// diverged from the serial order and its stamp is dead bookkeeping —
    /// most attempts push nothing or write only at the end, which leaves
    /// nearly every recorded read unstamped.
    static WRITES_QUEUED: Cell<bool> = const { Cell::new(false) };
}

/// Starts collecting reads, sharing the collection with any enclosing scope.
pub(crate) fn arm() {
    if DEPTH.with(Cell::get) == 0 {
        TRACK_MEMBER_MAPS.with(|track| track.set(false));
        READS.with(|reads| reads.borrow_mut().clear());
        MEMBER_MAPS.with(|maps| maps.borrow_mut().clear());
        MEMBER_KEYS.with(|keys| keys.borrow_mut().clear());
        SIGNATURES.with(|signatures| signatures.borrow_mut().clear());
        PARAMS.with(|params| params.borrow_mut().clear());
        MODULES.with(|modules| modules.borrow_mut().clear());
        SPECIAL_CALLS.with(|sites| sites.borrow_mut().clear());
        CONTRIBS.with(|slots| slots.borrow_mut().clear());
        DYNAMIC_FIELDS.with(|fields| fields.borrow_mut().clear());
        MISSING_MEMBER_SLOTS.with(|slots| slots.borrow_mut().clear());
        MEMBER_IDS.with(|ids| ids.borrow_mut().clear());
        TYPE_DECLS.with(|decls| decls.borrow_mut().clear());
        BOUND_DECLS.with(|decls| decls.borrow_mut().clear());
        VGUI_TYPE_DECLS.with(|decls| decls.borrow_mut().clear());
        DECL_MISCS.with(|decls| decls.borrow_mut().clear());
        DEFINITION_FACTS.with(|facts| facts.borrow_mut().clear());
        GLOBAL_PATHS.with(|paths| paths.borrow_mut().clear());
        OPERATOR_OWNERS.with(|owners| owners.borrow_mut().clear());
        METATABLE_SITES.with(|sites| sites.borrow_mut().clear());
        DEFINITION_SITES.with(|sites| sites.borrow_mut().clear());
        GMOD_METAS.with(|metas| metas.borrow_mut().clear());
        FILE_SCOPEDS.with(|files| files.borrow_mut().clear());
        DECL_CLAIMS.with(|claims| claims.borrow_mut().clear());
        DECL_DEFAULTS.with(|defaults| defaults.borrow_mut().clear());
        TYPE_DECL_SCAN.with(|scan| scan.set(false));
        READ_SEQS.with(|seqs| seqs.borrow_mut().clear());
        WRITES_QUEUED.with(|queued| queued.set(false));
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
}

/// Includes member-map shape reads for settled passes that validate them.
pub(crate) fn arm_with_member_maps() {
    arm();
    TRACK_MEMBER_MAPS.with(|track| track.set(true));
}

/// Stops collecting and hands back what was read since [`arm`].
///
/// A nested `disarm` returns a snapshot and leaves the outer collection in
/// place; only the outermost `disarm` takes. Unbalanced calls saturate rather
/// than underflow; every `arm` in the codebase is paired, so the saturated
/// path only takes whatever is left over.
pub(crate) fn disarm() -> InferenceReadSet {
    let depth = DEPTH.with(Cell::get);
    DEPTH.with(|armed| armed.set(depth.saturating_sub(1)));
    if depth > 1 {
        InferenceReadSet {
            type_owners: READS.with(|reads| reads.borrow().clone()),
            member_maps: MEMBER_MAPS.with(|maps| maps.borrow().clone()),
            member_keys: MEMBER_KEYS.with(|keys| keys.borrow().clone()),
            signatures: SIGNATURES.with(|signatures| signatures.borrow().clone()),
            params: PARAMS.with(|params| params.borrow().clone()),
            modules: MODULES.with(|modules| modules.borrow().clone()),
            special_calls: SPECIAL_CALLS.with(|sites| sites.borrow().clone()),
            contribs: CONTRIBS.with(|slots| slots.borrow().clone()),
            dynamic_fields: DYNAMIC_FIELDS.with(|fields| fields.borrow().clone()),
            missing_member_slots: MISSING_MEMBER_SLOTS.with(|slots| slots.borrow().clone()),
            member_ids: MEMBER_IDS.with(|ids| ids.borrow().clone()),
            type_decls: TYPE_DECLS.with(|decls| decls.borrow().clone()),
            bound_decls: BOUND_DECLS.with(|decls| decls.borrow().clone()),
            vgui_type_decls: VGUI_TYPE_DECLS.with(|decls| decls.borrow().clone()),
            decl_miscs: DECL_MISCS.with(|decls| decls.borrow().clone()),
            decl_claims: DECL_CLAIMS.with(|claims| claims.borrow().clone()),
            decl_defaults: DECL_DEFAULTS.with(|defaults| defaults.borrow().clone()),
            definition_facts: DEFINITION_FACTS.with(|facts| facts.borrow().clone()),
            global_paths: GLOBAL_PATHS.with(|paths| paths.borrow().clone()),
            operator_owners: OPERATOR_OWNERS.with(|owners| owners.borrow().clone()),
            metatable_sites: METATABLE_SITES.with(|sites| sites.borrow().clone()),
            definition_sites: DEFINITION_SITES.with(|sites| sites.borrow().clone()),
            gmod_metas: GMOD_METAS.with(|metas| metas.borrow().clone()),
            file_scopeds: FILE_SCOPEDS.with(|files| files.borrow().clone()),
            type_decl_scan: TYPE_DECL_SCAN.with(Cell::get),
            read_seqs: READ_SEQS.with(|seqs| seqs.borrow().clone()),
        }
    } else {
        InferenceReadSet {
            type_owners: READS.with(|reads| std::mem::take(&mut *reads.borrow_mut())),
            member_maps: MEMBER_MAPS.with(|maps| std::mem::take(&mut *maps.borrow_mut())),
            member_keys: MEMBER_KEYS.with(|keys| std::mem::take(&mut *keys.borrow_mut())),
            signatures: SIGNATURES.with(|signatures| std::mem::take(&mut *signatures.borrow_mut())),
            params: PARAMS.with(|params| std::mem::take(&mut *params.borrow_mut())),
            modules: MODULES.with(|modules| std::mem::take(&mut *modules.borrow_mut())),
            special_calls: SPECIAL_CALLS.with(|sites| std::mem::take(&mut *sites.borrow_mut())),
            contribs: CONTRIBS.with(|slots| std::mem::take(&mut *slots.borrow_mut())),
            dynamic_fields: DYNAMIC_FIELDS.with(|fields| std::mem::take(&mut *fields.borrow_mut())),
            missing_member_slots: MISSING_MEMBER_SLOTS
                .with(|slots| std::mem::take(&mut *slots.borrow_mut())),
            member_ids: MEMBER_IDS.with(|ids| std::mem::take(&mut *ids.borrow_mut())),
            type_decls: TYPE_DECLS.with(|decls| std::mem::take(&mut *decls.borrow_mut())),
            bound_decls: BOUND_DECLS.with(|decls| std::mem::take(&mut *decls.borrow_mut())),
            vgui_type_decls: VGUI_TYPE_DECLS.with(|decls| std::mem::take(&mut *decls.borrow_mut())),
            decl_miscs: DECL_MISCS.with(|decls| std::mem::take(&mut *decls.borrow_mut())),
            decl_claims: DECL_CLAIMS.with(|claims| std::mem::take(&mut *claims.borrow_mut())),
            decl_defaults: DECL_DEFAULTS
                .with(|defaults| std::mem::take(&mut *defaults.borrow_mut())),
            definition_facts: DEFINITION_FACTS
                .with(|facts| std::mem::take(&mut *facts.borrow_mut())),
            global_paths: GLOBAL_PATHS.with(|paths| std::mem::take(&mut *paths.borrow_mut())),
            operator_owners: OPERATOR_OWNERS
                .with(|owners| std::mem::take(&mut *owners.borrow_mut())),
            metatable_sites: METATABLE_SITES.with(|sites| std::mem::take(&mut *sites.borrow_mut())),
            definition_sites: DEFINITION_SITES
                .with(|sites| std::mem::take(&mut *sites.borrow_mut())),
            gmod_metas: GMOD_METAS.with(|metas| std::mem::take(&mut *metas.borrow_mut())),
            file_scopeds: FILE_SCOPEDS.with(|files| std::mem::take(&mut *files.borrow_mut())),
            type_decl_scan: TYPE_DECL_SCAN.with(|scan| scan.replace(false)),
            read_seqs: READ_SEQS.with(|seqs| std::mem::take(&mut *seqs.borrow_mut())),
        }
    }
}

/// Hands out the next event ordinal. Deferred-write pushes stamp their keys
/// with it, so a read's ordinal can be compared against the push that wrote
/// the same slot. Reads stamp theirs inside their `record_*` call.
#[inline]
pub(crate) fn next_seq() -> u64 {
    SEQ.with(|seq| {
        let next = seq.get().wrapping_add(1);
        seq.set(next);
        next
    })
}

/// The write-side ordinal: stamps the push and arms read stamping so a read
/// that follows it can be ordered against it. Reads before the first push
/// stay unstamped — they observed the pre-write state either way.
#[inline]
pub(crate) fn next_write_seq() -> u64 {
    WRITES_QUEUED.with(|queued| queued.set(true));
    next_seq()
}

/// Stamps `key` with the ordinal of this read, once the scope has queued a
/// write worth ordering against. Repeated reads keep the latest, which is
/// the comparison `self_conflicted` wants: any read that ran after the
/// slot's first queued write may have diverged.
#[inline]
fn note_read_seq(key: WriteKey) {
    if !WRITES_QUEUED.with(Cell::get) {
        return;
    }
    let seq = next_seq();
    READ_SEQS.with(|seqs| {
        seqs.borrow_mut().insert(key, seq);
    });
}

#[inline]
pub(crate) fn record(owner: &LuaTypeOwner) {
    if DEPTH.with(Cell::get) == 0 {
        return;
    }
    READS.with(|reads| {
        reads.borrow_mut().insert(owner.clone());
    });
    note_read_seq(WriteKey::TypeOwner(owner.clone()));
}

#[inline]
pub(crate) fn record_signature(signature_id: &LuaSignatureId) {
    if DEPTH.with(Cell::get) > 0 {
        SIGNATURES.with(|signatures| {
            signatures.borrow_mut().insert(*signature_id);
        });
        note_read_seq(WriteKey::Signature(*signature_id));
    }
}

/// Records a read of a signature's parameter contract (`param_docs`).
///
/// `LuaSignature::get_param_info_by_id/by_name` cannot record: the signature
/// does not know its own id. Callers that hold the id record at the shared
/// inference boundaries instead (`infer_param`, closure/special-call
/// resolution), which covers every read an armed scope can make.
#[inline]
pub(crate) fn record_param(signature_id: &LuaSignatureId) {
    if DEPTH.with(Cell::get) > 0 {
        PARAMS.with(|params| {
            params.borrow_mut().insert(*signature_id);
        });
        note_read_seq(WriteKey::Param(*signature_id));
    }
}

/// Records a read of a module's exported type, keyed by the exporting file.
#[inline]
pub(crate) fn record_module(file_id: &FileId) {
    if DEPTH.with(Cell::get) > 0 {
        MODULES.with(|modules| {
            modules.borrow_mut().insert(*file_id);
        });
        note_read_seq(WriteKey::Module(*file_id));
    }
}

/// Records a read of one call-site's special-call materializations, keyed by
/// the calling file and the call's start offset.
#[inline]
pub(crate) fn record_special_call(file_id: &FileId, position: TextSize) {
    if DEPTH.with(Cell::get) > 0 {
        SPECIAL_CALLS.with(|sites| {
            sites.borrow_mut().insert((*file_id, position));
        });
        note_read_seq(WriteKey::SpecialCall(*file_id, position));
    }
}

/// Records a read of one call-site-inferred parameter fact, keyed by the
/// callee slot that holds it.
#[inline]
pub(crate) fn record_contrib(signature_id: &LuaSignatureId, param_idx: usize) {
    if DEPTH.with(Cell::get) > 0 {
        CONTRIBS.with(|slots| {
            slots.borrow_mut().insert((*signature_id, param_idx));
        });
        note_read_seq(WriteKey::Contrib(*signature_id, param_idx));
    }
}

/// Records a read of one dynamic field's definitions, keyed by owner and
/// field name (`None` is the wildcard bucket).
#[inline]
pub(crate) fn record_dynamic_field(owner: &DynamicFieldOwner, field_name: Option<&str>) {
    if DEPTH.with(Cell::get) > 0 {
        DYNAMIC_FIELDS.with(|fields| {
            fields
                .borrow_mut()
                .insert((owner.clone(), field_name.map(SmolStr::new)));
        });
        note_read_seq(WriteKey::DynamicField(
            owner.clone(),
            field_name.map(SmolStr::new),
        ));
    }
}

#[inline]
pub(crate) fn record_missing_member_slot(
    owner: &LuaMemberOwner,
    key: &LuaMemberKey,
    member_count: usize,
    dynamic_field_count: usize,
) {
    if DEPTH.with(Cell::get) > 0 {
        MISSING_MEMBER_SLOTS.with(|slots| {
            slots.borrow_mut().insert(
                (owner.clone(), key.clone()),
                (member_count, dynamic_field_count),
            );
        });
        note_read_seq(WriteKey::MemberSlot(owner.clone(), key.clone()));
    }
}

#[inline]
pub(crate) fn record_member_map(index: &crate::LuaMemberIndex, owner: &LuaMemberOwner) {
    if DEPTH.with(Cell::get) > 0 && TRACK_MEMBER_MAPS.with(Cell::get) {
        MEMBER_MAPS.with(|maps| {
            maps.borrow_mut()
                .entry(owner.clone())
                .or_insert_with(|| index.read_version(owner));
        });
        note_read_seq(WriteKey::MemberOwner(owner.clone()));
    }
}

/// Records a key-indexed member read (`get_current_members_for_key`).
///
/// The reverse owner lookup answers from the members filed under one key, so
/// the version is that key's slot: `(canonical_revision, key_move_epoch,
/// slot_len)`. A new owner gaining the key grows the slot; a re-home between
/// owners stamps the key's move epoch. First read wins per collection,
/// matching `record_member_map`.
#[inline]
pub(crate) fn record_member_key_read(index: &crate::LuaMemberIndex, key: &LuaMemberKey) {
    if DEPTH.with(Cell::get) > 0 && TRACK_MEMBER_MAPS.with(Cell::get) {
        MEMBER_KEYS.with(|keys| {
            keys.borrow_mut()
                .entry(key.clone())
                .or_insert_with(|| index.key_read_version(key));
        });
        note_read_seq(WriteKey::MemberKey(key.clone()));
    }
}

/// Records a member-object read by id (`get_member`, `get_member_owner`,
/// provenance and bookkeeping-flag reads), stamped with the member's own
/// move epoch — first read wins, matching `record_member_map`.
#[inline]
pub(crate) fn record_member_id(index: &crate::LuaMemberIndex, member_id: &LuaMemberId) {
    if DEPTH.with(Cell::get) > 0 {
        MEMBER_IDS.with(|ids| {
            ids.borrow_mut()
                .entry(*member_id)
                .or_insert_with(|| index.member_object_version(member_id));
        });
        note_read_seq(WriteKey::MemberId(*member_id));
    }
}

/// Records a `---` type-declaration read (`find_type_decl`, super-type and
/// generic-parameter lookups), stamped with the (decl, map family) write
/// epoch on the type index — the earliest read wins.
#[inline]
pub(crate) fn record_type_decl(
    epochs: &KeyedEpochs<(LuaTypeDeclId, TypeDeclKind)>,
    decl_id: &LuaTypeDeclId,
    kind: TypeDeclKind,
) {
    if DEPTH.with(Cell::get) > 0 {
        let key = (decl_id.clone(), kind);
        TYPE_DECLS.with(|decls| {
            decls
                .borrow_mut()
                .entry(key.clone())
                .or_insert_with(|| epochs.epoch(&key));
        });
        note_read_seq(WriteKey::TypeDecl(decl_id.clone()));
    }
}

/// Records a `decls_bound_to_type` read, stamped with the (bound-to type,
/// observed file) write epoch on the type index — the earliest read wins.
#[inline]
pub(crate) fn record_bound_decl(
    epochs: &KeyedEpochs<(LuaTypeDeclId, FileId)>,
    type_id: &LuaTypeDeclId,
    file_id: FileId,
) {
    if DEPTH.with(Cell::get) > 0 {
        let key = (type_id.clone(), file_id);
        BOUND_DECLS.with(|decls| {
            decls
                .borrow_mut()
                .entry(key.clone())
                .or_insert_with(|| epochs.epoch(&key));
        });
        note_read_seq(WriteKey::TypeDecl(type_id.clone()));
    }
}

/// Records a vgui parent-chain read (`get_vgui_panel_parent_chain`,
/// `vgui_panel_parent_chain_is_complete`), stamped with the child decl's
/// current write epoch on the gmod metadata index which owns those maps.
#[inline]
pub(crate) fn record_vgui_type_decl(epochs: &KeyedEpochs<LuaTypeDeclId>, decl_id: &LuaTypeDeclId) {
    if DEPTH.with(Cell::get) > 0 {
        VGUI_TYPE_DECLS.with(|decls| {
            decls
                .borrow_mut()
                .entry(decl_id.clone())
                .or_insert_with(|| epochs.epoch(decl_id));
        });
        note_read_seq(WriteKey::TypeDecl(decl_id.clone()));
    }
}

/// Records a decl-object or decl-anchored side-entry read (`get_decl`,
/// `get_decl_mut`), stamped with the decl's current write epoch on the
/// decl index — the earliest read wins.
#[inline]
pub(crate) fn record_decl_misc(epochs: &KeyedEpochs<LuaDeclId>, decl_id: &LuaDeclId) {
    if DEPTH.with(Cell::get) > 0 {
        DECL_MISCS.with(|decls| {
            decls
                .borrow_mut()
                .entry(*decl_id)
                .or_insert_with(|| epochs.epoch(decl_id));
        });
        note_read_seq(WriteKey::DeclMisc(*decl_id));
    }
}

/// Records a `decl_write_claim` read, stamped with the claim's current epoch
/// on the type index so a later claim write on the same decl stales this
/// reader alone.
#[inline]
pub(crate) fn record_decl_claim(epochs: &KeyedEpochs<LuaDeclId>, decl_id: &LuaDeclId) {
    if DEPTH.with(Cell::get) > 0 {
        let epoch = epochs.epoch(decl_id);
        DECL_CLAIMS.with(|claims| {
            claims.borrow_mut().insert(*decl_id, epoch);
        });
        note_read_seq(WriteKey::DeclMisc(*decl_id));
    }
}

/// Records an `inferred_string_defaults` read, stamped with the decl's
/// current write epoch on the property index which owns that map.
#[inline]
pub(crate) fn record_decl_default(epochs: &KeyedEpochs<LuaDeclId>, decl_id: &LuaDeclId) {
    if DEPTH.with(Cell::get) > 0 {
        DECL_DEFAULTS.with(|defaults| {
            defaults
                .borrow_mut()
                .entry(*decl_id)
                .or_insert_with(|| epochs.epoch(decl_id));
        });
        note_read_seq(WriteKey::DeclMisc(*decl_id));
    }
}

/// Records a definition-anchored inferred-fact read, stamped with the
/// definition's current write epoch on the type index.
#[inline]
pub(crate) fn record_definition_fact(
    epochs: &KeyedEpochs<LuaDefinitionId>,
    definition_id: &LuaDefinitionId,
) {
    if DEPTH.with(Cell::get) > 0 {
        DEFINITION_FACTS.with(|facts| {
            facts
                .borrow_mut()
                .entry(*definition_id)
                .or_insert_with(|| epochs.epoch(definition_id));
        });
        note_read_seq(WriteKey::DefinitionFact(*definition_id));
    }
}

/// Records a global-path metadata read (definition sites, provenance),
/// stamped with the path's current write epoch on the member index.
#[inline]
pub(crate) fn record_global_path(epochs: &KeyedEpochs<GlobalId>, global_id: &GlobalId) {
    if DEPTH.with(Cell::get) > 0 {
        GLOBAL_PATHS.with(|paths| {
            paths
                .borrow_mut()
                .entry(global_id.clone())
                .or_insert_with(|| epochs.epoch(global_id));
        });
        note_read_seq(WriteKey::GlobalPath(global_id.clone()));
    }
}

/// Records an operator-table read, stamped with the operand owner's current
/// write epoch on the operator index.
#[inline]
pub(crate) fn record_operator_owner(
    epochs: &KeyedEpochs<LuaOperatorOwner>,
    owner: &LuaOperatorOwner,
) {
    if DEPTH.with(Cell::get) > 0 {
        OPERATOR_OWNERS.with(|owners| {
            owners
                .borrow_mut()
                .entry(owner.clone())
                .or_insert_with(|| epochs.epoch(owner));
        });
        note_read_seq(WriteKey::OperatorOwner(owner.clone()));
    }
}

/// Records a setmetatable binding read, stamped with the table site's
/// current write epoch on the metatable index.
#[inline]
pub(crate) fn record_metatable(
    epochs: &KeyedEpochs<InFiled<TextRange>>,
    site: &InFiled<TextRange>,
) {
    if DEPTH.with(Cell::get) > 0 {
        METATABLE_SITES.with(|sites| {
            sites
                .borrow_mut()
                .entry(site.clone())
                .or_insert_with(|| epochs.epoch(site));
        });
        note_read_seq(WriteKey::Metatable(site.clone()));
    }
}

/// Records a table-literal → owning-path read (`definition_site_owner` /
/// `canonical`), stamped with the literal's current write epoch on the
/// member index.
#[inline]
pub(crate) fn record_definition_site(
    epochs: &KeyedEpochs<InFiled<TextRange>>,
    site: &InFiled<TextRange>,
) {
    if DEPTH.with(Cell::get) > 0 {
        DEFINITION_SITES.with(|sites| {
            sites
                .borrow_mut()
                .entry(site.clone())
                .or_insert_with(|| epochs.epoch(site));
        });
        note_read_seq(WriteKey::DefinitionSite(site.clone()));
    }
}

/// Records a name-keyed gmod-metadata read (vgui panels, derma skins,
/// net/system registrations).
#[inline]
pub(crate) fn record_gmod_meta(name: &SmolStr) {
    if DEPTH.with(Cell::get) > 0 {
        GMOD_METAS.with(|metas| {
            metas.borrow_mut().insert(name.clone());
        });
        note_read_seq(WriteKey::GmodMeta(name.clone()));
    }
}

/// Records a whole-map enumeration of the type declarations — any
/// `TypeDecl` write stales it, so the merge checks writes by tag.
#[inline]
pub(crate) fn record_type_decl_scan() {
    if DEPTH.with(Cell::get) > 0 {
        TYPE_DECL_SCAN.with(|scan| scan.set(true));
    }
}

/// Records a `FileId`-keyed side-map read (namespaces, decl trees, per-file
/// signature lists, accessor calls), stamped with that slot's current write
/// epoch on the owning index — the earliest read wins, matching
/// `record_member_map`.
#[inline]
pub(crate) fn record_file_scoped(
    epochs: &KeyedEpochs<FileId>,
    file_id: &FileId,
    kind: FileScopedKind,
) {
    if DEPTH.with(Cell::get) > 0 {
        FILE_SCOPEDS.with(|files| {
            files
                .borrow_mut()
                .entry((*file_id, kind))
                .or_insert_with(|| epochs.epoch(file_id));
        });
        note_read_seq(WriteKey::FileScoped(*file_id));
    }
}

/// Index domains a read set records but per-key versions cannot cover:
/// their reads enumerate whole maps or names rather than a recordable key,
/// so a domain-wide counter answers "did anything here move since the
/// capture" instead. Coarser than a key version — any write stales every
/// reader of the domain — but the fixpoint's exit is movement-based, so an
/// over-stale round is bounded work, never wrongness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UntrackedDomain {
    /// `---` type declarations: only the `find_type_decls` whole-map scan
    /// consults this — keyed decl readers use `decl_key_epochs` instead.
    TypeDecl = 0,
    /// Name-keyed gmod metadata (vgui panels, derma skins, net/system
    /// registrations, signature-level gmod entries).
    GmodMeta = 1,
}

pub(crate) const UNTRACKED_DOMAIN_COUNT: usize = 2;

/// Which file-keyed side-map family a `file_scopeds` entry covers. The maps
/// live in different indexes and move on different schedules — a single
/// per-file epoch let an unrelated insert (a new signature) stale readers of
/// maps it never touched (the decl tree), so each family keeps its own slot,
/// and the epochs live on the index that owns the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FileScopedKind {
    /// `decl_trees` — declaration trees (scope and decl-object storage).
    DeclTree = 0,
    /// `file_namespace`, `file_using_namespace`, `file_types`.
    TypeFile = 1,
    /// `in_file_signatures` / `in_file_receiver_out_param_member_names`.
    SignatureFile = 2,
    /// `function_scope_ranges` — member-function scope windows.
    MemberScope = 3,
    /// `factory_bindings` — setmetatable factory call sites.
    MetatableFactory = 4,
    /// `file_metadata` — per-file gmod call metadata.
    GmodMetaFile = 5,
    /// `in_filed_operator_map` / `operators`.
    OperatorFile = 6,
}

pub(crate) const FILE_SCOPED_KIND_COUNT: usize = 7;

/// Which decl-keyed side map a `type_decls` entry covers. A base class
/// collects children as scripted classes resolve mid-fixpoint (`sub_types`),
/// but the same `LuaTypeDeclId` also keys `supers`, `type_bound_decls`,
/// `generic_params`, and the decl object itself — a single epoch per decl let
/// a new `X -> Panel` edge stale every reader of `Panel`'s stable maps, so
/// each family keeps its own slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TypeDeclKind {
    /// `full_name_type_map` — the decl object and its locations.
    Object = 0,
    /// `supers` — a decl's own super edges.
    Supers = 1,
    /// `sub_types` — a base's child list.
    SubTypes = 2,
    /// `generic_params`.
    GenericParams = 3,
}

/// Routes a `file_scopeds` entry to the epoch store on the index that owns
/// the map — the is_stale counterpart of `record_file_scoped`.
#[inline]
fn file_scoped_write_epoch(db: &crate::DbIndex, file_id: &FileId, kind: FileScopedKind) -> u64 {
    match kind {
        FileScopedKind::DeclTree => db.get_decl_index().tree_write_epochs.epoch(file_id),
        FileScopedKind::TypeFile => db.get_type_index().file_write_epochs.epoch(file_id),
        FileScopedKind::SignatureFile => db.get_signature_index().file_write_epochs.epoch(file_id),
        FileScopedKind::MemberScope => db.get_member_index().file_write_epochs.epoch(file_id),
        FileScopedKind::MetatableFactory => {
            db.get_metatable_index().file_write_epochs.epoch(file_id)
        }
        FileScopedKind::GmodMetaFile => db
            .get_gmod_class_metadata_index()
            .file_write_epochs
            .epoch(file_id),
        FileScopedKind::OperatorFile => db.get_operator_index().file_write_epochs.epoch(file_id),
    }
}

/// The owning store's clock for one `file_scopeds` kind — the capture-side
/// counterpart of `file_scoped_write_epoch`.
#[inline]
fn file_scoped_store_clock(db: &crate::DbIndex, kind: FileScopedKind) -> u64 {
    match kind {
        FileScopedKind::DeclTree => db.get_decl_index().tree_write_epochs.clock(),
        FileScopedKind::TypeFile => db.get_type_index().file_write_epochs.clock(),
        FileScopedKind::SignatureFile => db.get_signature_index().file_write_epochs.clock(),
        FileScopedKind::MemberScope => db.get_member_index().file_write_epochs.clock(),
        FileScopedKind::MetatableFactory => db.get_metatable_index().file_write_epochs.clock(),
        FileScopedKind::GmodMetaFile => {
            db.get_gmod_class_metadata_index().file_write_epochs.clock()
        }
        FileScopedKind::OperatorFile => db.get_operator_index().file_write_epochs.clock(),
    }
}

/// Every file-scoped store's clock in `FileScopedKind` order, for the
/// capture.
fn file_scoped_store_clocks(db: &crate::DbIndex) -> [u64; FILE_SCOPED_KIND_COUNT] {
    [
        file_scoped_store_clock(db, FileScopedKind::DeclTree),
        file_scoped_store_clock(db, FileScopedKind::TypeFile),
        file_scoped_store_clock(db, FileScopedKind::SignatureFile),
        file_scoped_store_clock(db, FileScopedKind::MemberScope),
        file_scoped_store_clock(db, FileScopedKind::MetatableFactory),
        file_scoped_store_clock(db, FileScopedKind::GmodMetaFile),
        file_scoped_store_clock(db, FileScopedKind::OperatorFile),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InferenceReadVersion {
    type_epoch: u64,
    return_epoch: u64,
    param_epoch: u64,
    module_epoch: u64,
    special_epoch: u64,
    contrib_epoch: u64,
    dynamic_epoch: u64,
    untracked_epochs: [u64; UNTRACKED_DOMAIN_COUNT],
    /// Store clocks for the per-key epoch families carried inside `reads`:
    /// an unchanged clock proves every key epoch inside that store is
    /// unchanged, so the family's entries need no walk.
    decl_clock: u64,
    claim_clock: u64,
    default_clock: u64,
    file_clocks: [u64; FILE_SCOPED_KIND_COUNT],
    /// The member index's `move_clock`: every owner/key/member-id epoch is
    /// stamped from it, so equality covers all three `member_reads_changed`
    /// families.
    member_clock: u64,
    /// The formerly-untracked domains' keyed stores: `type_decls` on the
    /// type index, `vgui_type_decls` on the gmod metadata index,
    /// `definition_facts` on the type index, `global_paths` and
    /// `definition_sites` on the member index, `operator_owners` on the
    /// operator index, `metatable_sites` on the metatable index.
    decl_key_clock: u64,
    bound_decl_clock: u64,
    vgui_decl_clock: u64,
    fact_clock: u64,
    global_path_clock: u64,
    site_clock: u64,
    operator_clock: u64,
    metatable_clock: u64,
}

impl InferenceReadVersion {
    pub(crate) fn capture(db: &crate::DbIndex) -> Self {
        Self {
            type_epoch: db.get_type_index().type_writes(),
            return_epoch: db.get_signature_index().return_writes(),
            param_epoch: db.get_signature_index().param_writes(),
            module_epoch: db.get_module_index().module_writes(),
            special_epoch: db.get_flow_index().special_call_writes(),
            contrib_epoch: db.get_call_site_param_index().contrib_writes(),
            dynamic_epoch: db.get_dynamic_field_index().dynamic_writes(),
            untracked_epochs: db.untracked_write_epochs(),
            decl_clock: db.get_decl_index().decl_write_epochs.clock(),
            claim_clock: db.get_type_index().claim_write_epochs.clock(),
            default_clock: db.get_property_index().default_write_epochs.clock(),
            file_clocks: file_scoped_store_clocks(db),
            member_clock: db.get_member_index().move_clock(),
            decl_key_clock: db.get_type_index().decl_key_epochs.clock(),
            bound_decl_clock: db.get_type_index().bound_decl_epochs.clock(),
            vgui_decl_clock: db.get_gmod_class_metadata_index().decl_key_epochs.clock(),
            fact_clock: db.get_type_index().fact_epochs.clock(),
            global_path_clock: db.get_member_index().global_path_epochs.clock(),
            site_clock: db.get_member_index().site_epochs.clock(),
            operator_clock: db.get_operator_index().operator_epochs.clock(),
            metatable_clock: db.get_metatable_index().site_epochs.clock(),
        }
    }

    pub(crate) fn is_stale(&self, db: &crate::DbIndex, reads: &InferenceReadSet) -> bool {
        // An unchanged capture covers every epoch-keyed read: the store
        // clocks prove each per-key epoch inside `reads` is what it was, and
        // `member_clock` covers the member-side versions. Only the slot
        // content probes still need their walk.
        if Self::capture(db) == *self {
            return self.missing_slots_changed(db, reads);
        }
        self.value_reads_changed(db, reads) || self.member_reads_changed(db, reads)
    }

    /// The missed member-slot probes compare live slot contents against the
    /// recorded counts — they observe map state, not epochs, so no clock
    /// capture can skip them.
    fn missing_slots_changed(&self, db: &crate::DbIndex, reads: &InferenceReadSet) -> bool {
        reads.missing_member_slots.iter().any(|((owner, key), (member_count, dynamic_count))| {
            db.get_member_index()
                .get_member_count_for_owner_key(owner, key)
                != *member_count
                || matches!(key, LuaMemberKey::Name(name) if crate::dynamic_field_owner_of(db, owner).is_some_and(|dynamic_owner| {
                    db.get_dynamic_field_index().field_definitions(&dynamic_owner, name.as_str()).len() != *dynamic_count
                }))
        })
    }

    pub(crate) fn member_reads_changed(
        &self,
        db: &crate::DbIndex,
        reads: &InferenceReadSet,
    ) -> bool {
        // Every owner/key/member-id epoch is stamped from `move_clock`: an
        // unchanged capture-side clock proves all three families fresh.
        if self.member_clock == db.get_member_index().move_clock() {
            return false;
        }
        reads
            .member_maps
            .iter()
            .any(|(owner, version)| db.get_member_index().read_version(owner) != *version)
            || reads
                .member_keys
                .iter()
                .any(|(key, version)| db.get_member_index().key_read_version(key) != *version)
            || reads.member_ids.iter().any(|(member_id, epoch)| {
                db.get_member_index().member_object_version(member_id) != *epoch
            })
    }

    fn value_reads_changed(&self, db: &crate::DbIndex, reads: &InferenceReadSet) -> bool {
        // Every family is keyed by write stamps drawn from a shared counter:
        // a family counter that never moved since the capture proves none of
        // its keys did, so the walk is skipped outright.
        let type_index = db.get_type_index();
        let signature_index = db.get_signature_index();
        (type_index.type_writes() > self.type_epoch
            && reads
                .type_owners
                .iter()
                .any(|key| type_index.type_write_version(key) > self.type_epoch))
            || (signature_index.return_writes() > self.return_epoch
                && reads
                    .signatures
                    .iter()
                    .any(|key| signature_index.return_write_version(key) > self.return_epoch))
            || (signature_index.param_writes() > self.param_epoch
                && reads
                    .params
                    .iter()
                    .any(|key| signature_index.param_write_version(key) > self.param_epoch))
            || (db.get_module_index().module_writes() > self.module_epoch
                && reads
                    .modules
                    .iter()
                    .any(|key| db.get_module_index().module_write_version(key) > self.module_epoch))
            || (db.get_flow_index().special_call_writes() > self.special_epoch
                && reads.special_calls.iter().any(|(file, pos)| {
                    db.get_flow_index().special_call_write_version(file, pos) > self.special_epoch
                }))
            || (db.get_call_site_param_index().contrib_writes() > self.contrib_epoch
                && reads.contribs.iter().any(|(sig, param)| {
                    db.get_call_site_param_index()
                        .contrib_write_version(sig, *param)
                        > self.contrib_epoch
                }))
            || (db.get_dynamic_field_index().dynamic_writes() > self.dynamic_epoch
                && reads.dynamic_fields.iter().any(|key| {
                    db.get_dynamic_field_index().dynamic_write_version(key) > self.dynamic_epoch
                }))
            || self.missing_slots_changed(db, reads)
            || self.untracked_reads_changed(db, reads)
    }

    /// The reads without their own per-key versions: whole-map scans and
    /// name-keyed gmod metadata still share domain counters (`type_decl_scan`
    /// answers for the entire decl map; `gmod_metas` span two indexes), and
    /// the keyed families walk their per-key epochs — gated on the owning
    /// store's clock so a family whose store never moved skips the walk.
    /// Counters live on the owning indexes
    /// (`DbIndex::untracked_write_epochs` sums the two domains shared between
    /// indexes), so a write on another `DbIndex` cannot stale this
    /// instance's readers.
    fn untracked_reads_changed(&self, db: &crate::DbIndex, reads: &InferenceReadSet) -> bool {
        let now = db.untracked_write_epochs();
        let epoch = |domain: UntrackedDomain| self.untracked_epochs[domain as usize];
        let changed = |domain: UntrackedDomain| epoch(domain) != now[domain as usize];
        (reads.type_decl_scan && changed(UntrackedDomain::TypeDecl))
            || (!reads.gmod_metas.is_empty() && changed(UntrackedDomain::GmodMeta))
            || (self.decl_key_clock != db.get_type_index().decl_key_epochs.clock()
                && reads.type_decls.iter().any(|(decl_id, epoch)| {
                    db.get_type_index().decl_key_epochs.epoch(decl_id) != *epoch
                }))
            || (self.bound_decl_clock != db.get_type_index().bound_decl_epochs.clock()
                && reads
                    .bound_decls
                    .iter()
                    .any(|(key, epoch)| db.get_type_index().bound_decl_epochs.epoch(key) != *epoch))
            || (self.vgui_decl_clock != db.get_gmod_class_metadata_index().decl_key_epochs.clock()
                && reads.vgui_type_decls.iter().any(|(decl_id, epoch)| {
                    db.get_gmod_class_metadata_index()
                        .decl_key_epochs
                        .epoch(decl_id)
                        != *epoch
                }))
            || (self.fact_clock != db.get_type_index().fact_epochs.clock()
                && reads.definition_facts.iter().any(|(definition, epoch)| {
                    db.get_type_index().fact_epochs.epoch(definition) != *epoch
                }))
            || (self.global_path_clock != db.get_member_index().global_path_epochs.clock()
                && reads.global_paths.iter().any(|(path, epoch)| {
                    db.get_member_index().global_path_epochs.epoch(path) != *epoch
                }))
            || (self.site_clock != db.get_member_index().site_epochs.clock()
                && reads
                    .definition_sites
                    .iter()
                    .any(|(site, epoch)| db.get_member_index().site_epochs.epoch(site) != *epoch))
            || (self.operator_clock != db.get_operator_index().operator_epochs.clock()
                && reads.operator_owners.iter().any(|(owner, epoch)| {
                    db.get_operator_index().operator_epochs.epoch(owner) != *epoch
                }))
            || (self.metatable_clock != db.get_metatable_index().site_epochs.clock()
                && reads.metatable_sites.iter().any(|(site, epoch)| {
                    db.get_metatable_index().site_epochs.epoch(site) != *epoch
                }))
            || reads.file_scopeds.iter().any(|((file_id, kind), epoch)| {
                // Skip the per-file epoch read when the kind's whole store
                // is unmoved since the capture.
                self.file_clocks[*kind as usize] != file_scoped_store_clock(db, *kind)
                    && file_scoped_write_epoch(db, file_id, *kind) != *epoch
            })
            || (self.decl_clock != db.get_decl_index().decl_write_epochs.clock()
                && reads.decl_miscs.iter().any(|(decl_id, epoch)| {
                    db.get_decl_index().decl_write_epochs.epoch(decl_id) != *epoch
                }))
            || (self.claim_clock != db.get_type_index().claim_write_epochs.clock()
                && reads.decl_claims.iter().any(|(decl_id, epoch)| {
                    db.get_type_index().claim_write_epochs.epoch(decl_id) != *epoch
                }))
            || (self.default_clock != db.get_property_index().default_write_epochs.clock()
                && reads.decl_defaults.iter().any(|(decl_id, epoch)| {
                    db.get_property_index().default_write_epochs.epoch(decl_id) != *epoch
                }))
    }
}

impl InferredReturnReadRecord {
    /// A record stamped with the write epochs of the moment the reads were
    /// taken — one call site for every attempt path.
    pub(crate) fn captured(db: &crate::DbIndex, reads: InferenceReadSet) -> Self {
        Self {
            reads,
            type_epoch: db.get_type_index().type_writes(),
            return_epoch: db.get_signature_index().return_writes(),
            param_epoch: db.get_signature_index().param_writes(),
            module_epoch: db.get_module_index().module_writes(),
            special_epoch: db.get_flow_index().special_call_writes(),
            contrib_epoch: db.get_call_site_param_index().contrib_writes(),
            dynamic_epoch: db.get_dynamic_field_index().dynamic_writes(),
            untracked_epochs: db.untracked_write_epochs(),
            decl_clock: db.get_decl_index().decl_write_epochs.clock(),
            claim_clock: db.get_type_index().claim_write_epochs.clock(),
            default_clock: db.get_property_index().default_write_epochs.clock(),
            file_clocks: file_scoped_store_clocks(db),
            member_clock: db.get_member_index().move_clock(),
            decl_key_clock: db.get_type_index().decl_key_epochs.clock(),
            bound_decl_clock: db.get_type_index().bound_decl_epochs.clock(),
            vgui_decl_clock: db.get_gmod_class_metadata_index().decl_key_epochs.clock(),
            fact_clock: db.get_type_index().fact_epochs.clock(),
            global_path_clock: db.get_member_index().global_path_epochs.clock(),
            site_clock: db.get_member_index().site_epochs.clock(),
            operator_clock: db.get_operator_index().operator_epochs.clock(),
            metatable_clock: db.get_metatable_index().site_epochs.clock(),
        }
    }

    /// Whether anything the recorded attempt read has since moved — value
    /// reads and the member maps/keys it consulted. Member reads only exist
    /// for scopes armed with [`arm_with_member_maps`]; an empty set can never
    /// be stale, so unarmed records keep their old behavior.
    pub(crate) fn is_stale(&self, db: &crate::DbIndex) -> bool {
        InferenceReadVersion {
            type_epoch: self.type_epoch,
            return_epoch: self.return_epoch,
            param_epoch: self.param_epoch,
            module_epoch: self.module_epoch,
            special_epoch: self.special_epoch,
            contrib_epoch: self.contrib_epoch,
            dynamic_epoch: self.dynamic_epoch,
            untracked_epochs: self.untracked_epochs,
            decl_clock: self.decl_clock,
            claim_clock: self.claim_clock,
            default_clock: self.default_clock,
            file_clocks: self.file_clocks,
            member_clock: self.member_clock,
            decl_key_clock: self.decl_key_clock,
            bound_decl_clock: self.bound_decl_clock,
            vgui_decl_clock: self.vgui_decl_clock,
            fact_clock: self.fact_clock,
            global_path_clock: self.global_path_clock,
            site_clock: self.site_clock,
            operator_clock: self.operator_clock,
            metatable_clock: self.metatable_clock,
        }
        .is_stale(db, &self.reads)
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use crate::{
        DbIndex, LuaDeclId, LuaIndex, LuaMember, LuaMemberFeature, LuaMemberId, LuaType,
        LuaTypeCache,
    };
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::TextRange;

    #[test]
    fn recorded_types_ignore_unrelated_writes() {
        let mut db = DbIndex::new();
        let owner = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), 1.into()));
        let other = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), 2.into()));
        let version = InferenceReadVersion::capture(&db);
        arm();
        record(&owner);
        let reads = disarm();
        db.get_type_index_mut()
            .bind_type(other, LuaTypeCache::InferType(LuaType::String));
        assert!(!version.is_stale(&db, &reads));
        db.get_type_index_mut()
            .bind_type(owner, LuaTypeCache::InferType(LuaType::Integer));
        assert!(version.is_stale(&db, &reads));
    }

    #[test]
    fn member_reads_invalidate_on_add_rekey_delete_and_reopen() {
        let mut db = DbIndex::new();
        let file = FileId::new(1);
        let owner = LuaMemberOwner::Element(crate::InFiled::new(
            file,
            TextRange::new(0.into(), 10.into()),
        ));
        let id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(1.into(), 2.into()),
            ),
            file,
        );
        // Each collect pairs the reads with the write epochs of the same
        // moment, the way `InferredReturnReadRecord::captured` does — the
        // member-object domain makes a stale version/read pairing visible.
        let collect = |db: &DbIndex| {
            let version = InferenceReadVersion::capture(db);
            arm_with_member_maps();
            let _ = db.get_member_index().get_members(&owner);
            (version, disarm())
        };
        let member = || {
            LuaMember::new(
                id,
                LuaMemberKey::Name("x".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            )
        };
        let (version, reads) = collect(&db);
        db.get_member_index_mut()
            .add_member(owner.clone(), member());
        assert!(version.is_stale(&db, &reads));
        let (version, reads) = collect(&db);
        assert!(!version.is_stale(&db, &reads));
        db.get_member_index_mut()
            .rekey_member(id, LuaMemberKey::Name("y".into()));
        assert!(version.is_stale(&db, &reads));
        let (version, reads) = collect(&db);
        db.get_member_index_mut().remove_files(&[file]);
        assert!(version.is_stale(&db, &reads));
        let (version, reads) = collect(&db);
        db.get_member_index_mut().add_member(owner, member());
        assert!(version.is_stale(&db, &reads));
    }

    /// A recorded type-decl read stales on a write anywhere in the decl
    /// domain — the counter is domain-wide, so only the stale direction is
    /// asserted (other tests share it).
    #[test]
    fn type_decl_read_stales_on_decl_write() {
        let mut db = DbIndex::new();
        let file = FileId::new(910);
        let decl_id = LuaTypeDeclId::global("EpochTypeDecl");
        let version = InferenceReadVersion::capture(&db);
        arm();
        record_type_decl(
            &db.get_type_index().decl_key_epochs,
            &decl_id,
            TypeDeclKind::Object,
        );
        let reads = disarm();
        db.get_type_index_mut().add_type_decl(
            file,
            crate::LuaTypeDecl::new(
                file,
                TextRange::new(0.into(), 1.into()),
                "EpochTypeDecl".to_string(),
                crate::LuaDeclTypeKind::Class,
                crate::LuaTypeFlag::None.into(),
                decl_id,
            ),
        );
        assert!(version.is_stale(&db, &reads));
    }

    /// A recorded member-object read stales on a member-object write
    /// (`rekey_member` bumps the domain), again only in the stale direction.
    #[test]
    fn member_object_read_stales_on_member_write() {
        let mut db = DbIndex::new();
        let file = FileId::new(911);
        let owner = LuaMemberOwner::Element(crate::InFiled::new(
            file,
            TextRange::new(0.into(), 10.into()),
        ));
        let id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(1.into(), 2.into()),
            ),
            file,
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                id,
                LuaMemberKey::Name("x".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        let version = InferenceReadVersion::capture(&db);
        arm();
        let _ = db.get_member_index().get_member(&id);
        let reads = disarm();
        db.get_member_index_mut()
            .rekey_member(id, LuaMemberKey::Name("y".into()));
        assert!(version.is_stale(&db, &reads));
    }

    /// File-scoped epochs are per (file, family): a write to another file or
    /// another family leaves a recorded read fresh, a write to the recorded
    /// slot stales it.
    #[test]
    fn file_scoped_read_stales_only_on_own_slot_write() {
        let db_file = FileId::new(991);
        let mut db = DbIndex::new();
        let version = InferenceReadVersion::capture(&db);
        arm();
        record_file_scoped(
            &db.get_decl_index().tree_write_epochs,
            &db_file,
            FileScopedKind::DeclTree,
        );
        let reads = disarm();
        db.get_decl_index_mut()
            .tree_write_epochs
            .note(FileId::new(992));
        db.get_signature_index_mut().file_write_epochs.note(db_file);
        assert!(!version.is_stale(&db, &reads));
        db.get_decl_index_mut().tree_write_epochs.note(db_file);
        assert!(version.is_stale(&db, &reads));
    }
}
