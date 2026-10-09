mod accessor_func;
mod call_site_param;
mod call_target;
mod cow_shards;
mod declaration;
mod dependency;
pub(crate) mod derivation;
mod diagnostic;
mod dynamic_field;
mod edit;
mod flow;
mod global;
mod gmod_class;
mod gmod_infer;
mod gmod_load;
mod gmod_network;
mod intern;
mod member;
mod metatable;
mod module;
mod net_site;
mod numeric_range_population;
mod operators;
mod property;
mod query;
mod reference;
mod schema;
mod semantic_decl;
mod signature;
mod traits;
mod r#type;
pub(crate) use intern::intern_smol_str;
pub(crate) use query::QueryFamily;
#[cfg(test)]
pub(crate) use query::QueryStats;

use rustc_hash::{FxHashMap, FxHashSet};
use std::{path::PathBuf, sync::Arc};

use crate::{Emmyrc, FileId, Vfs, profile::Profile};
pub use accessor_func::*;
pub use call_site_param::{CallSiteParamContribution, CallSiteParamIndex};
pub use call_target::{LuaCallTarget, LuaCallTargetIndex};
use cow_shards::{CowCell, CowShards};
pub use declaration::*;
pub use dependency::{
    LoadPathEdit, LuaDependencyIndex, LuaDependencyKind, LuaDependencySite, load_path_rename_edits,
};
pub use diagnostic::{AnalyzeError, DiagnosticAction, DiagnosticActionKind, DiagnosticIndex};
pub use dynamic_field::{
    DynamicFieldIndex, DynamicFieldOwner, canonical_dynamic_field_owner, dynamic_field_owner_of,
    is_pure_wildcard_registry,
};
pub use edit::*;
pub use flow::*;
pub use global::{GlobalId, LuaGlobalIndex};
pub use gmod_class::*;
pub use gmod_infer::*;
pub use gmod_load::*;
pub use gmod_network::*;
pub use member::*;
pub use metatable::{LuaMetatableIndex, SetmetatableFactoryBinding};
pub use module::*;
pub use numeric_range_population::*;
pub use operators::*;
pub use property::*;
pub use reference::*;
pub use schema::*;
pub use semantic_decl::*;
pub use signature::*;
pub use traits::LuaIndex;
pub use r#type::*;

/// Domain indexes live behind `Arc` so [`DbIndex::fork`] can hand a worker a
/// cheap handle for the parallel lua-analyze walk: cloning a fork bumps ref
/// counts, and the first mutable access to a domain clones just that domain
/// (`Arc::make_mut`), so an unwritten domain is shared with the base index for
/// free. [`DbIndex::fork_independent`] uses the same sharing for a long-lived
/// copy.
#[derive(Debug)]
pub struct DbIndex {
    decl_index: Arc<LuaDeclIndex>,
    references_index: Arc<LuaReferenceIndex>,
    types_index: Arc<LuaTypeIndex>,
    modules_index: Arc<LuaModuleIndex>,
    members_index: Arc<LuaMemberIndex>,
    property_index: Arc<LuaPropertyIndex>,
    signature_index: Arc<LuaSignatureIndex>,
    diagnostic_index: Arc<DiagnosticIndex>,
    operator_index: Arc<LuaOperatorIndex>,
    flow_index: Arc<LuaFlowIndex>,
    call_target_index: Arc<LuaCallTargetIndex>,
    accessor_func_index: Arc<AccessorFuncAnnotationIndex>,
    accessor_func_call_index: Arc<AccessorFuncCallIndex>,
    call_site_param_index: Arc<CallSiteParamIndex>,
    gmod_class_index: Arc<GmodClassMetadataIndex>,
    gmod_infer_index: Arc<GmodInferIndex>,
    gmod_load_index: Arc<GmodLoadIndex>,
    gmod_network_index: Arc<GmodNetworkIndex>,
    dynamic_field_index: Arc<DynamicFieldIndex>,
    vfs: Arc<Vfs>,
    file_dependencies_index: Arc<LuaDependencyIndex>,
    numeric_range_population_index: Arc<NumericRangePopulationIndex>,
    metatable_index: Arc<LuaMetatableIndex>,
    global_index: Arc<LuaGlobalIndex>,
    json_schema_index: Arc<JsonSchemaIndex>,
    /// Who read and produced which fact; see [`derivation`].
    derivations: Arc<derivation::ReaderGraph>,
    /// The workspace gmod call roles with their fingerprint when stored.
    gmod_call_roles: Option<(Arc<dyn derivation::CallRoles>, derivation::Fingerprint)>,
    /// Each call-role entry's fingerprint when the call roles were stored.
    call_role_fingerprints: CowCell<FxHashMap<derivation::CallRoleKey, derivation::Fingerprint>>,
    /// The global paths each file's syntax names.
    role_mentions: CowShards<FileId, Arc<[smol_str::SmolStr]>>,
    emmyrc: Arc<Emmyrc>,
    /// Each file's gmod helper scan with its fingerprint when stored.
    helper_scans: CowShards<FileId, (Arc<dyn derivation::DerivedFact>, derivation::Fingerprint)>,
    /// The set of files in `helper_scans`.
    helper_scan_files: derivation::SetFingerprint,
    /// Each file's own contribution to the gmod load graph.
    load_scans: CowShards<FileId, (Arc<dyn derivation::DerivedFact>, derivation::Fingerprint)>,
    /// The set of files in `load_scans`.
    load_scan_files: derivation::SetFingerprint,
    /// Each file's net call facts and function-body summaries.
    net_sites: CowShards<FileId, Arc<net_site::FileNetSites>>,
    /// Each file with the files whose net sites name its positions.
    net_helper_refs: CowShards<FileId, FxHashSet<FileId>>,
    /// Each file's function-body summaries with their fingerprint when stored.
    function_summaries:
        CowShards<FileId, (Arc<dyn derivation::DerivedFact>, derivation::Fingerprint)>,
    /// Bumped on every *mutable* handle to the type, member, signature or module
    /// index; memos over facts derived from those key on it. It covers those four
    /// and no others — the decl, global, dynamic-field and gmod-infer indexes all
    /// mutate without bumping it, so a memo reading those is not protected here
    /// and has to say how it stays correct. Widening a memo's read set means
    /// widening this too. May over-invalidate, never misses.
    /// Values come from a process-global counter so they are unique across
    /// instances (the memos are thread-local and outlive any one `DbIndex`).
    type_structure_revision: u64,
    /// The stage the engine run is at; [`derivation::AnalysisStage::Closed`]
    /// outside a run.
    analysis_stage: derivation::AnalysisStage,
    /// The memos of the facts computed when first read; see [`query`]. A
    /// fork shares them: a task reads them as a worker does, never writing.
    queries: Arc<query::Queries>,
}

/// See [`DbIndex::type_structure_revision`] — process-global so revision values
/// are unique across instances.
fn next_type_structure_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A stored derived value's fingerprint: the one taken when it was stored,
/// or taken afresh when fingerprinting with edits mapped back.
fn stored_fact_fingerprint(
    (fact, fingerprint): &(Arc<dyn derivation::DerivedFact>, derivation::Fingerprint),
) -> derivation::Fingerprint {
    if derivation::mapping_back() {
        fact.fingerprint().max(1)
    } else {
        *fingerprint
    }
}

fn downcast_fact<T: derivation::DerivedFact>(
    fact: &Arc<dyn derivation::DerivedFact>,
) -> Option<Arc<T>> {
    let fact: Arc<dyn std::any::Any + Send + Sync> = fact.clone();
    fact.downcast::<T>().ok()
}

#[allow(unused)]
impl Default for DbIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl DbIndex {
    /// Drops the index part by part, calling `dropped` with each part's name
    /// once it is gone, for a census of what each part holds.
    pub fn drop_by_part(&mut self, mut dropped: impl FnMut(&'static str)) {
        let Self {
            decl_index,
            references_index,
            types_index,
            modules_index,
            members_index,
            property_index,
            signature_index,
            diagnostic_index,
            operator_index,
            flow_index,
            call_target_index,
            accessor_func_index,
            accessor_func_call_index,
            call_site_param_index,
            gmod_class_index,
            gmod_infer_index,
            gmod_load_index,
            gmod_network_index,
            dynamic_field_index,
            vfs,
            file_dependencies_index,
            numeric_range_population_index,
            metatable_index,
            global_index,
            json_schema_index,
            derivations,
            gmod_call_roles,
            call_role_fingerprints,
            role_mentions,
            emmyrc,
            helper_scans,
            load_scans,
            net_sites,
            net_helper_refs,
            function_summaries,
            queries,
            ..
        } = std::mem::take(self);
        macro_rules! drop_parts {
            ($($part:ident),*) => {
                $(
                    drop($part);
                    dropped(stringify!($part));
                )*
            };
        }
        drop_parts!(
            decl_index,
            references_index,
            types_index,
            modules_index,
            members_index,
            property_index,
            signature_index,
            diagnostic_index,
            operator_index,
            flow_index,
            call_target_index,
            accessor_func_index,
            accessor_func_call_index,
            call_site_param_index,
            gmod_class_index,
            gmod_infer_index,
            gmod_load_index,
            gmod_network_index,
            dynamic_field_index,
            vfs,
            file_dependencies_index,
            numeric_range_population_index,
            metatable_index,
            global_index,
            json_schema_index,
            derivations,
            gmod_call_roles,
            call_role_fingerprints,
            role_mentions,
            emmyrc,
            helper_scans,
            load_scans,
            net_sites,
            net_helper_refs,
            function_summaries,
            queries
        );
    }

    pub fn new() -> Self {
        Self {
            type_structure_revision: next_type_structure_revision(),
            decl_index: Arc::new(LuaDeclIndex::new()),
            references_index: Arc::new(LuaReferenceIndex::new()),
            types_index: Arc::new(LuaTypeIndex::new()),
            modules_index: Arc::new(LuaModuleIndex::new()),
            members_index: Arc::new(LuaMemberIndex::new()),
            property_index: Arc::new(LuaPropertyIndex::new()),
            signature_index: Arc::new(LuaSignatureIndex::new()),
            diagnostic_index: Arc::new(DiagnosticIndex::new()),
            operator_index: Arc::new(LuaOperatorIndex::new()),
            flow_index: Arc::new(LuaFlowIndex::new()),
            call_target_index: Arc::new(LuaCallTargetIndex::new()),
            accessor_func_index: Arc::new(AccessorFuncAnnotationIndex::new()),
            accessor_func_call_index: Arc::new(AccessorFuncCallIndex::new()),
            call_site_param_index: Arc::new(CallSiteParamIndex::new()),
            gmod_class_index: Arc::new(GmodClassMetadataIndex::new()),
            gmod_infer_index: Arc::new(GmodInferIndex::new()),
            gmod_load_index: Arc::new(GmodLoadIndex::new()),
            gmod_network_index: Arc::new(GmodNetworkIndex::new()),
            dynamic_field_index: Arc::new(DynamicFieldIndex::new()),
            vfs: Arc::new(Vfs::new()),
            file_dependencies_index: Arc::new(LuaDependencyIndex::new()),
            numeric_range_population_index: Arc::new(NumericRangePopulationIndex::new()),
            metatable_index: Arc::new(LuaMetatableIndex::new()),
            global_index: Arc::new(LuaGlobalIndex::new()),
            json_schema_index: Arc::new(JsonSchemaIndex::new()),
            derivations: Arc::default(),
            gmod_call_roles: None,
            call_role_fingerprints: Default::default(),
            role_mentions: Default::default(),
            emmyrc: Arc::new(Emmyrc::default()),
            helper_scans: Default::default(),
            helper_scan_files: derivation::SetFingerprint::default(),
            load_scans: Default::default(),
            load_scan_files: derivation::SetFingerprint::default(),
            net_sites: Default::default(),
            net_helper_refs: Default::default(),
            function_summaries: Default::default(),
            analysis_stage: derivation::AnalysisStage::default(),
            queries: Arc::default(),
        }
    }

    pub(crate) fn reader_graph(&self) -> &derivation::ReaderGraph {
        &self.derivations
    }

    /// The reader graph, shared, for evaluations that view it; see
    /// [`derivation::view_graph`].
    pub(crate) fn reader_graph_shared(&self) -> Arc<derivation::ReaderGraph> {
        Arc::clone(&self.derivations)
    }

    /// Lets go of this fork's share of the reader graph, which its index
    /// then writes without copying it.
    pub(crate) fn release_reader_graph(&mut self) {
        self.derivations = Arc::default();
    }

    pub(crate) fn reader_graph_mut(&mut self) -> &mut derivation::ReaderGraph {
        Arc::make_mut(&mut self.derivations)
    }

    /// Restores a primary fact to its source-layer value, dropping what every
    /// derivation stored under it. The engine re-runs all of the key's
    /// producers afterwards, so a multi-writer key is rebuilt in canonical
    /// order.
    pub(crate) fn reset_fact(&mut self, key: &derivation::FactKey) {
        use derivation::FactKey as K;
        match key {
            K::TypeCache(owner) => self.get_type_index_mut().reset_type_cache(owner),
            K::TypeFallbacks(file_id) => self.get_type_index_mut().retract_fallbacks(*file_id),
            K::HelperScan(file_id) => self.remove_helper_scan(*file_id),
            K::RoleMentions(file_id) => self.remove_role_mentions(*file_id),
            K::LoadScan(file_id) => self.remove_load_scan(*file_id),
            K::NetCall(site) => self.reset_net_site(net_site::NetSiteKind::Call, site),
            K::NetHelper(site) => self.reset_net_site(net_site::NetSiteKind::Helper, site),
            K::NetFlows(site) => self
                .get_gmod_network_index_mut()
                .reset_call_flows(site.file_id, site.value),
            K::DeclClaim(decl_id) => self.get_type_index_mut().reset_decl_claim(decl_id),
            K::DefinitionFact(definition) => {
                self.get_type_index_mut().reset_definition_fact(definition)
            }
            K::Signature(signature_id) => {
                self.get_signature_index_mut().reset_signature(signature_id)
            }
            K::ValidGuardFile(file_id) => self
                .get_signature_index_mut()
                .remove_valid_guard_file_contributions(*file_id),
            K::InferredGuard(signature_id) => self
                .get_signature_index_mut()
                .reset_inferred_guard(signature_id),
            K::Member(member_id) => self.get_member_index_mut().reset_member(*member_id),
            K::MemberTransfer(member_id) => self.get_member_index_mut().reset_transfer(*member_id),
            K::TypeContrib(id) => self.get_type_index_mut().retract_contributions(id),
            K::PathClassContrib(id) => self.get_member_index_mut().retract_path_classes(id),
            K::OperatorContrib(id) => self.get_operator_index_mut().retract_contributions(id),
            K::ReferenceContrib(id) => self.get_reference_index_mut().retract_index_references(id),
            K::DeclContrib(id) => {
                for decl in self.get_decl_index_mut().retract_derived_decls(id) {
                    if decl.is_global() {
                        self.get_global_index_mut()
                            .remove_global_decl(decl.get_name(), decl.get_id());
                    }
                }
            }
            K::SpecialCall(file_id, position) => self
                .get_flow_index_mut()
                .reset_special_call(*file_id, *position),
            K::CallTarget(site) => self
                .get_call_target_index_mut()
                .reset_target(site.file_id, site.value),
            K::DynamicFieldSite(site) => self.get_dynamic_field_index_mut().retract_site(site),
            K::Module(file_id) => self.get_module_index_mut().reset_export(*file_id),
            K::Metatable(table) => self.get_metatable_index_mut().reset_table(table),
            K::MetatableFactory(file_id) => self
                .get_metatable_index_mut()
                .reset_factory_bindings(*file_id),
            K::LoadSites(file_id) => self
                .get_file_dependencies_index_mut()
                .reset_derived_sites(*file_id),
            K::DeclDefault(decl_id) => self
                .get_property_index_mut()
                .reset_inferred_string_defaults(decl_id),
            K::AccessorCalls(file_id) => {
                self.get_accessor_func_call_index_mut().reset_file(*file_id)
            }
            K::GmodInferFile(file_id) => self
                .get_gmod_infer_index_mut()
                .reset_file_metadata(*file_id),
            K::GmodClassFile(file_id) => self
                .get_gmod_class_metadata_index_mut()
                .remove_file_metadata(&[*file_id]),
            K::VguiRelations(file_id) => self
                .get_gmod_class_metadata_index_mut()
                .remove_vgui_file_relations(*file_id),
            K::CallSiteParam(site) => self
                .get_call_site_param_index_mut()
                .retract_call(site.file_id, site.value),
            K::CallSiteCallbacks(file_id) => self
                .get_call_site_param_index_mut()
                .retract_callbacks(*file_id),
            _ => {}
        }
    }

    /// Every call-role entry with its fingerprint, sorted: for comparing two
    /// analyses of the same workspace.
    pub fn call_role_digest(&self) -> Vec<String> {
        let mut entries = self
            .call_role_fingerprints
            .iter()
            .map(|(key, fingerprint)| format!("{key:?} = {fingerprint}"))
            .collect::<Vec<_>>();
        entries.sort_unstable();
        entries
    }

    /// Every stored per-file fact by kind, with its fingerprint, sorted: for
    /// comparing two analyses of the same workspace.
    pub fn stored_fact_fingerprints(&self) -> Vec<(&'static str, FileId, u64)> {
        let tables = [
            ("helper_scan", &self.helper_scans),
            ("load_scan", &self.load_scans),
            ("function_summaries", &self.function_summaries),
        ];
        let mut facts = tables
            .into_iter()
            .flat_map(|(kind, table)| {
                table
                    .iter()
                    .map(move |(file_id, (_, fingerprint))| (kind, *file_id, *fingerprint))
            })
            .chain(self.role_mentions.iter().map(|(file_id, mentions)| {
                (
                    "role_mentions",
                    *file_id,
                    derivation::fingerprint_of(mentions),
                )
            }))
            .chain(
                self.call_target_index
                    .file_fingerprints()
                    .map(|(file_id, fingerprint)| ("call_targets", file_id, fingerprint)),
            )
            .chain(
                self.net_sites
                    .iter()
                    .map(|(file_id, sites)| ("net_sites", *file_id, sites.fingerprint(*file_id))),
            )
            .collect::<Vec<_>>();
        facts.sort_unstable();
        facts
    }

    /// The call roles for a per-file pass, which reads the entries it looks up
    /// one by one.
    pub(crate) fn gmod_call_role_entries<T: derivation::CallRoles>(&self) -> Option<Arc<T>> {
        let (roles, _) = self.gmod_call_roles.as_ref()?;
        let roles: Arc<dyn std::any::Any + Send + Sync> = roles.clone();
        roles.downcast::<T>().ok()
    }

    /// The call roles taken out of the index, for the fold to update in place
    /// and store again with [`Self::update_gmod_call_roles`].
    pub(crate) fn take_gmod_call_roles<T: derivation::CallRoles>(&mut self) -> Option<Arc<T>> {
        let (roles, _) = self.gmod_call_roles.take()?;
        let roles: Arc<dyn std::any::Any + Send + Sync> = roles;
        roles.downcast::<T>().ok()
    }

    /// Stores the call roles, writing each of the `touched` entries that moved.
    pub(crate) fn update_gmod_call_roles<T: derivation::CallRoles>(
        &mut self,
        roles: Arc<T>,
        touched: &[derivation::CallRoleKey],
    ) {
        derivation::note_write(|| derivation::FactKey::HelperRegistry);
        for key in touched {
            let fingerprint = roles.entry_fingerprint(key);
            let stored = self
                .call_role_fingerprints
                .get(key)
                .copied()
                .unwrap_or(derivation::ABSENT);
            if fingerprint == stored {
                continue;
            }
            derivation::note_write(|| derivation::FactKey::CallRole(key.clone()));
            if fingerprint == derivation::ABSENT {
                self.call_role_fingerprints.remove(key);
            } else {
                self.call_role_fingerprints.insert(key.clone(), fingerprint);
            }
        }
        let fingerprint = roles.fingerprint().max(1);
        self.gmod_call_roles = Some((roles, fingerprint));
    }

    /// The global paths `file_id`'s syntax names.
    pub(crate) fn role_mentions(&self, file_id: FileId) -> Arc<[smol_str::SmolStr]> {
        derivation::note_read(|| derivation::FactKey::RoleMentions(file_id));
        self.role_mentions
            .get(&file_id)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn set_role_mentions(
        &mut self,
        file_id: FileId,
        mentions: Arc<[smol_str::SmolStr]>,
    ) {
        derivation::note_write(|| derivation::FactKey::RoleMentions(file_id));
        self.role_mentions.insert(file_id, mentions);
    }

    fn remove_role_mentions(&mut self, file_id: FileId) {
        if self.role_mentions.remove(&file_id).is_some() {
            derivation::note_write(|| derivation::FactKey::RoleMentions(file_id));
        }
    }

    /// The fingerprint of what a read of `key` observes now.
    pub(crate) fn fact_fingerprint(&self, key: &derivation::FactKey) -> derivation::Fingerprint {
        derivation::in_file(key.positional_file(), || self.fact_fingerprint_in_file(key))
    }

    fn fact_fingerprint_in_file(&self, key: &derivation::FactKey) -> derivation::Fingerprint {
        use derivation::FactKey as K;
        match key {
            K::TypeCache(_)
            | K::TypeFallbacks(_)
            | K::DefinitionFact(_)
            | K::TypeDecl(_)
            | K::TypeDeclLocations(_)
            | K::TypeDeclFiles(_)
            | K::TypeSupers(_)
            | K::TypeSuperTypes(_)
            | K::TypeSuperEdges(_)
            | K::TypeGenericParams(_)
            | K::SourceTypeDecl(_)
            | K::TypeContrib(_)
            | K::SubTypes(_)
            | K::BoundDecls(..)
            | K::DeclClaim(_)
            | K::TypeDeclScan
            | K::TypeFile(_)
            | K::TableClass(_) => self.types_index.fact_fingerprint(key),
            K::TypeSuperRealms(decl_id, file_id) => {
                let realms = derivation::untracked(|| {
                    self.types_index
                        .super_edges_in_file(decl_id, *file_id)
                        .map(|entry| {
                            self.gmod_infer_index
                                .get_realm_at_offset(file_id, entry.value.source_range.start())
                        })
                        .collect::<Vec<_>>()
                });
                if realms.is_empty() {
                    derivation::ABSENT
                } else {
                    derivation::fingerprint_of(&realms)
                }
            }
            K::Decl(_) | K::DeclTree(_) | K::DeclContrib(_) => {
                self.decl_index.fact_fingerprint(key)
            }
            K::Member(_)
            | K::MemberObject(_)
            | K::MemberHome(_)
            | K::MemberTransfer(_)
            | K::MemberMarks(_)
            | K::MemberSlot(..)
            | K::MemberOwnerMap(_)
            | K::MemberOwnerExprKeys(_)
            | K::MemberOwnerFields(_)
            | K::MemberOwnerPresence(_)
            | K::MemberKey(_)
            | K::MemberFile(_)
            | K::FunctionScope(_)
            | K::GlobalPath(_)
            | K::PathClassContrib(_)
            | K::DefinitionSite(_) => self.members_index.fact_fingerprint(key),
            K::Signature(_)
            | K::SignatureDecl(_)
            | K::SignatureReturnsNever(_)
            | K::SpecialCallSignature(_)
            | K::ValidGuard(_)
            | K::ValidGuardFile(_)
            | K::ValidGuardFiles
            | K::ValidGuardName(_)
            | K::InferredGuard(_)
            | K::SignatureFile(_)
            | K::ReceiverOutParam(_) => self.signature_index.fact_fingerprint(key),
            K::Contrib(..) | K::CallSiteFile(_) | K::CallSiteParam(_) | K::CallSiteCallbacks(_) => {
                self.call_site_param_index.fact_fingerprint(key)
            }
            K::Module(_)
            | K::ModuleMeta(_)
            | K::ModuleName(_)
            | K::ModulePath(_)
            | K::ModuleNamespace(_)
            | K::ModuleFiles => self.modules_index.fact_fingerprint(key),
            K::SpecialCall(..) | K::SpecialCallFile(_) | K::SpecialCallFor(..) | K::FlowTree(_) => {
                self.flow_index.fact_fingerprint(key)
            }
            K::CallTarget(_) => self.call_target_index.fact_fingerprint(key),
            K::DynamicField(..)
            | K::DynamicFieldWildcards(_)
            | K::DynamicFieldFinite(_)
            | K::DynamicFieldName(_)
            | K::UnattributedField(_)
            | K::DynamicFieldSite(_) => self.dynamic_field_index.fact_fingerprint(key),
            K::OperatorOwner(_) | K::OperatorFile(_) | K::OperatorContrib(_) => {
                self.operator_index.fact_fingerprint(key)
            }
            K::Metatable(_) | K::MetatableFactory(_) => self.metatable_index.fact_fingerprint(key),
            K::GmodMeta(_)
            | K::GmodClassFile(_)
            | K::GmodVguiRegistrations(_)
            | K::VguiChain(_)
            | K::VguiParentCallFiles
            | K::VguiRelations(_) => self.gmod_class_index.fact_fingerprint(key),
            K::GmodInferFile(_)
            | K::GmodRealm(_)
            | K::GmodRealmAt(..)
            | K::GmodMemberRealmAt(..)
            | K::GmodRealmInputs(_)
            | K::GmodInferScan
            | K::GmodSystem(..) => self.gmod_infer_index.fact_fingerprint(key),
            K::GmodLoadFile(_) => self.gmod_load_index.fact_fingerprint(key),
            K::NetFlows(_)
            | K::NetworkFile(_)
            | K::NetworkScan
            | K::NetSends(_)
            | K::NetReceives(_)
            | K::NetDefinitionUses(_)
            | K::NetHelperName(_)
            | K::NetOperationNames => self.gmod_network_index.fact_fingerprint(key),
            K::HelperRegistry => {
                self.gmod_call_roles
                    .as_ref()
                    .map_or(derivation::ABSENT, |(roles, fingerprint)| {
                        if derivation::mapping_back() {
                            roles.fingerprint().max(1)
                        } else {
                            *fingerprint
                        }
                    })
            }
            K::CallRole(entry) => match &self.gmod_call_roles {
                Some((roles, _)) if derivation::mapping_back() => roles.entry_fingerprint(entry),
                _ => self
                    .call_role_fingerprints
                    .get(entry)
                    .copied()
                    .unwrap_or(derivation::ABSENT),
            },
            K::RoleMentions(file_id) => {
                derivation::fingerprint_opt(self.role_mentions.get(file_id))
            }
            K::HelperScan(file_id) => self
                .helper_scans
                .get(file_id)
                .map_or(derivation::ABSENT, stored_fact_fingerprint),
            K::HelperScanFiles => self.helper_scan_files.get(),
            K::LoadScan(file_id) => self
                .load_scans
                .get(file_id)
                .map_or(derivation::ABSENT, stored_fact_fingerprint),
            K::LoadScanFiles => self.load_scan_files.get(),
            K::NetCall(site) => self.net_site_fingerprint(net_site::NetSiteKind::Call, site),
            K::NetHelper(site) => self.net_site_fingerprint(net_site::NetSiteKind::Helper, site),
            K::FunctionSummaries(file_id) => self
                .function_summaries
                .get(file_id)
                .map_or(derivation::ABSENT, stored_fact_fingerprint),
            K::Property(_) | K::DeclDefault(_) => self.property_index.fact_fingerprint(key),
            K::Global(_) | K::GlobalScan => self.global_index.fact_fingerprint(key),
            K::References(_)
            | K::DeclReferences(_)
            | K::ReferencesByName(_)
            | K::IndexReferenceFiles(_)
            | K::DerivedReferences(_)
            | K::ReferenceContrib(_) => self.references_index.fact_fingerprint(key),
            K::NumericPopulation(_) => self.numeric_range_population_index.fact_fingerprint(key),
            K::AccessorFunc(_) => self.accessor_func_index.fact_fingerprint(key),
            K::AccessorCalls(_) => self.accessor_func_call_index.fact_fingerprint(key),
            K::Dependency(_) | K::LoadSites(_) => {
                self.file_dependencies_index.fact_fingerprint(key)
            }
            K::DiagnosticFile(_) => self.diagnostic_index.fact_fingerprint(key),
            K::FileText(file_id) => self.vfs.text_fingerprint(file_id),
            K::FilePath(file_id) => self.vfs.path_fingerprint(file_id),
            K::PathFile(path) => self.vfs.occupant_fingerprint(path),
            K::Syntax(site) => self.vfs.statement_fingerprint(site),
            K::FileSet => self.vfs.file_set_fingerprint(),
            K::StageRule(rule) => derivation::fingerprint_of(&rule.holds_at(self.analysis_stage)),
            K::ReceiverAuthority(_) | K::CallbackParams(_) => self.queries.fingerprint(self, key),
        }
    }

    pub(crate) fn queries(&self) -> &query::Queries {
        &self.queries
    }

    /// Whether the method `signature_id` declares has an authoritative
    /// receiver.
    pub(crate) fn receiver_authority(&self, signature_id: LuaSignatureId) -> bool {
        *self.queries.receiver_authority.get(self, signature_id)
    }

    /// What the function `signature_id` passes to each callback parameter it
    /// calls.
    pub(crate) fn callback_params(
        &self,
        signature_id: LuaSignatureId,
    ) -> Arc<Vec<crate::compilation::CallbackArg>> {
        self.queries.callback_params.get(self, signature_id)
    }

    pub(crate) fn analysis_stage(&self) -> derivation::AnalysisStage {
        self.analysis_stage
    }

    /// Moves the analysis to `stage`, forgetting the cached fingerprints of the
    /// stage rules whose value that moves.
    pub(crate) fn set_analysis_stage(&mut self, stage: derivation::AnalysisStage) {
        let before = std::mem::replace(&mut self.analysis_stage, stage);
        for rule in derivation::StageRule::ALL {
            if rule.holds_at(before) != rule.holds_at(stage) {
                self.derivations
                    .forget_fingerprint(&derivation::HashedKey::new(
                        derivation::FactKey::StageRule(rule),
                    ));
            }
        }
    }

    /// Whether `rule` holds at the stage the analysis is at; a recorded read.
    pub(crate) fn stage_rule_holds(&self, rule: derivation::StageRule) -> bool {
        derivation::note_read(|| derivation::FactKey::StageRule(rule));
        rule.holds_at(self.analysis_stage)
    }

    pub(crate) fn get_helper_scan<T: derivation::DerivedFact>(
        &self,
        file_id: FileId,
    ) -> Option<Arc<T>> {
        derivation::note_read(|| derivation::FactKey::HelperScan(file_id));
        self.helper_scans
            .get(&file_id)
            .and_then(|(scan, _)| downcast_fact(scan))
    }

    pub(crate) fn set_helper_scan<T: derivation::DerivedFact>(
        &mut self,
        file_id: FileId,
        scan: Arc<T>,
    ) {
        derivation::note_write(|| derivation::FactKey::HelperScan(file_id));
        let fingerprint = derivation::in_file(Some(file_id), || scan.fingerprint()).max(1);
        if self
            .helper_scans
            .insert(file_id, (scan, fingerprint))
            .is_none()
        {
            self.helper_scan_files.insert(&file_id);
            derivation::note_write(|| derivation::FactKey::HelperScanFiles);
        }
    }

    fn remove_helper_scan(&mut self, file_id: FileId) {
        if self.helper_scans.remove(&file_id).is_some() {
            self.helper_scan_files.remove(&file_id);
            derivation::note_write(|| derivation::FactKey::HelperScan(file_id));
            derivation::note_write(|| derivation::FactKey::HelperScanFiles);
        }
    }

    pub(crate) fn set_load_scan<T: derivation::DerivedFact>(
        &mut self,
        file_id: FileId,
        scan: Arc<T>,
    ) {
        derivation::note_write(|| derivation::FactKey::LoadScan(file_id));
        let fingerprint = derivation::in_file(Some(file_id), || scan.fingerprint()).max(1);
        if self
            .load_scans
            .insert(file_id, (scan, fingerprint))
            .is_none()
        {
            self.load_scan_files.insert(&file_id);
            derivation::note_write(|| derivation::FactKey::LoadScanFiles);
        }
    }

    pub(crate) fn remove_load_scan(&mut self, file_id: FileId) {
        if self.load_scans.remove(&file_id).is_some() {
            self.load_scan_files.remove(&file_id);
            derivation::note_write(|| derivation::FactKey::LoadScan(file_id));
            derivation::note_write(|| derivation::FactKey::LoadScanFiles);
        }
    }

    /// Every file's load scan, in file order.
    pub(crate) fn load_scans<T: derivation::DerivedFact>(&self) -> Vec<(FileId, Arc<T>)> {
        derivation::note_read(|| derivation::FactKey::LoadScanFiles);
        let mut scans = self
            .load_scans
            .iter()
            .filter_map(|(file_id, (scan, _))| {
                derivation::note_read(|| derivation::FactKey::LoadScan(*file_id));
                Some((*file_id, downcast_fact(scan)?))
            })
            .collect::<Vec<_>>();
        scans.sort_unstable_by_key(|(file_id, _)| *file_id);
        scans
    }

    /// The net fact of the call at `range` of `file_id`, read as that call's
    /// fact alone.
    pub(crate) fn get_net_call<T: derivation::DerivedFact>(
        &self,
        file_id: FileId,
        range: rowan::TextRange,
    ) -> Option<Arc<T>> {
        derivation::note_read(|| derivation::FactKey::NetCall(crate::InFiled::new(file_id, range)));
        self.net_site(net_site::NetSiteKind::Call, file_id, range)
    }

    /// `site`'s file's net helpers, read as the one summary of the body at
    /// `site`.
    pub(crate) fn get_net_helper<T: derivation::DerivedFact>(
        &self,
        site: &crate::InFiled<rowan::TextRange>,
    ) -> Option<Arc<T>> {
        derivation::note_read(|| derivation::FactKey::NetHelper(site.clone()));
        self.net_site(net_site::NetSiteKind::Helper, site.file_id, site.value)
    }

    fn net_site<T: derivation::DerivedFact>(
        &self,
        kind: net_site::NetSiteKind,
        file_id: FileId,
        range: rowan::TextRange,
    ) -> Option<Arc<T>> {
        let (fact, _) = self.net_sites.get(&file_id)?.get(kind, range)?;
        downcast_fact(fact)
    }

    fn net_site_fingerprint(
        &self,
        kind: net_site::NetSiteKind,
        site: &crate::InFiled<rowan::TextRange>,
    ) -> derivation::Fingerprint {
        self.net_sites
            .get(&site.file_id)
            .and_then(|sites| sites.get(kind, site.value))
            .map_or(derivation::ABSENT, stored_fact_fingerprint)
    }

    /// Stores the net fact of the call at `range` of `file_id`.
    pub(crate) fn set_net_call<T: derivation::DerivedFact>(
        &mut self,
        file_id: FileId,
        range: rowan::TextRange,
        fact: Arc<T>,
    ) {
        self.store_net_site(net_site::NetSiteKind::Call, file_id, range, fact);
    }

    /// Stores the net summary of the function body at `range` of `file_id`.
    pub(crate) fn set_net_helper<T: derivation::DerivedFact>(
        &mut self,
        file_id: FileId,
        range: rowan::TextRange,
        summary: Arc<T>,
    ) {
        self.store_net_site(net_site::NetSiteKind::Helper, file_id, range, summary);
    }

    fn store_net_site(
        &mut self,
        kind: net_site::NetSiteKind,
        file_id: FileId,
        range: rowan::TextRange,
        fact: Arc<dyn derivation::DerivedFact>,
    ) {
        derivation::note_write(|| kind.key(file_id, range));
        let fingerprint = derivation::in_file(Some(file_id), || fact.fingerprint()).max(1);
        let sites = Arc::make_mut(self.net_sites.entry(file_id).or_default());
        let (mut linked, mut unlinked) = (Vec::new(), Vec::new());
        sites.insert(
            file_id,
            kind,
            range,
            (fact, fingerprint),
            &mut linked,
            &mut unlinked,
        );
        self.relink_net_sites(file_id, linked, unlinked);
    }

    fn reset_net_site(
        &mut self,
        kind: net_site::NetSiteKind,
        site: &crate::InFiled<rowan::TextRange>,
    ) {
        derivation::note_write(|| kind.key(site.file_id, site.value));
        let Some(sites) = self.net_sites.get_mut(&site.file_id) else {
            return;
        };
        if sites.get(kind, site.value).is_none() {
            return;
        }
        let mut unlinked = Vec::new();
        let sites = Arc::make_mut(sites);
        sites.remove(site.file_id, kind, site.value, &mut unlinked);
        if sites.is_empty() {
            self.net_sites.remove(&site.file_id);
        }
        self.relink_net_sites(site.file_id, Vec::new(), unlinked);
    }

    /// Drops every net site of `file_id`.
    fn remove_net_sites(&mut self, file_id: FileId) {
        let Some(sites) = self.net_sites.remove(&file_id) else {
            return;
        };
        if derivation::recording() {
            for (kind, range, _) in sites.sorted() {
                derivation::note_write(|| kind.key(file_id, range));
            }
        }
        self.relink_net_sites(file_id, Vec::new(), sites.named_files().collect());
    }

    /// Re-files `holder` under the files its net sites came to name and no
    /// longer name.
    fn relink_net_sites(&mut self, holder: FileId, linked: Vec<FileId>, unlinked: Vec<FileId>) {
        for file_id in unlinked {
            if let Some(holders) = self.net_helper_refs.get_mut(&file_id) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.net_helper_refs.remove(&file_id);
                }
            }
        }
        for file_id in linked {
            self.net_helper_refs
                .entry(file_id)
                .or_default()
                .insert(holder);
        }
    }

    /// Carries the positions of `remap`'s file that other files' net sites
    /// name to their new place. Returns the sites of each file that named
    /// one the edit destroyed.
    pub(crate) fn remap_net_sites(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        let mut holders = self
            .net_helper_refs
            .get(&remap.file_id)
            .map(|holders| holders.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        holders.sort_unstable();
        let mut lost = Vec::new();
        for holder in holders {
            let Some(sites) = self.net_sites.get(&holder) else {
                continue;
            };
            let mut moved = Vec::new();
            for (kind, range, (fact, _)) in sites.sorted() {
                match fact.remap(remap) {
                    crate::Remap::Moved(fact) => moved.push((kind, range, fact)),
                    crate::Remap::Lost => {
                        lost.push(crate::LostFact::new(holder, kind.key(holder, range)))
                    }
                    crate::Remap::Unrelated => {}
                }
            }
            for (kind, range, fact) in moved {
                self.store_net_site(kind, holder, range, fact);
            }
        }
        lost
    }

    pub(crate) fn get_function_summaries<T: derivation::DerivedFact>(
        &self,
        file_id: FileId,
    ) -> Option<Arc<T>> {
        derivation::note_file_fact_read(derivation::FileFact::FunctionSummaries, file_id);
        self.function_summaries
            .get(&file_id)
            .and_then(|(helpers, _)| downcast_fact(helpers))
    }

    /// Files `helpers` under `fingerprint`, computed by the caller as
    /// `helpers`' [`derivation::DerivedFact::fingerprint`] under
    /// [`derivation::in_file`]`(Some(file_id), ..)`.
    pub(crate) fn set_function_summaries<T: derivation::DerivedFact>(
        &mut self,
        file_id: FileId,
        helpers: Arc<T>,
        fingerprint: derivation::Fingerprint,
    ) {
        derivation::note_write(|| derivation::FactKey::FunctionSummaries(file_id));
        self.function_summaries
            .insert(file_id, (helpers, fingerprint));
    }

    /// The files that have a gmod helper scan, sorted.
    pub(crate) fn helper_scan_files(&self) -> Vec<FileId> {
        derivation::note_read(|| derivation::FactKey::HelperScanFiles);
        let mut file_ids = self.helper_scans.keys().copied().collect::<Vec<_>>();
        file_ids.sort_unstable();
        file_ids
    }

    pub fn remove_index(&mut self, mut file_ids: Vec<FileId>) {
        file_ids.sort_by_key(|file_id| file_id.id);
        file_ids.dedup();
        if file_ids.is_empty() {
            return;
        }

        let _profile = Profile::cond_new("remove indexes", file_ids.len() > 1);
        for &file_id in &file_ids {
            self.remove_helper_scan(file_id);
            self.remove_role_mentions(file_id);
            self.remove_load_scan(file_id);
            self.remove_net_sites(file_id);
            if self.function_summaries.remove(&file_id).is_some() {
                derivation::note_write(|| derivation::FactKey::FunctionSummaries(file_id));
            }
            self.queries.remove_file(file_id);
        }
        for &file_id in &file_ids {
            if let Some(path) = self.get_vfs().get_file_path(&file_id) {
                log::debug!(
                    "remove_index: file_id={:?} path={}",
                    file_id,
                    path.display()
                );
            } else {
                log::debug!("remove_index: file_id={:?} (no path)", file_id);
            }
        }
        self.remove_files(&file_ids);
    }

    pub fn get_metatable_index_mut(&mut self) -> &mut LuaMetatableIndex {
        Arc::make_mut(&mut self.metatable_index)
    }

    /// Reads record their own keys on `LuaMetatableIndex`'s methods — see
    /// `get_operator_index`.
    pub fn get_metatable_index(&self) -> &LuaMetatableIndex {
        self.metatable_index.as_ref()
    }

    pub fn get_decl_index_mut(&mut self) -> &mut LuaDeclIndex {
        Arc::make_mut(&mut self.decl_index)
    }

    pub fn get_reference_index_mut(&mut self) -> &mut LuaReferenceIndex {
        Arc::make_mut(&mut self.references_index)
    }

    /// Rebuilds the inference-derived state left stale by a batch of fact
    /// bindings. See [`LuaTypeIndex::flush_inference_derived_state`].
    pub fn flush_inference_derived_state(&mut self) {
        if self.types_index.inference_derived_state_is_stale() {
            Arc::make_mut(&mut self.types_index).flush_inference_derived_state();
        }
    }

    pub fn get_type_index_mut(&mut self) -> &mut LuaTypeIndex {
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.types_index)
    }

    /// See [`Self::type_structure_revision`]. Memos over type/member-derived
    /// facts must be discarded when this changes.
    pub fn type_structure_revision(&self) -> u64 {
        self.type_structure_revision
    }

    pub fn get_inference_fact(&self, node: &LuaInferenceNodeId) -> Option<LuaTypeFact> {
        match node {
            LuaInferenceNodeId::TypeOwner(owner) => self.types_index.get_type_fact(owner),
            LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(decl_id)) => self
                .types_index
                .get_type_fact(&LuaTypeOwner::Decl(*decl_id)),
            LuaInferenceNodeId::Definition(definition) => {
                self.types_index.get_definition_fact(definition).cloned()
            }
            LuaInferenceNodeId::SignatureParam {
                signature_id,
                param_idx,
            } => self
                .call_site_param_index
                .get_inferred_param_fact(signature_id, usize::from(*param_idx))
                .cloned(),
        }
    }

    pub fn publish_inference_facts(
        &mut self,
        mut updates: Vec<(LuaInferenceNodeId, LuaTypeFact)>,
    ) -> FxHashSet<FileId> {
        updates.sort_by(|(left_node, _), (right_node, _)| left_node.stable_cmp(right_node));

        let mut conflicting_nodes = FxHashSet::default();
        for pair in updates.windows(2) {
            let [(left_node, left_fact), (right_node, right_fact)] = pair else {
                unreachable!();
            };
            if left_node == right_node && left_fact != right_fact {
                conflicting_nodes.insert(left_node.clone());
            }
        }

        let mut changed_files = FxHashSet::default();
        let mut previous_node = None;
        for (node, fact) in updates {
            if conflicting_nodes.contains(&node) || previous_node.as_ref() == Some(&node) {
                continue;
            }
            previous_node = Some(node.clone());
            if self.get_inference_fact(&node).as_ref() == Some(&fact) {
                continue;
            }

            match node {
                LuaInferenceNodeId::TypeOwner(owner) => {
                    let file_id = Arc::make_mut(&mut self.types_index)
                        .force_bind_type_fact_unchecked(
                            owner,
                            LuaTypeCache::InferType(fact.typ().clone()),
                            LuaTypeFactMetadata::from_fact(&fact),
                        );
                    changed_files.insert(file_id);
                }
                LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(decl_id)) => {
                    let file_id = Arc::make_mut(&mut self.types_index)
                        .force_bind_type_fact_unchecked(
                            LuaTypeOwner::Decl(decl_id),
                            LuaTypeCache::InferType(fact.typ().clone()),
                            LuaTypeFactMetadata::from_fact(&fact),
                        );
                    changed_files.insert(file_id);
                }
                LuaInferenceNodeId::Definition(definition) => {
                    let file_id = Arc::make_mut(&mut self.types_index)
                        .bind_definition_fact_unchecked(definition, fact);
                    changed_files.insert(file_id);
                }
                LuaInferenceNodeId::SignatureParam { .. } => {}
            }
        }

        if !changed_files.is_empty() {
            // The writes above go direct rather than through `get_type_index_mut`.
            self.type_structure_revision = next_type_structure_revision();
            Arc::make_mut(&mut self.types_index).mark_inference_derived_state_dirty();
        }
        changed_files
    }

    pub fn get_module_index_mut(&mut self) -> &mut LuaModuleIndex {
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.modules_index)
    }

    #[track_caller]
    pub fn get_member_index_mut(&mut self) -> &mut LuaMemberIndex {
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.members_index)
    }

    #[track_caller]
    pub fn get_property_index_mut(&mut self) -> &mut LuaPropertyIndex {
        Arc::make_mut(&mut self.property_index)
    }

    pub fn get_signature_index_mut(&mut self) -> &mut LuaSignatureIndex {
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.signature_index)
    }

    pub fn get_diagnostic_index_mut(&mut self) -> &mut DiagnosticIndex {
        Arc::make_mut(&mut self.diagnostic_index)
    }

    pub fn get_operator_index_mut(&mut self) -> &mut LuaOperatorIndex {
        Arc::make_mut(&mut self.operator_index)
    }

    pub fn get_flow_index_mut(&mut self) -> &mut LuaFlowIndex {
        Arc::make_mut(&mut self.flow_index)
    }

    pub fn get_decl_index(&self) -> &LuaDeclIndex {
        self.decl_index.as_ref()
    }

    pub fn get_reference_index(&self) -> &LuaReferenceIndex {
        self.references_index.as_ref()
    }

    pub fn get_type_index(&self) -> &LuaTypeIndex {
        self.types_index.as_ref()
    }

    pub fn get_module_index(&self) -> &LuaModuleIndex {
        self.modules_index.as_ref()
    }

    pub fn get_member_index(&self) -> &LuaMemberIndex {
        self.members_index.as_ref()
    }

    /// `owner`'s members in an order every session agrees on: by their file's
    /// place in [`Vfs::file_order_key`], then by where each sits in its file.
    pub fn sorted_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let mut members = self.get_member_index().get_members(owner)?;
        let vfs = self.get_vfs();
        members.sort_by_cached_key(|member| {
            let id = member.get_id();
            (
                vfs.file_order_key(&id.file_id),
                member::member_id_sort_key(id),
            )
        });
        Some(members)
    }

    pub fn get_property_index(&self) -> &LuaPropertyIndex {
        self.property_index.as_ref()
    }

    pub fn get_signature_index(&self) -> &LuaSignatureIndex {
        self.signature_index.as_ref()
    }

    pub fn get_diagnostic_index(&self) -> &DiagnosticIndex {
        self.diagnostic_index.as_ref()
    }

    /// Reads record their own keys on `LuaOperatorIndex`'s methods — no
    /// catch-all note here, or every file touching the index poisons the
    /// domain bit for its successors.
    pub fn get_operator_index(&self) -> &LuaOperatorIndex {
        self.operator_index.as_ref()
    }

    pub fn get_flow_index(&self) -> &LuaFlowIndex {
        self.flow_index.as_ref()
    }

    pub fn get_call_target_index(&self) -> &LuaCallTargetIndex {
        self.call_target_index.as_ref()
    }

    pub fn get_call_target_index_mut(&mut self) -> &mut LuaCallTargetIndex {
        Arc::make_mut(&mut self.call_target_index)
    }

    pub fn get_accessor_func_index(&self) -> &AccessorFuncAnnotationIndex {
        self.accessor_func_index.as_ref()
    }

    pub fn get_accessor_func_index_mut(&mut self) -> &mut AccessorFuncAnnotationIndex {
        Arc::make_mut(&mut self.accessor_func_index)
    }

    pub fn get_accessor_func_call_index(&self) -> &AccessorFuncCallIndex {
        self.accessor_func_call_index.as_ref()
    }

    pub fn get_accessor_func_call_index_mut(&mut self) -> &mut AccessorFuncCallIndex {
        Arc::make_mut(&mut self.accessor_func_call_index)
    }

    pub fn get_call_site_param_index(&self) -> &CallSiteParamIndex {
        self.call_site_param_index.as_ref()
    }

    pub fn get_call_site_param_index_mut(&mut self) -> &mut CallSiteParamIndex {
        Arc::make_mut(&mut self.call_site_param_index)
    }

    /// Reads record their own keys on `GmodClassMetadataIndex`'s methods —
    /// see `get_operator_index`.
    pub fn get_gmod_class_metadata_index(&self) -> &GmodClassMetadataIndex {
        self.gmod_class_index.as_ref()
    }

    pub fn get_gmod_class_metadata_index_mut(&mut self) -> &mut GmodClassMetadataIndex {
        Arc::make_mut(&mut self.gmod_class_index)
    }

    pub fn get_gmod_infer_index(&self) -> &GmodInferIndex {
        self.gmod_infer_index.as_ref()
    }

    pub fn get_gmod_infer_index_mut(&mut self) -> &mut GmodInferIndex {
        Arc::make_mut(&mut self.gmod_infer_index)
    }

    pub fn get_gmod_load_index(&self) -> &GmodLoadIndex {
        self.gmod_load_index.as_ref()
    }

    pub fn get_gmod_load_index_mut(&mut self) -> &mut GmodLoadIndex {
        Arc::make_mut(&mut self.gmod_load_index)
    }

    pub fn get_gmod_network_index(&self) -> &GmodNetworkIndex {
        self.gmod_network_index.as_ref()
    }

    pub fn get_gmod_network_index_mut(&mut self) -> &mut GmodNetworkIndex {
        Arc::make_mut(&mut self.gmod_network_index)
    }

    /// The dynamic fields filed so far; none before their writers' stage of a
    /// run, so a run reads what it files, not what an earlier one left.
    pub fn get_dynamic_field_index(&self) -> &DynamicFieldIndex {
        static UNFILED: std::sync::LazyLock<DynamicFieldIndex> =
            std::sync::LazyLock::new(DynamicFieldIndex::default);
        if !self.dynamic_field_index.is_empty()
            && !self.stage_rule_holds(derivation::StageRule::DynamicFieldsFiled)
        {
            return &UNFILED;
        }
        self.dynamic_field_index.as_ref()
    }

    pub fn get_dynamic_field_index_mut(&mut self) -> &mut DynamicFieldIndex {
        Arc::make_mut(&mut self.dynamic_field_index)
    }

    pub fn get_vfs(&self) -> &Vfs {
        self.vfs.as_ref()
    }

    pub fn get_vfs_mut(&mut self) -> &mut Vfs {
        Arc::make_mut(&mut self.vfs)
    }

    pub fn get_file_dependencies_index(&self) -> &LuaDependencyIndex {
        self.file_dependencies_index.as_ref()
    }

    pub fn get_file_dependencies_index_mut(&mut self) -> &mut LuaDependencyIndex {
        Arc::make_mut(&mut self.file_dependencies_index)
    }

    pub fn get_numeric_range_population_index(&self) -> &NumericRangePopulationIndex {
        self.numeric_range_population_index.as_ref()
    }

    pub fn get_numeric_range_population_index_mut(&mut self) -> &mut NumericRangePopulationIndex {
        Arc::make_mut(&mut self.numeric_range_population_index)
    }

    pub fn get_global_index(&self) -> &LuaGlobalIndex {
        self.global_index.as_ref()
    }

    pub fn get_global_index_mut(&mut self) -> &mut LuaGlobalIndex {
        Arc::make_mut(&mut self.global_index)
    }

    pub fn get_json_schema_index(&self) -> &JsonSchemaIndex {
        self.json_schema_index.as_ref()
    }

    pub fn get_json_schema_index_mut(&mut self) -> &mut JsonSchemaIndex {
        Arc::make_mut(&mut self.json_schema_index)
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        Arc::make_mut(&mut self.vfs).update_config(config.clone());
        Arc::make_mut(&mut self.modules_index).update_config(config.clone());
        self.emmyrc = config;
    }

    pub fn get_emmyrc(&self) -> &Emmyrc {
        &self.emmyrc
    }

    pub fn get_effective_resource_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.modules_index.get_main_workspace_roots();

        for configured_path in self.emmyrc.resource.paths.iter().map(PathBuf::from) {
            if !paths.contains(&configured_path) {
                paths.push(configured_path);
            }
        }

        paths
    }

    /// A worker-side view of this index for the parallel lua-analyze walk.
    /// Every domain shares its `Arc` with the base until the first mutable
    /// accessor clones it (`Arc::make_mut`), so a fork costs refcount bumps
    /// plus one clone per domain actually written, the vfs included.
    ///
    /// The fork keeps the base's `type_structure_revision`, so it must not
    /// outlive the walk that created it; use [`Self::fork_independent`] for a
    /// copy that lives on.
    pub(crate) fn fork(&self) -> Self {
        Self {
            type_structure_revision: self.type_structure_revision,
            decl_index: Arc::clone(&self.decl_index),
            references_index: Arc::clone(&self.references_index),
            types_index: Arc::clone(&self.types_index),
            modules_index: Arc::clone(&self.modules_index),
            members_index: Arc::clone(&self.members_index),
            property_index: Arc::clone(&self.property_index),
            signature_index: Arc::clone(&self.signature_index),
            diagnostic_index: Arc::clone(&self.diagnostic_index),
            operator_index: Arc::clone(&self.operator_index),
            flow_index: Arc::clone(&self.flow_index),
            call_target_index: Arc::clone(&self.call_target_index),
            accessor_func_index: Arc::clone(&self.accessor_func_index),
            accessor_func_call_index: Arc::clone(&self.accessor_func_call_index),
            call_site_param_index: Arc::clone(&self.call_site_param_index),
            gmod_class_index: Arc::clone(&self.gmod_class_index),
            gmod_infer_index: Arc::clone(&self.gmod_infer_index),
            gmod_load_index: Arc::clone(&self.gmod_load_index),
            gmod_network_index: Arc::clone(&self.gmod_network_index),
            dynamic_field_index: Arc::clone(&self.dynamic_field_index),
            vfs: Arc::clone(&self.vfs),
            file_dependencies_index: Arc::clone(&self.file_dependencies_index),
            numeric_range_population_index: Arc::clone(&self.numeric_range_population_index),
            metatable_index: Arc::clone(&self.metatable_index),
            global_index: Arc::clone(&self.global_index),
            json_schema_index: Arc::clone(&self.json_schema_index),
            derivations: Arc::clone(&self.derivations),
            gmod_call_roles: self.gmod_call_roles.clone(),
            call_role_fingerprints: self.call_role_fingerprints.clone(),
            role_mentions: self.role_mentions.clone(),
            emmyrc: Arc::clone(&self.emmyrc),
            helper_scans: self.helper_scans.clone(),
            helper_scan_files: self.helper_scan_files,
            load_scans: self.load_scans.clone(),
            load_scan_files: self.load_scan_files,
            net_sites: self.net_sites.clone(),
            net_helper_refs: self.net_helper_refs.clone(),
            function_summaries: self.function_summaries.clone(),
            analysis_stage: self.analysis_stage,
            queries: Arc::clone(&self.queries),
        }
    }

    /// A copy-on-write copy that evolves independently of `self`. It takes a
    /// fresh `type_structure_revision`, so thread-local memos keyed by the
    /// revision never hand one copy's answers to the other.
    pub(crate) fn fork_independent(&self) -> Self {
        Self {
            type_structure_revision: next_type_structure_revision(),
            queries: Arc::new(query::Queries::clone(&self.queries)),
            ..self.fork()
        }
    }

    /// A fork of the index for one task: it writes its own copies of the
    /// shards and values it changes, for [`Self::install_tasks`].
    pub(crate) fn task_fork(&self) -> Self {
        macro_rules! fork {
            ($($domain:ident),*; $($field:ident),*) => {
                Self {
                    $($domain: Arc::new(cow_shards::TaskFork::task_fork(&*self.$domain)),)*
                    $($field: cow_shards::TaskFork::task_fork(&self.$field),)*
                    vfs: Arc::clone(&self.vfs),
                    emmyrc: Arc::clone(&self.emmyrc),
                    derivations: Arc::clone(&self.derivations),
                    gmod_call_roles: self.gmod_call_roles.clone(),
                    type_structure_revision: self.type_structure_revision,
                    analysis_stage: self.analysis_stage,
                    queries: Arc::clone(&self.queries),
                }
            };
        }
        fork!(
            decl_index,
            references_index,
            types_index,
            modules_index,
            members_index,
            property_index,
            signature_index,
            diagnostic_index,
            operator_index,
            flow_index,
            call_target_index,
            accessor_func_index,
            accessor_func_call_index,
            call_site_param_index,
            gmod_class_index,
            gmod_infer_index,
            gmod_load_index,
            gmod_network_index,
            dynamic_field_index,
            file_dependencies_index,
            numeric_range_population_index,
            metatable_index,
            global_index,
            json_schema_index;
            call_role_fingerprints,
            role_mentions,
            helper_scans,
            helper_scan_files,
            load_scans,
            load_scan_files,
            net_sites,
            net_helper_refs,
            function_summaries
        )
    }

    /// Takes in what each of `forks`, task forks of `frozen`, changed, in
    /// order. Domains
    /// install on the thread pool, each taking every fork in order. Returns
    /// each fork's conflicts.
    pub(crate) fn install_tasks(
        &mut self,
        frozen: &Self,
        mut forks: Vec<Self>,
    ) -> Vec<Vec<&'static str>> {
        let mut conflicts = vec![Vec::new(); forks.len()];
        macro_rules! domains {
            ($forks:ident, $reports:ident; $($domain:ident),*) => {
                let mut $reports = Vec::new();
                $(
                    let _ = stringify!($domain);
                    $reports.push(vec![Vec::new(); $forks.len()]);
                )*
                rayon::scope(|scope| {
                    let mut out = $reports.iter_mut();
                    $({
                        let target = &mut self.$domain;
                        let base = &frozen.$domain;
                        let installed = $forks
                            .iter_mut()
                            .map(|fork| std::mem::replace(&mut fork.$domain, Arc::clone(base)))
                            .collect::<Vec<_>>();
                        let out = out.next().expect("a report per domain");
                        scope.spawn(move |_| {
                            let wrote = installed
                                .into_iter()
                                .enumerate()
                                .filter(|(_, fork)| cow_shards::TaskFork::wrote(&**fork, base))
                                .map(|(at, fork)| (at, Arc::unwrap_or_clone(fork)))
                                .collect::<Vec<_>>();
                            if !wrote.is_empty() {
                                Arc::make_mut(target).install_tasks_reporting(base, wrote, out);
                            }
                        });
                    })*
                });
            };
        }
        domains!(
            forks,
            reports;
            decl_index,
            references_index,
            types_index,
            modules_index,
            members_index,
            property_index,
            signature_index,
            diagnostic_index,
            operator_index,
            flow_index,
            call_target_index,
            accessor_func_index,
            accessor_func_call_index,
            call_site_param_index,
            gmod_class_index,
            gmod_infer_index,
            gmod_load_index,
            gmod_network_index,
            dynamic_field_index,
            file_dependencies_index,
            numeric_range_population_index,
            metatable_index,
            global_index,
            json_schema_index
        );
        for reported in reports {
            for (at, fields) in reported.into_iter().enumerate() {
                conflicts[at].extend(fields);
            }
        }
        for (fork, conflicts) in forks.into_iter().zip(&mut conflicts) {
            self.install_task_fields(frozen, fork, conflicts);
        }
        conflicts
    }

    /// Folds what the installs of a superstep kept apart while the frozen
    /// index shared their storage back into it, once the frozen index and
    /// the forks are gone.
    pub(crate) fn flatten_installs(&mut self) {
        macro_rules! flatten {
            ($($domain:ident),*; $($field:ident),*) => {
                rayon::scope(|scope| {
                    $(if let Some(domain) = Arc::get_mut(&mut self.$domain) {
                        scope.spawn(move |_| cow_shards::TaskFork::flatten(domain));
                    })*
                });
                $(cow_shards::TaskFork::flatten(&mut self.$field);)*
            };
        }
        flatten!(
            decl_index,
            references_index,
            types_index,
            modules_index,
            members_index,
            property_index,
            signature_index,
            diagnostic_index,
            operator_index,
            flow_index,
            call_target_index,
            accessor_func_index,
            accessor_func_call_index,
            call_site_param_index,
            gmod_class_index,
            gmod_infer_index,
            gmod_load_index,
            gmod_network_index,
            dynamic_field_index,
            file_dependencies_index,
            numeric_range_population_index,
            metatable_index,
            global_index,
            json_schema_index;
            call_role_fingerprints,
            role_mentions,
            helper_scans,
            helper_scan_files,
            load_scans,
            load_scan_files,
            net_sites,
            net_helper_refs,
            function_summaries
        );
    }

    /// Takes in what `fork`, a task fork of `frozen`, changed of the fields
    /// the index holds itself.
    fn install_task_fields(
        &mut self,
        frozen: &Self,
        fork: Self,
        conflicts: &mut Vec<&'static str>,
    ) {
        debug_assert!(
            Arc::ptr_eq(&fork.vfs, &frozen.vfs) && Arc::ptr_eq(&fork.emmyrc, &frozen.emmyrc)
        );
        debug_assert_eq!(fork.analysis_stage, frozen.analysis_stage);
        let roles = |index: &Self| {
            index
                .gmod_call_roles
                .as_ref()
                .map(|(_, fingerprint)| *fingerprint)
        };
        let (now, was, forked) = (roles(self), roles(frozen), roles(&fork));
        if forked != was {
            if now != was && now != forked {
                conflicts.push("DbIndex.gmod_call_roles");
            }
            self.gmod_call_roles = fork.gmod_call_roles.clone();
        }
        if fork.type_structure_revision != frozen.type_structure_revision {
            self.type_structure_revision = next_type_structure_revision();
        }
        macro_rules! fields {
            ($($field:ident $(=> $merge:expr)?),*) => {
                $(if cow_shards::install_field!(self.$field, frozen.$field, fork.$field $(, $merge)?) {
                    conflicts.push(concat!("DbIndex.", stringify!($field)));
                })*
            };
        }
        fields!(
            call_role_fingerprints,
            role_mentions,
            helper_scans,
            helper_scan_files,
            load_scans,
            load_scan_files,
            net_sites,
            net_helper_refs => cow_shards::merge_set,
            function_summaries
        );
    }
}

impl LuaIndex for DbIndex {
    fn remove(&mut self, file_id: FileId) {
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.decl_index).remove(file_id);
        Arc::make_mut(&mut self.references_index).remove(file_id);
        Arc::make_mut(&mut self.types_index).remove(file_id);
        Arc::make_mut(&mut self.modules_index).remove(file_id);
        Arc::make_mut(&mut self.members_index).remove(file_id);
        Arc::make_mut(&mut self.property_index).remove(file_id);
        Arc::make_mut(&mut self.signature_index).remove(file_id);
        Arc::make_mut(&mut self.diagnostic_index).remove(file_id);
        Arc::make_mut(&mut self.operator_index).remove(file_id);
        Arc::make_mut(&mut self.flow_index).remove(file_id);
        Arc::make_mut(&mut self.call_target_index).remove(file_id);
        Arc::make_mut(&mut self.accessor_func_index).remove(file_id);
        Arc::make_mut(&mut self.accessor_func_call_index).remove(file_id);
        Arc::make_mut(&mut self.call_site_param_index).remove(file_id);
        Arc::make_mut(&mut self.gmod_class_index).remove(file_id);
        Arc::make_mut(&mut self.gmod_infer_index).remove(file_id);
        Arc::make_mut(&mut self.gmod_load_index).remove(file_id);
        Arc::make_mut(&mut self.gmod_network_index).remove(file_id);
        Arc::make_mut(&mut self.dynamic_field_index).remove(file_id);
        Arc::make_mut(&mut self.file_dependencies_index).remove(file_id);
        Arc::make_mut(&mut self.numeric_range_population_index).remove(file_id);
        Arc::make_mut(&mut self.metatable_index).remove(file_id);
        Arc::make_mut(&mut self.global_index).remove(file_id);
        Arc::make_mut(&mut self.json_schema_index).remove(file_id);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        self.type_structure_revision = next_type_structure_revision();
        if let [file_id] = file_ids {
            self.remove(*file_id);
            return;
        }

        use crate::profile::phase;
        phase("remove/decl", || {
            Arc::make_mut(&mut self.decl_index).remove_files(file_ids)
        });
        phase("remove/references", || {
            Arc::make_mut(&mut self.references_index).remove_files(file_ids)
        });
        phase("remove/types", || {
            Arc::make_mut(&mut self.types_index).remove_files(file_ids)
        });
        phase("remove/modules", || {
            Arc::make_mut(&mut self.modules_index).remove_files(file_ids)
        });
        phase("remove/members", || {
            Arc::make_mut(&mut self.members_index).remove_files(file_ids)
        });
        phase("remove/property", || {
            Arc::make_mut(&mut self.property_index).remove_files(file_ids)
        });
        phase("remove/signature", || {
            Arc::make_mut(&mut self.signature_index).remove_files(file_ids)
        });
        phase("remove/diagnostic", || {
            Arc::make_mut(&mut self.diagnostic_index).remove_files(file_ids)
        });
        phase("remove/operator", || {
            Arc::make_mut(&mut self.operator_index).remove_files(file_ids)
        });
        phase("remove/flow", || {
            Arc::make_mut(&mut self.flow_index).remove_files(file_ids)
        });
        phase("remove/call_target", || {
            Arc::make_mut(&mut self.call_target_index).remove_files(file_ids)
        });
        phase("remove/accessor_func", || {
            Arc::make_mut(&mut self.accessor_func_index).remove_files(file_ids)
        });
        phase("remove/accessor_func_call", || {
            Arc::make_mut(&mut self.accessor_func_call_index).remove_files(file_ids)
        });
        phase("remove/call_site_param", || {
            Arc::make_mut(&mut self.call_site_param_index).remove_files(file_ids)
        });
        phase("remove/gmod_class", || {
            Arc::make_mut(&mut self.gmod_class_index).remove_files(file_ids)
        });
        phase("remove/gmod_infer", || {
            Arc::make_mut(&mut self.gmod_infer_index).remove_files(file_ids)
        });
        phase("remove/gmod_load", || {
            Arc::make_mut(&mut self.gmod_load_index).remove_files(file_ids)
        });
        phase("remove/gmod_network", || {
            Arc::make_mut(&mut self.gmod_network_index).remove_files(file_ids)
        });
        phase("remove/dynamic_field", || {
            Arc::make_mut(&mut self.dynamic_field_index).remove_files(file_ids)
        });
        phase("remove/file_dependencies", || {
            Arc::make_mut(&mut self.file_dependencies_index).remove_files(file_ids)
        });
        phase("remove/numeric_range", || {
            for &file_id in file_ids {
                Arc::make_mut(&mut self.numeric_range_population_index).remove(file_id);
            }
        });
        phase("remove/metatable", || {
            Arc::make_mut(&mut self.metatable_index).remove_files(file_ids)
        });
        phase("remove/global", || {
            Arc::make_mut(&mut self.global_index).remove_files(file_ids)
        });
        phase("remove/json_schema", || {
            Arc::make_mut(&mut self.json_schema_index).remove_files(file_ids)
        });
    }

    fn clear(&mut self) {
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.decl_index).clear();
        Arc::make_mut(&mut self.references_index).clear();
        Arc::make_mut(&mut self.types_index).clear();
        Arc::make_mut(&mut self.modules_index).clear();
        Arc::make_mut(&mut self.members_index).clear();
        Arc::make_mut(&mut self.property_index).clear();
        Arc::make_mut(&mut self.signature_index).clear();
        Arc::make_mut(&mut self.diagnostic_index).clear();
        Arc::make_mut(&mut self.operator_index).clear();
        Arc::make_mut(&mut self.flow_index).clear();
        Arc::make_mut(&mut self.call_target_index).clear();
        Arc::make_mut(&mut self.accessor_func_index).clear();
        Arc::make_mut(&mut self.accessor_func_call_index).clear();
        Arc::make_mut(&mut self.call_site_param_index).clear();
        Arc::make_mut(&mut self.gmod_class_index).clear();
        Arc::make_mut(&mut self.gmod_infer_index).clear();
        Arc::make_mut(&mut self.gmod_load_index).clear();
        Arc::make_mut(&mut self.gmod_network_index).clear();
        Arc::make_mut(&mut self.dynamic_field_index).clear();
        Arc::make_mut(&mut self.file_dependencies_index).clear();
        Arc::make_mut(&mut self.numeric_range_population_index).clear();
        Arc::make_mut(&mut self.metatable_index).clear();
        Arc::make_mut(&mut self.global_index).clear();
        Arc::make_mut(&mut self.json_schema_index).clear();
        self.helper_scans.clear();
        self.helper_scan_files = derivation::SetFingerprint::default();
        self.load_scans.clear();
        self.load_scan_files = derivation::SetFingerprint::default();
        self.net_sites.clear();
        self.net_helper_refs.clear();
        self.function_summaries.clear();
        self.derivations = Arc::default();
        self.gmod_call_roles = None;
        self.call_role_fingerprints.clear();
        self.role_mentions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::DbIndex;
    use crate::db_index::derivation::{AnalysisStage, FactKey, HashedKey, StageRule};

    /// A stage rule's value follows the stage, so a fingerprint cached under
    /// one stage is not what a read under the next is answered with.
    #[test]
    fn a_stage_change_forgets_the_rule_fingerprints_it_moves() {
        let mut db = DbIndex::new();
        let rule = HashedKey::new(FactKey::StageRule(StageRule::FloorUnsettled));
        db.set_analysis_stage(AnalysisStage::Open);
        db.reader_graph_mut().intern_read(rule.clone());
        let at_open = db
            .reader_graph()
            .current_fingerprint(&rule, |key| db.fact_fingerprint(key.key()));

        db.set_analysis_stage(AnalysisStage::Closed);
        let at_closed = db
            .reader_graph()
            .current_fingerprint(&rule, |key| db.fact_fingerprint(key.key()));

        assert_ne!(at_open, db.fact_fingerprint(rule.key()), "the rule moves");
        assert_eq!(at_closed, db.fact_fingerprint(rule.key()));
    }
}
