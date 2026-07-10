mod accessor_func;
mod call_site_param;
mod declaration;
mod dependency;
mod diagnostic;
mod dynamic_field;
mod flow;
mod global;
mod gmod_class;
mod gmod_infer;
mod gmod_load;
mod gmod_network;
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

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use crate::{Emmyrc, FileId, Vfs, profile::Profile};
pub use accessor_func::*;
pub use call_site_param::CallSiteParamIndex;
pub use declaration::*;
pub use dependency::{LuaDependencyIndex, LuaDependencyKind, LuaDependencySite};
pub use diagnostic::{AnalyzeError, DiagnosticAction, DiagnosticActionKind, DiagnosticIndex};
pub use dynamic_field::{DynamicFieldIndex, DynamicFieldOwner};
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

#[derive(Debug)]
pub struct DbIndex {
    decl_index: LuaDeclIndex,
    references_index: LuaReferenceIndex,
    types_index: LuaTypeIndex,
    modules_index: LuaModuleIndex,
    members_index: LuaMemberIndex,
    property_index: LuaPropertyIndex,
    signature_index: LuaSignatureIndex,
    diagnostic_index: DiagnosticIndex,
    operator_index: LuaOperatorIndex,
    flow_index: LuaFlowIndex,
    accessor_func_index: AccessorFuncAnnotationIndex,
    accessor_func_call_index: AccessorFuncCallIndex,
    call_site_param_index: CallSiteParamIndex,
    gmod_class_index: GmodClassMetadataIndex,
    gmod_infer_index: GmodInferIndex,
    gmod_load_index: GmodLoadIndex,
    gmod_network_index: GmodNetworkIndex,
    dynamic_field_index: DynamicFieldIndex,
    vfs: Vfs,
    file_dependencies_index: LuaDependencyIndex,
    numeric_range_population_index: NumericRangePopulationIndex,
    metatable_index: LuaMetatableIndex,
    global_index: LuaGlobalIndex,
    json_schema_index: JsonSchemaIndex,
    emmyrc: Arc<Emmyrc>,
    /// Revision-keyed cache for workspace-wide derived structures that are pure
    /// functions of VFS content (currently the gmod net-helper registry). Stored
    /// type-erased so `db_index` stays decoupled from the analyzer crate layer.
    /// Invalidated automatically by comparing `Vfs::content_revision`.
    helper_registry_cache: RevisionedCache,
}

/// Type-erased, revision-keyed cache slot (see `DbIndex::helper_registry_cache`).
#[derive(Default)]
struct RevisionedCache(Option<(u64, Arc<dyn std::any::Any + Send + Sync>)>);

impl std::fmt::Debug for RevisionedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some((rev, _)) => write!(f, "RevisionedCache(rev={rev})"),
            None => write!(f, "RevisionedCache(empty)"),
        }
    }
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
            decl_index: LuaDeclIndex::new(),
            references_index: LuaReferenceIndex::new(),
            types_index: LuaTypeIndex::new(),
            modules_index: LuaModuleIndex::new(),
            members_index: LuaMemberIndex::new(),
            property_index: LuaPropertyIndex::new(),
            signature_index: LuaSignatureIndex::new(),
            diagnostic_index: DiagnosticIndex::new(),
            operator_index: LuaOperatorIndex::new(),
            flow_index: LuaFlowIndex::new(),
            accessor_func_index: AccessorFuncAnnotationIndex::new(),
            accessor_func_call_index: AccessorFuncCallIndex::new(),
            call_site_param_index: CallSiteParamIndex::new(),
            gmod_class_index: GmodClassMetadataIndex::new(),
            gmod_infer_index: GmodInferIndex::new(),
            gmod_load_index: GmodLoadIndex::new(),
            gmod_network_index: GmodNetworkIndex::new(),
            dynamic_field_index: DynamicFieldIndex::new(),
            vfs: Vfs::new(),
            file_dependencies_index: LuaDependencyIndex::new(),
            numeric_range_population_index: NumericRangePopulationIndex::new(),
            metatable_index: LuaMetatableIndex::new(),
            global_index: LuaGlobalIndex::new(),
            json_schema_index: JsonSchemaIndex::new(),
            emmyrc: Arc::new(Emmyrc::default()),
            helper_registry_cache: RevisionedCache::default(),
        }
    }

    /// Fetch the cached gmod net-helper registry if it was built at `revision`.
    /// Type-erased so the `db_index` layer need not name the analyzer's registry
    /// type; the caller downcasts to its concrete `T`.
    pub fn get_cached_helper_registry<T: std::any::Any + Send + Sync>(
        &self,
        revision: u64,
    ) -> Option<Arc<T>> {
        match &self.helper_registry_cache.0 {
            Some((cached_rev, value)) if *cached_rev == revision => {
                value.clone().downcast::<T>().ok()
            }
            _ => None,
        }
    }

    /// Store a freshly built gmod net-helper registry keyed by the VFS content
    /// revision it was derived from.
    pub fn set_cached_helper_registry<T: std::any::Any + Send + Sync>(
        &mut self,
        revision: u64,
        value: Arc<T>,
    ) {
        self.helper_registry_cache.0 = Some((revision, value));
    }

    pub fn remove_index(&mut self, mut file_ids: Vec<FileId>) {
        file_ids.sort_by_key(|file_id| file_id.id);
        file_ids.dedup();
        if file_ids.is_empty() {
            return;
        }

        let _profile = Profile::cond_new("remove indexes", file_ids.len() > 1);
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
        &mut self.metatable_index
    }

    pub fn get_metatable_index(&self) -> &LuaMetatableIndex {
        &self.metatable_index
    }

    pub fn get_decl_index_mut(&mut self) -> &mut LuaDeclIndex {
        &mut self.decl_index
    }

    pub fn get_reference_index_mut(&mut self) -> &mut LuaReferenceIndex {
        &mut self.references_index
    }

    pub fn get_type_index_mut(&mut self) -> &mut LuaTypeIndex {
        &mut self.types_index
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
    ) -> HashSet<FileId> {
        updates.sort_by(|(left_node, _), (right_node, _)| left_node.stable_cmp(right_node));

        let mut conflicting_nodes = HashSet::new();
        for pair in updates.windows(2) {
            let [(left_node, left_fact), (right_node, right_fact)] = pair else {
                unreachable!();
            };
            if left_node == right_node && left_fact != right_fact {
                conflicting_nodes.insert(left_node.clone());
            }
        }

        let mut changed_files = HashSet::new();
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
                    let file_id = self.types_index.force_bind_type_fact_unchecked(
                        owner,
                        LuaTypeCache::InferType(fact.typ().clone()),
                        LuaTypeFactMetadata::from_fact(&fact),
                    );
                    changed_files.insert(file_id);
                }
                LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(decl_id)) => {
                    let file_id = self.types_index.force_bind_type_fact_unchecked(
                        LuaTypeOwner::Decl(decl_id),
                        LuaTypeCache::InferType(fact.typ().clone()),
                        LuaTypeFactMetadata::from_fact(&fact),
                    );
                    changed_files.insert(file_id);
                }
                LuaInferenceNodeId::Definition(definition) => {
                    let file_id = self
                        .types_index
                        .bind_definition_fact_unchecked(definition, fact);
                    changed_files.insert(file_id);
                }
                LuaInferenceNodeId::SignatureParam { .. } => {}
            }
        }

        self.types_index
            .rebuild_inference_derived_state(&changed_files);
        changed_files
    }

    pub fn get_module_index_mut(&mut self) -> &mut LuaModuleIndex {
        &mut self.modules_index
    }

    pub fn get_member_index_mut(&mut self) -> &mut LuaMemberIndex {
        &mut self.members_index
    }

    pub fn get_property_index_mut(&mut self) -> &mut LuaPropertyIndex {
        &mut self.property_index
    }

    pub fn get_signature_index_mut(&mut self) -> &mut LuaSignatureIndex {
        &mut self.signature_index
    }

    pub fn get_diagnostic_index_mut(&mut self) -> &mut DiagnosticIndex {
        &mut self.diagnostic_index
    }

    pub fn get_operator_index_mut(&mut self) -> &mut LuaOperatorIndex {
        &mut self.operator_index
    }

    pub fn get_flow_index_mut(&mut self) -> &mut LuaFlowIndex {
        &mut self.flow_index
    }

    pub fn get_decl_index(&self) -> &LuaDeclIndex {
        &self.decl_index
    }

    pub fn get_reference_index(&self) -> &LuaReferenceIndex {
        &self.references_index
    }

    pub fn get_type_index(&self) -> &LuaTypeIndex {
        &self.types_index
    }

    pub fn get_module_index(&self) -> &LuaModuleIndex {
        &self.modules_index
    }

    pub fn get_member_index(&self) -> &LuaMemberIndex {
        &self.members_index
    }

    pub fn get_property_index(&self) -> &LuaPropertyIndex {
        &self.property_index
    }

    pub fn get_signature_index(&self) -> &LuaSignatureIndex {
        &self.signature_index
    }

    pub fn get_diagnostic_index(&self) -> &DiagnosticIndex {
        &self.diagnostic_index
    }

    pub fn get_operator_index(&self) -> &LuaOperatorIndex {
        &self.operator_index
    }

    pub fn get_flow_index(&self) -> &LuaFlowIndex {
        &self.flow_index
    }

    pub fn get_accessor_func_index(&self) -> &AccessorFuncAnnotationIndex {
        &self.accessor_func_index
    }

    pub fn get_accessor_func_index_mut(&mut self) -> &mut AccessorFuncAnnotationIndex {
        &mut self.accessor_func_index
    }

    pub fn get_accessor_func_call_index(&self) -> &AccessorFuncCallIndex {
        &self.accessor_func_call_index
    }

    pub fn get_accessor_func_call_index_mut(&mut self) -> &mut AccessorFuncCallIndex {
        &mut self.accessor_func_call_index
    }

    pub fn get_call_site_param_index(&self) -> &CallSiteParamIndex {
        &self.call_site_param_index
    }

    pub fn get_call_site_param_index_mut(&mut self) -> &mut CallSiteParamIndex {
        &mut self.call_site_param_index
    }

    pub fn get_gmod_class_metadata_index(&self) -> &GmodClassMetadataIndex {
        &self.gmod_class_index
    }

    pub fn get_gmod_class_metadata_index_mut(&mut self) -> &mut GmodClassMetadataIndex {
        &mut self.gmod_class_index
    }

    pub fn get_gmod_infer_index(&self) -> &GmodInferIndex {
        &self.gmod_infer_index
    }

    pub fn get_gmod_infer_index_mut(&mut self) -> &mut GmodInferIndex {
        &mut self.gmod_infer_index
    }

    pub fn get_gmod_load_index(&self) -> &GmodLoadIndex {
        &self.gmod_load_index
    }

    pub fn get_gmod_load_index_mut(&mut self) -> &mut GmodLoadIndex {
        &mut self.gmod_load_index
    }

    pub fn get_gmod_network_index(&self) -> &GmodNetworkIndex {
        &self.gmod_network_index
    }

    pub fn get_gmod_network_index_mut(&mut self) -> &mut GmodNetworkIndex {
        &mut self.gmod_network_index
    }

    pub fn get_dynamic_field_index(&self) -> &DynamicFieldIndex {
        &self.dynamic_field_index
    }

    pub fn get_dynamic_field_index_mut(&mut self) -> &mut DynamicFieldIndex {
        &mut self.dynamic_field_index
    }

    pub fn get_vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn get_vfs_mut(&mut self) -> &mut Vfs {
        &mut self.vfs
    }

    pub fn get_file_dependencies_index(&self) -> &LuaDependencyIndex {
        &self.file_dependencies_index
    }

    pub fn get_file_dependencies_index_mut(&mut self) -> &mut LuaDependencyIndex {
        &mut self.file_dependencies_index
    }

    pub fn get_numeric_range_population_index(&self) -> &NumericRangePopulationIndex {
        &self.numeric_range_population_index
    }

    pub fn get_numeric_range_population_index_mut(&mut self) -> &mut NumericRangePopulationIndex {
        &mut self.numeric_range_population_index
    }

    pub fn get_global_index(&self) -> &LuaGlobalIndex {
        &self.global_index
    }

    pub fn get_global_index_mut(&mut self) -> &mut LuaGlobalIndex {
        &mut self.global_index
    }

    pub fn get_json_schema_index(&self) -> &JsonSchemaIndex {
        &self.json_schema_index
    }

    pub fn get_json_schema_index_mut(&mut self) -> &mut JsonSchemaIndex {
        &mut self.json_schema_index
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        self.vfs.update_config(config.clone());
        self.modules_index.update_config(config.clone());
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
}

impl LuaIndex for DbIndex {
    fn remove(&mut self, file_id: FileId) {
        self.decl_index.remove(file_id);
        self.references_index.remove(file_id);
        self.types_index.remove(file_id);
        self.modules_index.remove(file_id);
        self.members_index.remove(file_id);
        self.property_index.remove(file_id);
        self.signature_index.remove(file_id);
        self.diagnostic_index.remove(file_id);
        self.operator_index.remove(file_id);
        self.flow_index.remove(file_id);
        self.accessor_func_index.remove(file_id);
        self.accessor_func_call_index.remove(file_id);
        self.call_site_param_index.remove(file_id);
        self.gmod_class_index.remove(file_id);
        self.gmod_infer_index.remove(file_id);
        self.gmod_load_index.remove(file_id);
        self.gmod_network_index.remove(file_id);
        self.dynamic_field_index.remove(file_id);
        self.file_dependencies_index.remove(file_id);
        self.numeric_range_population_index.remove(file_id);
        self.metatable_index.remove(file_id);
        self.global_index.remove(file_id);
        self.json_schema_index.remove(file_id);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        if let [file_id] = file_ids {
            self.remove(*file_id);
            return;
        }

        self.decl_index.remove_files(file_ids);
        self.references_index.remove_files(file_ids);
        self.types_index.remove_files(file_ids);
        self.modules_index.remove_files(file_ids);
        self.members_index.remove_files(file_ids);
        self.property_index.remove_files(file_ids);
        self.signature_index.remove_files(file_ids);
        self.diagnostic_index.remove_files(file_ids);
        self.operator_index.remove_files(file_ids);
        self.flow_index.remove_files(file_ids);
        self.accessor_func_index.remove_files(file_ids);
        self.accessor_func_call_index.remove_files(file_ids);
        self.call_site_param_index.remove_files(file_ids);
        self.gmod_class_index.remove_files(file_ids);
        self.gmod_infer_index.remove_files(file_ids);
        self.gmod_load_index.remove_files(file_ids);
        self.gmod_network_index.remove_files(file_ids);
        self.dynamic_field_index.remove_files(file_ids);
        self.file_dependencies_index.remove_files(file_ids);
        for &file_id in file_ids {
            self.numeric_range_population_index.remove(file_id);
        }
        self.metatable_index.remove_files(file_ids);
        self.global_index.remove_files(file_ids);
        self.json_schema_index.remove_files(file_ids);
    }

    fn clear(&mut self) {
        self.decl_index.clear();
        self.references_index.clear();
        self.types_index.clear();
        self.modules_index.clear();
        self.members_index.clear();
        self.property_index.clear();
        self.signature_index.clear();
        self.diagnostic_index.clear();
        self.operator_index.clear();
        self.flow_index.clear();
        self.accessor_func_index.clear();
        self.accessor_func_call_index.clear();
        self.call_site_param_index.clear();
        self.gmod_class_index.clear();
        self.gmod_infer_index.clear();
        self.gmod_load_index.clear();
        self.gmod_network_index.clear();
        self.dynamic_field_index.clear();
        self.file_dependencies_index.clear();
        self.numeric_range_population_index.clear();
        self.metatable_index.clear();
        self.global_index.clear();
        self.json_schema_index.clear();
    }
}
