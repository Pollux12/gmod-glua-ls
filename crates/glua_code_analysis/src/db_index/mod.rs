mod accessor_func;
mod call_site_param;
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
mod numeric_range_population;
mod operators;
mod property;
mod reference;
mod schema;
mod semantic_decl;
mod signature;
mod traits;
mod r#type;
pub(crate) use intern::intern_smol_str;

use rustc_hash::{FxHashMap, FxHashSet};
use std::{path::PathBuf, sync::Arc};

use crate::{Emmyrc, FileId, Vfs, profile::Profile};
pub use accessor_func::*;
pub use call_site_param::{CallSiteParamContribution, CallSiteParamIndex};
pub use declaration::*;
pub use dependency::{LuaDependencyIndex, LuaDependencyKind, LuaDependencySite};
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
    gmod_call_roles: Option<(Arc<dyn derivation::DerivedFact>, derivation::Fingerprint)>,
    emmyrc: Arc<Emmyrc>,
    /// Each file's gmod helper scan with its fingerprint when stored.
    helper_scans: FxHashMap<FileId, (Arc<dyn derivation::DerivedFact>, derivation::Fingerprint)>,
    /// The set of files in `helper_scans`.
    helper_scan_files: derivation::SetFingerprint,
    /// Each file's net helper summaries with their fingerprint when stored.
    net_helpers: FxHashMap<FileId, (Arc<dyn derivation::DerivedFact>, derivation::Fingerprint)>,
    /// Bumped on every *mutable* handle to the type, member, signature or module
    /// index; memos over facts derived from those key on it. It covers those four
    /// and no others — the decl, global, dynamic-field and gmod-infer indexes all
    /// mutate without bumping it, so a memo reading those is not protected here
    /// and has to say how it stays correct. Widening a memo's read set means
    /// widening this too. May over-invalidate, never misses.
    /// Values come from a process-global counter so they are unique across
    /// instances (the memos are thread-local and outlive any one `DbIndex`).
    type_structure_revision: u64,
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
            emmyrc: Arc::new(Emmyrc::default()),
            helper_scans: FxHashMap::default(),
            helper_scan_files: derivation::SetFingerprint::default(),
            net_helpers: FxHashMap::default(),
        }
    }

    pub(crate) fn reader_graph(&self) -> &derivation::ReaderGraph {
        &self.derivations
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
            K::HelperScan(file_id) => self.remove_helper_scan(*file_id),
            K::NetHelpers(file_id) => self.remove_net_helpers(*file_id),
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
            K::DynamicFieldFile(file_id) => {
                self.get_dynamic_field_index_mut().remove_files(&[*file_id])
            }
            K::NetworkFile(file_id) => self.get_gmod_network_index_mut().remove_file(*file_id),
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
                .remove_files(&[*file_id]),
            K::CallSiteFile(file_id) => self.get_call_site_param_index_mut().retract_file(*file_id),
            _ => {}
        }
    }

    pub(crate) fn get_gmod_call_roles<T: derivation::DerivedFact>(&self) -> Option<Arc<T>> {
        derivation::note_read(|| derivation::FactKey::HelperRegistry);
        self.gmod_call_roles
            .as_ref()
            .and_then(|(roles, _)| downcast_fact(roles))
    }

    pub(crate) fn set_gmod_call_roles<T: derivation::DerivedFact>(&mut self, roles: Arc<T>) {
        derivation::note_write(|| derivation::FactKey::HelperRegistry);
        let fingerprint = roles.fingerprint().max(1);
        self.gmod_call_roles = Some((roles, fingerprint));
    }

    /// The fingerprint of what a read of `key` observes now.
    pub(crate) fn fact_fingerprint(&self, key: &derivation::FactKey) -> derivation::Fingerprint {
        derivation::in_file(key.positional_file(), || self.fact_fingerprint_in_file(key))
    }

    fn fact_fingerprint_in_file(&self, key: &derivation::FactKey) -> derivation::Fingerprint {
        use derivation::FactKey as K;
        match key {
            K::TypeCache(_)
            | K::DefinitionFact(_)
            | K::TypeDecl(_)
            | K::SourceTypeDecl(_)
            | K::TypeContrib(_)
            | K::SubTypes(_)
            | K::BoundDecls(..)
            | K::DeclClaim(_)
            | K::TypeDeclScan
            | K::TypeFile(_)
            | K::TableClass(_) => self.types_index.fact_fingerprint(key),
            K::Decl(_) | K::DeclTree(_) | K::DeclContrib(_) => {
                self.decl_index.fact_fingerprint(key)
            }
            K::Member(_)
            | K::MemberSlot(..)
            | K::MemberOwnerMap(_)
            | K::MemberOwnerExprKeys(_)
            | K::MemberKey(_)
            | K::MemberFile(_)
            | K::GlobalPath(_)
            | K::PathClassContrib(_)
            | K::DefinitionSite(_) => self.members_index.fact_fingerprint(key),
            K::Signature(_)
            | K::ValidGuard(_)
            | K::ValidGuardFile(_)
            | K::ValidGuardFiles
            | K::ValidGuardNames
            | K::InferredGuard(_)
            | K::SignatureFile(_)
            | K::ReceiverOutParam(_) => self.signature_index.fact_fingerprint(key),
            K::Contrib(..) | K::CallSiteFile(_) => self.call_site_param_index.fact_fingerprint(key),
            K::Module(_)
            | K::ModuleMeta(_)
            | K::ModuleName(_)
            | K::ModuleNamespace(_)
            | K::ModuleFiles => self.modules_index.fact_fingerprint(key),
            K::SpecialCall(..) | K::SpecialCallFile(_) | K::FlowTree(_) => {
                self.flow_index.fact_fingerprint(key)
            }
            K::DynamicField(..)
            | K::DynamicFieldWildcards(_)
            | K::DynamicFieldFinite(_)
            | K::UnattributedField(_)
            | K::DynamicFieldFile(_) => self.dynamic_field_index.fact_fingerprint(key),
            K::OperatorOwner(_) | K::OperatorFile(_) | K::OperatorContrib(_) => {
                self.operator_index.fact_fingerprint(key)
            }
            K::Metatable(_) | K::MetatableFactory(_) => self.metatable_index.fact_fingerprint(key),
            K::GmodMeta(_) | K::GmodClassFile(_) | K::VguiChain(_) | K::VguiParentCallFiles => {
                self.gmod_class_index.fact_fingerprint(key)
            }
            K::GmodInferFile(_) | K::GmodRealm(_) | K::GmodInferScan => {
                self.gmod_infer_index.fact_fingerprint(key)
            }
            K::GmodLoadFile(_) => self.gmod_load_index.fact_fingerprint(key),
            K::NetworkFile(_) | K::NetworkScan | K::NetHelperNames => {
                self.gmod_network_index.fact_fingerprint(key)
            }
            K::HelperRegistry => self
                .gmod_call_roles
                .as_ref()
                .map_or(derivation::ABSENT, stored_fact_fingerprint),
            K::HelperScan(file_id) => self
                .helper_scans
                .get(file_id)
                .map_or(derivation::ABSENT, stored_fact_fingerprint),
            K::HelperScanFiles => self.helper_scan_files.get(),
            K::NetHelpers(file_id) => self
                .net_helpers
                .get(file_id)
                .map_or(derivation::ABSENT, stored_fact_fingerprint),
            K::Property(_) | K::DeclDefault(_) => self.property_index.fact_fingerprint(key),
            K::Global(_) | K::GlobalScan => self.global_index.fact_fingerprint(key),
            K::References(_)
            | K::ReferencesByName(_)
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
            K::FileSet => self.vfs.file_set_fingerprint(),
        }
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

    pub(crate) fn get_net_helpers<T: derivation::DerivedFact>(
        &self,
        file_id: FileId,
    ) -> Option<Arc<T>> {
        derivation::note_read(|| derivation::FactKey::NetHelpers(file_id));
        self.net_helpers
            .get(&file_id)
            .and_then(|(helpers, _)| downcast_fact(helpers))
    }

    pub(crate) fn set_net_helpers<T: derivation::DerivedFact>(
        &mut self,
        file_id: FileId,
        helpers: Arc<T>,
    ) {
        derivation::note_write(|| derivation::FactKey::NetHelpers(file_id));
        let fingerprint = derivation::in_file(Some(file_id), || helpers.fingerprint()).max(1);
        self.net_helpers.insert(file_id, (helpers, fingerprint));
    }

    fn remove_net_helpers(&mut self, file_id: FileId) {
        if self.net_helpers.remove(&file_id).is_some() {
            derivation::note_write(|| derivation::FactKey::NetHelpers(file_id));
        }
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
            self.remove_net_helpers(file_id);
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
        Arc::make_mut(&mut self.types_index).flush_inference_derived_state();
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
        // Writes into `types_index` below go direct rather than through
        // `get_type_index_mut`, so bump the revision and note the touch here
        // too.
        self.type_structure_revision = next_type_structure_revision();
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

    pub fn get_dynamic_field_index(&self) -> &DynamicFieldIndex {
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
            emmyrc: Arc::clone(&self.emmyrc),
            helper_scans: self.helper_scans.clone(),
            helper_scan_files: self.helper_scan_files,
            net_helpers: self.net_helpers.clone(),
        }
    }

    /// A copy-on-write copy that evolves independently of `self`. It takes a
    /// fresh `type_structure_revision`, so thread-local memos keyed by the
    /// revision never hand one copy's answers to the other.
    pub(crate) fn fork_independent(&self) -> Self {
        Self {
            type_structure_revision: next_type_structure_revision(),
            ..self.fork()
        }
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
        self.net_helpers.clear();
        self.derivations = Arc::default();
        self.gmod_call_roles = None;
    }
}
