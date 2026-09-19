mod accessor_func;
mod call_site_param;
mod declaration;
mod dependency;
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
pub(crate) mod op_log;
mod operators;
mod property;
mod reference;
mod schema;
mod semantic_decl;
mod signature;
mod traits;
mod r#type;
pub(crate) use intern::intern_smol_str;
pub(crate) use r#type::read_set;

use rustc_hash::{FxHashMap, FxHashSet};
use std::{path::PathBuf, sync::Arc};

use self::read_set::WriteKey;
use crate::{Emmyrc, FileId, Vfs, profile::Profile};
pub use accessor_func::*;
pub use call_site_param::{CallSiteParamIndex, CallSiteSourceId};
pub(crate) use call_site_param::{CallSiteReturnConsumer, CallSiteReturnConsumerTarget};
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
/// free. Forks are scoped to the walk and dropped before merge; mutation entry
/// points outside the walk rely on there being no live fork.
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
    emmyrc: Arc<Emmyrc>,
    /// Revision-keyed cache for workspace-wide derived structures that are pure
    /// functions of VFS content (currently the gmod net-helper registry). Stored
    /// type-erased so `db_index` stays decoupled from the analyzer crate layer.
    /// Invalidated automatically by comparing `Vfs::content_revision`.
    helper_registry_cache: RevisionedCache,
    file_helper_scan_cache: FxHashMap<FileId, Arc<dyn std::any::Any + Send + Sync>>,
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
            emmyrc: Arc::new(Emmyrc::default()),
            helper_registry_cache: RevisionedCache::default(),
            file_helper_scan_cache: FxHashMap::default(),
        }
    }

    /// One file's contribution to the gmod net-helper scan.
    pub fn get_cached_file_helper_scan<T: std::any::Any + Send + Sync>(
        &self,
        file_id: FileId,
    ) -> Option<Arc<T>> {
        op_log::note_untracked_read(op_log::UntrackedDomain::HelperCache);
        self.file_helper_scan_cache
            .get(&file_id)
            .and_then(|value| value.clone().downcast::<T>().ok())
    }

    pub fn set_cached_file_helper_scan<T: std::any::Any + Send + Sync>(
        &mut self,
        file_id: FileId,
        value: Arc<T>,
    ) {
        if op_log::recording() {
            let value2 = value.clone();
            op_log::push_one(
                op_log::Domain::HelperCache,
                WriteKey::HelperCache(file_id),
                move |db| db.set_cached_file_helper_scan(file_id, value2),
            );
        }
        self.file_helper_scan_cache.insert(file_id, value);
    }

    /// Fetch the cached gmod net-helper registry if it was built at `revision`.
    /// Type-erased so the `db_index` layer need not name the analyzer's registry
    /// type; the caller downcasts to its concrete `T`.
    pub fn get_cached_helper_registry<T: std::any::Any + Send + Sync>(
        &self,
        revision: u64,
    ) -> Option<Arc<T>> {
        op_log::note_untracked_read(op_log::UntrackedDomain::HelperCache);
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
        if op_log::recording() {
            let value2 = value.clone();
            op_log::push_one(
                op_log::Domain::HelperCache,
                WriteKey::HelperRegistry,
                move |db| db.set_cached_helper_registry(revision, value2),
            );
        }
        self.helper_registry_cache.0 = Some((revision, value));
    }

    pub fn remove_index(&mut self, mut file_ids: Vec<FileId>) {
        file_ids.sort_by_key(|file_id| file_id.id);
        file_ids.dedup();
        if file_ids.is_empty() {
            return;
        }

        let _profile = Profile::cond_new("remove indexes", file_ids.len() > 1);
        // Same reasoning as in `clear`: both halves of the revision key can
        // return to a previously cached value after a remove + update while the
        // underlying index has changed.
        self.helper_registry_cache = RevisionedCache::default();
        for &file_id in &file_ids {
            self.file_helper_scan_cache.remove(&file_id);
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
        op_log::note_domain_touch(op_log::Domain::Metatable);
        Arc::make_mut(&mut self.metatable_index)
    }

    /// Reads record their own keys on `LuaMetatableIndex`'s methods — see
    /// `get_operator_index`.
    pub fn get_metatable_index(&self) -> &LuaMetatableIndex {
        self.metatable_index.as_ref()
    }

    pub fn get_decl_index_mut(&mut self) -> &mut LuaDeclIndex {
        op_log::note_domain_touch(op_log::Domain::Decl);
        Arc::make_mut(&mut self.decl_index)
    }

    pub fn get_reference_index_mut(&mut self) -> &mut LuaReferenceIndex {
        op_log::note_domain_touch(op_log::Domain::Reference);
        Arc::make_mut(&mut self.references_index)
    }

    /// Rebuilds the inference-derived state left stale by a batch of fact
    /// bindings. See [`LuaTypeIndex::flush_inference_derived_state`].
    pub fn flush_inference_derived_state(&mut self) {
        op_log::note_domain_touch(op_log::Domain::Type);
        Arc::make_mut(&mut self.types_index).flush_inference_derived_state();
    }

    pub fn get_type_index_mut(&mut self) -> &mut LuaTypeIndex {
        op_log::note_domain_touch(op_log::Domain::Type);
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
        op_log::note_domain_touch(op_log::Domain::Type);
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
        Arc::make_mut(&mut self.types_index).flush_inference_derived_state();
        changed_files
    }

    pub fn get_module_index_mut(&mut self) -> &mut LuaModuleIndex {
        op_log::note_domain_touch(op_log::Domain::Module);
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.modules_index)
    }

    #[track_caller]
    pub fn get_member_index_mut(&mut self) -> &mut LuaMemberIndex {
        op_log::note_domain_touch(op_log::Domain::Member);
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.members_index)
    }

    #[track_caller]
    pub fn get_property_index_mut(&mut self) -> &mut LuaPropertyIndex {
        op_log::note_domain_touch(op_log::Domain::Property);
        Arc::make_mut(&mut self.property_index)
    }

    pub fn get_signature_index_mut(&mut self) -> &mut LuaSignatureIndex {
        op_log::note_domain_touch(op_log::Domain::Signature);
        self.type_structure_revision = next_type_structure_revision();
        Arc::make_mut(&mut self.signature_index)
    }

    pub fn get_diagnostic_index_mut(&mut self) -> &mut DiagnosticIndex {
        op_log::note_domain_touch(op_log::Domain::Diagnostic);
        Arc::make_mut(&mut self.diagnostic_index)
    }

    pub fn get_operator_index_mut(&mut self) -> &mut LuaOperatorIndex {
        op_log::note_domain_touch(op_log::Domain::Operator);
        Arc::make_mut(&mut self.operator_index)
    }

    pub fn get_flow_index_mut(&mut self) -> &mut LuaFlowIndex {
        op_log::note_domain_touch(op_log::Domain::Flow);
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

    /// The current write counters for the read-set domains without per-key
    /// clocks, in [`read_set::UntrackedDomain`] order. The counters live on
    /// the owning indexes — `TypeDecl` and `GmodMeta` each span two of them
    /// (the gmod index's `vgui_forwarding_parents` and the signature index's
    /// `receiver_out_param_member_names` feed the same readers), so those
    /// slots sum. Instance-scoped: a write on another `DbIndex` cannot
    /// stale this instance's readers.
    pub(crate) fn untracked_write_epochs(&self) -> [u64; read_set::UNTRACKED_DOMAIN_COUNT] {
        [
            self.types_index.type_decl_writes + self.gmod_class_index.type_decl_writes,
            self.signature_index.gmod_meta_writes + self.gmod_class_index.gmod_meta_writes,
        ]
    }

    pub fn get_module_index(&self) -> &LuaModuleIndex {
        self.modules_index.as_ref()
    }

    pub fn get_member_index(&self) -> &LuaMemberIndex {
        self.members_index.as_ref()
    }

    pub fn get_property_index(&self) -> &LuaPropertyIndex {
        op_log::note_untracked_read(op_log::UntrackedDomain::Property);
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
        op_log::note_domain_touch(op_log::Domain::AccessorFunc);
        Arc::make_mut(&mut self.accessor_func_index)
    }

    pub fn get_accessor_func_call_index(&self) -> &AccessorFuncCallIndex {
        self.accessor_func_call_index.as_ref()
    }

    pub fn get_accessor_func_call_index_mut(&mut self) -> &mut AccessorFuncCallIndex {
        op_log::note_domain_touch(op_log::Domain::AccessorFuncCall);
        Arc::make_mut(&mut self.accessor_func_call_index)
    }

    pub fn get_call_site_param_index(&self) -> &CallSiteParamIndex {
        self.call_site_param_index.as_ref()
    }

    pub fn get_call_site_param_index_mut(&mut self) -> &mut CallSiteParamIndex {
        op_log::note_domain_touch(op_log::Domain::CallSiteParam);
        Arc::make_mut(&mut self.call_site_param_index)
    }

    /// Reads record their own keys on `GmodClassMetadataIndex`'s methods —
    /// see `get_operator_index`.
    pub fn get_gmod_class_metadata_index(&self) -> &GmodClassMetadataIndex {
        self.gmod_class_index.as_ref()
    }

    pub fn get_gmod_class_metadata_index_mut(&mut self) -> &mut GmodClassMetadataIndex {
        op_log::note_domain_touch(op_log::Domain::GmodClass);
        Arc::make_mut(&mut self.gmod_class_index)
    }

    pub fn get_gmod_infer_index(&self) -> &GmodInferIndex {
        op_log::note_untracked_read(op_log::UntrackedDomain::GmodInfer);
        self.gmod_infer_index.as_ref()
    }

    pub fn get_gmod_infer_index_mut(&mut self) -> &mut GmodInferIndex {
        op_log::note_domain_touch(op_log::Domain::GmodInfer);
        Arc::make_mut(&mut self.gmod_infer_index)
    }

    pub fn get_gmod_load_index(&self) -> &GmodLoadIndex {
        self.gmod_load_index.as_ref()
    }

    pub fn get_gmod_load_index_mut(&mut self) -> &mut GmodLoadIndex {
        op_log::note_domain_touch(op_log::Domain::GmodLoad);
        Arc::make_mut(&mut self.gmod_load_index)
    }

    pub fn get_gmod_network_index(&self) -> &GmodNetworkIndex {
        self.gmod_network_index.as_ref()
    }

    pub fn get_gmod_network_index_mut(&mut self) -> &mut GmodNetworkIndex {
        op_log::note_domain_touch(op_log::Domain::GmodNetwork);
        Arc::make_mut(&mut self.gmod_network_index)
    }

    pub fn get_dynamic_field_index(&self) -> &DynamicFieldIndex {
        self.dynamic_field_index.as_ref()
    }

    pub fn get_dynamic_field_index_mut(&mut self) -> &mut DynamicFieldIndex {
        op_log::note_domain_touch(op_log::Domain::DynamicField);
        Arc::make_mut(&mut self.dynamic_field_index)
    }

    pub fn get_vfs(&self) -> &Vfs {
        self.vfs.as_ref()
    }

    pub fn get_vfs_mut(&mut self) -> &mut Vfs {
        // Vfs is not `Clone`, so `make_mut` is unavailable; forks only exist
        // inside the parallel lua-analyze walk where the vfs is read-only, so
        // a strong count above one here would mean a fork outlived the walk.
        Arc::get_mut(&mut self.vfs).expect("vfs mutation while a DbIndex fork is live")
    }

    pub fn get_file_dependencies_index(&self) -> &LuaDependencyIndex {
        self.file_dependencies_index.as_ref()
    }

    pub fn get_file_dependencies_index_mut(&mut self) -> &mut LuaDependencyIndex {
        op_log::note_domain_touch(op_log::Domain::Dependency);
        Arc::make_mut(&mut self.file_dependencies_index)
    }

    pub fn get_numeric_range_population_index(&self) -> &NumericRangePopulationIndex {
        self.numeric_range_population_index.as_ref()
    }

    pub fn get_numeric_range_population_index_mut(&mut self) -> &mut NumericRangePopulationIndex {
        op_log::note_domain_touch(op_log::Domain::NumericRange);
        Arc::make_mut(&mut self.numeric_range_population_index)
    }

    pub fn get_global_index(&self) -> &LuaGlobalIndex {
        self.global_index.as_ref()
    }

    pub fn get_global_index_mut(&mut self) -> &mut LuaGlobalIndex {
        op_log::note_domain_touch(op_log::Domain::Global);
        Arc::make_mut(&mut self.global_index)
    }

    pub fn get_json_schema_index(&self) -> &JsonSchemaIndex {
        self.json_schema_index.as_ref()
    }

    pub fn get_json_schema_index_mut(&mut self) -> &mut JsonSchemaIndex {
        op_log::note_domain_touch(op_log::Domain::JsonSchema);
        Arc::make_mut(&mut self.json_schema_index)
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        Arc::get_mut(&mut self.vfs)
            .expect("vfs mutation while a DbIndex fork is live")
            .update_config(config.clone());
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
    /// plus one clone per domain actually written. The vfs is shared
    /// read-only: `get_vfs_mut` panics while any fork is live.
    ///
    /// A fork must not outlive the walk that created it — mutation entry
    /// points outside the walk assume exclusive ownership.
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
            emmyrc: Arc::clone(&self.emmyrc),
            helper_registry_cache: RevisionedCache(
                self.helper_registry_cache
                    .0
                    .as_ref()
                    .map(|(rev, value)| (*rev, Arc::clone(value))),
            ),
            file_helper_scan_cache: self.file_helper_scan_cache.clone(),
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
        // Derived caches have to go with the facts they were derived from.
        // `clear` does not touch the VFS, so `content_revision` and the
        // signature count both return to their old values once the same file
        // set is re-indexed — the revision key would hand back a registry built
        // against the discarded index.
        self.helper_registry_cache = RevisionedCache::default();
        self.file_helper_scan_cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::DbIndex;
    use crate::FileId;

    #[test]
    fn remove_index_resets_the_helper_registry_cache() {
        let mut db = DbIndex::new();
        let revision = 7u64;
        db.set_cached_helper_registry(revision, Arc::new(42u32));
        assert_eq!(
            db.get_cached_helper_registry::<u32>(revision).as_deref(),
            Some(&42)
        );

        db.remove_index(vec![FileId::new(1)]);

        assert!(db.get_cached_helper_registry::<u32>(revision).is_none());
    }
}
