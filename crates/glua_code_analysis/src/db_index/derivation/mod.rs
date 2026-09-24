//! Derivations: the re-runnable units that produce every inferred fact.
//!
//! A derivation is identified by the syntax site whose recipe produces it.
//! While one is evaluated, the index accessors report every fact they read
//! (with its fingerprint) and every fact they write here; the engine stores
//! the result in the [`ReaderGraph`] and re-evaluates exactly the readers of
//! facts whose fingerprint changed.

mod diagnostic_reads;
mod graph;
mod positions;

use std::cell::RefCell;

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;

pub use diagnostic_reads::DiagnosticReads;

/// A value an analyzer derivation stores on the index without the index
/// naming its type, fingerprinted on demand.
pub trait DerivedFact: std::any::Any + Send + Sync {
    fn fingerprint(&self) -> Fingerprint;
}

impl std::fmt::Debug for dyn DerivedFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DerivedFact")
    }
}
pub use graph::ReaderGraph;
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

/// One readable fact. Every analysis read of an index lands on exactly one
/// key, and [`crate::DbIndex::fact_fingerprint`] hashes exactly what a read
/// of that key can observe.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FactKey {
    /// A stored type and its fact metadata.
    TypeCache(LuaTypeOwner),
    /// A definition-anchored inferred fact.
    DefinitionFact(LuaDefinitionId),
    /// A `---` type declaration: its object, supertypes, generic parameters
    /// and alias sources.
    TypeDecl(LuaTypeDeclId),
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
    /// One dynamic field (`None`: the wildcard bucket).
    DynamicField(DynamicFieldOwner, Option<SmolStr>),
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
    /// The gmod realm and hook aggregates across files.
    GmodInferScan,
    /// A file's load edges, roots and execution environment.
    GmodLoadFile(FileId),
    /// A file's network flows.
    NetworkFile(FileId),
    /// Network flows by message name (whole-map).
    NetworkScan,
    /// The workspace net-helper registry.
    HelperRegistry,
    /// One file's gmod helper scan: its call roles and net-helper definitions.
    HelperScan(FileId),
    /// The set of files that have a gmod helper scan.
    HelperScanFiles,
    /// The names of functions that can produce a net operation.
    NetHelperNames,
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
    /// The set of files in the workspace.
    FileSet,
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
                | K::NetworkFile(_)
                | K::HelperRegistry
                | K::HelperScan(_)
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
        )
    }

    /// The file whose slot the key names, or `None` for a workspace-wide key.
    /// Removing that file's facts removes whatever any derivation stored under
    /// the key.
    /// Whether the edit remap (`remap_into_file`) rewrites the positions of an
    /// edited file that this key's value names.
    pub fn follows_edits(&self) -> bool {
        use FactKey as K;
        matches!(
            self,
            K::TypeCache(_)
                | K::DefinitionFact(_)
                | K::Signature(_)
                | K::DynamicField(..)
                | K::Metatable(_)
                | K::SpecialCall(..)
                | K::Member(_)
                | K::MemberSlot(..)
                | K::MemberOwnerMap(_)
        )
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
            K::MemberSlot(owner, _) | K::MemberOwnerMap(owner) => match owner {
                LuaMemberOwner::Element(site) => Some(site.file_id),
                _ => None,
            },
            K::DefinitionSite(site) | K::Metatable(site) | K::TableClass(site) => {
                Some(site.file_id)
            }
            K::Signature(signature_id)
            | K::ValidGuard(signature_id)
            | K::InferredGuard(signature_id)
            | K::Contrib(signature_id, _) => Some(signature_id.get_file_id()),
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
            | K::NetworkFile(file_id)
            | K::HelperScan(file_id)
            | K::References(file_id)
            | K::AccessorCalls(file_id)
            | K::Dependency(file_id)
            | K::LoadSites(file_id)
            | K::DiagnosticFile(file_id)
            | K::CallSiteFile(file_id)
            | K::DerivedReferences(file_id)
            | K::FileText(file_id) => Some(*file_id),
            K::DynamicField(DynamicFieldOwner::Table(site), _) => Some(site.file_id),
            K::OperatorOwner(LuaOperatorOwner::Table(site)) => Some(site.file_id),
            K::TypeDecl(_)
            | K::SourceTypeDecl(_)
            | K::SubTypes(_)
            | K::TypeDeclScan
            | K::MemberKey(_)
            | K::GlobalPath(_)
            | K::ReceiverOutParam(_)
            | K::ModuleName(_)
            | K::ModuleNamespace(_)
            | K::ModuleFiles
            | K::DynamicField(..)
            | K::UnattributedField(_)
            | K::OperatorOwner(_)
            | K::GmodMeta(_)
            | K::VguiParentCallFiles
            | K::VguiChain(_)
            | K::GmodInferScan
            | K::NetworkScan
            | K::HelperRegistry
            | K::HelperScanFiles
            | K::FileSet
            | K::ValidGuardFiles
            | K::ValidGuardNames
            | K::NetHelperNames
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
    reads: FxHashSet<FactKey>,
    outputs: FxHashSet<FactKey>,
    /// Fingerprints taken when an earlier stage of a staged evaluation ended:
    /// other evaluations write between its stages.
    staged: rustc_hash::FxHashMap<FactKey, Fingerprint>,
}

impl Evaluation {
    /// The keys the evaluation wrote.
    pub fn outputs(&self) -> impl Iterator<Item = &FactKey> {
        self.outputs.iter()
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
        fingerprint: impl Fn(&FactKey) -> Fingerprint,
    ) -> (DerivationId, Vec<(FactKey, Fingerprint)>, Vec<FactKey>) {
        let outputs = self.outputs;
        let file_id = self.id.file_id;
        let reads = self
            .reads
            .into_iter()
            .filter(|key| {
                let own = key.owning_file() == Some(file_id);
                // A source layer runs before any of its file's derivations,
                // so its own file's facts are source values it is part of.
                !(key.is_primary() && outputs.contains(key))
                    && !(own && (key.is_file_source() || self.id.kind == DerivationKind::Source))
            })
            .map(|key| {
                let seen = match self.staged.get(&key) {
                    Some(seen) if !outputs.contains(&key) => *seen,
                    _ => fingerprint(&key),
                };
                (key, seen)
            })
            .collect();
        (self.id, reads, outputs.into_iter().collect())
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Evaluation>> = const { RefCell::new(None) };
    static UNTRACKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static WRITE_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CAPTURED: RefCell<Option<FxHashSet<FactKey>>> = const { RefCell::new(None) };
}

/// Runs `f` outside any evaluation and returns every key it wrote: a reset
/// moves facts other derivations produced, whose readers must be re-checked.
pub fn capture_writes(f: impl FnOnce()) -> FxHashSet<FactKey> {
    debug_assert!(!recording(), "writes captured inside an evaluation");
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

/// Starts recording the evaluation of `id` on this thread.
pub fn begin(id: DerivationId) {
    CURRENT.with(|current| {
        let previous = current.borrow_mut().replace(Evaluation {
            id,
            reads: FxHashSet::default(),
            outputs: FxHashSet::default(),
            staged: rustc_hash::FxHashMap::default(),
        });
        debug_assert!(
            previous.is_none(),
            "derivation {id:?} began inside {previous:?}"
        );
    });
}

/// Continues recording an evaluation that [`end`] handed back, for a
/// derivation evaluated in stages.
pub fn resume(evaluation: Evaluation) {
    CURRENT.with(|current| {
        let previous = current.borrow_mut().replace(evaluation);
        debug_assert!(previous.is_none(), "resumed inside {previous:?}");
    });
}

/// Stops recording and hands back what the evaluation read and wrote.
pub fn end() -> Evaluation {
    CURRENT
        .with(|current| current.borrow_mut().take())
        .expect("derivation::end without begin")
}

/// Whether an evaluation is recording on this thread. Callers building a
/// costly key check this first.
#[inline]
pub fn recording() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

/// Records a read of `key` by the current evaluation.
pub fn note_read(key: impl FnOnce() -> FactKey) {
    if UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            evaluation.reads.insert(key());
        }
    });
}

/// Records a read of `file_id`'s text. A derivation's own file is not a
/// read: its derivations are replaced whenever that text changes.
pub fn note_file_read(file_id: FileId) {
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut()
            && evaluation.id.file_id != file_id
        {
            evaluation.reads.insert(FactKey::FileText(file_id));
        }
    });
}

/// Records that the current evaluation produced `key`.
pub fn note_write(key: impl FnOnce() -> FactKey) {
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            WRITE_GENERATION.with(|generation| generation.set(generation.get() + 1));
            evaluation.outputs.insert(key());
        } else {
            CAPTURED.with(|captured| {
                if let Some(captured) = captured.borrow_mut().as_mut() {
                    captured.insert(key());
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
