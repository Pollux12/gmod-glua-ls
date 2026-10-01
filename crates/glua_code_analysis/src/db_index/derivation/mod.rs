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

use std::cell::RefCell;

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;

mod paged;
pub use frames::{
    Gates, OpenFrame, Provenance, Reuse, ScopeMark, check_cache, close_frame, consult_absorbed,
    consult_cycle, consult_gates, consult_realm, consult_scope, consult_unkeyed, frame_depth,
    new_stamp, note_recomputed, note_recomputing, note_written, open_frame, open_frame_absorbing,
    report_frames, reuse, scope_mark, taint, take_reused_across,
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

    /// The ranges of the parts other files read one at a time, as
    /// [`FactKey::NetHelper`].
    fn part_ranges(&self) -> Vec<TextRange> {
        Vec::new()
    }

    /// One part's fingerprint, [`ABSENT`] when there is none.
    fn part_fingerprint(&self, _range: TextRange) -> Fingerprint {
        ABSENT
    }
}

/// The workspace call roles and net-helper registry: the folds read it whole,
/// a per-file pass entry by entry, as [`FactKey::CallRole`].
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
pub use graph::{KeyIdx, KeyTable, ReaderGraph};
pub use positions::{
    EditMaps, FilePosition, Local, fingerprint_before_edits, fingerprinting, hash_local, in_file,
    mapping_back, unmapped,
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
}

impl StageRule {
    pub const ALL: [Self; 2] = [Self::SkipUntypedWriters, Self::FloorUnsettled];

    /// The first stage the rule holds at.
    pub fn holds_from(self) -> AnalysisStage {
        match self {
            Self::SkipUntypedWriters => AnalysisStage::SkipUntyped,
            Self::FloorUnsettled => AnalysisStage::Closed,
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
    /// A type declaration's supertypes.
    TypeSupers(LuaTypeDeclId),
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
    /// The dynamic-field definitions of one file.
    DynamicFieldFile(FileId),
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
    /// The call-site parameter evidence one file's calls contributed.
    CallSiteFile(FileId),
    /// A module's export and metadata.
    Module(FileId),
    /// A module's metadata without its export: path, name, workspace, flags
    /// and legacy environments.
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
    /// What the callee of each of a file's calls resolves to.
    CallTargets(FileId),
    /// A file's flow tree and casts.
    FlowTree(FileId),
    /// One dynamic field (`None`: the owner's named fields as a whole).
    DynamicField(DynamicFieldOwner, Option<SmolStr>),
    /// An owner's wildcard (dynamic-key) definition sites.
    DynamicFieldWildcards(DynamicFieldOwner),
    /// An owner's members written through a finite set of names.
    DynamicFieldFinite(DynamicFieldOwner),
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
    /// The set of files that have vgui parent calls.
    VguiParentCallFiles,
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
    /// overloads, generic params and colon and vararg flags. Read out of its
    /// [`FactKey::Signature`], which also holds what inference resolved.
    SignatureDecl(LuaSignatureId),
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
    /// A file's network flows.
    NetworkFile(FileId),
    /// Network flows by message name (whole-map).
    NetworkScan,
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
    /// One file's net call roles and function-body net summaries.
    NetHelpers(FileId),
    /// One function body's net summary, by the body's block.
    NetHelper(InFiled<TextRange>),
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
    /// Name-keyed references across files.
    ReferencesByName(SmolStr),
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
    /// One top-level statement of a file, by its range: its kind and text.
    Syntax(InFiled<TextRange>),
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

/// The fingerprint of a present value; never [`ABSENT`].
pub fn fingerprint_of<T: std::hash::Hash + ?Sized>(value: &T) -> Fingerprint {
    use std::hash::Hasher;
    let mut hasher = rustc_hash::FxHasher::default();
    positions::hash_value(value, &mut hasher);
    let hash = hasher.finish();
    hash.max(1)
}

/// An order-independent fingerprint of a set, kept up to date as members
/// come and go so reading it costs nothing however large the set grows.
#[derive(Debug, Clone, Copy, Default)]
pub struct SetFingerprint(Fingerprint);

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
                | K::Signature(_)
                | K::ValidGuard(_)
                | K::InferredGuard(_)
                | K::Module(_)
                | K::SpecialCall(..)
                | K::CallTargets(_)
                | K::FlowTree(_)
                | K::Metatable(_)
                | K::VguiChain(_)
                | K::GmodInferFile(_)
                | K::GmodRealm(_)
                | K::GmodLoadFile(_)
                | K::LoadScan(_)
                | K::NetworkFile(_)
                | K::HelperRegistry
                | K::CallRole(_)
                | K::HelperScan(_)
                | K::RoleMentions(_)
                | K::NetHelpers(_)
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
                | K::DynamicFieldFile(_)
                | K::GmodClassFile(_)
                | K::CallSiteFile(_)
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
    /// remap re-homes.
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
                | K::DynamicFieldFile(_)
                | K::TypeCache(_)
                | K::TypeFallbacks(_)
                | K::DefinitionFact(_)
                | K::Signature(_)
                | K::SignatureDecl(_)
                | K::DynamicField(..)
                | K::DynamicFieldWildcards(_)
                | K::DynamicFieldFinite(_)
                | K::Metatable(_)
                | K::SpecialCall(..)
                | K::Member(_)
                | K::MemberSlot(..)
                | K::MemberOwnerMap(_)
                | K::MemberOwnerExprKeys(_)
                | K::MemberOwnerFields(_)
                | K::Contrib(..)
                | K::CallSiteFile(_)
                | K::NetHelpers(_)
                | K::NetHelper(_)
                | K::NetworkFile(_)
                | K::NetworkScan
                | K::CallTargets(_)
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
            FactKey::GmodRealm(_) | FactKey::GmodInferFile(_) | FactKey::Signature(_)
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
            FactKey::SignatureDecl(signature_id) => Some(FactKey::Signature(*signature_id)),
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
            K::Member(member_id) => Some(member_id.file_id),
            K::NetHelper(site) => Some(site.file_id),
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
            | K::Metatable(site)
            | K::TableClass(site)
            | K::Syntax(site) => Some(site.file_id),
            K::Signature(signature_id)
            | K::SignatureDecl(signature_id)
            | K::ValidGuard(signature_id)
            | K::InferredGuard(signature_id)
            | K::Contrib(signature_id, _)
            | K::ReceiverAuthority(signature_id)
            | K::CallbackParams(signature_id)
            | K::CallRole(CallRoleKey::Signature(signature_id)) => Some(signature_id.get_file_id()),
            K::SpecialCall(file_id, _)
            | K::SpecialCallFile(file_id)
            | K::CallTargets(file_id)
            | K::ValidGuardFile(file_id)
            | K::TypeFile(file_id)
            | K::DeclTree(file_id)
            | K::MemberFile(file_id)
            | K::SignatureFile(file_id)
            | K::DynamicFieldFile(file_id)
            | K::Module(file_id)
            | K::ModuleMeta(file_id)
            | K::FlowTree(file_id)
            | K::OperatorFile(file_id)
            | K::MetatableFactory(file_id)
            | K::GmodClassFile(file_id)
            | K::GmodInferFile(file_id)
            | K::GmodRealm(file_id)
            | K::GmodLoadFile(file_id)
            | K::LoadScan(file_id)
            | K::NetworkFile(file_id)
            | K::HelperScan(file_id)
            | K::RoleMentions(file_id)
            | K::NetHelpers(file_id)
            | K::FunctionSummaries(file_id)
            | K::References(file_id)
            | K::AccessorCalls(file_id)
            | K::Dependency(file_id)
            | K::LoadSites(file_id)
            | K::DiagnosticFile(file_id)
            | K::CallSiteFile(file_id)
            | K::TypeFallbacks(file_id)
            | K::DerivedReferences(file_id)
            | K::FileText(file_id) => Some(*file_id),
            K::DynamicField(DynamicFieldOwner::Table(site), _)
            | K::DynamicFieldWildcards(DynamicFieldOwner::Table(site))
            | K::DynamicFieldFinite(DynamicFieldOwner::Table(site)) => Some(site.file_id),
            K::OperatorOwner(LuaOperatorOwner::Table(site)) => Some(site.file_id),
            K::TypeDecl(_)
            | K::TypeDeclLocations(_)
            | K::TypeSupers(_)
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
            | K::UnattributedField(_)
            | K::OperatorOwner(_)
            | K::GmodMeta(_)
            | K::VguiParentCallFiles
            | K::VguiChain(_)
            | K::GmodInferScan
            | K::GmodSystem(..)
            | K::NetworkScan
            | K::HelperRegistry
            | K::CallRole(CallRoleKey::Path(_) | CallRoleKey::Global(_) | CallRoleKey::Method(_))
            | K::HelperScanFiles
            | K::LoadScanFiles
            | K::FileSet
            | K::StageRule(_)
            | K::ValidGuardFiles
            | K::ValidGuardName(_)
            | K::NetHelperName(_)
            | K::Property(_)
            | K::Global(_)
            | K::GlobalScan
            | K::ReferencesByName(_)
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
    /// The workspace vgui parent chains.
    VguiChains,
    /// A file's `AccessorFunc` getters and setters.
    AccessorFunc,
    /// A file's dynamic fields.
    DynamicFields,
    /// A file's setmetatable factory members.
    SetmetatableFactory,
    /// What the callee of each of a file's calls resolves to.
    CallTargets,
    /// A query: whether a method's receiver is authoritative.
    ReceiverAuthority,
    /// A query: what a function passes to each callback parameter it calls.
    CallbackParams,
    /// A file's call-site parameter contributions and return consumers.
    CallSiteParams,
    /// A file's local types guessed from their uses.
    LocalFallbacks,
    /// A file's net call roles and function-body net summaries.
    NetHelpers,
    /// A file's net message flows.
    Network,
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
            kind => kind as u8,
        }
    }

    /// The first stage a derivation of this kind runs at: work of the kind
    /// queued before it waits until the run reaches it.
    pub fn first_stage(self) -> AnalysisStage {
        match self {
            // Locals are guessed only once unsettled reads floor.
            Self::LocalFallbacks => StageRule::FloorUnsettled.holds_from(),
            // The passes that infer across a whole file run once the
            // statements settled: run earlier, a file's pass runs again whole
            // for each read that settles after it.
            Self::DynamicFields
            | Self::CallTargets
            | Self::ReceiverAuthority
            | Self::CallbackParams
            | Self::CallSiteParams
            | Self::NetHelpers
            | Self::Network => AnalysisStage::Closed,
            _ => AnalysisStage::Open,
        }
    }

    /// Whether derivations of this kind are statements.
    pub fn is_statement(self) -> bool {
        self.rank() == Self::LocalStat.rank()
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
    /// The keys read since the evaluation began or last resumed, each once:
    /// where each was last logged and whether reading it taints.
    local: Vec<LocalRead>,
    /// The entry of `local` each slot of the lent table read has.
    by_slot: rustc_hash::FxHashMap<KeyIdx, u32>,
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
    /// Cheap identities of keys already read, each with its entry of
    /// `local`; see [`note_read_by`].
    read_identities: rustc_hash::FxHashMap<(u8, u128), u32>,
    /// Keys already read that no identity names, each with its entry of
    /// `local`.
    plain_reads: hashbrown::HashTable<(HashedKey, u32)>,
    outputs: FxHashSet<HashedKey>,
    /// Keys the evaluation re-filed without producing them, see [`rehoming`].
    moved: FxHashSet<HashedKey>,
    /// Fingerprints taken when an earlier stage of a staged evaluation ended:
    /// other evaluations write between its stages.
    staged: rustc_hash::FxHashMap<HashedKey, Fingerprint>,
}

impl Evaluation {
    /// The keys the evaluation wrote.
    pub fn outputs(&self) -> impl Iterator<Item = &HashedKey> {
        self.outputs.iter()
    }

    /// The keys the evaluation re-filed without producing them: their readers
    /// are re-checked, but the evaluation is not one of their producers.
    pub fn moved(&self) -> impl Iterator<Item = &HashedKey> {
        self.moved.iter()
    }

    /// Fingerprints the reads made so far, for an evaluation suspended while
    /// other evaluations run: what it read is compared with what it saw, not
    /// with what they wrote afterwards.
    pub fn fingerprint_stage(&mut self, fingerprint: impl Fn(&FactKey) -> Fingerprint) {
        for key in &self.reads {
            if !self.staged.contains_key(key) {
                self.staged.insert(key.clone(), fingerprint(key));
            }
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

    /// The evaluation's interned reads, its reads made while no key table
    /// was lent, and its outputs.
    #[allow(clippy::type_complexity)]
    pub fn into_recorded(self) -> (DerivationId, Vec<KeyIdx>, Vec<HashedKey>, Vec<HashedKey>) {
        // A resumed evaluation logs again what it read before it resumed.
        let mut slots = self.read;
        slots.sort_unstable();
        slots.dedup();
        debug_assert!({
            let mut logged = self.log.clone();
            logged.sort_unstable();
            logged.dedup();
            logged == slots
        });
        (
            self.id,
            slots,
            self.reads.into_iter().collect(),
            self.outputs.into_iter().collect(),
        )
    }

    /// The reads of an evaluation that viewed `graph` (see [`view_graph`]),
    /// each fingerprinted as it stands.
    pub fn into_viewed_reads(
        self,
        graph: &ReaderGraph,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
    ) -> ViewedReads {
        let id = self.id;
        let mut slots = self
            .viewed
            .into_iter()
            .filter(|slot| records_read(id, graph.key(*slot), |_| false))
            .map(|slot| (slot, graph.slot_fingerprint(slot, &fingerprint)))
            .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|(slot, _)| *slot);
        let keys = self
            .reads
            .into_iter()
            .filter(|key| records_read(id, key, |_| false))
            .map(|key| {
                let seen = fingerprint(&key);
                (key, seen)
            })
            .collect();
        ViewedReads { slots, keys }
    }

    /// [`Self::into_parts`] with each read's fingerprint left to the caller,
    /// except those an earlier stage already took.
    #[allow(clippy::type_complexity)]
    pub fn into_unfingerprinted_parts(
        self,
    ) -> (
        DerivationId,
        Vec<(HashedKey, Option<Fingerprint>)>,
        Vec<HashedKey>,
    ) {
        debug_assert!(self.log.is_empty(), "interned reads need the key table");
        let outputs = self.outputs;
        let id = self.id;
        let reads = self
            .reads
            .into_iter()
            .filter(|key| records_read(id, key, |key| outputs.contains(key)))
            .map(|key| {
                let seen = self
                    .staged
                    .get(&key)
                    .filter(|_| !outputs.contains(&key))
                    .copied();
                (key, seen)
            })
            .collect();
        (self.id, reads, outputs.into_iter().collect())
    }
}

/// What an evaluation that viewed the reader graph read, with the
/// fingerprints it saw: slots of the graph as it stood, which hold only until
/// the graph next changes, and the keys it had no slot for.
#[derive(Debug, Clone, Default)]
pub struct ViewedReads {
    slots: Vec<(KeyIdx, Fingerprint)>,
    keys: Vec<(HashedKey, Fingerprint)>,
}

impl ViewedReads {
    /// Reads made with no graph viewed, each with the fingerprint it saw.
    pub fn from_keys(keys: Vec<(HashedKey, Fingerprint)>) -> Self {
        Self {
            slots: Vec::new(),
            keys,
        }
    }

    /// Every key read, for tests.
    #[cfg(test)]
    pub fn keys<'a>(&'a self, graph: &'a ReaderGraph) -> impl Iterator<Item = &'a FactKey> {
        self.slots
            .iter()
            .map(|(slot, _)| graph.key(*slot).key())
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

/// What this thread records into, kept in one thread-local so a read looks it
/// up once.
struct Recorder {
    /// The evaluation recording.
    current: Option<Evaluation>,
    /// The key table lent to this thread; see [`lend_keys`].
    keys: Option<KeyTable>,
    /// The reader graph evaluations read without a lent table.
    view: Option<std::sync::Arc<ReaderGraph>>,
    /// How deep [`untracked`] regions nest.
    untracked: u32,
    /// How deep [`rehoming`] regions nest.
    rehoming: u32,
    /// The reads noted, duplicates included; see [`take_noted_reads`].
    noted: u64,
}

thread_local! {
    static RECORDER: RefCell<Recorder> = const {
        RefCell::new(Recorder {
            current: None,
            keys: None,
            view: None,
            untracked: 0,
            rehoming: 0,
            noted: 0,
        })
    };
    static LAST_WRITE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Evaluations suspended on this thread by one begun inside them.
    static SUSPENDED: RefCell<Vec<Evaluation>> = const { RefCell::new(Vec::new()) };
    static WRITE_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CAPTURED: RefCell<Option<FxHashSet<HashedKey>>> = const { RefCell::new(None) };
    /// The lookups of evaluations that ended, for the next ones to begin with.
    static SPARE: RefCell<Vec<Spare>> = const { RefCell::new(Vec::new()) };
    /// The evaluations to run again; see [`rerun_current`].
    static RERUNS: RefCell<Vec<DerivationId>> = const { RefCell::new(Vec::new()) };
}

/// Lets evaluations on this thread reuse memoised values against `graph`
/// without a lent table, until the returned guard drops.
pub fn view_graph(graph: std::sync::Arc<ReaderGraph>) -> ViewedGraph {
    RECORDER.with_borrow_mut(|recorder| recorder.view = Some(graph));
    ViewedGraph
}

/// Ends [`view_graph`] when dropped, unwinding included.
#[must_use]
pub struct ViewedGraph;

impl Drop for ViewedGraph {
    fn drop(&mut self) {
        RECORDER.with_borrow_mut(|recorder| recorder.view = None);
    }
}

/// Runs `f` on the evaluation recording on this thread and the graph it
/// views, if there are both.
fn with_viewed<T>(f: impl FnOnce(&mut Evaluation, &ReaderGraph) -> T) -> Option<T> {
    RECORDER.with_borrow_mut(|recorder| {
        let Recorder {
            current: Some(evaluation),
            view: Some(graph),
            ..
        } = recorder
        else {
            return None;
        };
        Some(f(evaluation, graph))
    })
}

/// Runs `f` outside any evaluation and returns every key it wrote: a reset
/// moves facts other derivations produced, whose readers must be re-checked.
pub fn capture_writes(f: impl FnOnce()) -> FxHashSet<HashedKey> {
    debug_assert!(
        RECORDER.with_borrow(|recorder| recorder.current.is_none()),
        "writes captured inside an evaluation"
    );
    let previous = CAPTURED.with(|captured| captured.replace(Some(FxHashSet::default())));
    f();
    CAPTURED
        .with(|captured| captured.replace(previous))
        .unwrap_or_default()
}

/// Advances whenever an evaluation writes a fact, so inference memoised
/// before the write can tell it is stale.
pub fn write_generation() -> u64 {
    WRITE_GENERATION.with(std::cell::Cell::get)
}

/// Runs `f` without recording its reads. For an index's own bookkeeping: a
/// lookup that only decides where a write is stored, when the index keeps
/// that placement consistent itself whatever the order of writes.
pub fn untracked<T>(f: impl FnOnce() -> T) -> T {
    RECORDER.with_borrow_mut(|recorder| recorder.untracked += 1);
    let result = f();
    RECORDER.with_borrow_mut(|recorder| recorder.untracked -= 1);
    result
}

/// Runs `f` with its writes recorded as moves, not outputs. For an index
/// re-filing facts other derivations produced, when the index keeps that
/// placement consistent itself whatever the order of writes: the moved facts'
/// readers are re-checked, but the current derivation does not become one of
/// their producers.
pub fn rehoming<T>(f: impl FnOnce() -> T) -> T {
    RECORDER.with_borrow_mut(|recorder| recorder.rehoming += 1);
    let result = f();
    RECORDER.with_borrow_mut(|recorder| recorder.rehoming -= 1);
    result
}

/// Lends `table` to this thread: evaluations begun until [`take_keys`]
/// intern their reads as they make them.
pub fn lend_keys(table: KeyTable) {
    RECORDER.with_borrow_mut(|recorder| recorder.keys = Some(table));
}

/// Takes back the table [`lend_keys`] lent.
pub fn take_keys() -> KeyTable {
    RECORDER
        .with_borrow_mut(|recorder| recorder.keys.take())
        .expect("take_keys without lend_keys")
}

/// Drops a lent table left behind by an evaluation that unwound, so later
/// evaluations on this thread do not intern into it.
pub fn forget_lent_keys() {
    RECORDER.with_borrow_mut(|recorder| recorder.keys = None);
}

/// Starts recording the evaluation of `id` on this thread.
pub fn begin(id: DerivationId) {
    let stamp = next_stamp();
    let spare = SPARE
        .with(|spare| spare.borrow_mut().pop())
        .unwrap_or_default();
    start(Evaluation {
        id,
        log: Vec::new(),
        read: Vec::new(),
        local: spare.local,
        by_slot: spare.by_slot,
        frames: spare.frames,
        unkeyed: spare.unkeyed,
        checks: FxHashSet::default(),
        log_handle: std::sync::Arc::new(frames::LogHandle::default()),
        stamp,
        reads: FxHashSet::default(),
        viewed: FxHashSet::default(),
        read_identities: spare.read_identities,
        named_reads: spare.named_reads,
        plain_reads: spare.plain_reads,
        outputs: FxHashSet::default(),
        moved: FxHashSet::default(),
        staged: rustc_hash::FxHashMap::default(),
    });
}

/// The lookups an evaluation needs only while it records, kept emptied for
/// the next evaluation on the thread to begin with.
#[derive(Default)]
struct Spare {
    local: Vec<LocalRead>,
    by_slot: rustc_hash::FxHashMap<KeyIdx, u32>,
    read_identities: rustc_hash::FxHashMap<(u8, u128), u32>,
    named_reads: hashbrown::HashTable<NamedRead>,
    plain_reads: hashbrown::HashTable<(HashedKey, u32)>,
    frames: Vec<frames::Frame>,
    unkeyed: Vec<u64>,
}

/// The most entries a kept lookup may have room for: emptying one costs its
/// room, which a rare large evaluation must not make every later one pay.
const SPARE_ROOM: usize = 1 << 12;

/// How many kept lookups a thread holds: as many as evaluations nest.
const SPARE_DEPTH: usize = 8;

impl Spare {
    /// Takes `evaluation`'s lookups, emptied, if they are small enough to keep.
    fn take(evaluation: &mut Evaluation) -> Self {
        fn kept<T: Default>(lookup: &mut T, capacity: impl Fn(&T) -> usize) -> T {
            let lookup = std::mem::take(lookup);
            if capacity(&lookup) > SPARE_ROOM {
                T::default()
            } else {
                lookup
            }
        }
        let mut spare = Self {
            local: kept(&mut evaluation.local, Vec::capacity),
            by_slot: kept(&mut evaluation.by_slot, |map| map.capacity()),
            read_identities: kept(&mut evaluation.read_identities, |map| map.capacity()),
            named_reads: kept(&mut evaluation.named_reads, hashbrown::HashTable::capacity),
            plain_reads: kept(&mut evaluation.plain_reads, hashbrown::HashTable::capacity),
            frames: std::mem::take(&mut evaluation.frames),
            unkeyed: std::mem::take(&mut evaluation.unkeyed),
        };
        spare.local.clear();
        spare.by_slot.clear();
        spare.read_identities.clear();
        spare.named_reads.clear();
        spare.plain_reads.clear();
        spare.frames.clear();
        spare.unkeyed.clear();
        spare
    }
}

/// Makes `evaluation` the one recording on this thread. One already recording
/// is suspended until `evaluation` ends: a thread waiting in the pool runs
/// other work, which may record its own evaluation.
fn start(evaluation: Evaluation) {
    if let Some(previous) =
        RECORDER.with_borrow_mut(|recorder| recorder.current.replace(evaluation))
    {
        SUSPENDED.with(|suspended| suspended.borrow_mut().push(previous));
    }
}

/// Runs `f` as the evaluation of `id`, nested in whatever this thread is
/// recording, and returns what it read, as keys. Its reads are its own: the
/// suspended evaluation's key table, view and untracked or re-homing regions
/// do not apply to it, so what it read can be filed whatever it ran inside.
pub fn detached<T>(id: DerivationId, f: impl FnOnce() -> T) -> (T, Vec<HashedKey>) {
    let (keys, view, untracked, rehoming) = RECORDER.with_borrow_mut(|recorder| {
        (
            recorder.keys.take(),
            recorder.view.take(),
            std::mem::replace(&mut recorder.untracked, 0),
            std::mem::replace(&mut recorder.rehoming, 0),
        )
    });
    begin(id);
    let result = f();
    let evaluation = end();
    RECORDER.with_borrow_mut(|recorder| {
        recorder.keys = keys;
        recorder.view = view;
        recorder.untracked = untracked;
        recorder.rehoming = rehoming;
    });
    let (_, reads, _) = evaluation.into_unfingerprinted_parts();
    (result, reads.into_iter().map(|(key, _)| key).collect())
}

/// Continues recording an evaluation that [`end`] handed back, for a
/// derivation evaluated in stages.
pub fn resume(mut evaluation: Evaluation) {
    // Its lookups went back to the thread when it ended, so it notes its
    // reads afresh, and memos it reused before replay into it again.
    evaluation.stamp = next_stamp();
    start(evaluation);
}

/// Stops recording and hands back what the evaluation read and wrote.
pub fn end() -> Evaluation {
    let mut evaluation = RECORDER.with_borrow_mut(|recorder| {
        let evaluation = recorder
            .current
            .take()
            .expect("derivation::end without begin");
        recorder.current = SUSPENDED.with(|suspended| suspended.borrow_mut().pop());
        evaluation
    });
    evaluation.log_handle.freeze(&evaluation.log);
    let spare = Spare::take(&mut evaluation);
    SPARE.with(|kept| {
        // A resumed evaluation ends without having begun on this thread.
        let mut kept = kept.borrow_mut();
        if kept.len() < SPARE_DEPTH {
            kept.push(spare);
        }
    });
    evaluation
}

/// Whether a key table is lent to this thread.
#[cfg(test)]
pub fn keys_lent() -> bool {
    RECORDER.with_borrow(|recorder| recorder.keys.is_some())
}

/// Whether an evaluation is recording on this thread. Callers building a
/// costly key check this first.
#[inline]
pub fn recording() -> bool {
    RECORDER.with_borrow(|recorder| recorder.current.is_some())
        || CAPTURED.with(|captured| captured.borrow().is_some())
}

/// The evaluation recording on this thread, outside untracked regions, with
/// the table lent to it and the graph it views; counts the read noted.
#[inline]
fn noting<T>(
    f: impl FnOnce(&mut Evaluation, Option<&mut KeyTable>, Option<&ReaderGraph>) -> T,
) -> Option<T> {
    RECORDER.with_borrow_mut(|recorder| {
        let Recorder {
            current: Some(evaluation),
            keys,
            view,
            untracked: 0,
            noted,
            ..
        } = recorder
        else {
            return None;
        };
        *noted += 1;
        Some(f(evaluation, keys.as_mut(), view.as_deref()))
    })
}

/// Records a read of `key` by the current evaluation.
pub fn note_read(key: impl FnOnce() -> FactKey) {
    noting(|evaluation, table, view| evaluation.note_key(table, view, HashedKey::new(key())));
}

/// Orders every evaluation, frame and write on every thread: a key table keeps
/// the stamps its slots were logged and written at from one re-analysis to the
/// next, and the next may run on another thread.
static STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_stamp() -> u64 {
    STAMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// The stamp of the latest write an evaluation made on this thread.
fn last_write() -> u64 {
    LAST_WRITE.with(std::cell::Cell::get)
}

fn latest_stamp() -> u64 {
    STAMP.load(std::sync::atomic::Ordering::Relaxed)
}

/// A key an evaluation read, as it keeps it while it records.
#[derive(Debug, Clone, Copy)]
struct LocalRead {
    /// Its slot in the lent table.
    slot: KeyIdx,
    /// One past where the log last took it (0: nowhere).
    logged: u32,
    /// Whether reading it taints the open frames (see [`KeyTable::taints`]),
    /// which does not change while the evaluation records.
    taints: bool,
}

/// The entry of a read made without a lent table: nothing logs it.
const UNLOGGED: u32 = u32::MAX;

impl Evaluation {
    /// The stamp of the innermost open frame, or of the evaluation.
    fn frame_stamp(&self) -> u64 {
        self.frames.last().map_or(self.stamp, |frame| frame.stamp)
    }

    /// Where the reads of the innermost open frame begin in the log, 0 with
    /// none open.
    fn frame_start(&self) -> u32 {
        self.frames.last().map_or(0, |frame| frame.start)
    }

    /// Logs the read `local` into the innermost open frame, unless the
    /// frame holds it already.
    #[inline]
    fn log_local(&mut self, local: u32) {
        let start = self.frame_start();
        let entry = &mut self.local[local as usize];
        if entry.logged > start {
            return;
        }
        entry.logged = u32::try_from(self.log.len() + 1).expect("fewer than 2^32 reads");
        self.log.push(entry.slot);
    }

    /// The entry of `local` for `slot` of the lent table, made on its first
    /// read.
    fn local_of(&mut self, table: &KeyTable, slot: KeyIdx) -> u32 {
        let local = &mut self.local;
        let read = &mut self.read;
        *self.by_slot.entry(slot).or_insert_with(|| {
            read.push(slot);
            local.push(LocalRead {
                slot,
                logged: 0,
                taints: table.taints(slot),
            });
            u32::try_from(local.len() - 1).expect("fewer than 2^32 reads")
        })
    }

    /// Records again the read the entry `local` keeps.
    #[inline]
    fn reread(&mut self, local: u32) {
        if local == UNLOGGED {
            return;
        }
        self.log_local(local);
        if self.local[local as usize].taints {
            frames::taint_evaluation(self);
        }
    }

    /// Records the first read of `key` since the evaluation began or resumed:
    /// interned and logged if a key table is lent, else as its slot in the
    /// viewed graph if it has one. Returns its entry of `local`, or
    /// [`UNLOGGED`].
    fn first_read(
        &mut self,
        table: Option<&mut KeyTable>,
        view: Option<&ReaderGraph>,
        key: &HashedKey,
    ) -> u32 {
        let Some(table) = table else {
            match view.and_then(|graph| graph.find_slot(key)) {
                Some(slot) => {
                    self.viewed.insert(slot);
                }
                None => {
                    self.reads.insert(key.clone());
                }
            }
            return UNLOGGED;
        };
        let slot = table.intern_read(key, self.stamp);
        let local = self.local_of(table, slot);
        self.reread(local);
        local
    }

    /// Records a read of `key`, which no identity names.
    fn note_key(
        &mut self,
        table: Option<&mut KeyTable>,
        view: Option<&ReaderGraph>,
        key: HashedKey,
    ) {
        if let Some((_, local)) = self.plain_reads.find(key.hash, |(read, _)| *read == key) {
            let local = *local;
            self.reread(local);
            return;
        }
        let local = self.first_read(table, view, &key);
        self.plain_reads
            .insert_unique(key.hash, (key, local), |(read, _)| read.hash);
    }

    /// Records a read of `slot` of the lent table that a reused value made.
    fn replay(&mut self, table: &KeyTable, slot: KeyIdx) {
        let local = self.local_of(table, slot);
        self.log_local(local);
    }
}

/// Records a read of `key`, which `identity` names exactly: a repeat within
/// the evaluation is one small-set probe instead of building and hashing the
/// key. The identity must be unique to the key for as long as the recorded
/// key lives.
fn note_read_by(identity: (u8, u128), key: impl FnOnce() -> FactKey) {
    noting(
        |evaluation, table, view| match evaluation.read_identities.get(&identity) {
            Some(local) => {
                let local = *local;
                evaluation.reread(local);
            }
            None => {
                let local = evaluation.first_read(table, view, &HashedKey::new(key()));
                evaluation.read_identities.insert(identity, local);
            }
        },
    );
}

/// A read [`note_named_read`] recorded: the owner identity and name that
/// name the key, and its entry of [`Evaluation::local`].
#[derive(Debug)]
struct NamedRead {
    tag: u8,
    owner: u128,
    name: Option<SmolStr>,
    local: u32,
}

fn named_read_hash(tag: u8, owner: u128, name: Option<&str>) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one((tag, owner, name))
}

/// Records a read of the key `key` builds, which the owner identity `owner`
/// and `name` name exactly under `tag`: a repeat within the evaluation is one
/// probe of a small table, without building the key or its name.
fn note_named_read(tag: u8, owner: u128, name: Option<&str>, key: impl FnOnce() -> FactKey) {
    noting(|evaluation, table, view| {
        let hash = named_read_hash(tag, owner, name);
        let found = evaluation
            .named_reads
            .find(hash, |read| {
                read.tag == tag && read.owner == owner && read.name.as_deref() == name
            })
            .map(|read| read.local);
        match found {
            Some(local) => evaluation.reread(local),
            None => {
                let local = evaluation.first_read(table, view, &HashedKey::new(key()));
                evaluation.named_reads.insert_unique(
                    hash,
                    NamedRead {
                        tag,
                        owner,
                        name: name.map(SmolStr::new),
                        local,
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
    });
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

/// Records a read of `decl_id`'s supertypes.
pub fn note_type_supers_read(decl_id: &LuaTypeDeclId) {
    note_read_by(
        (u8::MAX - 7, u128::from(decl_id.interned_address())),
        || FactKey::TypeSupers(decl_id.clone()),
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

/// An exact identity for a member.
fn member_identity(id: &LuaMemberId) -> u128 {
    let syntax = id.get_syntax_id();
    let range = syntax.get_range();
    u128::from(id.file_id.id) << 80
        | u128::from(syntax.get_kind() as u16) << 64
        | u128::from(u32::from(range.start())) << 32
        | u128::from(u32::from(range.end()))
}

/// Records a read of the member object `id`.
pub fn note_member_read(id: LuaMemberId) {
    note_read_by((u8::MAX - 1, member_identity(&id)), || FactKey::Member(id));
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

/// Records a read of `owner`'s stored type.
pub fn note_type_cache_read(owner: &LuaTypeOwner) {
    match owner {
        LuaTypeOwner::Decl(id) => note_read_by(
            (u8::MAX - 5, file_position(id.file_id, id.position)),
            || FactKey::TypeCache(owner.clone()),
        ),
        LuaTypeOwner::Member(id) => note_read_by((u8::MAX - 6, member_identity(id)), || {
            FactKey::TypeCache(owner.clone())
        }),
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
    RECORDER.with_borrow_mut(|recorder| std::mem::take(&mut recorder.noted))
}

/// Records a read of the top-level statement `statement` of `file_id`. A
/// derivation's own file is not a read, as for [`note_file_read`].
pub fn note_syntax_read(file_id: FileId, statement: TextRange) {
    noting(|evaluation, table, view| {
        if evaluation.id.file_id != file_id {
            let key = HashedKey::new(FactKey::Syntax(InFiled::new(file_id, statement)));
            evaluation.note_key(table, view, key);
        }
    });
}

/// Records a read of `file_id`'s text. A derivation's own file is not a
/// read: its derivations are replaced whenever that text changes.
pub fn note_file_read(file_id: FileId) {
    noting(|evaluation, table, view| {
        if evaluation.id.file_id != file_id {
            evaluation.note_key(table, view, HashedKey::new(FactKey::FileText(file_id)));
        }
    });
}

/// Records that the current evaluation produced `key`.
pub fn note_write(key: impl FnOnce() -> FactKey) {
    RECORDER.with_borrow_mut(|recorder| {
        let Recorder {
            current,
            keys,
            rehoming,
            ..
        } = recorder;
        let Some(evaluation) = current.as_mut() else {
            CAPTURED.with(|captured| {
                if let Some(captured) = captured.borrow_mut().as_mut() {
                    captured.insert(HashedKey::new(key()));
                }
            });
            return;
        };
        let key = HashedKey::new(key());
        let written = next_stamp();
        LAST_WRITE.with(|last| last.set(written));
        // Inference memoised during an evaluation read only keys the
        // evaluation recorded, so a write it did not read leaves it valid.
        let read_here = match keys.as_mut() {
            None => true,
            Some(table) => {
                let slot = table.find(&key);
                if let Some(slot) = slot {
                    table.mark_slot_written(slot, written);
                }
                slot.is_some_and(|slot| evaluation.by_slot.contains_key(&slot))
            }
        };
        if read_here {
            WRITE_GENERATION.with(|generation| generation.set(generation.get() + 1));
        }
        if *rehoming > 0 {
            evaluation.moved.insert(key);
        } else {
            evaluation.outputs.insert(key);
        }
    });
}

/// Whether a write happening now belongs to the source layer: outside any
/// evaluation, or inside a file's [`DerivationKind::Source`] derivation.
/// Indexes keep a source write so a reset can restore it.
pub fn writes_source() -> bool {
    RECORDER.with_borrow(|recorder| {
        recorder
            .current
            .as_ref()
            .is_none_or(|evaluation| evaluation.id.kind == DerivationKind::Source)
    })
}

/// Whether a write of a type `file_id` owns belongs to that file's local
/// fallbacks: the types it guesses for what its statements left untyped or
/// only as a base class, which readers see over what the statements left.
pub fn writes_fallback_of(file_id: FileId) -> bool {
    RECORDER.with_borrow(|recorder| {
        recorder.current.as_ref().is_some_and(|evaluation| {
            evaluation.id.kind == DerivationKind::LocalFallbacks && evaluation.id.file_id == file_id
        })
    })
}

/// Whether the evaluation recording on this thread computes `key` again: it
/// produced it before, and the engine reset it for this run.
pub fn recomputes(key: FactKey) -> bool {
    RECORDER.with_borrow(|recorder| {
        recorder.keys.as_ref().is_some_and(|table| {
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
pub fn dynamic_fields_visible() -> bool {
    current().is_none_or(|id| id.kind != DerivationKind::DynamicFields)
}

/// The derivation being evaluated on this thread, if any.
pub fn current() -> Option<DerivationId> {
    RECORDER.with_borrow(|recorder| recorder.current.as_ref().map(|evaluation| evaluation.id))
}
