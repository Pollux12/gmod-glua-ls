//! Derivations: the re-runnable units that produce every inferred fact.
//!
//! A derivation is identified by the syntax site whose recipe produces it.
//! While one is evaluated, the index accessors report every fact they read
//! (with its fingerprint) and every fact they write here; the engine stores
//! the result in the [`ReaderGraph`] and re-evaluates exactly the readers of
//! facts whose fingerprint changed.

mod frames;
mod graph;
mod positions;
mod transport;

pub(crate) use transport::query_requires_canonical as worker_requires_canonical;
pub(crate) use transport::{
    ReadState, RecordingSnapshot, ReleaseQuarantine, TaskRecorder, TaskRecording, compute_recorded,
    note_observed_query_read, require_canonical_query, worker_query_readiness,
};

use std::cell::RefCell;

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;

mod paged;
pub use frames::{
    CALL_SITE_PARAMS_GATE, DIAGNOSTICS_PHASE_GATE, DYNAMIC_FIELDS_GATE, FORCE_PHASE_GATE, Gates,
    ORDERED_PHASE_GATE, OpenFrame, Provenance, Reuse, ScopeMark, check_cache, close_frame,
    consult_absorbed, consult_cycle, consult_gates, consult_realm, consult_scope, consult_unkeyed,
    frame_depth, new_stamp, note_recomputed, note_recomputing, note_written, open_frame,
    open_frame_absorbing, open_frame_viewed, report_frames, reuse, scope_mark, taint,
    take_reused_across,
};

/// A value an analyzer derivation stores on the index without the index
/// naming its type, fingerprinted on demand.
pub trait DerivedFact: std::any::Any + Send + Sync {
    fn fingerprint(&self) -> Fingerprint;

    /// The files other than `own` whose positions the value names.
    fn named_files(&self, _own: FileId) -> FxHashSet<FileId> {
        FxHashSet::default()
    }

    /// The value with the positions of `remap`'s file it names moved to their
    /// new place: `Unrelated` when none of them moved, `Lost` when one did not
    /// survive the edit.
    fn remap(&self, _remap: &crate::FileRemap) -> crate::Remap<std::sync::Arc<dyn DerivedFact>> {
        crate::Remap::Unrelated
    }
}

/// The workspace call roles and net-helper registry: the folds read it whole,
/// a per-file pass entry by entry, as [`FactKey::CallRole`].
/// Two stored facts are the same when they are one allocation.
impl PartialEq for dyn DerivedFact {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self, other)
    }
}

pub trait CallRoles: DerivedFact {
    /// Every entry.
    fn entries(&self) -> Vec<CallRoleKey>;

    /// One entry's fingerprint, [`ABSENT`] when there is none.
    fn entry_fingerprint(&self, key: &CallRoleKey) -> Fingerprint;
}

impl std::fmt::Debug for dyn CallRoles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallRoles")
    }
}

impl std::fmt::Debug for dyn DerivedFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DerivedFact")
    }
}
pub use graph::{KeptRecord, KeyIdx, KeyTable, ReaderGraph};
pub use positions::{
    EditMaps, FilePosition, Local, fingerprint_before_edits, fingerprinting,
    hash_apart_from_positions, hash_local, in_file, mapping_back, unmapped,
};

use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
use smol_str::SmolStr;

use crate::{
    DynamicFieldOwner, FileId, GlobalId, InFiled, LuaDeclId, LuaDefinitionId, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaOperatorOwner, LuaSemanticDeclId, LuaSignatureId,
    LuaTypeDeclId, LuaTypeOwner,
};

/// How far an engine run has settled. A read whose answer only holds once no
/// derivation can add a fact any more waits for the stage that answers it,
/// so the order derivations happen to run in cannot decide it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnalysisStage {
    /// Derivations still add facts: a read of one that has not landed, a
    /// field no writer declares included, stays unanswered, and so does a
    /// member slot some of whose writers are untyped.
    Open,
    /// A slot's untyped writers are left out while another has a type.
    SkipUntyped,
    /// Settled: what is still unanswered reads as a floor. Everything outside
    /// an engine run reads at this stage.
    #[default]
    Closed,
}

impl AnalysisStage {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Open => Some(Self::SkipUntyped),
            Self::SkipUntyped => Some(Self::Closed),
            Self::Closed => None,
        }
    }
}

/// A rule of reading that holds from one [`AnalysisStage`] on. A read that
/// consults one records it, so the engine re-evaluates the reader when the
/// rule starts to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageRule {
    /// A member slot's untyped writers are left out while another writer has
    /// a type.
    SkipUntypedWriters,
    /// A read of a fact that has not landed answers with a floor: unknown, or
    /// nil for a field no writer declares.
    FloorUnsettled,
    /// The dynamic fields their writers file once the statements settle are
    /// read; before then a run reads none, as a cold build does, rather than
    /// what an earlier run left.
    DynamicFieldsFiled,
}

impl StageRule {
    pub const ALL: [Self; 3] = [
        Self::SkipUntypedWriters,
        Self::FloorUnsettled,
        Self::DynamicFieldsFiled,
    ];

    /// The first stage the rule holds at.
    pub fn holds_from(self) -> AnalysisStage {
        match self {
            Self::SkipUntypedWriters => AnalysisStage::SkipUntyped,
            Self::FloorUnsettled => AnalysisStage::Closed,
            Self::DynamicFieldsFiled => DerivationKind::DynamicFieldWrite.first_stage(),
        }
    }

    /// Whether the rule holds at `stage`.
    pub fn holds_at(self, stage: AnalysisStage) -> bool {
        stage >= self.holds_from()
    }
}

/// One entry of the workspace call roles and net-helper registry, as a
/// per-file pass looks it up: present or absent, so an entry added later
/// reaches exactly the files that asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CallRoleKey {
    /// A global call path's annotated roles.
    Path(SmolStr),
    /// The annotated role presence of methods sharing a receiver method name.
    ReceiverMethod(SmolStr),
    /// The helper body a global function names.
    Global(GlobalId),
    /// The helper body a method name uniquely names.
    Method(SmolStr),
    /// The helper body a signature resolves to.
    Signature(LuaSignatureId),
}

/// One readable fact. Every analysis read of an index lands on exactly one
/// key, and [`crate::DbIndex::fact_fingerprint`] hashes exactly what a read
/// of that key can observe.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FactKey {
    /// A stored type and its fact metadata.
    TypeCache(LuaTypeOwner),
    /// The types a file's local fallbacks published over what its statements
    /// left (see [`writes_fallback_of`]).
    TypeFallbacks(FileId),
    /// A definition-anchored inferred fact.
    DefinitionFact(LuaDefinitionId),
    /// A `---` type declaration: its object, alias origin included, and the
    /// flags any of its locations carries, but not where it is declared.
    TypeDecl(LuaTypeDeclId),
    /// Where a type declaration is declared: its locations and their flags.
    TypeDeclLocations(LuaTypeDeclId),
    /// The files a type declaration is declared in and the flags each location
    /// carries, not where in the file. Read out of its
    /// [`FactKey::TypeDeclLocations`].
    TypeDeclFiles(LuaTypeDeclId),
    /// A type declaration's supertypes.
    TypeSupers(LuaTypeDeclId),
    /// A type declaration's distinct supertypes in order, not where each is
    /// declared. Read out of its [`FactKey::TypeSupers`].
    TypeSuperTypes(LuaTypeDeclId),
    /// A type declaration's super edges as the file and type of each in
    /// order, not where in the file. Read out of its [`FactKey::TypeSupers`].
    TypeSuperEdges(LuaTypeDeclId),
    /// The gmod realm at each of a type declaration's super edges in a file,
    /// in order. Read out of the file's [`FactKey::GmodRealm`]; the type index
    /// also notes it written when the edges in the file change.
    TypeSuperRealms(LuaTypeDeclId, FileId),
    /// A type declaration's generic parameters.
    TypeGenericParams(LuaTypeDeclId),
    /// A type declaration as the source layer declares it: whether it does,
    /// its kind and attribute type.
    SourceTypeDecl(LuaTypeDeclId),
    /// The type declaration locations and super edges one derivation added.
    TypeContrib(DerivationId),
    /// The class a registration turned a table literal into.
    TableClass(InFiled<TextRange>),
    /// The global-path class candidates one derivation proposed.
    PathClassContrib(DerivationId),
    /// The declarations one derivation synthesized.
    DeclContrib(DerivationId),
    /// The member-key references one derivation added.
    ReferenceContrib(DerivationId),
    /// What one dynamic-field write site filed: an assignment or function
    /// statement, or a call, by its range.
    DynamicFieldSite(InFiled<TextRange>),
    /// The direct subtypes of a type declaration.
    SubTypes(LuaTypeDeclId),
    /// The declarations of one file whose stored type names a class.
    BoundDecls(LuaTypeDeclId, FileId),
    /// The positional claim on a local's type.
    DeclClaim(LuaDeclId),
    /// Every type declaration (whole-map enumeration).
    TypeDeclScan,
    /// A file's namespace, `using` list and declared types.
    TypeFile(FileId),
    /// One declaration object.
    Decl(LuaDeclId),
    /// A file's whole declaration tree.
    DeclTree(FileId),
    /// A member object with its owner, flags and function scope.
    Member(LuaMemberId),
    /// A member object alone: its key, feature and global id. Read out of its
    /// [`FactKey::Member`].
    MemberObject(LuaMemberId),
    /// Where a member lives now: its owner, whether it is listed there, and
    /// the global path it was homed through. Its own writers and a transfer
    /// both move it.
    MemberHome(LuaMemberId),
    /// The class a scripted-class synthesis moved a member to, and whether a
    /// global path's class change leaves it there.
    MemberTransfer(LuaMemberId),
    /// A member's path provenance, flags and function scope. Read out of its
    /// [`FactKey::Member`].
    MemberMarks(LuaMemberId),
    /// The visible members of one owner under one key.
    MemberSlot(LuaMemberOwner, LuaMemberKey),
    /// Every member of one owner.
    MemberOwnerMap(LuaMemberOwner),
    /// The members of one owner whose key is an expression.
    MemberOwnerExprKeys(LuaMemberOwner),
    /// The members of one owner that are not methods: what a structural
    /// check against the owner compares.
    MemberOwnerFields(LuaMemberOwner),
    /// Whether one owner has a member map at all.
    MemberOwnerPresence(LuaMemberOwner),
    /// Every current member under one key, across owners.
    MemberKey(LuaMemberKey),
    /// A file's members and member-function scopes.
    MemberFile(FileId),
    /// A global path's definition sites, class and provenance.
    GlobalPath(GlobalId),
    /// The owner a table literal initialises.
    DefinitionSite(InFiled<TextRange>),
    /// A signature object.
    Signature(LuaSignatureId),
    /// The workspace's effective valid-guard mask for a signature.
    ValidGuard(LuaSignatureId),
    /// The positive guard inferred from a signature's body, and its owner.
    InferredGuard(LuaSignatureId),
    /// One file's valid-guard signatures.
    ValidGuardFile(FileId),
    /// The files that have valid-guard signatures.
    ValidGuardFiles,
    /// Whether a definition of this name is marked `valid_guard`.
    ValidGuardName(SmolStr),
    /// A file's signatures and receiver out-param names.
    SignatureFile(FileId),
    /// Out-param member names a receiver type is known to carry.
    ReceiverOutParam(SmolStr),
    /// A call-site-inferred parameter of a callee.
    Contrib(LuaSignatureId, usize),
    /// The call-site parameter evidence one file holds, every call's and its
    /// callback receivers'.
    CallSiteFile(FileId),
    /// The call-site parameter evidence one call contributed.
    CallSiteParam(InFiled<TextRange>),
    /// The call-site parameter evidence one file's callback receivers
    /// contributed.
    CallSiteCallbacks(FileId),
    /// A module's export and metadata.
    Module(FileId),
    /// A module's workspace, its meta flag and its legacy environments: what
    /// a reader asking where a file belongs sees, not its path or name.
    ModuleMeta(FileId),
    /// The modules whose name (last path segment) is this: every module
    /// lookup of a path ending in it.
    ModuleName(SmolStr),
    /// The modules filed at one full module path.
    ModulePath(SmolStr),
    /// The legacy `module()` namespaces under this first path segment.
    ModuleNamespace(SmolStr),
    /// The set of modules: their files, paths and workspaces.
    ModuleFiles,
    /// One call site's special-call facts.
    SpecialCall(FileId, TextSize),
    /// Every special-call effect of one file.
    SpecialCallFile(FileId),
    /// The special-call effects of one file that narrow one variable
    /// reference: their positions and targets, read out of its
    /// [`FactKey::SpecialCallFile`].
    SpecialCallFor(FileId, crate::VarRefId),
    /// What the callee of one call resolves to.
    CallTarget(InFiled<TextRange>),
    /// A file's flow tree and casts.
    FlowTree(FileId),
    /// One dynamic field (`None`: the owner's named fields as a whole).
    DynamicField(DynamicFieldOwner, Option<SmolStr>),
    /// An owner's wildcard (dynamic-key) definition sites.
    DynamicFieldWildcards(DynamicFieldOwner),
    /// An owner's members written through a finite set of names.
    DynamicFieldFinite(DynamicFieldOwner),
    /// The types that hold a direct dynamic field, by name.
    DynamicFieldName(SmolStr),
    /// An unattributed dynamic field, by name.
    UnattributedField(SmolStr),
    /// One operand owner's operator table.
    OperatorOwner(LuaOperatorOwner),
    /// A file's operators.
    OperatorFile(FileId),
    /// The operators one derivation installed.
    OperatorContrib(DerivationId),
    /// One setmetatable binding site.
    Metatable(InFiled<TextRange>),
    /// A file's setmetatable factory bindings.
    MetatableFactory(FileId),
    /// Name-keyed gmod metadata (vgui panels, derma skins, net/system names).
    GmodMeta(SmolStr),
    /// A file's gmod class call metadata.
    GmodClassFile(FileId),
    /// A file's `vgui.Register` and `derma.DefineControl` calls, read out of
    /// its [`FactKey::GmodClassFile`].
    GmodVguiRegistrations(FileId),
    /// The set of files that have vgui parent calls.
    VguiParentCallFiles,
    /// What one file's own vgui parent calls resolve to.
    VguiRelations(FileId),
    /// A vgui panel's parent chain.
    VguiChain(LuaTypeDeclId),
    /// A file's gmod realm, hook, system and scoped-class metadata.
    GmodInferFile(FileId),
    /// A file's realm as the workspace load graph decided it.
    GmodRealm(FileId),
    /// The realm and state mask at one offset of a file, read out of its
    /// [`FactKey::GmodRealm`].
    GmodRealmAt(FileId, TextSize),
    /// A signature as declared: its params with their docs and attributes,
    /// overloads, generic params and colon and vararg flags, and the local
    /// function declaration it is bound to. Read out of its
    /// [`FactKey::Signature`], which also holds what inference resolved.
    SignatureDecl(LuaSignatureId),
    /// Whether a signature returns `never`. Read out of its
    /// [`FactKey::Signature`].
    SignatureReturnsNever(LuaSignatureId),
    /// Whether a signature takes special-call params (string-template or
    /// constructor params, out-params, return aliases). Read out of its
    /// [`FactKey::Signature`].
    SpecialCallSignature(LuaSignatureId),
    /// The `---@realm` annotation covering one offset of a file, read out of
    /// its [`FactKey::GmodInferFile`].
    GmodMemberRealmAt(FileId, TextSize),
    /// A file's own realm evidence (its realm branches and `---@realm`
    /// annotation), read out of its [`FactKey::GmodInferFile`].
    GmodRealmInputs(FileId),
    /// The gmod realm and hook aggregates across files.
    GmodInferScan,
    /// The gmod system registrations of one kind under one name, across files.
    GmodSystem(crate::db_index::GmodSystemRegistrationKind, SmolStr),
    /// A file's load edges, roots and execution environment.
    GmodLoadFile(FileId),
    /// A file's own contribution to the load graph: its execution-environment
    /// flow and dynamic loaders.
    LoadScan(FileId),
    /// Which files have a load scan.
    LoadScanFiles,
    /// A file's network flows, assembled from its calls' [`FactKey::NetFlows`].
    NetworkFile(FileId),
    /// Network flows by message name (whole-map).
    NetworkScan,
    /// The network flows sent under one message name.
    NetSends(SmolStr),
    /// The network flows received under one message name.
    NetReceives(SmolStr),
    /// Whether flows elsewhere materialize the net send defined at a site,
    /// which then stands for them.
    NetDefinitionUses(InFiled<TextRange>),
    /// The workspace net-helper registry.
    HelperRegistry,
    /// One entry of the workspace call roles and net-helper registry.
    CallRole(CallRoleKey),
    /// The global paths one file's syntax names.
    RoleMentions(FileId),
    /// One file's gmod helper scan: its call roles and net-helper definitions.
    HelperScan(FileId),
    /// The set of files that have a gmod helper scan.
    HelperScanFiles,
    /// Whether a name names a function that can produce a net operation.
    NetHelperName(SmolStr),
    /// The names the net operations are declared under.
    NetOperationNames,
    /// What one call is in the net subsystem: its role, the function body it
    /// expands into, and whether it can expand into a helper.
    NetCall(InFiled<TextRange>),
    /// One function body's net summary, by the body's block.
    NetHelper(InFiled<TextRange>),
    /// The net flows one call starts, receives or materializes.
    NetFlows(InFiled<TextRange>),
    /// A file's function-body summaries, from its syntax and local references.
    FunctionSummaries(FileId),
    /// A property (docs, attributes, visibility) of a semantic declaration.
    Property(LuaSemanticDeclId),
    /// A declaration's inferred string default.
    DeclDefault(LuaDeclId),
    /// Global declarations of one name.
    Global(SmolStr),
    /// Every global declaration (whole-map enumeration).
    GlobalScan,
    /// A file's reference index.
    References(FileId),
    /// Where one local declaration is read and written, read out of its
    /// file's [`FactKey::References`].
    DeclReferences(LuaDeclId),
    /// Name-keyed references across files.
    ReferencesByName(SmolStr),
    IndexReferenceFiles(SmolStr),
    /// The member-key references derivations resolved in one file.
    DerivedReferences(FileId),
    /// Numeric-range populations of one global table.
    NumericPopulation(SmolStr),
    /// Accessor-function annotations of one function name.
    AccessorFunc(SmolStr),
    /// A file's accessor-function calls.
    AccessorCalls(FileId),
    /// A file's dependency edges and the sites that name it.
    Dependency(FileId),
    /// The load-call dependency sites derivations found in a file.
    LoadSites(FileId),
    /// A file's diagnostic-control annotations.
    DiagnosticFile(FileId),
    /// A file's text and syntax tree.
    FileText(FileId),
    /// A file's path and its place in the file order.
    FilePath(FileId),
    /// Which file is at a path: the one there whose text readers see.
    PathFile(std::sync::Arc<std::path::Path>),
    /// One statement of a file, by its range: its kind and text, and the kind
    /// and range of every node between it and the top-level block.
    Syntax(InFiled<TextRange>),
    /// The innermost function statement or closure of a file around a
    /// position, by its range.
    FunctionScope(InFiled<TextSize>),
    /// The set of files in the workspace.
    FileSet,
    /// Whether a stage rule holds at the stage the engine run is in.
    StageRule(StageRule),
    /// Whether a method's receiver is authoritative, as its body infers it.
    ReceiverAuthority(LuaSignatureId),
    /// What a function passes to each callback parameter it calls.
    CallbackParams(LuaSignatureId),
}

/// A fact key with its hash taken once, so the sets and maps that record,
/// fingerprint and index it do not hash the key again.
#[derive(Debug, Clone)]
pub struct HashedKey {
    hash: u64,
    key: FactKey,
}

impl HashedKey {
    pub fn new(key: FactKey) -> Self {
        use std::hash::BuildHasher;
        Self {
            hash: rustc_hash::FxBuildHasher.hash_one(&key),
            key,
        }
    }

    pub fn key(&self) -> &FactKey {
        &self.key
    }
}

/// Sorts `keys` by their stored hashes on worker threads and drops repeats.
pub(crate) fn dedup_keys(keys: &mut Vec<HashedKey>) {
    use rayon::prelude::*;
    keys.par_sort_unstable_by_key(|key| key.hash);
    keys.dedup();
}

impl PartialEq for HashedKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.key == other.key
    }
}

impl Eq for HashedKey {}

impl std::hash::Hash for HashedKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl std::ops::Deref for HashedKey {
    type Target = FactKey;

    fn deref(&self) -> &FactKey {
        &self.key
    }
}

/// Hash of a fact's value, [`ABSENT`] when the key holds nothing.
pub type Fingerprint = u64;
pub const ABSENT: Fingerprint = 0;

/// What a read is recorded as having seen when the value it saw is not
/// known: no value fingerprints to it, so the read never holds.
pub const UNSEEN: Fingerprint = Fingerprint::MAX;

/// The fingerprint of a present value; never [`ABSENT`]. Fx hashes are
/// nearly linear in their input words, so the hash is mixed before use: a
/// wrapping sum of fingerprints would otherwise take equal steps for unequal
/// changes, such as two members moving between the same two keys.
pub fn fingerprint_of<T: std::hash::Hash + ?Sized>(value: &T) -> Fingerprint {
    use std::hash::Hasher;
    let mut hasher = rustc_hash::FxHasher::default();
    positions::hash_value(value, &mut hasher);
    mix(hasher.finish()).max(1)
}

/// MurmurHash3's 64-bit finalizer: a bijection in which every input bit
/// flips each output bit with probability about one half.
fn mix(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

/// An order-independent fingerprint of a set, kept up to date as members
/// come and go so reading it costs nothing however large the set grows.
#[derive(Debug, Clone, Copy, Default)]
pub struct SetFingerprint(Fingerprint);

/// Tasks fold members in and out apart, so their changes add up.
impl crate::db_index::cow_shards::TaskFork for SetFingerprint {
    fn task_fork(&self) -> Self {
        *self
    }

    fn wrote(&self, frozen: &Self) -> bool {
        self.0 != frozen.0
    }

    fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
        self.0 = self.0.wrapping_add(fork.0.wrapping_sub(frozen.0));
        false
    }
}

impl SetFingerprint {
    pub fn insert<T: std::hash::Hash + ?Sized>(&mut self, member: &T) {
        self.0 = self.0.wrapping_add(fingerprint_of(member));
    }

    pub fn remove<T: std::hash::Hash + ?Sized>(&mut self, member: &T) {
        self.0 = self.0.wrapping_sub(fingerprint_of(member));
    }

    pub fn get(&self) -> Fingerprint {
        self.0
    }
}

/// The fingerprint of an optional value.
pub fn fingerprint_opt<T: std::hash::Hash>(value: Option<&T>) -> Fingerprint {
    value.map_or(ABSENT, fingerprint_of)
}

impl FactKey {
    /// Whether the key holds one value that writes overwrite, as opposed to a
    /// view other keys determine (a member slot is the members filed under
    /// it; a scan is every entry) or a set that writers add to independently
    /// (see [`Self::is_additive`]). Before a derivation re-runs its exclusive
    /// keys are reset to the source layer, and every derivation that also
    /// wrote one of them re-runs with it.
    pub fn is_primary(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::TypeCache(_)
                | K::TypeFallbacks(_)
                | K::DefinitionFact(_)
                | K::DeclClaim(_)
                | K::Decl(_)
                | K::Member(_)
                | K::MemberTransfer(_)
                | K::Signature(_)
                | K::ValidGuard(_)
                | K::InferredGuard(_)
                | K::Module(_)
                | K::SpecialCall(..)
                | K::CallTarget(_)
                | K::FlowTree(_)
                | K::Metatable(_)
                | K::VguiChain(_)
                | K::VguiRelations(_)
                | K::GmodInferFile(_)
                | K::GmodRealm(_)
                | K::GmodLoadFile(_)
                | K::LoadScan(_)
                | K::NetCall(_)
                | K::NetHelper(_)
                | K::NetFlows(_)
                | K::HelperRegistry
                | K::CallRole(_)
                | K::HelperScan(_)
                | K::RoleMentions(_)
                | K::DeclDefault(_)
                | K::AccessorCalls(_)
                | K::LoadSites(_)
                | K::ValidGuardFile(_)
                | K::MetatableFactory(_)
                | K::DiagnosticFile(_)
                | K::TypeFile(_)
                | K::TypeContrib(_)
                | K::PathClassContrib(_)
                | K::DeclContrib(_)
                | K::ReferenceContrib(_)
                | K::OperatorContrib(_)
                | K::DynamicFieldSite(_)
                | K::GmodClassFile(_)
                | K::CallSiteParam(_)
                | K::CallSiteCallbacks(_)
        )
    }

    /// Whether only the owning file's source layer writes the key, so a read
    /// of it by one of that file's own derivations needs no record: a change
    /// replaces all of them.
    pub fn is_file_source(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::DeclTree(_)
                | K::References(_)
                | K::FlowTree(_)
                | K::TypeFile(_)
                | K::ModuleMeta(_)
                | K::DiagnosticFile(_)
                | K::FunctionSummaries(_)
        )
    }

    /// Whether the edit remap (`remap_into_file`) rewrites the positions of an
    /// edited file that this key's value names. A decl claim, module metadata,
    /// module names, an owner's presence and a member-key entry name no
    /// position outside the writer's own file, so they never name an edited
    /// file's position for a writer that did not run. A writer's share of an
    /// edited file's member fingerprint is an owner there, which the member
    /// remap re-homes. A file's realm names only its own branch ranges, and a
    /// load fact only the ranges of its incoming edges, which the gmod remaps
    /// move.
    pub fn follows_edits(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::DeclClaim(_)
                | K::MemberFile(_)
                | K::MemberOwnerPresence(_)
                | K::MemberKey(_)
                | K::Module(_)
                | K::ModuleMeta(_)
                | K::ModuleName(_)
                | K::ModulePath(_)
                | K::DynamicFieldSite(_)
                | K::TypeCache(_)
                | K::TypeFallbacks(_)
                | K::DefinitionFact(_)
                | K::Signature(_)
                | K::SignatureDecl(_)
                | K::SignatureReturnsNever(_)
                | K::SpecialCallSignature(_)
                | K::DynamicField(..)
                | K::DynamicFieldWildcards(_)
                | K::DynamicFieldFinite(_)
                | K::Metatable(_)
                | K::SpecialCall(..)
                | K::Member(_)
                | K::MemberObject(_)
                | K::MemberHome(_)
                | K::MemberTransfer(_)
                | K::MemberMarks(_)
                | K::MemberSlot(..)
                | K::MemberOwnerMap(_)
                | K::MemberOwnerExprKeys(_)
                | K::MemberOwnerFields(_)
                | K::Contrib(..)
                | K::CallSiteFile(_)
                | K::CallSiteParam(_)
                | K::CallSiteCallbacks(_)
                | K::NetCall(_)
                | K::NetHelper(_)
                | K::NetFlows(_)
                | K::NetworkFile(_)
                | K::NetworkScan
                | K::NetSends(_)
                | K::NetReceives(_)
                | K::NetDefinitionUses(_)
                | K::GmodRealm(_)
                | K::GmodLoadFile(_)
                | K::CallTarget(_)
                | K::ReceiverAuthority(_)
                | K::CallbackParams(_)
        )
    }

    /// For a key that folds every writer's share, the key holding `writer`'s
    /// own share, which it writes alongside: the locations and super edges a
    /// derivation added to a type declaration (it only ever declares classes,
    /// so they are all it adds), and a file's entries in the gmod scan and its
    /// system registrations.
    pub fn writer_share(&self, writer: DerivationId) -> Option<FactKey> {
        use FactKey as K;
        match self {
            K::TypeDecl(_) | K::TypeDeclLocations(_) | K::TypeSupers(_) => {
                Some(K::TypeContrib(writer))
            }
            K::GmodInferScan | K::GmodSystem(..) => Some(K::GmodInferFile(writer.file_id)),
            _ => None,
        }
    }

    /// Whether removing the owning file keeps what other files' derivations
    /// stored under the key: the member, dynamic-field and call-site indexes
    /// file each contribution under its contributing file, and the edit remap
    /// re-homes an owner or signature that moved, or reports its contributors
    /// when it is gone. A file's member fingerprint keeps the share of each
    /// owner another file's members still hold. The call-role fold keeps its
    /// entries until it runs again, which the re-indexed file's helper scan
    /// decides, and the load fold its realm and load facts, which the file's
    /// realm inputs and load scan decide.
    pub fn keeps_foreign_contributions(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::MemberSlot(..)
                | K::MemberFile(_)
                | K::MemberOwnerMap(_)
                | K::MemberOwnerExprKeys(_)
                | K::MemberOwnerFields(_)
                | K::MemberOwnerPresence(_)
                | K::DynamicField(..)
                | K::DynamicFieldWildcards(_)
                | K::DynamicFieldFinite(_)
                | K::Contrib(..)
                | K::CallRole(_)
                | K::GmodRealm(_)
                | K::GmodLoadFile(_)
        )
    }

    /// Whether the key is a stage rule: only a stage barrier moves it, and the
    /// barrier queues the rule's readers itself.
    pub fn is_stage_rule(&self) -> bool {
        matches!(self, FactKey::StageRule(_))
    }

    /// The file whose positions the key names, which an edit of that file
    /// renames: the owning file, or the file of a positional property owner.
    pub fn positional_file(&self) -> Option<FileId> {
        match self {
            FactKey::Property(LuaSemanticDeclId::Member(id)) => Some(id.file_id),
            FactKey::Property(LuaSemanticDeclId::LuaDecl(id)) => Some(id.file_id),
            FactKey::Property(LuaSemanticDeclId::Signature(id)) => Some(id.get_file_id()),
            FactKey::NetDefinitionUses(site) => Some(site.file_id),
            _ => self.owning_file(),
        }
    }

    /// The type of the expression key a member slot or member-key entry is
    /// filed under, which can name other files' table literals.
    pub fn expr_key_type(&self) -> Option<&crate::LuaType> {
        match self {
            FactKey::MemberSlot(_, LuaMemberKey::ExprType(typ))
            | FactKey::MemberKey(LuaMemberKey::ExprType(typ)) => Some(typ),
            _ => None,
        }
    }

    /// Whether keys are read out of this one ([`Self::base`]).
    pub fn is_base(&self) -> bool {
        matches!(
            self,
            FactKey::GmodRealm(_)
                | FactKey::GmodInferFile(_)
                | FactKey::Member(_)
                | FactKey::Signature(_)
                | FactKey::ReferencesByName(_)
                | FactKey::References(_)
                | FactKey::SpecialCallFile(_)
                | FactKey::GmodClassFile(_)
                | FactKey::TypeDeclLocations(_)
                | FactKey::TypeSupers(_)
        )
    }

    /// The key this one is read out of: writing the base moves every key read
    /// out of it, so each reader re-checks its own part and re-runs only when
    /// that part changed.
    pub fn base(&self) -> Option<FactKey> {
        match self {
            FactKey::GmodRealmAt(file_id, _) => Some(FactKey::GmodRealm(*file_id)),
            FactKey::GmodMemberRealmAt(file_id, _) | FactKey::GmodRealmInputs(file_id) => {
                Some(FactKey::GmodInferFile(*file_id))
            }
            FactKey::SignatureDecl(signature_id)
            | FactKey::SignatureReturnsNever(signature_id)
            | FactKey::SpecialCallSignature(signature_id) => {
                Some(FactKey::Signature(*signature_id))
            }
            FactKey::MemberObject(member_id) | FactKey::MemberMarks(member_id) => {
                Some(FactKey::Member(*member_id))
            }
            FactKey::IndexReferenceFiles(name) => Some(FactKey::ReferencesByName(name.clone())),
            FactKey::DeclReferences(decl_id) => Some(FactKey::References(decl_id.file_id)),
            FactKey::SpecialCallFor(file_id, _) => Some(FactKey::SpecialCallFile(*file_id)),
            FactKey::GmodVguiRegistrations(file_id) => Some(FactKey::GmodClassFile(*file_id)),
            FactKey::TypeDeclFiles(decl_id) => Some(FactKey::TypeDeclLocations(decl_id.clone())),
            FactKey::TypeSuperTypes(decl_id) | FactKey::TypeSuperEdges(decl_id) => {
                Some(FactKey::TypeSupers(decl_id.clone()))
            }
            FactKey::TypeSuperRealms(_, file_id) => Some(FactKey::GmodRealm(*file_id)),
            _ => None,
        }
    }

    /// The file whose slot the key names, or `None` for a workspace-wide key.
    /// Removing that file's facts removes whatever any derivation stored under
    /// the key.
    pub fn owning_file(&self) -> Option<FileId> {
        use FactKey as K;
        match self {
            K::TypeCache(owner) => Some(owner.get_file_id()),
            K::DefinitionFact(id) => Some(id.file_id()),
            K::BoundDecls(_, file_id) => Some(*file_id),
            K::DeclClaim(decl_id) | K::Decl(decl_id) | K::DeclDefault(decl_id) => {
                Some(decl_id.file_id)
            }
            K::Member(member_id)
            | K::MemberObject(member_id)
            | K::MemberHome(member_id)
            | K::MemberTransfer(member_id)
            | K::MemberMarks(member_id) => Some(member_id.file_id),
            K::NetCall(site) | K::NetHelper(site) | K::NetFlows(site) => Some(site.file_id),
            K::GmodRealmAt(file_id, _)
            | K::GmodMemberRealmAt(file_id, _)
            | K::GmodRealmInputs(file_id) => Some(*file_id),
            K::MemberSlot(owner, _)
            | K::MemberOwnerMap(owner)
            | K::MemberOwnerExprKeys(owner)
            | K::MemberOwnerFields(owner)
            | K::MemberOwnerPresence(owner) => match owner {
                LuaMemberOwner::Element(site) => Some(site.file_id),
                _ => None,
            },
            K::DefinitionSite(site)
            | K::CallTarget(site)
            | K::CallSiteParam(site)
            | K::Metatable(site)
            | K::TableClass(site)
            | K::DynamicFieldSite(site)
            | K::Syntax(site) => Some(site.file_id),
            K::Signature(signature_id)
            | K::SignatureDecl(signature_id)
            | K::SignatureReturnsNever(signature_id)
            | K::SpecialCallSignature(signature_id)
            | K::ValidGuard(signature_id)
            | K::InferredGuard(signature_id)
            | K::Contrib(signature_id, _)
            | K::ReceiverAuthority(signature_id)
            | K::CallbackParams(signature_id)
            | K::CallRole(CallRoleKey::Signature(signature_id)) => Some(signature_id.get_file_id()),
            K::SpecialCall(file_id, _)
            | K::SpecialCallFile(file_id)
            | K::SpecialCallFor(file_id, _)
            | K::ValidGuardFile(file_id)
            | K::TypeFile(file_id)
            | K::DeclTree(file_id)
            | K::MemberFile(file_id)
            | K::SignatureFile(file_id)
            | K::Module(file_id)
            | K::ModuleMeta(file_id)
            | K::FlowTree(file_id)
            | K::OperatorFile(file_id)
            | K::MetatableFactory(file_id)
            | K::GmodClassFile(file_id)
            | K::GmodVguiRegistrations(file_id)
            | K::VguiRelations(file_id)
            | K::GmodInferFile(file_id)
            | K::GmodRealm(file_id)
            | K::GmodLoadFile(file_id)
            | K::LoadScan(file_id)
            | K::NetworkFile(file_id)
            | K::HelperScan(file_id)
            | K::RoleMentions(file_id)
            | K::FunctionSummaries(file_id)
            | K::References(file_id)
            | K::DeclReferences(LuaDeclId { file_id, .. })
            | K::FunctionScope(InFiled { file_id, .. })
            | K::AccessorCalls(file_id)
            | K::Dependency(file_id)
            | K::LoadSites(file_id)
            | K::DiagnosticFile(file_id)
            | K::CallSiteFile(file_id)
            | K::CallSiteCallbacks(file_id)
            | K::TypeFallbacks(file_id)
            | K::DerivedReferences(file_id)
            | K::FileText(file_id)
            | K::FilePath(file_id) => Some(*file_id),
            K::DynamicField(DynamicFieldOwner::Table(site), _)
            | K::DynamicFieldWildcards(DynamicFieldOwner::Table(site))
            | K::DynamicFieldFinite(DynamicFieldOwner::Table(site)) => Some(site.file_id),
            K::OperatorOwner(LuaOperatorOwner::Table(site)) => Some(site.file_id),
            K::TypeDecl(_)
            | K::TypeDeclLocations(_)
            | K::TypeDeclFiles(_)
            | K::TypeSupers(_)
            | K::TypeSuperTypes(_)
            | K::TypeSuperEdges(_)
            | K::TypeSuperRealms(..)
            | K::TypeGenericParams(_)
            | K::SourceTypeDecl(_)
            | K::SubTypes(_)
            | K::TypeDeclScan
            | K::MemberKey(_)
            | K::GlobalPath(_)
            | K::ReceiverOutParam(_)
            | K::ModuleName(_)
            | K::ModulePath(_)
            | K::ModuleNamespace(_)
            | K::ModuleFiles
            | K::DynamicField(..)
            | K::DynamicFieldWildcards(_)
            | K::DynamicFieldFinite(_)
            | K::DynamicFieldName(_)
            | K::UnattributedField(_)
            | K::OperatorOwner(_)
            | K::GmodMeta(_)
            | K::VguiParentCallFiles
            | K::VguiChain(_)
            | K::GmodInferScan
            | K::GmodSystem(..)
            | K::NetworkScan
            | K::NetSends(_)
            | K::NetReceives(_)
            | K::NetDefinitionUses(_)
            | K::HelperRegistry
            | K::CallRole(
                CallRoleKey::Path(_)
                | CallRoleKey::ReceiverMethod(_)
                | CallRoleKey::Global(_)
                | CallRoleKey::Method(_),
            )
            | K::HelperScanFiles
            | K::LoadScanFiles
            | K::FileSet
            | K::PathFile(_)
            | K::StageRule(_)
            | K::ValidGuardFiles
            | K::ValidGuardName(_)
            | K::NetHelperName(_)
            | K::NetOperationNames
            | K::Property(_)
            | K::Global(_)
            | K::GlobalScan
            | K::ReferencesByName(_)
            | K::IndexReferenceFiles(_)
            | K::NumericPopulation(_)
            | K::AccessorFunc(_)
            | K::TypeContrib(_)
            | K::PathClassContrib(_)
            | K::DeclContrib(_)
            | K::ReferenceContrib(_)
            | K::OperatorContrib(_) => None,
        }
    }
}

/// What a derivation derives. The declaration order is the order a cold build
/// evaluates kinds in: each kind comes after the kinds whose facts it mostly
/// reads, so few evaluations read a fact before its producer ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DerivationKind {
    /// A file's syntactic facts: declarations, docs, flow, references and
    /// gmod call-site metadata. Replaced only when the file's text changes.
    Source,
    /// A file's gmod helper scan.
    GmodHelperScan,
    /// The workspace gmod call roles and net-helper registry, folded from
    /// every file's helper scan.
    GmodCallRoles,
    /// The workspace net-helper names, folded from every file's references.
    NetHelperNames,
    /// A file's annotated `load`-style call sites.
    GmodLoadCallSites,
    /// A file's gmod hook, system, realm and scoped-class metadata, vgui
    /// registrations and network-var wrappers.
    GmodPre,
    /// The workspace load graph, execution environments and file realms.
    GmodLoad,
    /// A file's valid-guard signatures.
    ValidGuardScan,
    /// The workspace effective valid-guard signatures.
    ValidGuards,
    /// A closure's inferred positive guards.
    Guard,
    LocalStat,
    AssignStat,
    ForRangeStat,
    FuncStat,
    LocalFuncStat,
    TableField,
    Closure,
    Call,
    Setmetatable,
    /// A file's module export.
    ChunkReturn,
    /// A file's annotated scripted-class and vgui parent call sites.
    GmodClassCallSites,
    /// A file's synthesized scripted-class members, registrations and numeric
    /// populations.
    GmodPost,
    /// What a file's own vgui parent calls resolve to.
    VguiRelations,
    /// The workspace vgui parent chains.
    VguiChains,
    /// A file's `AccessorFunc` getters and setters.
    AccessorFunc,
    /// The dynamic fields one write site files: an assignment or function
    /// statement with an indexed target, or a call with two or more
    /// arguments, which can set fields through `setmetatable` or a setter
    /// helper.
    DynamicFieldWrite,
    /// A file's setmetatable factory members.
    SetmetatableFactory,
    /// What the callee of one call resolves to.
    CallTarget,
    /// A query: whether a method's receiver is authoritative.
    ReceiverAuthority,
    /// A query: what a function passes to each callback parameter it calls.
    CallbackParams,
    /// What one call contributes to the parameters of the functions it calls
    /// and passes closures to.
    CallSiteParam,
    /// A file's callback receiver contributions: what its named vgui
    /// callbacks receive.
    CallSiteParams,
    /// A file's local types guessed from their uses.
    LocalFallbacks,
    /// What one call is in the net subsystem.
    NetCall,
    /// One function body's net summary.
    NetHelper,
    /// The net flows one call starts, receives or materializes.
    NetFlows,
    /// A file's diagnostics: reads only, never evaluated by the engine.
    Diagnostics,
}

impl DerivationKind {
    pub fn rank(self) -> u8 {
        match self {
            Self::LocalStat
            | Self::AssignStat
            | Self::ForRangeStat
            | Self::FuncStat
            | Self::LocalFuncStat
            | Self::TableField
            | Self::Closure
            | Self::Call
            | Self::Setmetatable => Self::LocalStat as u8,
            Self::LocalFallbacks => Self::AccessorFunc as u8,
            kind => kind as u8,
        }
    }

    /// The first stage a derivation of this kind runs at: work of the kind
    /// queued before it waits until the run reaches it.
    pub fn first_stage(self) -> AnalysisStage {
        match self {
            // Locals are guessed only once unsettled reads floor.
            Self::LocalFallbacks => StageRule::FloorUnsettled.holds_from(),
            // The passes that infer across a whole file, and the per-site
            // dynamic-field writes, call targets and net derivations, run once
            // the statements settled: run earlier, each runs again for every
            // read that settles after it.
            Self::DynamicFieldWrite
            | Self::CallTarget
            | Self::ReceiverAuthority
            | Self::CallbackParams
            | Self::CallSiteParam
            | Self::CallSiteParams
            | Self::NetCall
            | Self::NetHelper
            | Self::NetFlows => AnalysisStage::Closed,
            _ => AnalysisStage::Open,
        }
    }

    /// Whether derivations of this kind are statements.
    pub fn is_statement(self) -> bool {
        self.rank() == Self::LocalStat.rank()
    }

    /// Whether a loop of statements can close through derivations of this
    /// kind: statements, and the passes that run once the statements settle,
    /// which read what statements settled to and write facts statements read
    /// back. A cold build settles such a loop from the statements alone.
    pub fn closes_loops(self) -> bool {
        self.is_statement() || (self.first_stage() == AnalysisStage::Closed && !self.is_query())
    }

    /// Whether derivations of this kind are queries: computed when first
    /// read rather than scheduled, and re-computed when a read of theirs moves.
    pub fn is_query(self) -> bool {
        matches!(self, Self::ReceiverAuthority | Self::CallbackParams)
    }
}

/// One derivation: a recipe kind applied at a syntax site of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DerivationId {
    pub kind: DerivationKind,
    pub file_id: FileId,
    syntax_kind: LuaSyntaxKind,
    start: u32,
    end: u32,
}

impl DerivationId {
    pub fn new(kind: DerivationKind, file_id: FileId, syntax: LuaSyntaxId) -> Self {
        let range = syntax.get_range();
        Self {
            kind,
            file_id,
            syntax_kind: syntax.get_kind(),
            start: range.start().into(),
            end: range.end().into(),
        }
    }

    /// A derivation over the whole workspace.
    pub fn workspace(kind: DerivationKind) -> Self {
        Self {
            kind,
            file_id: FileId::VIRTUAL,
            syntax_kind: LuaSyntaxKind::None,
            start: 0,
            end: 0,
        }
    }

    /// The derivation every other one orders at or after.
    pub(crate) fn floor() -> Self {
        Self {
            kind: DerivationKind::Source,
            file_id: FileId::new(0),
            syntax_kind: LuaSyntaxKind::None,
            start: 0,
            end: 0,
        }
    }

    /// `file_id`'s diagnostics, whatever its text.
    pub fn diagnostics(file_id: FileId) -> Self {
        Self {
            kind: DerivationKind::Diagnostics,
            file_id,
            syntax_kind: LuaSyntaxKind::None,
            start: 0,
            end: 0,
        }
    }

    pub fn syntax_id(&self) -> LuaSyntaxId {
        LuaSyntaxId::new(
            self.syntax_kind.into(),
            TextRange::new(self.start.into(), self.end.into()),
        )
    }

    fn order_key(&self) -> (u8, FileId, u32, u32, DerivationKind, u16) {
        // Walk nodes of one file interleave in source order.
        (
            self.kind.rank(),
            self.file_id,
            self.start,
            self.end,
            self.kind,
            self.syntax_kind as u16,
        )
    }
}

impl Ord for DerivationId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order_key().cmp(&other.order_key())
    }
}

impl PartialOrd for DerivationId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// What one evaluation read and wrote.
#[derive(Debug)]
pub struct Evaluation {
    pub id: DerivationId,
    /// Reads interned in the lent [`KeyTable`], in order, once per open
    /// frame: a frame's reads are the log from where it opened.
    log: Vec<KeyIdx>,
    /// The slots of the log, each once.
    read: Vec<KeyIdx>,
    /// The open frames, innermost last; see [`frames`].
    frames: Vec<frames::Frame>,
    /// References the open frames read as walking from no keyed origin, once
    /// per frame; see [`frames::consult_unkeyed`].
    unkeyed: Vec<u64>,
    /// Under `GLUALS_CHECK_CACHE`, the reuses already checked.
    checks: FxHashSet<u64>,
    /// The log as memos outliving the evaluation see it.
    log_handle: std::sync::Arc<frames::LogHandle>,
    /// The stamp the evaluation began under.
    stamp: u64,
    /// Reads made while no key table is lent, of keys the viewed graph (see
    /// [`view_graph`]) has no slot for.
    reads: FxHashSet<HashedKey>,
    /// Reads made while viewing a graph, of keys it has a slot for.
    viewed: FxHashSet<KeyIdx>,
    /// Keys already read that an owner identity and a name name, see
    /// [`note_named_read`].
    named_reads: hashbrown::HashTable<NamedRead>,
    /// Cheap identities of keys already in `reads`, see [`note_read_by`].
    read_identities: rustc_hash::FxHashMap<(u8, u128), KeyIdx>,
    /// The slots this evaluation already interned, by key hash: a repeat
    /// read probes them instead of the key table's index.
    seen_slots: hashbrown::HashTable<(u64, KeyIdx)>,
    /// The keys the evaluation wrote, pushed once per write.
    outputs: Vec<HashedKey>,
    /// Keys the evaluation re-filed without producing them, see [`rehoming`].
    moved: FxHashSet<HashedKey>,
    /// Fingerprints taken when an earlier stage of a staged evaluation ended:
    /// other evaluations write between its stages.
    staged: rustc_hash::FxHashMap<HashedKey, Fingerprint>,
}

/// An evaluation a superstep task ended, kept until the barrier files it: its
/// results without the lookups it needed only while it recorded, and with the
/// sets a task's evaluation rarely fills boxed apart.
pub(crate) struct EndedEvaluation {
    pub id: DerivationId,
    log: Vec<KeyIdx>,
    read: Vec<KeyIdx>,
    log_handle: std::sync::Arc<frames::LogHandle>,
    stamp: u64,
    outputs: Vec<HashedKey>,
    rare: Option<Box<RareResults>>,
}

/// The results of an [`EndedEvaluation`] that are empty for most.
#[derive(Default)]
struct RareResults {
    checks: FxHashSet<u64>,
    reads: FxHashSet<HashedKey>,
    viewed: FxHashSet<KeyIdx>,
    moved: FxHashSet<HashedKey>,
    staged: rustc_hash::FxHashMap<HashedKey, Fingerprint>,
}

impl EndedEvaluation {
    /// The keys the evaluation wrote, once per write.
    pub fn outputs(&self) -> impl Iterator<Item = &HashedKey> {
        self.outputs.iter()
    }

    /// The keys the evaluation re-filed without producing them.
    pub fn moved(&self) -> impl Iterator<Item = &HashedKey> {
        self.rare.iter().flat_map(|rare| rare.moved.iter())
    }

    /// The evaluation as it ended, to file.
    pub(crate) fn into_evaluation(self) -> Evaluation {
        let rare = self.rare.map(|rare| *rare).unwrap_or_default();
        Evaluation {
            id: self.id,
            log: self.log,
            read: self.read,
            frames: Vec::new(),
            unkeyed: Vec::new(),
            checks: rare.checks,
            log_handle: self.log_handle,
            stamp: self.stamp,
            reads: rare.reads,
            viewed: rare.viewed,
            named_reads: hashbrown::HashTable::new(),
            read_identities: rustc_hash::FxHashMap::default(),
            seen_slots: hashbrown::HashTable::new(),
            outputs: self.outputs,
            moved: rare.moved,
            staged: rare.staged,
        }
    }
}

impl Evaluation {
    /// The evaluation as a task keeps it once it ended: the lookups
    /// [`end_unfrozen`] took are empty.
    pub(crate) fn into_ended(self) -> EndedEvaluation {
        let Self {
            id,
            log,
            read,
            checks,
            log_handle,
            stamp,
            reads,
            viewed,
            outputs,
            moved,
            staged,
            ..
        } = self;
        let rare = (!checks.is_empty()
            || !reads.is_empty()
            || !viewed.is_empty()
            || !moved.is_empty()
            || !staged.is_empty())
        .then(|| {
            Box::new(RareResults {
                checks,
                reads,
                viewed,
                moved,
                staged,
            })
        });
        EndedEvaluation {
            id,
            log,
            read,
            log_handle,
            stamp,
            outputs,
            rare,
        }
    }

    /// The keys the evaluation wrote, once per write.
    pub fn outputs(&self) -> impl Iterator<Item = &HashedKey> {
        self.outputs.iter()
    }

    /// The keys the evaluation read while no key table was lent.
    pub fn unlent_reads(&self) -> impl Iterator<Item = &HashedKey> {
        self.reads.iter()
    }

    /// Takes the fingerprint of each read made while no key table was lent
    /// into its slot's cache in `graph`, where it has one: an evaluation
    /// computed off the root's thread fingerprints its reads where it ran.
    /// Query outputs are left to the root, where fingerprinting may compute
    /// them.
    pub fn fingerprint_unlent_reads(
        &self,
        graph: &ReaderGraph,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
    ) {
        for key in &self.reads {
            if let Some(slot) = graph.find_slot(key)
                && !graph.keys().is_query(slot)
            {
                graph.slot_fingerprint(slot, &fingerprint);
            }
        }
    }

    /// The keys the evaluation re-filed without producing them: their readers
    /// are re-checked, but the evaluation is not one of their producers.
    pub fn moved(&self) -> impl Iterator<Item = &HashedKey> {
        self.moved.iter()
    }

    /// Fingerprints the reads made so far, for an evaluation suspended while
    /// other evaluations run: what it read is compared with what it saw, not
    /// with what they wrote afterwards. Reads [`records_read`] drops are not
    /// fingerprinted: they are dropped either way.
    pub fn fingerprint_stage(&mut self, fingerprint: impl Fn(&FactKey) -> Fingerprint) {
        let id = self.id;
        self.outputs.sort_unstable_by_key(|key| key.hash);
        self.outputs.dedup();
        for key in &self.reads {
            if self.staged.contains_key(key) {
                continue;
            }
            if !records_read(id, key, |key| contains_hashed(&self.outputs, key)) {
                continue;
            }
            self.staged.insert(key.clone(), fingerprint(key));
        }
    }

    /// The evaluation's reads, fingerprinted by `fingerprint` as they stand
    /// now, and its outputs. A primary fact it produced itself is not a read;
    /// a shared one still is, since other writers can move it.
    ///
    /// Fingerprints are taken when the evaluation ends: nothing but the
    /// evaluation itself writes while it runs, so any difference from what a
    /// read saw is the evaluation's own contribution, which must not make it
    /// its own reader.
    pub fn into_parts(
        self,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
    ) -> (DerivationId, Vec<(HashedKey, Fingerprint)>, Vec<HashedKey>) {
        let (id, reads, outputs) = self.into_unfingerprinted_parts();
        let reads = reads
            .into_iter()
            .map(|(key, seen)| {
                let seen = seen.unwrap_or_else(|| fingerprint(&key));
                (key, seen)
            })
            .collect();
        (id, reads, outputs)
    }

    /// The evaluation's interned reads, each with the fingerprint in `seen`
    /// at its position among them, and its outputs: a task's evaluation,
    /// whose reads were fingerprinted where it ran.
    pub fn into_recorded_seen(
        mut self,
        seen: Vec<Fingerprint>,
    ) -> (DerivationId, Vec<(KeyIdx, Fingerprint)>, Vec<HashedKey>) {
        debug_assert_eq!(self.read.len(), seen.len());
        debug_assert!(self.reads.is_empty(), "a task reads through its entries");
        let mut slots = std::mem::take(&mut self.read)
            .into_iter()
            .zip(seen)
            .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|(slot, _)| *slot);
        slots.dedup_by_key(|(slot, _)| *slot);
        self.outputs.sort_unstable_by_key(|key| key.hash);
        self.outputs.dedup();
        let id = self.id;
        let outputs = std::mem::take(&mut self.outputs);
        Spare::park(&mut self);
        (id, slots, outputs)
    }

    /// The evaluation's interned reads, its reads made while no key table
    /// was lent, and its outputs.
    #[allow(clippy::type_complexity)]
    pub fn into_recorded(mut self) -> (DerivationId, Vec<KeyIdx>, Vec<HashedKey>, Vec<HashedKey>) {
        // A resumed evaluation logs again what it read before it resumed.
        let mut slots = std::mem::take(&mut self.read);
        slots.sort_unstable();
        slots.dedup();
        debug_assert!({
            let mut logged = self.log.clone();
            logged.sort_unstable();
            logged.dedup();
            logged == slots
        });
        self.outputs.sort_unstable_by_key(|key| key.hash);
        self.outputs.dedup();
        let id = self.id;
        let reads = self.reads.drain().collect();
        let outputs = std::mem::take(&mut self.outputs);
        Spare::park(&mut self);
        (id, slots, reads, outputs)
    }

    /// The reads of an evaluation that viewed `graph` (see [`view_graph`]),
    /// each fingerprinted as it stands.
    pub fn into_viewed_reads(
        mut self,
        graph: &ReaderGraph,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
    ) -> ViewedReads {
        let id = self.id;
        // A key read before the graph was viewed is a slot read if it has one.
        let mut keys = Vec::new();
        for key in self.reads.drain() {
            match graph.find_slot(&key) {
                Some(slot) => {
                    self.viewed.insert(slot);
                }
                None if records_read(id, &key, |_| false) => {
                    let seen = fingerprint(&key);
                    keys.push((key, seen));
                }
                None => {}
            }
        }
        let mut slots = self
            .viewed
            .drain()
            .filter(|slot| records_read(id, graph.key(*slot), |_| false))
            .map(|slot| (slot, graph.slot_fingerprint(slot, &fingerprint)))
            .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|(slot, _)| *slot);
        let seen = slots.iter().map(|(_, seen)| *seen).collect();
        let slots = slots.iter().map(|(slot, _)| *slot).collect();
        Spare::park(&mut self);
        ViewedReads {
            slots,
            seen,
            keys: keys.into_boxed_slice(),
        }
    }

    /// [`Self::into_parts`] with each read's fingerprint left to the caller,
    /// except those an earlier stage already took.
    #[allow(clippy::type_complexity)]
    pub fn into_unfingerprinted_parts(
        mut self,
    ) -> (
        DerivationId,
        Vec<(HashedKey, Option<Fingerprint>)>,
        Vec<HashedKey>,
    ) {
        debug_assert!(self.log.is_empty(), "interned reads need the key table");
        self.outputs.sort_unstable_by_key(|key| key.hash);
        self.outputs.dedup();
        let id = self.id;
        let reads = self
            .reads
            .drain()
            .filter(|key| records_read(id, key, |key| contains_hashed(&self.outputs, key)))
            .map(|key| {
                let seen = self
                    .staged
                    .get(&key)
                    .filter(|_| !contains_hashed(&self.outputs, &key))
                    .copied();
                (key, seen)
            })
            .collect();
        let outputs = std::mem::take(&mut self.outputs);
        Spare::park(&mut self);
        (id, reads, outputs)
    }
}

/// What an evaluation that viewed the reader graph read, with the
/// fingerprints it saw: slots of the graph as it stood, which hold only until
/// the graph next changes, each beside the fingerprint at its position in
/// `seen`, and the keys it had no slot for.
#[derive(Debug, Clone, Default)]
pub struct ViewedReads {
    slots: Box<[KeyIdx]>,
    seen: Box<[Fingerprint]>,
    keys: Box<[(HashedKey, Fingerprint)]>,
}

impl ViewedReads {
    /// Reads made with no graph viewed, each with the fingerprint it saw.
    pub fn from_keys(keys: Vec<(HashedKey, Fingerprint)>) -> Self {
        Self {
            slots: Box::default(),
            seen: Box::default(),
            keys: keys.into_boxed_slice(),
        }
    }

    /// The slot reads, each with the fingerprint it saw, and the key reads.
    #[allow(clippy::type_complexity)]
    pub(crate) fn into_parts(self) -> (Vec<(KeyIdx, Fingerprint)>, Vec<(HashedKey, Fingerprint)>) {
        let slots = self
            .slots
            .iter()
            .copied()
            .zip(self.seen.iter().copied())
            .collect();
        (slots, self.keys.into_vec())
    }

    /// The heap bytes the reads hold, their keys' own heap aside.
    pub(crate) fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        self.slots.len() * size_of::<KeyIdx>()
            + self.seen.len() * size_of::<Fingerprint>()
            + self.keys.len() * size_of::<(HashedKey, Fingerprint)>()
    }

    /// Every key read, for tests.
    #[cfg(test)]
    pub fn keys<'a>(&'a self, graph: &'a ReaderGraph) -> impl Iterator<Item = &'a FactKey> {
        self.slots
            .iter()
            .map(|slot| graph.key(*slot).key())
            .chain(self.keys.iter().map(|(key, _)| key.key()))
    }
}

/// Whether evaluation `id` records its read of `key`. A primary fact it
/// produced itself is not a read, nor is a source fact of its own file: a
/// change replaces all of that file's derivations, and a source layer runs
/// before any of its file's derivations.
pub(crate) fn records_read(
    id: DerivationId,
    key: &HashedKey,
    is_output: impl Fn(&HashedKey) -> bool,
) -> bool {
    let own = key.owning_file() == Some(id.file_id);
    !(key.is_primary() && is_output(key))
        && !(own && (key.is_file_source() || id.kind == DerivationKind::Source))
}

/// Whether `key` is one of `sorted`, a vec sorted by [`HashedKey`]'s stored
/// hash and deduplicated.
fn contains_hashed(sorted: &[HashedKey], key: &HashedKey) -> bool {
    let start = sorted.partition_point(|probe| probe.hash < key.hash);
    sorted[start..]
        .iter()
        .take_while(|probe| probe.hash == key.hash)
        .any(|probe| probe == key)
}

thread_local! {
    static KEYS: RefCell<Option<KeyTable>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Option<Evaluation>> = const { RefCell::new(None) };
    /// Evaluations suspended on this thread by one begun inside them.
    static SUSPENDED: RefCell<Vec<Evaluation>> = const { RefCell::new(Vec::new()) };
    static UNTRACKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static REHOMING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static WRITE_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CAPTURED: RefCell<Option<FxHashSet<HashedKey>>> = const { RefCell::new(None) };
    /// The positions among the current evaluation's outputs of the writes
    /// noted since [`noting_writes_once`] began, with the evaluation's stamp.
    static NOTED_ONCE: RefCell<Option<(u64, Vec<usize>)>> = const { RefCell::new(None) };
    static NOTED_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The reader graph evaluations on this thread read without a lent table.
    static VIEW: RefCell<Option<std::sync::Arc<ReaderGraph>>> = const { RefCell::new(None) };
    /// The lookups of evaluations that ended, for the next ones to begin with.
    static SPARE: RefCell<Vec<Spare>> = const { RefCell::new(Vec::new()) };
    /// The evaluations to run again; see [`rerun_current`].
    static RERUNS: RefCell<Vec<DerivationId>> = const { RefCell::new(Vec::new()) };
}

/// Lets evaluations on this thread reuse memoised values against `graph`
/// without a lent table, until the returned guard drops.
pub fn view_graph(graph: std::sync::Arc<ReaderGraph>) -> ViewedGraph {
    VIEW.with(|view| *view.borrow_mut() = Some(graph));
    ViewedGraph
}

/// Ends [`view_graph`] when dropped, unwinding included.
#[must_use]
pub struct ViewedGraph;

impl Drop for ViewedGraph {
    fn drop(&mut self) {
        VIEW.with(|view| view.borrow_mut().take());
    }
}

/// Runs `f` on the evaluation recording on this thread and the graph it
/// views, if there are both.
fn with_viewed<T>(f: impl FnOnce(&mut Evaluation, &ReaderGraph) -> T) -> Option<T> {
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let evaluation = current.as_mut()?;
        VIEW.with(|view| view.borrow().as_deref().map(|graph| f(evaluation, graph)))
    })
}

/// Runs `f` outside any evaluation and returns every key it wrote: a reset
/// moves facts other derivations produced, whose readers must be re-checked.
pub fn capture_writes(f: impl FnOnce()) -> FxHashSet<HashedKey> {
    debug_assert!(
        CURRENT.with(|current| current.borrow().is_none()),
        "writes captured inside an evaluation"
    );
    let mut previous = Some(CAPTURED.with(|captured| captured.replace(Some(FxHashSet::default()))));
    let _restore = OnDrop(move || {
        CAPTURED.with(|captured| *captured.borrow_mut() = previous.take().flatten())
    });
    f();
    CAPTURED
        .with(|captured| captured.borrow_mut().take())
        .unwrap_or_default()
}

/// Advances whenever an evaluation writes a fact, so inference memoised
/// before the write can tell it is stale.
pub fn write_generation() -> u64 {
    WRITE_GENERATION.with(std::cell::Cell::get)
}

/// Advances the write generation for a change of the index made outside an
/// evaluation, such as a switch to another view of it.
pub(crate) fn advance_write_generation() {
    WRITE_GENERATION.with(|generation| generation.set(generation.get() + 1));
}

/// Runs its function when dropped, unwinding included.
pub(crate) struct OnDrop<F: FnMut()>(pub(crate) F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Runs `f` without recording its reads. For an index's own bookkeeping: a
/// lookup that only decides where a write is stored, when the index keeps
/// that placement consistent itself whatever the order of writes.
pub fn untracked<T>(f: impl FnOnce() -> T) -> T {
    UNTRACKED.with(|depth| depth.set(depth.get() + 1));
    let _depth = OnDrop(|| UNTRACKED.with(|depth| depth.set(depth.get() - 1)));
    f()
}

/// Runs `f` noting each output it writes once: a write of a key `f` already
/// noted as an output adds nothing to the evaluation's outputs, which are a
/// set, and is skipped.
pub fn noting_writes_once<T>(f: impl FnOnce() -> T) -> T {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            NOTED_ONCE.with(|noted| noted.borrow_mut().take());
        }
    }
    if NOTED_ONCE.with(|noted| noted.borrow().is_some()) {
        return f();
    }
    let stamp =
        CURRENT.with(|current| current.borrow().as_ref().map(|evaluation| evaluation.stamp));
    let Some(stamp) = stamp else {
        return f();
    };
    NOTED_ONCE.with(|noted| *noted.borrow_mut() = Some((stamp, Vec::new())));
    let _scope = Scope;
    f()
}

/// Runs `f` with its writes recorded as moves, not outputs. For an index
/// re-filing facts other derivations produced, when the index keeps that
/// placement consistent itself whatever the order of writes: the moved facts'
/// readers are re-checked, but the current derivation does not become one of
/// their producers.
pub fn rehoming<T>(f: impl FnOnce() -> T) -> T {
    REHOMING.with(|depth| depth.set(depth.get() + 1));
    let _depth = OnDrop(|| REHOMING.with(|depth| depth.set(depth.get() - 1)));
    f()
}

/// Lends `table` to this thread: evaluations begun until [`take_keys`]
/// intern their reads as they make them.
pub fn lend_keys(table: KeyTable) {
    KEYS.with(|keys| *keys.borrow_mut() = Some(table));
}

/// Takes back the table [`lend_keys`] lent.
pub fn take_keys() -> KeyTable {
    KEYS.with(|keys| keys.borrow_mut().take())
        .expect("take_keys without lend_keys")
}

/// Drops a lent table left behind by an evaluation that unwound, so later
/// evaluations on this thread do not intern into it.
pub fn forget_lent_keys() {
    KEYS.with(|keys| keys.borrow_mut().take());
}

/// An evaluation [`begin`] or [`resume`] started on this thread. Dropped while
/// the evaluation unwinds before it ends, it discards the evaluation and
/// whatever began inside it, and the evaluation it suspended records again:
/// the thread records as it did before it began.
#[must_use = "held until the evaluation ends, so an unwind restores the thread's recording"]
pub struct Recording {
    stamp: u64,
    /// How many evaluations were suspended when it started.
    suspended: usize,
    /// Whether it suspended one that was recording.
    suspends: bool,
}

impl Drop for Recording {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let started = |evaluation: &Evaluation| evaluation.stamp == self.stamp;
        let live = CURRENT.with(|current| current.borrow().as_ref().is_some_and(started))
            || SUSPENDED.with(|suspended| {
                suspended
                    .borrow()
                    .get(self.suspended..)
                    .is_some_and(|above| above.iter().any(started))
            });
        if !live {
            return;
        }
        let above = SUSPENDED.with(|suspended| suspended.borrow_mut().split_off(self.suspended));
        let previous = self.suspends.then(|| above.into_iter().next()).flatten();
        let discarded = CURRENT.with(|current| current.replace(previous));
        drop(discarded);
    }
}

/// Starts recording the evaluation of `id` on this thread.
pub fn begin(id: DerivationId) -> Recording {
    let stamp = next_stamp();
    let spare = SPARE
        .with(|spare| spare.borrow_mut().pop())
        .unwrap_or_default();
    start(Evaluation {
        id,
        log: spare.log,
        read: spare.read,
        frames: spare.frames,
        unkeyed: spare.unkeyed,
        checks: spare.checks,
        log_handle: std::sync::Arc::new(frames::LogHandle::default()),
        stamp,
        reads: spare.reads,
        viewed: spare.viewed,
        read_identities: spare.read_identities,
        named_reads: spare.named_reads,
        seen_slots: spare.seen_slots,
        outputs: spare.outputs,
        moved: spare.moved,
        staged: spare.staged,
    })
}

/// The lookups an evaluation needs only while it records, and the buffers it
/// collects its results in, kept emptied for the next evaluation on the
/// thread to begin with.
#[derive(Default)]
struct Spare {
    read_identities: rustc_hash::FxHashMap<(u8, u128), KeyIdx>,
    named_reads: hashbrown::HashTable<NamedRead>,
    seen_slots: hashbrown::HashTable<(u64, KeyIdx)>,
    frames: Vec<frames::Frame>,
    unkeyed: Vec<u64>,
    checks: FxHashSet<u64>,
    log: Vec<KeyIdx>,
    read: Vec<KeyIdx>,
    reads: FxHashSet<HashedKey>,
    viewed: FxHashSet<KeyIdx>,
    outputs: Vec<HashedKey>,
    moved: FxHashSet<HashedKey>,
    staged: rustc_hash::FxHashMap<HashedKey, Fingerprint>,
}

/// The most entries a kept buffer may have room for: emptying one costs its
/// room, which a rare large evaluation must not make every later one pay.
const SPARE_ROOM: usize = 1 << 12;

impl Spare {
    /// Takes the lookups `evaluation` needed only while it recorded.
    fn take(evaluation: &mut Evaluation) -> Self {
        let mut spare = Self {
            read_identities: std::mem::take(&mut evaluation.read_identities),
            named_reads: std::mem::take(&mut evaluation.named_reads),
            seen_slots: std::mem::take(&mut evaluation.seen_slots),
            frames: std::mem::take(&mut evaluation.frames),
            unkeyed: std::mem::take(&mut evaluation.unkeyed),
            ..Self::default()
        };
        spare.settle();
        spare
    }

    /// Takes `evaluation`'s spent result buffers and keeps them like its
    /// lookups.
    fn park(evaluation: &mut Evaluation) {
        let mut spare = Self {
            checks: std::mem::take(&mut evaluation.checks),
            log: std::mem::take(&mut evaluation.log),
            read: std::mem::take(&mut evaluation.read),
            reads: std::mem::take(&mut evaluation.reads),
            viewed: std::mem::take(&mut evaluation.viewed),
            outputs: std::mem::take(&mut evaluation.outputs),
            moved: std::mem::take(&mut evaluation.moved),
            staged: std::mem::take(&mut evaluation.staged),
            ..Self::default()
        };
        spare.settle();
        SPARE.with(|kept| {
            let mut kept = kept.borrow_mut();
            match kept.last_mut() {
                Some(last) => last.absorb(spare),
                None => kept.push(spare),
            }
        });
    }

    /// Empties every kept buffer, dropping the ones that outgrew
    /// [`SPARE_ROOM`].
    fn settle(&mut self) {
        macro_rules! settle {
            ($($field:ident),*) => {$(
                if self.$field.capacity() > SPARE_ROOM {
                    self.$field = Default::default();
                }
                self.$field.clear();
            )*};
        }
        settle!(
            read_identities,
            named_reads,
            seen_slots,
            frames,
            unkeyed,
            checks,
            log,
            read,
            reads,
            viewed,
            outputs,
            moved,
            staged
        );
    }

    /// Keeps the roomier of each buffer.
    fn absorb(&mut self, other: Self) {
        macro_rules! absorb {
            ($($field:ident),*) => {$(
                if other.$field.capacity() > self.$field.capacity() {
                    self.$field = other.$field;
                }
            )*};
        }
        absorb!(
            read_identities,
            named_reads,
            seen_slots,
            frames,
            unkeyed,
            checks,
            log,
            read,
            reads,
            viewed,
            outputs,
            moved,
            staged
        );
    }
}

/// Makes `evaluation` the one recording on this thread. One already recording
/// is suspended until `evaluation` ends: a thread waiting in the pool runs
/// other work, which may record its own evaluation.
fn start(evaluation: Evaluation) -> Recording {
    let stamp = evaluation.stamp;
    let suspended = SUSPENDED.with(|suspended| suspended.borrow().len());
    let previous = CURRENT.with(|current| current.borrow_mut().replace(evaluation));
    let suspends = previous.is_some();
    if let Some(previous) = previous {
        SUSPENDED.with(|suspended| suspended.borrow_mut().push(previous));
    }
    Recording {
        stamp,
        suspended,
        suspends,
    }
}

/// Runs `f` as the evaluation of `id`, nested in whatever this thread is
/// recording, and returns what it read, as keys. Its reads are its own: the
/// suspended evaluation's key table, view and untracked or re-homing regions
/// do not apply to it, so what it read can be filed whatever it ran inside.
pub fn detached<T>(id: DerivationId, f: impl FnOnce() -> T) -> (T, Vec<HashedKey>) {
    let saved = transport::SavedRecording::take();
    let _recording = begin(id);
    let result = f();
    let evaluation = end();
    drop(saved);
    let (_, reads, _) = evaluation.into_unfingerprinted_parts();
    (result, reads.into_iter().map(|(key, _)| key).collect())
}

/// Continues recording an evaluation that [`end`] handed back, for a
/// derivation evaluated in stages.
pub fn resume(mut evaluation: Evaluation) -> Recording {
    // Slots other evaluations logged since it began would read as logged by
    // it.
    evaluation.stamp = next_stamp();
    // Identities noted without this thread's key table name no slot in it.
    evaluation.read_identities.clear();
    evaluation.named_reads.clear();
    evaluation.seen_slots.clear();
    start(evaluation)
}

/// Stops recording and hands back what the evaluation read and wrote.
pub fn suspend() -> Evaluation {
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let evaluation = current.take().expect("derivation::suspend without begin");
        *current = SUSPENDED.with(|suspended| suspended.borrow_mut().pop());
        evaluation
    })
}

pub fn end() -> Evaluation {
    let evaluation = end_unfrozen();
    evaluation.log_handle.freeze(&evaluation.log);
    evaluation
}

/// [`end`] with the log left unfrozen: a task's evaluation logs entries of
/// its own until the barrier canonicalises them.
pub(super) fn end_unfrozen() -> Evaluation {
    let mut evaluation = suspend();
    let spare = Spare::take(&mut evaluation);
    SPARE.with(|kept| {
        // A resumed evaluation ends without having begun on this thread.
        let mut kept = kept.borrow_mut();
        match kept.last_mut() {
            Some(last) => last.absorb(spare),
            None => kept.push(spare),
        }
    });
    evaluation
}

/// Whether a key table is lent to this thread.
#[cfg(test)]
pub fn keys_lent() -> bool {
    KEYS.with(|keys| keys.borrow().is_some())
}

/// Whether an evaluation is recording on this thread. Callers building a
/// costly key check this first.
#[inline]
pub fn recording() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
        || CAPTURED.with(|captured| captured.borrow().is_some())
}

/// Records a read of `key` by the current evaluation.
pub fn note_read(key: impl FnOnce() -> FactKey) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            NOTED_READS.with(|count| count.set(count.get() + 1));
            record_read(evaluation, HashedKey::new(key()));
        }
    });
}

/// Records reads of `keys`, made elsewhere for the evaluation recording on
/// this thread, as that evaluation's own.
pub fn note_reads(keys: impl IntoIterator<Item = HashedKey>) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            for key in keys {
                NOTED_READS.with(|count| count.set(count.get() + 1));
                record_read(evaluation, key);
            }
        }
    });
}

/// Orders every evaluation, frame and write on every thread: a key table keeps
/// the stamps its slots were logged and written at from one re-analysis to the
/// next, and the next may run on another thread.
static STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_stamp() -> u64 {
    STAMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

fn latest_stamp() -> u64 {
    STAMP.load(std::sync::atomic::Ordering::Relaxed)
}

impl Evaluation {
    /// The stamp of the innermost open frame, or of the evaluation.
    fn frame_stamp(&self) -> u64 {
        self.frames.last().map_or(self.stamp, |frame| frame.stamp)
    }

    fn log_slot(&mut self, table: &mut KeyTable, slot: KeyIdx) {
        let (first, is_query) = table.log(slot, self.frame_stamp(), self.stamp);
        if is_query && let Some(frame) = self.frames.last_mut() {
            frame.has_query = true;
        }
        if let Some(first) = first {
            self.log.push(slot);
            if first {
                self.read.push(slot);
            }
        }
    }
}

/// Records a read of `key`, interned if a key table is lent, else as its
/// slot in the viewed graph if it has one. Returns its slot in the lent table.
fn record_read(evaluation: &mut Evaluation, key: HashedKey) -> Option<KeyIdx> {
    let mut key = Some(key);
    if let Some(local) = transport::with_keys(|worker| {
        worker.record(evaluation, key.take().expect("a key to record"))
    }) {
        return Some(local);
    }
    let key = key?;
    KEYS.with(|keys| match keys.borrow_mut().as_mut() {
        Some(table) => {
            // A slot the evaluation interned stays live while it records, so
            // its key hash names it again without probing the table's index.
            let seen = evaluation
                .seen_slots
                .find(key.hash, |entry| table.live_key(entry.1) == Some(&key))
                .map(|entry| entry.1);
            let slot = match seen {
                Some(slot) => slot,
                None => {
                    let hash = key.hash;
                    let slot = table.intern_read(key, evaluation.stamp);
                    evaluation
                        .seen_slots
                        .insert_unique(hash, (hash, slot), |entry| entry.0);
                    slot
                }
            };
            evaluation.log_slot(table, slot);
            if table.taints(slot) {
                frames::taint_evaluation(evaluation);
            }
            Some(slot)
        }
        None => {
            let viewed = VIEW.with(|view| {
                view.borrow()
                    .as_deref()
                    .and_then(|graph| graph.find_slot(&key))
            });
            match viewed {
                Some(slot) => {
                    evaluation.viewed.insert(slot);
                }
                None => {
                    evaluation.reads.insert(key);
                }
            }
            None
        }
    })
}

fn log_recorded(evaluation: &mut Evaluation, slot: KeyIdx) {
    if transport::with_keys(|worker| worker.log(evaluation, slot)).is_none() {
        KEYS.with(|keys| {
            if let Some(table) = keys.borrow_mut().as_mut() {
                evaluation.log_slot(table, slot);
                if table.taints(slot) {
                    frames::taint_evaluation(evaluation);
                }
            }
        });
    }
}

/// Records a read of `key`, which `identity` names exactly: a repeat within
/// the evaluation is one small-set probe instead of building and hashing the
/// key. The identity must be unique to the key for as long as the recorded
/// key lives.
fn note_read_by(identity: (u8, u128), key: impl FnOnce() -> FactKey) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            NOTED_READS.with(|count| count.set(count.get() + 1));
            match evaluation.read_identities.get(&identity).copied() {
                Some(slot) => log_recorded(evaluation, slot),
                None => {
                    if let Some(slot) = record_read(evaluation, HashedKey::new(key())) {
                        evaluation.read_identities.insert(identity, slot);
                    } else {
                        evaluation.read_identities.insert(identity, KeyIdx::MAX);
                    }
                }
            }
        }
    });
}

/// A read [`note_named_read`] recorded: the owner identity and name that
/// name the key, and its slot ([`KeyIdx::MAX`]: none, no table lent).
#[derive(Debug)]
struct NamedRead {
    tag: u8,
    owner: u128,
    name: Option<SmolStr>,
    slot: KeyIdx,
}

fn named_read_hash(tag: u8, owner: u128, name: Option<&str>) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one((tag, owner, name))
}

/// Records a read of the key `key` builds, which the owner identity `owner`
/// and `name` name exactly under `tag`: a repeat within the evaluation is one
/// probe of a small table, without building the key or its name.
fn note_named_read(tag: u8, owner: u128, name: Option<&str>, key: impl FnOnce() -> FactKey) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let Some(evaluation) = current.as_mut() else {
            return;
        };
        NOTED_READS.with(|count| count.set(count.get() + 1));
        let hash = named_read_hash(tag, owner, name);
        let found = evaluation
            .named_reads
            .find(hash, |read| {
                read.tag == tag && read.owner == owner && read.name.as_deref() == name
            })
            .map(|read| read.slot);
        match found {
            Some(slot) => log_recorded(evaluation, slot),
            None => {
                let slot = record_read(evaluation, HashedKey::new(key())).unwrap_or(KeyIdx::MAX);
                evaluation.named_reads.insert_unique(
                    hash,
                    NamedRead {
                        tag,
                        owner,
                        name: name.map(SmolStr::new),
                        slot,
                    },
                    |read| named_read_hash(read.tag, read.owner, read.name.as_deref()),
                );
            }
        }
    });
}

/// A member or dynamic-field owner's identity for [`note_named_read`]:
/// interned ids by address, table literals by position, each variant tagged.
fn owner_identity(kind: u128, address_or_site: u128) -> u128 {
    kind << 120 | address_or_site
}

fn site_identity(site: &InFiled<TextRange>) -> u128 {
    u128::from(site.file_id.id) << 64
        | u128::from(u32::from(site.value.start())) << 32
        | u128::from(u32::from(site.value.end()))
}

/// Records a read of the members of `owner` under `key`.
pub fn note_member_slot_read(owner: &LuaMemberOwner, key: &LuaMemberKey) {
    let name = match key {
        LuaMemberKey::Name(name) => Some(name.as_str()),
        LuaMemberKey::None => None,
        LuaMemberKey::Integer(_) | LuaMemberKey::ExprType(_) => {
            note_read(|| FactKey::MemberSlot(owner.clone(), key.clone()));
            return;
        }
    };
    let owner_id = match owner {
        LuaMemberOwner::Type(id) => owner_identity(1, u128::from(id.interned_address())),
        LuaMemberOwner::GlobalPath(path) => owner_identity(2, u128::from(path.interned_address())),
        LuaMemberOwner::Element(site) => owner_identity(3, site_identity(site)),
        LuaMemberOwner::LocalUnresolve => owner_identity(4, 0),
    };
    note_named_read(u8::MAX - 17, owner_id, name, || {
        FactKey::MemberSlot(owner.clone(), key.clone())
    });
}

/// Records a read of `owner`'s dynamic field `name` (`None`: its named
/// fields as a whole).
pub fn note_dynamic_field_read(owner: &DynamicFieldOwner, name: Option<&str>) {
    let owner_id = match owner {
        DynamicFieldOwner::Type(id) => owner_identity(1, u128::from(id.interned_address())),
        DynamicFieldOwner::GlobalPath(path) => {
            owner_identity(2, u128::from(path.interned_address()))
        }
        DynamicFieldOwner::Table(site) => owner_identity(3, site_identity(site)),
    };
    note_named_read(u8::MAX - 18, owner_id, name, || {
        FactKey::DynamicField(owner.clone(), name.map(SmolStr::new))
    });
}

/// A per-file fact read often enough to be worth [`note_read_by`].
#[derive(Debug, Clone, Copy)]
pub enum FileFact {
    GmodRealm,
    GmodInferFile,
    ModuleMeta,
    References,
    FlowTree,
    DeclTree,
    FunctionSummaries,
    FilePath,
}

/// Records a read of `file_id`'s `fact`.
pub fn note_file_fact_read(fact: FileFact, file_id: FileId) {
    note_read_by((fact as u8, u128::from(file_id.id)), || match fact {
        FileFact::GmodRealm => FactKey::GmodRealm(file_id),
        FileFact::GmodInferFile => FactKey::GmodInferFile(file_id),
        FileFact::ModuleMeta => FactKey::ModuleMeta(file_id),
        FileFact::References => FactKey::References(file_id),
        FileFact::FlowTree => FactKey::FlowTree(file_id),
        FileFact::DeclTree => FactKey::DeclTree(file_id),
        FileFact::FunctionSummaries => FactKey::FunctionSummaries(file_id),
        FileFact::FilePath => FactKey::FilePath(file_id),
    });
}

/// Records a read of `file_id`'s path, its own file's included: a move keeps
/// a file's derivations, and re-runs those that read where it is.
pub fn note_file_path_read(file_id: FileId) {
    note_file_fact_read(FileFact::FilePath, file_id);
}

/// Records a read of the global path `path`; see [`note_type_decl_read`].
pub fn note_global_path_read(path: &GlobalId) {
    note_read_by((u8::MAX - 14, u128::from(path.interned_address())), || {
        FactKey::GlobalPath(path.clone())
    });
}

/// Records a read of the owner the table literal `site` initialises.
pub fn note_definition_site_read(site: &InFiled<TextRange>) {
    let identity = u128::from(site.file_id.id) << 64
        | u128::from(u32::from(site.value.start())) << 32
        | u128::from(u32::from(site.value.end()));
    note_read_by((u8::MAX - 15, identity), || {
        FactKey::DefinitionSite(site.clone())
    });
}

/// Records a read of the type declaration `decl_id`. Interned ids compare by
/// address, which the recorded key keeps alive for the evaluation.
pub fn note_type_decl_read(decl_id: &LuaTypeDeclId) {
    note_read_by((u8::MAX, u128::from(decl_id.interned_address())), || {
        FactKey::TypeDecl(decl_id.clone())
    });
}

/// Records a read of where `decl_id` is declared.
pub fn note_type_decl_locations_read(decl_id: &LuaTypeDeclId) {
    note_read_by(
        (u8::MAX - 11, u128::from(decl_id.interned_address())),
        || FactKey::TypeDeclLocations(decl_id.clone()),
    );
}

/// Records a read of the files `decl_id` is declared in and the flags each
/// location carries.
pub fn note_type_decl_files_read(decl_id: &LuaTypeDeclId) {
    note_read_by(
        (u8::MAX - 23, u128::from(decl_id.interned_address())),
        || FactKey::TypeDeclFiles(decl_id.clone()),
    );
}

/// Records a read of `decl_id`'s super edges as the file and type of each.
pub fn note_type_super_edges_read(decl_id: &LuaTypeDeclId) {
    note_read_by(
        (u8::MAX - 7, u128::from(decl_id.interned_address())),
        || FactKey::TypeSuperEdges(decl_id.clone()),
    );
}

/// Records a read of the gmod realm at each of `decl_id`'s super edges in
/// `file_id`.
pub fn note_type_super_realms_read(decl_id: &LuaTypeDeclId, file_id: FileId) {
    note_read_by(
        (
            u8::MAX - 25,
            u128::from(decl_id.interned_address()) << 32 | u128::from(file_id.id),
        ),
        || FactKey::TypeSuperRealms(decl_id.clone(), file_id),
    );
}

/// Records a read of `decl_id`'s distinct supertypes in order.
pub fn note_type_super_types_read(decl_id: &LuaTypeDeclId) {
    note_read_by(
        (u8::MAX - 24, u128::from(decl_id.interned_address())),
        || FactKey::TypeSuperTypes(decl_id.clone()),
    );
}

/// Records a read of `decl_id`'s generic parameters.
pub fn note_type_generic_params_read(decl_id: &LuaTypeDeclId) {
    note_read_by(
        (u8::MAX - 8, u128::from(decl_id.interned_address())),
        || FactKey::TypeGenericParams(decl_id.clone()),
    );
}

/// An exact identity for a position in a file.
fn file_position(file_id: FileId, position: TextSize) -> u128 {
    u128::from(file_id.id) << 32 | u128::from(u32::from(position))
}

/// Records a read of the realm at `offset` in `file_id`.
pub fn note_realm_at_read(file_id: FileId, offset: TextSize) {
    // A derivation's own file's realm moves as a whole for it: an edit of the
    // file replaces its derivations, and the load graph moves every offset.
    if current().is_some_and(|id| id.file_id == file_id) {
        note_file_fact_read(FileFact::GmodRealm, file_id);
        return;
    }
    note_read_by((u8::MAX - 9, file_position(file_id, offset)), || {
        FactKey::GmodRealmAt(file_id, offset)
    });
}

/// Records a read of `file_id`'s own realm evidence.
pub fn note_realm_inputs_read(file_id: FileId) {
    note_read_by((u8::MAX - 13, u128::from(file_id.id)), || {
        FactKey::GmodRealmInputs(file_id)
    });
}

/// Records a read of the `---@realm` annotation covering `offset` in
/// `file_id`.
pub fn note_member_realm_at_read(file_id: FileId, offset: TextSize) {
    note_read_by((u8::MAX - 10, file_position(file_id, offset)), || {
        FactKey::GmodMemberRealmAt(file_id, offset)
    });
}

/// An exact identity for the member a node declares, slot 0.
fn member_identity(id: &LuaMemberId) -> u128 {
    let syntax = id.get_syntax_id();
    let range = syntax.get_range();
    u128::from(id.file_id.id) << 80
        | u128::from(syntax.get_kind() as u16) << 64
        | u128::from(u32::from(range.start())) << 32
        | u128::from(u32::from(range.end()))
}

/// Records a read of the fact `key` names about the member `id`: deduplicated by
/// its exact identity for the member a node declares, and by the fact itself for
/// the further members one node makes in other slots, which share that identity.
fn note_member_read(kind: u8, id: &LuaMemberId, key: impl FnOnce() -> FactKey) {
    if id.get_slot() == 0 {
        note_read_by((kind, member_identity(id)), key);
    } else {
        note_read(key);
    }
}

/// Records a read of the current owner of the member `id`.
pub fn note_member_home_read(id: LuaMemberId) {
    note_member_read(u8::MAX - 1, &id, || FactKey::MemberHome(id));
}

/// Records a read of the path provenance, flags and function scope of the
/// member `id`.
pub fn note_member_marks_read(id: LuaMemberId) {
    note_member_read(u8::MAX - 22, &id, || FactKey::MemberMarks(id));
}

/// Records a read of the member object `id` alone: its key, feature and
/// global id.
pub fn note_member_object_read(id: LuaMemberId) {
    note_member_read(u8::MAX - 20, &id, || FactKey::MemberObject(id));
}

/// Records a read of the declaration object `id`.
pub fn note_decl_read(id: LuaDeclId) {
    note_read_by(
        (u8::MAX - 2, file_position(id.file_id, id.position)),
        || FactKey::Decl(id),
    );
}

/// Records a read of the special-call facts at `position` in `file_id`.
pub fn note_special_call_read(file_id: FileId, position: TextSize) {
    note_read_by((u8::MAX - 3, file_position(file_id, position)), || {
        FactKey::SpecialCall(file_id, position)
    });
}

/// Records a read of the signature `id` as declared.
pub fn note_signature_decl_read(id: LuaSignatureId) {
    note_read_by(
        (
            u8::MAX - 12,
            file_position(id.get_file_id(), id.get_position()),
        ),
        || FactKey::SignatureDecl(id),
    );
}

/// Records a read of whether the signature `id` returns `never`.
pub fn note_signature_returns_never_read(id: LuaSignatureId) {
    note_read_by(
        (
            u8::MAX - 21,
            file_position(id.get_file_id(), id.get_position()),
        ),
        || FactKey::SignatureReturnsNever(id),
    );
}

/// Records a read of whether the signature `id` takes special-call params.
pub fn note_special_call_signature_read(id: LuaSignatureId) {
    note_read_by(
        (
            u8::MAX - 19,
            file_position(id.get_file_id(), id.get_position()),
        ),
        || FactKey::SpecialCallSignature(id),
    );
}

/// Records a read of the signature object `id`.
pub fn note_signature_read(id: LuaSignatureId) {
    note_read_by(
        (
            u8::MAX - 4,
            file_position(id.get_file_id(), id.get_position()),
        ),
        || FactKey::Signature(id),
    );
}

#[derive(Debug, Default)]
pub(crate) struct FactRead(std::sync::atomic::AtomicU32);

impl Clone for FactRead {
    fn clone(&self) -> Self {
        Self(std::sync::atomic::AtomicU32::new(
            self.0.load(std::sync::atomic::Ordering::Relaxed),
        ))
    }
}

/// A read handle only caches where its key sits, so it never tells two
/// values apart.
impl PartialEq for FactRead {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl FactRead {
    pub(crate) fn recomputes(&self, owner: &LuaTypeOwner) -> bool {
        use std::sync::atomic::Ordering;
        if let Some(windowed) = transport::with_keys(|worker| {
            worker.windowed(&HashedKey::new(FactKey::TypeCache(owner.clone())))
        }) {
            return windowed;
        }
        KEYS.with(|keys| {
            keys.borrow().as_ref().is_some_and(|table| {
                table
                    .matching_slot(
                        self.0.load(Ordering::Relaxed),
                        |fact| matches!(fact, FactKey::TypeCache(stored) if stored == owner),
                    )
                    .or_else(|| table.find(&HashedKey::new(FactKey::TypeCache(owner.clone()))))
                    .is_some_and(|slot| table.windowed(slot))
            })
        })
    }

    pub(crate) fn note(
        &self,
        key: impl FnOnce() -> FactKey,
        matches: impl Fn(&FactKey) -> bool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if UNTRACKED.with(|depth| depth.get() > 0) {
            return true;
        }
        CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            let Some(evaluation) = current.as_mut() else {
                return true;
            };
            let mut key = Some(key);
            let on_worker = transport::with_keys(|worker| {
                let hint = self.0.load(Ordering::Relaxed);
                if let Some(global) = worker.graph().keys().matching_slot(hint, &matches) {
                    worker
                        .record_known(evaluation, global)
                        .expect("snapshot hint is live");
                } else {
                    let key = HashedKey::new(key.take().expect("a key to build")());
                    if let Some(global) = worker.graph().find_slot(&key) {
                        self.0.store(global, Ordering::Relaxed);
                        worker
                            .record_known(evaluation, global)
                            .expect("snapshot key is live");
                    } else {
                        worker.record(evaluation, key);
                    }
                }
            });
            if on_worker.is_some() {
                NOTED_READS.with(|count| count.set(count.get() + 1));
                return true;
            }
            let key = key.expect("the worker did not build the key");
            if KEYS.with(|keys| keys.borrow().is_none())
                && VIEW.with(|view| view.borrow().is_none())
            {
                return false;
            }
            NOTED_READS.with(|count| count.set(count.get() + 1));
            let slot = self.0.load(Ordering::Relaxed);
            KEYS.with(|keys| {
                let mut keys = keys.borrow_mut();
                if let Some(table) = keys.as_mut() {
                    let slot = table.matching_slot(slot, &matches).unwrap_or_else(|| {
                        let slot = table.intern_read(HashedKey::new(key()), evaluation.stamp);
                        self.0.store(slot, Ordering::Relaxed);
                        slot
                    });
                    evaluation.log_slot(table, slot);
                    if table.taints(slot) {
                        frames::taint_evaluation(evaluation);
                    }
                } else {
                    VIEW.with(|view| {
                        let view = view.borrow();
                        let slot = view
                            .as_ref()
                            .and_then(|graph| graph.keys().matching_slot(slot, &matches));
                        if let Some(slot) = slot {
                            evaluation.viewed.insert(slot);
                        } else {
                            let key = HashedKey::new(key());
                            if let Some(graph) = view.as_ref()
                                && let Some(slot) = graph.find_slot(&key)
                            {
                                self.0.store(slot, Ordering::Relaxed);
                                evaluation.viewed.insert(slot);
                            } else {
                                evaluation.reads.insert(key);
                            }
                        }
                    });
                }
            });
            true
        })
    }
}

/// Records a read of `owner`'s stored type.
pub fn note_type_cache_read(owner: &LuaTypeOwner) {
    match owner {
        LuaTypeOwner::Decl(id) => note_read_by(
            (u8::MAX - 5, file_position(id.file_id, id.position)),
            || FactKey::TypeCache(owner.clone()),
        ),
        LuaTypeOwner::Member(id) => {
            note_member_read(u8::MAX - 6, id, || FactKey::TypeCache(owner.clone()))
        }
        LuaTypeOwner::SyntaxId(id) => {
            let range = id.value.get_range();
            let identity = u128::from(id.file_id.id) << 80
                | u128::from(id.value.get_kind() as u16) << 64
                | u128::from(u32::from(range.start())) << 32
                | u128::from(u32::from(range.end()));
            note_read_by((u8::MAX - 16, identity), || {
                FactKey::TypeCache(owner.clone())
            });
        }
    }
}

/// How many reads evaluations on this thread noted, duplicates included,
/// since the last call.
pub fn take_noted_reads() -> u64 {
    NOTED_READS.with(|count| count.replace(0))
}

/// Records a read of the top-level statement `statement` of `file_id`. A
/// derivation's own file is not a read, as for [`note_file_read`].
pub fn note_syntax_read(file_id: FileId, statement: TextRange) {
    note_read_of_file(file_id, || {
        FactKey::Syntax(InFiled::new(file_id, statement))
    });
}

/// Records a read of `file_id`'s text. A derivation's own file is not a
/// read: its derivations are replaced whenever that text changes.
pub fn note_file_read(file_id: FileId) {
    note_read_of_file(file_id, || FactKey::FileText(file_id));
}

/// Records a read of the innermost function scope of `file_id` around
/// `position`. A derivation's own file is not a read, as for
/// [`note_file_read`].
pub fn note_function_scope_read(file_id: FileId, position: TextSize) {
    note_read_of_file(file_id, || {
        FactKey::FunctionScope(InFiled::new(file_id, position))
    });
}

fn note_read_of_file(file_id: FileId, key: impl FnOnce() -> FactKey) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut()
            && evaluation.id.file_id != file_id
        {
            record_read(evaluation, HashedKey::new(key()));
        }
    });
}

/// Records that the current evaluation produced `key`.
pub fn note_write(key: impl FnOnce() -> FactKey) {
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            let key = HashedKey::new(key());
            let rehoming = REHOMING.with(|depth| depth.get() > 0);
            let noted = !rehoming
                && NOTED_ONCE.with(|noted| {
                    noted.borrow().as_ref().is_some_and(|(stamp, at)| {
                        *stamp == evaluation.stamp
                            && at
                                .iter()
                                .any(|at| evaluation.outputs.get(*at) == Some(&key))
                    })
                });
            if noted {
                return;
            }
            let written = next_stamp();
            // Inference memoised during an evaluation read only keys the
            // evaluation recorded, so a write it did not read leaves it valid.
            let read_here =
                transport::with_keys(|worker| worker.note_write(&key, evaluation.stamp, written))
                    .unwrap_or_else(|| {
                        KEYS.with(|keys| {
                            keys.borrow_mut().as_mut().is_none_or(|table| {
                                table.mark_written_logged(&key, evaluation.stamp, written)
                            })
                        })
                    });
            if read_here {
                WRITE_GENERATION.with(|generation| generation.set(generation.get() + 1));
            }
            if rehoming {
                evaluation.moved.insert(key);
            } else {
                NOTED_ONCE.with(|noted| {
                    if let Some((stamp, at)) = noted.borrow_mut().as_mut()
                        && *stamp == evaluation.stamp
                    {
                        at.push(evaluation.outputs.len());
                    }
                });
                evaluation.outputs.push(key);
            }
        } else {
            CAPTURED.with(|captured| {
                if let Some(captured) = captured.borrow_mut().as_mut() {
                    captured.insert(HashedKey::new(key()));
                }
            });
        }
    });
}

/// Whether a write happening now belongs to the source layer: outside any
/// evaluation, or inside a file's [`DerivationKind::Source`] derivation.
/// Indexes keep a source write so a reset can restore it.
pub fn writes_source() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_none_or(|evaluation| evaluation.id.kind == DerivationKind::Source)
    })
}

/// Whether a write of a type `file_id` owns belongs to that file's local
/// fallbacks: the types it guesses for what its statements left untyped or
/// only as a base class, which readers see over what the statements left.
pub fn writes_fallback_of(file_id: FileId) -> bool {
    CURRENT.with(|current| {
        current.borrow().as_ref().is_some_and(|evaluation| {
            evaluation.id.kind == DerivationKind::LocalFallbacks && evaluation.id.file_id == file_id
        })
    })
}

/// Whether the evaluation recording on this thread computes `key` again: it
/// produced it before, and the engine reset it for this run.
pub fn recomputes(key: FactKey) -> bool {
    if let Some(windowed) =
        transport::with_keys(|worker| worker.windowed(&HashedKey::new(key.clone())))
    {
        return windowed;
    }
    KEYS.with(|keys| {
        keys.borrow().as_ref().is_some_and(|table| {
            table
                .find(&HashedKey::new(key))
                .is_some_and(|slot| table.windowed(slot))
        })
    })
}

/// Marks the evaluation recording on this thread to run again: it wrote a
/// fact that, the first time it produced it, it may have read with another
/// layer over it.
pub fn rerun_current() {
    if let Some(id) = current() {
        RERUNS.with(|reruns| reruns.borrow_mut().push(id));
    }
}

/// The evaluations [`rerun_current`] marked on this thread, emptied.
pub fn take_reruns() -> Vec<DerivationId> {
    RERUNS.with(|reruns| std::mem::take(&mut *reruns.borrow_mut()))
}

/// Whether member listings on this thread include dynamic fields: everywhere
/// but in the pass that collects them, which must not read what it builds.
/// The open frames consult the view's dynamic-field gate, so a memo answered
/// either way holds only under a view that answers the same.
pub fn dynamic_fields_visible() -> bool {
    consult_gates(DYNAMIC_FIELDS_GATE);
    current().is_none_or(|id| id.kind != DerivationKind::DynamicFieldWrite)
}

/// The derivation being evaluated on this thread, if any.
pub fn current() -> Option<DerivationId> {
    CURRENT.with(|current| current.borrow().as_ref().map(|evaluation| evaluation.id))
}

#[cfg(test)]
mod noting_tests {
    use super::*;

    fn noted(evaluation: &Evaluation, key: &FactKey) -> usize {
        evaluation
            .outputs
            .iter()
            .filter(|output| output.key() == key)
            .count()
    }

    #[test]
    fn a_scope_notes_each_output_once_and_leaves_writes_outside_it_alone() {
        let file = FileId::new(1);
        let _recording = begin(DerivationId::workspace(DerivationKind::GmodLoad));
        noting_writes_once(|| {
            note_write(|| FactKey::MemberFile(file));
            note_write(|| FactKey::NetworkScan);
            note_write(|| FactKey::MemberFile(file));
        });
        note_write(|| FactKey::NetworkScan);
        let evaluation = end();

        assert_eq!(
            (
                noted(&evaluation, &FactKey::MemberFile(file)),
                noted(&evaluation, &FactKey::NetworkScan)
            ),
            (1, 2)
        );
    }
}

#[cfg(test)]
mod member_read_tests {
    use super::*;

    /// The members one node makes in different slots are read as different facts.
    #[test]
    fn reads_of_members_one_node_makes_are_recorded_for_each_slot() {
        let file = FileId::new(1);
        let call = glua_parser::LuaSyntaxId::new(
            glua_parser::LuaSyntaxKind::CallExpr.into(),
            rowan::TextRange::new(0.into(), 6.into()),
        );
        let owners =
            [0, 1, 2].map(|slot| LuaTypeOwner::Member(LuaMemberId::new_in_slot(call, file, slot)));
        let _recording = begin(DerivationId::workspace(DerivationKind::GmodLoad));
        for owner in &owners {
            note_type_cache_read(owner);
            note_type_cache_read(owner);
        }
        let (_, reads, _) = end().into_unfingerprinted_parts();
        let mut read_keys = reads.into_iter().map(|(key, _)| key).collect::<Vec<_>>();
        let mut expected = owners
            .iter()
            .map(|owner| HashedKey::new(FactKey::TypeCache(owner.clone())))
            .collect::<Vec<_>>();
        read_keys.sort_by_key(|key| format!("{key:?}"));
        expected.sort_by_key(|key| format!("{key:?}"));
        assert_eq!(read_keys, expected);
    }
}

#[cfg(test)]
mod unwind_tests {
    use super::*;

    fn reads(evaluation: Evaluation) -> Vec<HashedKey> {
        let (_, reads, _) = evaluation.into_unfingerprinted_parts();
        reads.into_iter().map(|(key, _)| key).collect()
    }

    /// An evaluation that panics leaves the thread recording as it did before
    /// it began: the one it suspended records again, and the next evaluation
    /// records only its own reads.
    #[test]
    fn an_evaluation_that_unwinds_leaves_the_thread_recording_as_before() {
        let unwound = std::panic::catch_unwind(|| capture_writes(|| panic!("a reset panics")));
        assert!(unwound.is_err());
        let outer_id = DerivationId::workspace(DerivationKind::GmodLoad);
        let _outer = begin(outer_id);
        let unwound = std::panic::catch_unwind(|| {
            let _inner = begin(DerivationId::workspace(DerivationKind::ValidGuards));
            let _nested = begin(DerivationId::workspace(DerivationKind::NetHelperNames));
            rehoming(|| untracked(|| panic!("an evaluation panics")));
        });
        assert!(unwound.is_err());

        assert_eq!(current(), Some(outer_id));
        assert_eq!(SUSPENDED.with(|suspended| suspended.borrow().len()), 0);
        assert_eq!(
            (
                UNTRACKED.with(std::cell::Cell::get),
                REHOMING.with(std::cell::Cell::get),
                CAPTURED.with(|captured| captured.borrow().is_some()),
            ),
            (0, 0, false)
        );
        note_read(|| FactKey::NetworkScan);
        assert_eq!(reads(end()), vec![HashedKey::new(FactKey::NetworkScan)]);
        assert!(!recording());

        let _next = begin(DerivationId::workspace(DerivationKind::ValidGuards));
        note_read(|| FactKey::MemberFile(FileId::new(1)));
        assert_eq!(
            reads(end()),
            vec![HashedKey::new(FactKey::MemberFile(FileId::new(1)))]
        );
    }
}
