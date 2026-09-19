//! Replayable record of every index write a file's analysis performed.
//!
//! The parallel Lua walk runs each file against a private copy-on-write fork of
//! the [`DbIndex`]. A fork's writes live and die inside it, so while the walk
//! runs the mutating index methods also push a replayable record here: the keys
//! the write touched plus a closure that re-issues the same call against the
//! canonical index. When the file finishes, the armed scope drains its log into
//! the file's outcome, and the merge thread replays the writes in file order.
//!
//! Replaying the *call* rather than the fork's final value keeps merge
//! semantics serial-exact: `bind_type` re-derives its merge against live state,
//! `add_member` appends to the live owner map, and so on — exactly what the
//! serial walk's call would have done at that program point.
//!
//! The log is a thread local armed like the read-set collector: outermost arm
//! clears, each file drains at its boundary. Mut methods pay one thread-local
//! flag read on the serial path and nothing more.

use std::cell::{Cell, RefCell};

use crate::db_index::r#type::read_set::WriteKey;
use crate::{DbIndex, FileId, LuaDeclId, LuaMemberId, LuaSignatureId, LuaTypeDeclId};

/// One index domain, tagged on every logged write so the merge can (a) answer
/// "did file f write to a domain an earlier chunk's file also wrote" without
/// comparing key spaces, and (b) detect instrumentation gaps — a domain the
/// fork actually mutated whose ops never logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Domain {
    Type,
    Member,
    Signature,
    Module,
    Metatable,
    Operator,
    Property,
    Flow,
    Decl,
    Reference,
    AccessorFunc,
    AccessorFuncCall,
    CallSiteParam,
    GmodClass,
    GmodInfer,
    GmodLoad,
    GmodNetwork,
    DynamicField,
    Dependency,
    NumericRange,
    Global,
    Diagnostic,
    JsonSchema,
    /// The type-erased helper caches on `DbIndex` — outside the domain Arcs,
    /// so no `get_*_mut` ever sets this bit; ops tagged with it only dirty
    /// the [`UntrackedDomain::HelperCache`] read mask.
    HelperCache,
}

/// One recorded write: the domain it lands in, the slots it touched, and the
/// replayable call.
///
/// `keys` feeds the merge's prefix set — later files' read sets are intersected
/// with it to detect reads the fork could not have seen. `apply` re-issues the
/// write against the canonical index at merge time.
pub(crate) struct LoggedOp {
    pub domain: Domain,
    pub keys: Box<[WriteKey]>,
    pub apply: Box<dyn FnOnce(&mut DbIndex) + Send>,
}

/// Domains whose reads the walk does not record per-key. Reading one of them
/// sets a bit in [`UNTRACKED_READS`]; a file that read a domain an earlier
/// chunk's file wrote is re-run serially — coarse, but these domains are
/// mutated rarely enough during the walk that the over-selection is cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum UntrackedDomain {
    Operator = 0,
    Metatable = 1,
    GmodInfer = 2,
    GmodClass = 3,
    /// The decl feature cache (`decl_features`) and decl write claims —
    /// decl-anchored side entries inference consults without recording.
    DeclMisc = 4,
    /// Member global-path metadata: definition sites, path provenance, class
    /// candidate flags.
    MemberGlobal = 5,
    /// Signature auxiliary maps: inferred guard dependencies/facts, return
    /// consumers, payload holders, receiver out-param names.
    SignatureAux = 6,
    /// Definition-anchored inferred facts (`bind_definition_fact`).
    DefinitionFact = 7,
    /// Property index reads (file-scoped writes are still covered per-file,
    /// this bit guards the rare cross-decl lookups).
    Property = 8,
    /// The gmod net-helper registry and per-file helper scans — read through
    /// `DbIndex` cache slots outside the domain Arcs.
    HelperCache = 9,
}

/// Maps the untracked read domain to its mask bit.
#[inline]
pub(crate) fn untracked_bit(domain: UntrackedDomain) -> u32 {
    1 << (domain as u8)
}

/// An object the walk borrowed mutably through a `get_*_mut` accessor.
///
/// The accessor cannot see what the caller pokes, so the id is noted here and
/// the merge emits a whole-object replace at the file boundary: the object is
/// keyed by a file-unique id, so the fork's final value is what the serial
/// walk's pokes would have produced. Replay order is safe — every create op
/// for the same id ran earlier in the same file's log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum MutRef {
    Signature(LuaSignatureId),
    Member(LuaMemberId),
    TypeDecl(LuaTypeDeclId),
    Decl(LuaDeclId),
    /// The file's whole declaration tree — `get_decl_tree_mut` hands the tree
    /// itself out, so the boundary replace re-installs the fork's copy.
    DeclTree(FileId),
    Module(FileId),
}

/// One mutable-borrow note plus the number of ops logged before it — the
/// position the note takes in the file's write stream. The merge replaces the
/// object after the last op replays, so any *later* op in the same domain
/// that touches the object's slot would have seen the post-poke value in the
/// serial walk but the pre-poke value here; such a file is re-run serially.
#[derive(Debug, Clone)]
pub(crate) struct MutRefNote {
    pub reference: MutRef,
    pub ordinal: usize,
}

impl MutRef {
    /// The write keys a boundary replace for this object must publish, so a
    /// later file that read the same slot is rerun.
    pub(crate) fn keys(&self) -> Box<[WriteKey]> {
        match self {
            MutRef::Signature(id) => Box::new([WriteKey::Signature(*id)]),
            MutRef::Member(id) => Box::new([WriteKey::MemberId(*id)]),
            MutRef::TypeDecl(id) => {
                Box::new([WriteKey::TypeDecl(id.clone()), WriteKey::TypeDeclScan])
            }
            MutRef::Decl(id) => Box::new([WriteKey::DeclMisc(*id)]),
            MutRef::DeclTree(file_id) => Box::new([WriteKey::FileScoped(*file_id)]),
            MutRef::Module(file_id) => Box::new([WriteKey::Module(*file_id)]),
        }
    }

    /// The domain a boundary replace for this object mutates; compared
    /// against the fork's pointer-diff to catch uninstrumented writes.
    pub(crate) fn domain(&self) -> Domain {
        match self {
            MutRef::Signature(_) => Domain::Signature,
            MutRef::Member(_) => Domain::Member,
            MutRef::TypeDecl(_) => Domain::Type,
            MutRef::Decl(_) | MutRef::DeclTree(_) => Domain::Decl,
            MutRef::Module(_) => Domain::Module,
        }
    }

    /// The slot set an op may not touch after this object's first mutable
    /// borrow — identical to [`Self::keys`]: an op only observes the poke
    /// when its keys name the object itself or a scan that iterates it (the
    /// `TypeDeclScan` sentinel already rides in `keys`). File-scoped keys
    /// deliberately stay out — they would falsely conflict with the same
    /// file's ordinary writes.
    pub(crate) fn touch_keys(&self) -> Box<[WriteKey]> {
        self.keys()
    }
}

impl Domain {
    /// Mask bit for the touched-domain word. Every `get_*_mut` accessor sets
    /// its bit while armed, so a file whose fork took a mutable handle
    /// without logging a matching op (an uninstrumented write path) is
    /// detected at the boundary and re-run serially.
    #[inline]
    pub(crate) fn bit(self) -> u32 {
        1 << (self as u8)
    }
}

thread_local! {
    /// Arming depth. The worker arms around each file's walk; recorders push
    /// only while armed, so the serial walk and other phases pay a single
    /// thread-local read per write.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static OPS: RefCell<Vec<LoggedOp>> = const { RefCell::new(Vec::new()) };
    /// Mutable borrows handed out while armed; drained with the ops at the
    /// file boundary. Each note carries its position in the op stream so the
    /// merge can order the boundary replace against the logged writes.
    static MUT_REFS: RefCell<Vec<MutRefNote>> = const { RefCell::new(Vec::new()) };
    /// Coarse per-domain read flags for domains without per-key read
    /// recording; see [`UntrackedDomain`].
    static UNTRACKED_READS: Cell<u32> = const { Cell::new(0) };
    /// Per-domain bits set by every `get_*_mut` accessor — a mutable handle
    /// taken without a matching logged op marks an uninstrumented write
    /// path, and the merge re-runs that file serially.
    static TOUCHED: Cell<u32> = const { Cell::new(0) };
    /// First caller site per touched domain — diagnostics for the coverage
    /// audit, populated only while `GLUALS_PARWALK_STATS` is set.
    static TOUCH_SITES: RefCell<Vec<(u8, &'static std::panic::Location<'static>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Starts collecting ops. The first arm clears the log; nested arms share the
/// outer collection.
pub(crate) fn arm_ops() {
    if DEPTH.with(Cell::get) == 0 {
        OPS.with(|ops| ops.borrow_mut().clear());
        MUT_REFS.with(|refs| refs.borrow_mut().clear());
        UNTRACKED_READS.with(|mask| mask.set(0));
        TOUCHED.with(|mask| mask.set(0));
        TOUCH_SITES.with(|sites| sites.borrow_mut().clear());
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
}

/// Whether any scope is currently recording — checked by every mutating index
/// method before it builds a closure.
#[inline(always)]
pub(crate) fn recording() -> bool {
    DEPTH.with(Cell::get) > 0
}

/// Stops collecting and hands back the recorded ops plus the untracked-domain
/// read mask and the touched-domain mask. The outermost disarm takes the
/// log; a nested one returns empty and leaves the outer collection intact
/// (ops are not `Clone`).
pub(crate) fn drain_ops() -> (Vec<LoggedOp>, Vec<MutRefNote>, u32, u32) {
    let depth = DEPTH.with(Cell::get);
    DEPTH.with(|d| d.set(depth.saturating_sub(1)));
    if depth <= 1 {
        let ops = OPS.with(|ops| std::mem::take(&mut *ops.borrow_mut()));
        let mut_refs = MUT_REFS.with(|refs| std::mem::take(&mut *refs.borrow_mut()));
        let reads = UNTRACKED_READS.with(|mask| mask.replace(0));
        let touched = TOUCHED.with(|mask| mask.replace(0));
        (ops, mut_refs, reads, touched)
    } else {
        (Vec::new(), Vec::new(), 0, 0)
    }
}

/// First `get_*_mut` caller site per domain bit, in domain order — populated
/// only when `GLUALS_PARWALK_STATS` is set, drained with the ops.
pub(crate) fn drain_touch_sites() -> Vec<(u8, &'static std::panic::Location<'static>)> {
    TOUCH_SITES.with(|sites| std::mem::take(&mut *sites.borrow_mut()))
}

/// Records that the armed scope borrowed `reference` mutably. Called by the
/// `get_*_mut` accessors before handing out the reference; the merge turns
/// each note into a whole-object replace op at the file boundary.
#[inline]
pub(crate) fn note_mut_ref(reference: MutRef) {
    if DEPTH.with(Cell::get) == 0 {
        return;
    }
    let ordinal = OPS.with(|ops| ops.borrow().len());
    MUT_REFS.with(|refs| {
        refs.borrow_mut().push(MutRefNote { reference, ordinal });
    });
}

/// Records one write under the given domain and keys. `apply` must re-issue
/// the same index mutation it shadows when invoked with the canonical index;
/// it runs exactly once, on the merge thread.
#[inline]
pub(crate) fn push<F>(domain: Domain, keys: Box<[WriteKey]>, apply: F)
where
    F: FnOnce(&mut DbIndex) + Send + 'static,
{
    OPS.with(|ops| {
        ops.borrow_mut().push(LoggedOp {
            domain,
            keys,
            apply: Box::new(apply),
        });
    });
}

/// Convenience for the common single-key write.
#[inline]
pub(crate) fn push_one<F>(domain: Domain, key: WriteKey, apply: F)
where
    F: FnOnce(&mut DbIndex) + Send + 'static,
{
    push(domain, Box::new([key]), apply);
}

/// Marks that the armed scope read an untracked domain. Called by the
/// domain's `DbIndex` getter — every read funnels through it, so one note per
/// getter covers every read path the walk can take.
#[inline(always)]
pub(crate) fn note_untracked_read(domain: UntrackedDomain) {
    if DEPTH.with(Cell::get) == 0 {
        return;
    }
    UNTRACKED_READS.with(|mask| mask.set(mask.get() | untracked_bit(domain)));
}

/// Marks that the armed scope took a `&mut` handle on `domain`. Called by
/// every `get_*_mut` accessor on `DbIndex`; the merge compares this mask
/// against the domains that logged ops, so a write path that was never
/// instrumented cannot slip through silently.
#[inline(always)]
#[track_caller]
pub(crate) fn note_domain_touch(domain: Domain) {
    if DEPTH.with(Cell::get) == 0 {
        return;
    }
    TOUCHED.with(|mask| mask.set(mask.get() | domain.bit()));
    if STATS.load(std::sync::atomic::Ordering::Relaxed) {
        let site = std::panic::Location::caller();
        TOUCH_SITES.with(|sites| {
            let mut sites = sites.borrow_mut();
            if !sites.iter().any(|(d, _)| *d == domain as u8) {
                sites.push((domain as u8, site));
            }
        });
    }
}

/// Whether touch-site diagnostics are being collected — set from the
/// `GLUALS_PARWALK_STATS` env var at dispatch.
static STATS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Arms touch-site diagnostics — called once at parallel dispatch.
pub(crate) fn set_stats_enabled(enabled: bool) {
    STATS.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
