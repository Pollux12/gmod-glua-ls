//! Derivations: the re-runnable units that produce every inferred fact.
//!
//! A derivation is identified by the syntax site whose recipe produces it.
//! While one is evaluated, the index accessors report every fact they read
//! (with its fingerprint) and every fact they write here; the engine stores
//! the result in the [`ReaderGraph`] and re-evaluates exactly the readers of
//! facts whose fingerprint changed.

mod diagnostic_reads;
mod frames;
mod graph;
mod positions;

use std::cell::RefCell;

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;

pub use diagnostic_reads::DiagnosticReads;
pub use frames::{
    Gates, OpenFrame, Provenance, ScopeMark, check_tsc, close_frame, consult_absorbed,
    consult_cycle, consult_gates, consult_realm, consult_scope, describe_reads, frame_depth,
    new_stamp, note_written, open_frame, open_frame_absorbing, provenance_keys, report_frames,
    reuse, scope_mark, taint, take_reused_across,
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
    EditMaps, FilePosition, fingerprint_before_edits, fingerprinting, hash_local, in_file,
    mapping_back,
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

    /// Whether the rule holds at `stage`.
    pub fn holds_at(self, stage: AnalysisStage) -> bool {
        stage
            >= match self {
                Self::SkipUntypedWriters => AnalysisStage::SkipUntyped,
                Self::FloorUnsettled => AnalysisStage::Closed,
            }
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
    /// A definition-anchored inferred fact.
    DefinitionFact(LuaDefinitionId),
    /// A `---` type declaration: its object, alias origin included.
    TypeDecl(LuaTypeDeclId),
    /// A type declaration's supertypes.
    TypeSupers(LuaTypeDeclId),
    /// A type declaration's generic parameters.
    TypeGenericParams(LuaTypeDeclId),
    /// A type declaration as the source layer declares it: its source
    /// locations, kind and attribute type.
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
    /// The names of the definitions marked `valid_guard`.
    ValidGuardNames,
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
    /// The `---@realm` annotation covering one offset of a file, read out of
    /// its [`FactKey::GmodInferFile`].
    GmodMemberRealmAt(FileId, TextSize),
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

    pub fn into_key(self) -> FactKey {
        self.key
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
                | K::DefinitionFact(_)
                | K::DeclClaim(_)
                | K::Decl(_)
                | K::Member(_)
                | K::Signature(_)
                | K::ValidGuard(_)
                | K::InferredGuard(_)
                | K::Module(_)
                | K::SpecialCall(..)
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
    /// file's position for a writer that did not run.
    pub fn follows_edits(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::DeclClaim(_)
                | K::MemberOwnerPresence(_)
                | K::MemberKey(_)
                | K::Module(_)
                | K::ModuleMeta(_)
                | K::ModuleName(_)
                | K::ModulePath(_)
                | K::DynamicFieldFile(_)
                | K::TypeCache(_)
                | K::DefinitionFact(_)
                | K::Signature(_)
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
            K::TypeDecl(_) | K::TypeSupers(_) => Some(K::TypeContrib(writer)),
            K::GmodInferScan | K::GmodSystem(..) => Some(K::GmodInferFile(writer.file_id)),
            _ => None,
        }
    }

    /// Whether removing the owning file keeps what other files' derivations
    /// stored under the key: the member, dynamic-field and call-site indexes
    /// file each contribution under its contributing file, and the edit remap
    /// re-homes an owner or signature that moved, or reports its contributors
    /// when it is gone. The call-role fold keeps its entries until it runs
    /// again, which the re-indexed file's helper scan decides.
    pub fn keeps_foreign_contributions(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::MemberSlot(..)
                | K::MemberOwnerMap(_)
                | K::MemberOwnerExprKeys(_)
                | K::MemberOwnerFields(_)
                | K::MemberOwnerPresence(_)
                | K::DynamicField(..)
                | K::DynamicFieldWildcards(_)
                | K::DynamicFieldFinite(_)
                | K::Contrib(..)
                | K::CallRole(_)
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

    /// The key this one is read out of: writing the base moves every key read
    /// out of it, so each reader re-checks its own part and re-runs only when
    /// that part changed.
    pub fn base(&self) -> Option<FactKey> {
        match self {
            FactKey::GmodRealmAt(file_id, _) => Some(FactKey::GmodRealm(*file_id)),
            FactKey::GmodMemberRealmAt(file_id, _) => Some(FactKey::GmodInferFile(*file_id)),
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
            K::GmodRealmAt(file_id, _) | K::GmodMemberRealmAt(file_id, _) => Some(*file_id),
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
            | K::ValidGuard(signature_id)
            | K::InferredGuard(signature_id)
            | K::Contrib(signature_id, _)
            | K::CallRole(CallRoleKey::Signature(signature_id)) => Some(signature_id.get_file_id()),
            K::SpecialCall(file_id, _)
            | K::SpecialCallFile(file_id)
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
            | K::DerivedReferences(file_id)
            | K::FileText(file_id) => Some(*file_id),
            K::DynamicField(DynamicFieldOwner::Table(site), _)
            | K::DynamicFieldWildcards(DynamicFieldOwner::Table(site))
            | K::DynamicFieldFinite(DynamicFieldOwner::Table(site)) => Some(site.file_id),
            K::OperatorOwner(LuaOperatorOwner::Table(site)) => Some(site.file_id),
            K::TypeDecl(_)
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
            | K::ValidGuardNames
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
    /// The open frames, innermost last; see [`frames`].
    frames: Vec<frames::Frame>,
    /// The log as memos outliving the evaluation see it.
    log_handle: std::sync::Arc<frames::LogHandle>,
    /// The stamp the evaluation began under.
    stamp: u64,
    /// Reads made while no key table is lent.
    reads: FxHashSet<HashedKey>,
    /// Cheap identities of keys already in `reads`, see [`note_read_by`].
    read_identities: rustc_hash::FxHashMap<(u8, u128), KeyIdx>,
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
        let mut slots = self.log;
        slots.sort_unstable();
        slots.dedup();
        (
            self.id,
            slots,
            self.reads.into_iter().collect(),
            self.outputs.into_iter().collect(),
        )
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

thread_local! {
    static KEYS: RefCell<Option<KeyTable>> = const { RefCell::new(None) };
    static STAMP: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static LAST_WRITE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CURRENT: RefCell<Option<Evaluation>> = const { RefCell::new(None) };
    /// Evaluations suspended on this thread by one begun inside them.
    static SUSPENDED: RefCell<Vec<Evaluation>> = const { RefCell::new(Vec::new()) };
    static UNTRACKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static REHOMING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static WRITE_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CAPTURED: RefCell<Option<FxHashSet<HashedKey>>> = const { RefCell::new(None) };
    static NOTED_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Runs `f` outside any evaluation and returns every key it wrote: a reset
/// moves facts other derivations produced, whose readers must be re-checked.
pub fn capture_writes(f: impl FnOnce()) -> FxHashSet<HashedKey> {
    debug_assert!(
        CURRENT.with(|current| current.borrow().is_none()),
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
    UNTRACKED.with(|depth| depth.set(depth.get() + 1));
    let result = f();
    UNTRACKED.with(|depth| depth.set(depth.get() - 1));
    result
}

/// Runs `f` with its writes recorded as moves, not outputs. For an index
/// re-filing facts other derivations produced, when the index keeps that
/// placement consistent itself whatever the order of writes: the moved facts'
/// readers are re-checked, but the current derivation does not become one of
/// their producers.
pub fn rehoming<T>(f: impl FnOnce() -> T) -> T {
    REHOMING.with(|depth| depth.set(depth.get() + 1));
    let result = f();
    REHOMING.with(|depth| depth.set(depth.get() - 1));
    result
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

/// Starts recording the evaluation of `id` on this thread.
pub fn begin(id: DerivationId) {
    let stamp = next_stamp();
    start(Evaluation {
        id,
        log: Vec::new(),
        frames: Vec::new(),
        log_handle: std::sync::Arc::new(frames::LogHandle(Default::default(), Some(id))),
        stamp,
        reads: FxHashSet::default(),
        read_identities: rustc_hash::FxHashMap::default(),
        outputs: FxHashSet::default(),
        moved: FxHashSet::default(),
        staged: rustc_hash::FxHashMap::default(),
    });
}

/// Makes `evaluation` the one recording on this thread. One already recording
/// is suspended until `evaluation` ends: a thread waiting in the pool runs
/// other work, which may record its own evaluation.
fn start(evaluation: Evaluation) {
    CURRENT.with(|current| {
        if let Some(previous) = current.borrow_mut().replace(evaluation) {
            SUSPENDED.with(|suspended| suspended.borrow_mut().push(previous));
        }
    });
}

/// Continues recording an evaluation that [`end`] handed back, for a
/// derivation evaluated in stages.
pub fn resume(mut evaluation: Evaluation) {
    // Stamps are per thread: one taken on another thread can name an
    // evaluation this thread already ran.
    evaluation.stamp = next_stamp();
    // Identities noted without this thread's key table name no slot in it.
    evaluation.read_identities.clear();
    start(evaluation);
}

/// Stops recording and hands back what the evaluation read and wrote.
pub fn end() -> Evaluation {
    let evaluation = CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let evaluation = current.take().expect("derivation::end without begin");
        *current = SUSPENDED.with(|suspended| suspended.borrow_mut().pop());
        evaluation
    });
    evaluation.log_handle.freeze(&evaluation.log);
    evaluation
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

fn next_stamp() -> u64 {
    STAMP.with(|stamp| {
        let next = stamp.get() + 1;
        stamp.set(next);
        next
    })
}

/// The stamp of the latest write an evaluation made on this thread.
fn last_write() -> u64 {
    LAST_WRITE.with(std::cell::Cell::get)
}

fn latest_stamp() -> u64 {
    STAMP.with(std::cell::Cell::get)
}

impl Evaluation {
    /// The stamp of the innermost open frame, or of the evaluation.
    fn frame_stamp(&self) -> u64 {
        self.frames.last().map_or(self.stamp, |frame| frame.stamp)
    }

    fn log_slot(&mut self, table: &mut KeyTable, slot: KeyIdx) {
        if table.log(slot, self.frame_stamp(), latest_stamp()) {
            self.log.push(slot);
        }
    }
}

/// Records a read of `key`, interned if a key table is lent. Returns its slot.
fn record_read(evaluation: &mut Evaluation, key: HashedKey) -> Option<KeyIdx> {
    KEYS.with(|keys| match keys.borrow_mut().as_mut() {
        Some(table) => {
            let slot = table.intern_read(key, evaluation.stamp);
            evaluation.log_slot(table, slot);
            Some(slot)
        }
        None => {
            evaluation.reads.insert(key);
            None
        }
    })
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
                Some(slot) => KEYS.with(|keys| {
                    if let Some(table) = keys.borrow_mut().as_mut() {
                        evaluation.log_slot(table, slot);
                    }
                }),
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

/// Records a read of the type declaration `decl_id`. Interned ids compare by
/// address, which the recorded key keeps alive for the evaluation.
pub fn note_type_decl_read(decl_id: &LuaTypeDeclId) {
    note_read_by((u8::MAX, u128::from(decl_id.interned_address())), || {
        FactKey::TypeDecl(decl_id.clone())
    });
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
    note_read_by((u8::MAX - 9, file_position(file_id, offset)), || {
        FactKey::GmodRealmAt(file_id, offset)
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
        LuaTypeOwner::SyntaxId(_) => note_read(|| FactKey::TypeCache(owner.clone())),
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
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut()
            && evaluation.id.file_id != file_id
        {
            record_read(
                evaluation,
                HashedKey::new(FactKey::Syntax(InFiled::new(file_id, statement))),
            );
        }
    });
}

/// Records a read of `file_id`'s text. A derivation's own file is not a
/// read: its derivations are replaced whenever that text changes.
pub fn note_file_read(file_id: FileId) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut()
            && evaluation.id.file_id != file_id
        {
            record_read(evaluation, HashedKey::new(FactKey::FileText(file_id)));
        }
    });
}

/// Records that the current evaluation produced `key`.
pub fn note_write(key: impl FnOnce() -> FactKey) {
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            let key = HashedKey::new(key());
            // Inference memoised during an evaluation read only keys the
            // evaluation recorded, so a write it did not read leaves it valid.
            let read_here = KEYS.with(|keys| {
                keys.borrow()
                    .as_ref()
                    .is_none_or(|table| table.logged_since(&key, evaluation.stamp))
            });
            if read_here {
                WRITE_GENERATION.with(|generation| generation.set(generation.get() + 1));
            }
            let written = next_stamp();
            LAST_WRITE.with(|last| last.set(written));
            KEYS.with(|keys| {
                if let Some(table) = keys.borrow_mut().as_mut() {
                    table.mark_written(&key, written);
                }
            });
            if REHOMING.with(|depth| depth.get() > 0) {
                evaluation.moved.insert(key);
            } else {
                evaluation.outputs.insert(key);
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

/// The derivation being evaluated on this thread, if any.
pub fn current() -> Option<DerivationId> {
    CURRENT.with(|current| current.borrow().as_ref().map(|evaluation| evaluation.id))
}
