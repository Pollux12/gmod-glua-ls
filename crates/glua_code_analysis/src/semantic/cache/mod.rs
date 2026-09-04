mod cache_options;

pub use cache_options::{CacheOptions, LuaAnalysisPhase};
use glua_parser::LuaSyntaxId;
use internment::ArcIntern;
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use std::{collections::HashSet, sync::Arc};

use crate::{
    DbIndex, FileId, FlowId, GmodRealm, InFiled, LuaDeclId, LuaFunctionType, LuaInferredGuardOwner,
    LuaMemberId, LuaMemberKey, LuaSemanticDeclId, LuaSignatureId, VarRefId, VarRefRootId,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::{
        generic::FuncGenericBinding,
        infer::{InferFailReason, ParamInferenceSource},
    },
};

pub type FlowCacheInnerKey = (FlowId, GmodRealm, FlowOrigin);

/// The syntax-derived facts about one dynamic-field definition site.
///
/// Both are a pure function of the definition's file tree, so they are read
/// once per definition instead of re-walking from the root on every access
/// position that consults it.
#[derive(Debug, Clone, Copy, Default)]
pub struct DynamicFieldDefinitionSyntax {
    pub member_id: Option<LuaMemberId>,
    pub enclosing_assign_range: Option<TextRange>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub enum FlowOrigin {
    #[default]
    Real,
    NilCounterfactual,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VarRefCacheRootKey {
    Decl(LuaDeclId),
    Member(LuaMemberId),
    SelfRef(LuaDeclId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VarRefCacheKey {
    VarRef(LuaDeclId),
    SelfRef(LuaDeclId),
    IndexRef(VarRefCacheRootKey, ArcIntern<SmolStr>),
    GlobalName(ArcIntern<SmolStr>),
}

impl From<&VarRefRootId> for VarRefCacheRootKey {
    fn from(value: &VarRefRootId) -> Self {
        match value {
            VarRefRootId::Decl(decl_id) => Self::Decl(*decl_id),
            VarRefRootId::Member(member_id) => Self::Member(*member_id),
            VarRefRootId::SelfRef(self_ref_id) => Self::SelfRef(self_ref_id.self_decl_id),
        }
    }
}

impl From<&VarRefId> for VarRefCacheKey {
    fn from(value: &VarRefId) -> Self {
        match value {
            VarRefId::VarRef(decl_id) => Self::VarRef(*decl_id),
            VarRefId::SelfRef(self_ref_id) => Self::SelfRef(self_ref_id.self_decl_id),
            VarRefId::IndexRef(root, path) => Self::IndexRef(root.into(), path.clone()),
            VarRefId::GlobalName(name, _) => Self::GlobalName(name.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CacheEntry<T> {
    Ready,
    Cache(T),
    /// Cached error result — used during diagnostics to prevent recomputation
    /// of expressions whose type couldn't be resolved.
    Error(InferFailReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStrTplTypeDecl {
    pub file_id: FileId,
    pub source_range: TextRange,
    pub type_decl_id: LuaTypeDeclId,
    pub super_type: LuaType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericCallBindings {
    Known(Vec<FuncGenericBinding>),
    Ambiguous,
}

impl GenericCallBindings {
    pub fn record(&mut self, new_bindings: Vec<FuncGenericBinding>) {
        match self {
            GenericCallBindings::Known(existing) => {
                if existing != &new_bindings {
                    *self = GenericCallBindings::Ambiguous;
                }
            }
            GenericCallBindings::Ambiguous => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct LuaInferCache {
    file_id: FileId,
    config: CacheOptions,
    pub expr_cache: FxHashMap<LuaSyntaxId, CacheEntry<LuaType>>,
    pub call_cache:
        FxHashMap<(LuaSyntaxId, Option<usize>, LuaType), CacheEntry<Arc<LuaFunctionType>>>,
    pub call_arg_types_cache:
        FxHashMap<(LuaSyntaxId, Option<usize>), Arc<Vec<(LuaType, TextRange)>>>,
    pub generic_call_bindings: FxHashMap<LuaSyntaxId, GenericCallBindings>,
    pub flow_node_cache:
        FxHashMap<VarRefCacheKey, FxHashMap<FlowCacheInnerKey, CacheEntry<LuaType>>>,
    pub flow_query_realm: Option<GmodRealm>,
    /// Scratch memo for one top-level closure-baseline query. Without it that
    /// walk re-derives each merge point once per path into it.
    ///
    /// Cleared when the outermost baseline query returns: a baseline answer
    /// depends on how far the pass has got, which is not in the key, so one
    /// must never answer a later query.
    pub baseline_flow_memo: FxHashMap<(VarRefCacheKey, FlowCacheInnerKey), LuaType>,
    pub baseline_flow_depth: u32,
    pub flow_node_realm_cache: FxHashMap<FlowId, GmodRealm>,
    pub index_ref_origin_type_cache: FxHashMap<VarRefCacheKey, CacheEntry<LuaType>>,
    pub param_type_cache: FxHashMap<LuaDeclId, CacheEntry<LuaType>>,
    pub(crate) param_type_source_cache: FxHashMap<LuaDeclId, CacheEntry<ParamInferenceSource>>,
    pub expr_var_ref_id_cache: FxHashMap<LuaSyntaxId, VarRefId>,
    pub narrow_by_literal_stop_position_cache: HashSet<LuaSyntaxId>,
    pub scripted_global_singleton_type_cache: Option<FxHashMap<String, LuaTypeDeclId>>,
    pub pending_str_tpl_type_decls: Vec<PendingStrTplTypeDecl>,
    /// Cache for `self` type per enclosing method (keyed by LuaFuncStat syntax_id).
    /// Avoids repeated ancestor walks and type resolution for each `self` reference
    /// within the same method body.
    pub self_type_cache: FxHashMap<LuaSyntaxId, Option<LuaType>>,
    /// Region-aware base type seed for an implicit `self` flow query, set by
    /// `infer_self` for the duration of a single `infer_expr_narrow_type_with_self_base`
    /// call. When the flow walk reaches the origin for the matching `SelfRef`,
    /// this seed is used as the base type instead of the (position-insensitive)
    /// receiver decl/member cache, so reused locals resolve `self` per region
    /// while still going through the normal narrowing pipeline.
    pub self_base_seed: Option<(VarRefId, LuaType)>,
    /// Cache for `find_decl` results so that multiple diagnostic checkers
    /// processing the same file don't redo the full member-resolution chain.
    pub decl_cache: FxHashMap<LuaSyntaxId, Option<LuaSemanticDeclId>>,
    /// Cache for resolved generic-for variable types. For `pairs` loops over
    /// templated tables, each use of the loop value can otherwise re-run the
    /// full iterator inference from the enclosing `for` statement.
    pub for_range_iter_var_type_cache: FxHashMap<LuaDeclId, CacheEntry<LuaType>>,
    /// Cache for the in-place `ipairs` transform recogniser, keyed by the array
    /// local's declaration. `None` means the block holds no such transform loop;
    /// `Some((loop_end, element_type))` records where the transform completes and
    /// the element type it leaves, so a read is answered without re-scanning the
    /// declaration's block each time.
    pub in_place_ipairs_transform_cache: FxHashMap<LuaDeclId, Option<(TextSize, LuaType)>>,
    pub dynamic_field_scope_metatable_cache:
        FxHashMap<TextRange, FxHashMap<VarRefId, Vec<(TextRange, LuaType)>>>,
    pub dynamic_field_resolution_cache: FxHashMap<
        (LuaType, LuaMemberKey, Option<TextSize>),
        Option<(LuaType, Option<LuaSemanticDeclId>)>,
    >,
    pub local_class_table_member_ids_cache:
        FxHashMap<(LuaTypeDeclId, LuaMemberKey), Arc<Vec<LuaMemberId>>>,
    pub dynamic_field_type_cache: FxHashMap<LuaMemberId, Option<LuaType>>,
    pub dynamic_field_definition_syntax_cache:
        FxHashMap<InFiled<TextRange>, DynamicFieldDefinitionSyntax>,
    pub dynamic_field_resolving: HashSet<LuaMemberId>,
    pub vgui_parent_fallback_calls: FxHashSet<LuaSyntaxId>,
    /// `GetParent` reads answered *through* a resolved vgui parent chain.
    ///
    /// The mirror of [`Self::vgui_parent_fallback_calls`]: a chain answer taken
    /// before every group's relations landed can be one the final chain state
    /// contradicts, so the files holding these reads are re-derived alongside
    /// the fallback files once the chains settle.
    pub vgui_parent_chain_calls: FxHashSet<LuaSyntaxId>,
    /// Call sites of a local function, keyed by its declaration. Syntax ids,
    /// not nodes: red nodes are `!Send`.
    pub local_function_call_sites_cache: FxHashMap<LuaDeclId, Arc<Vec<LuaSyntaxId>>>,
    /// Whether a call diverges, keyed by the call expression. The flow walk asks
    /// this of every call node it reaches, and answering means resolving what
    /// the call targets through the reference, property, member and signature
    /// indexes.
    pub call_returns_never_cache: FxHashMap<LuaSyntaxId, bool>,
    /// Whether a call site's signature is generic, keyed by the call
    /// expression. Consulted per call-site during inference, so re-inferring
    /// the call prefix each time is avoided.
    pub signature_is_generic_cache: FxHashMap<LuaSyntaxId, Option<bool>>,
    /// How many computed-key reads were answered by merging the sibling
    /// members of a table. Each of those answers is a snapshot of whichever
    /// writers the batch had indexed, so a value bound from one is re-derived
    /// once the index settles. See `settled_sibling_merge_read_candidates`.
    pub sibling_merge_reads: u32,
    inferred_guard_dependencies: FxHashSet<LuaInferredGuardOwner>,
    inferred_return_reads:
        FxHashMap<LuaSignatureId, crate::db_index::read_set::InferredReturnReadRecord>,
    /// Member reads that found no member. A read that fails records no type
    /// cache and so no dependency edge, yet the file's answer changes the
    /// moment another file defines the member; these are what the analyzer
    /// files as that edge. See `LuaMemberIndex::set_missed_member_reads`.
    missed_member_reads: FxHashSet<(crate::LuaMemberOwner, LuaMemberKey)>,
}

impl LuaInferCache {
    pub fn new(file_id: FileId, config: CacheOptions) -> Self {
        Self {
            file_id,
            config,
            expr_cache: FxHashMap::default(),
            call_cache: FxHashMap::default(),
            call_arg_types_cache: FxHashMap::default(),
            flow_node_cache: FxHashMap::default(),
            flow_query_realm: None,
            baseline_flow_memo: FxHashMap::default(),
            baseline_flow_depth: 0,
            flow_node_realm_cache: FxHashMap::default(),
            index_ref_origin_type_cache: FxHashMap::default(),
            param_type_cache: FxHashMap::default(),
            param_type_source_cache: FxHashMap::default(),
            expr_var_ref_id_cache: FxHashMap::default(),
            narrow_by_literal_stop_position_cache: HashSet::default(),
            scripted_global_singleton_type_cache: None,
            pending_str_tpl_type_decls: Vec::new(),
            self_type_cache: FxHashMap::default(),
            self_base_seed: None,
            decl_cache: FxHashMap::default(),
            for_range_iter_var_type_cache: FxHashMap::default(),
            in_place_ipairs_transform_cache: FxHashMap::default(),
            dynamic_field_scope_metatable_cache: FxHashMap::default(),
            dynamic_field_resolution_cache: FxHashMap::default(),
            local_class_table_member_ids_cache: FxHashMap::default(),
            dynamic_field_type_cache: FxHashMap::default(),
            dynamic_field_definition_syntax_cache: FxHashMap::default(),
            dynamic_field_resolving: HashSet::default(),
            vgui_parent_fallback_calls: FxHashSet::default(),
            vgui_parent_chain_calls: FxHashSet::default(),
            local_function_call_sites_cache: FxHashMap::default(),
            call_returns_never_cache: FxHashMap::default(),
            signature_is_generic_cache: FxHashMap::default(),
            generic_call_bindings: FxHashMap::default(),
            sibling_merge_reads: 0,
            inferred_guard_dependencies: FxHashSet::default(),
            inferred_return_reads: FxHashMap::default(),
            missed_member_reads: FxHashSet::default(),
        }
    }

    pub fn get_config(&self) -> &CacheOptions {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut CacheOptions {
        &mut self.config
    }

    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    pub fn set_phase(&mut self, phase: LuaAnalysisPhase) {
        self.config.analysis_phase = phase;
    }

    pub fn record_generic_call_bindings(
        &mut self,
        syntax_id: LuaSyntaxId,
        bindings: Vec<FuncGenericBinding>,
    ) {
        match self.generic_call_bindings.entry(syntax_id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().record(bindings);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(GenericCallBindings::Known(bindings));
            }
        }
    }

    pub fn get_generic_call_bindings(
        &self,
        syntax_id: &LuaSyntaxId,
    ) -> Option<&[FuncGenericBinding]> {
        match self.generic_call_bindings.get(syntax_id) {
            Some(GenericCallBindings::Known(bindings)) => Some(bindings),
            _ => None,
        }
    }

    pub fn add_pending_str_tpl_type_decl(
        &mut self,
        source_range: TextRange,
        type_decl_id: LuaTypeDeclId,
        super_type: LuaType,
    ) {
        let pending = PendingStrTplTypeDecl {
            file_id: self.file_id,
            source_range,
            type_decl_id,
            super_type,
        };

        if !self
            .pending_str_tpl_type_decls
            .iter()
            .any(|exist| exist == &pending)
        {
            self.pending_str_tpl_type_decls.push(pending);
        }
    }

    pub fn take_pending_str_tpl_type_decls(&mut self) -> Vec<PendingStrTplTypeDecl> {
        std::mem::take(&mut self.pending_str_tpl_type_decls)
    }

    pub fn add_inferred_guard_dependency(&mut self, owner: LuaInferredGuardOwner) {
        self.inferred_guard_dependencies.insert(owner);
    }

    pub fn take_inferred_guard_dependencies(&mut self) -> FxHashSet<LuaInferredGuardOwner> {
        std::mem::take(&mut self.inferred_guard_dependencies)
    }

    pub(crate) fn record_inferred_return_reads(
        &mut self,
        signature_id: LuaSignatureId,
        record: crate::db_index::read_set::InferredReturnReadRecord,
    ) {
        self.inferred_return_reads.insert(signature_id, record);
    }

    pub(crate) fn take_inferred_return_reads(
        &mut self,
    ) -> FxHashMap<LuaSignatureId, crate::db_index::read_set::InferredReturnReadRecord> {
        std::mem::take(&mut self.inferred_return_reads)
    }

    pub fn record_missed_member_read(&mut self, owner: crate::LuaMemberOwner, key: LuaMemberKey) {
        crate::db_index::read_set::record_missing_member_slot(&owner, &key);
        self.missed_member_reads.insert((owner, key));
    }

    pub fn take_missed_member_reads(&mut self) -> FxHashSet<(crate::LuaMemberOwner, LuaMemberKey)> {
        std::mem::take(&mut self.missed_member_reads)
    }

    pub fn clear(&mut self) {
        self.expr_cache.clear();
        self.call_cache.clear();
        self.call_arg_types_cache.clear();
        self.flow_node_cache.clear();
        self.flow_query_realm = None;
        self.flow_node_realm_cache.clear();
        self.index_ref_origin_type_cache.clear();
        self.param_type_cache.clear();
        self.param_type_source_cache.clear();
        self.expr_var_ref_id_cache.clear();
        self.narrow_by_literal_stop_position_cache.clear();
        self.scripted_global_singleton_type_cache = None;
        self.pending_str_tpl_type_decls.clear();
        self.self_type_cache.clear();
        self.self_base_seed = None;
        self.decl_cache.clear();
        self.for_range_iter_var_type_cache.clear();
        self.in_place_ipairs_transform_cache.clear();
        self.dynamic_field_scope_metatable_cache.clear();
        self.dynamic_field_resolution_cache.clear();
        self.local_class_table_member_ids_cache.clear();
        self.dynamic_field_type_cache.clear();
        self.dynamic_field_definition_syntax_cache.clear();
        self.dynamic_field_resolving.clear();
        self.vgui_parent_fallback_calls.clear();
        self.vgui_parent_chain_calls.clear();
        self.call_returns_never_cache.clear();
        self.signature_is_generic_cache.clear();
        self.generic_call_bindings.clear();
    }

    /// Drops every narrowing answer this file has memoised.
    ///
    /// A *successful* flow answer survives
    /// [`Self::clear_deferred_inference_results`], and narrowing a name reads
    /// the type of whatever it was derived from — so once a value the walk read
    /// as "not determined yet" becomes known, every answer that read it is
    /// void, whichever variable it happens to be keyed under. There is no way
    /// to drop only the ones that read it: the key names what was narrowed, not
    /// what the narrowing consulted.
    pub fn clear_flow_results(&mut self) {
        self.flow_node_cache.clear();
        self.flow_query_realm = None;
        self.index_ref_origin_type_cache.clear();
    }

    /// Drops the narrowing answers that a settling value can have invalidated.
    ///
    /// An answer that came back undetermined is one the walk could not pin
    /// down, so it is exactly what a value becoming known makes wrong. An
    /// answer that determined something did not read that value as unsettled,
    /// and those are the expensive ones to rebuild — a fifth of a cold index if
    /// the whole cache goes.
    pub fn clear_undetermined_flow_results(&mut self) {
        fn settled(entry: &CacheEntry<LuaType>) -> bool {
            match entry {
                CacheEntry::Cache(typ) => !crate::db_index::is_undetermined_type(typ),
                _ => false,
            }
        }
        self.flow_node_cache.retain(|_, inner| {
            inner.retain(|_, entry| settled(entry));
            !inner.is_empty()
        });
        self.index_ref_origin_type_cache
            .retain(|_, entry| settled(entry));
    }

    /// Discards what a `for ... in pairs(t)` answer was built from, so it can be
    /// taken again against a member map that has since grown.
    pub fn clear_iter_var_results(&mut self) {
        self.clear_deferred_inference_results();
        self.for_range_iter_var_type_cache.clear();
    }

    /// Discards the inference a wave of deferred resolution can have
    /// invalidated.
    pub fn clear_deferred_inference_results(&mut self) {
        self.expr_cache.clear();
        self.call_cache.clear();
        self.call_arg_types_cache.clear();
        // A resolved signature return is exactly what turns this answer from
        // `false` to `true`, so it cannot survive a wave.
        self.call_returns_never_cache.clear();
        self.signature_is_generic_cache.clear();
        self.generic_call_bindings.clear();
        self.flow_node_cache.retain(|_, inner| {
            inner.retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
            !inner.is_empty()
        });
        self.param_type_cache
            .retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
        self.param_type_source_cache
            .retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
        self.for_range_iter_var_type_cache
            .retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
        // The transform's element type comes from inferring the loop body's
        // right-hand side, which a resolved signature return can change.
        self.in_place_ipairs_transform_cache.clear();
    }

    /// Clears inference results that can become stale as deferred declarations,
    /// members, signatures, and dynamic fields are resolved. Structural caches
    /// derived only from the immutable syntax/reference/flow indexes survive the
    /// phase transition so unresolve does not rebuild them for every expression.
    pub fn clear_for_unresolve(&mut self, db: &DbIndex) {
        self.expr_cache.clear();
        self.call_cache.clear();
        self.call_arg_types_cache.clear();
        self.flow_node_cache.clear();
        self.flow_query_realm = None;
        self.index_ref_origin_type_cache.clear();
        self.param_type_cache.clear();
        self.param_type_source_cache.clear();
        self.call_returns_never_cache.clear();
        self.signature_is_generic_cache.clear();
        self.generic_call_bindings.clear();
        // Local reference identities come directly from immutable reference
        // indexes and are safe to retain. Global/member/self roots can be
        // selected through types and overloads that unresolve is about to
        // materialize, so discard only those semantic entries.
        self.expr_var_ref_id_cache
            .retain(|syntax_id, var_ref_id| var_ref_is_phase_stable(db, syntax_id, var_ref_id));
        self.narrow_by_literal_stop_position_cache.clear();
        self.pending_str_tpl_type_decls.clear();
        self.self_type_cache.clear();
        self.self_base_seed = None;
        self.decl_cache.clear();
        self.for_range_iter_var_type_cache.clear();
        self.in_place_ipairs_transform_cache.clear();
        self.dynamic_field_scope_metatable_cache.clear();
        self.dynamic_field_resolution_cache.clear();
        self.local_class_table_member_ids_cache.clear();
        self.dynamic_field_type_cache.clear();
        self.dynamic_field_definition_syntax_cache.clear();
        self.dynamic_field_resolving.clear();
        self.vgui_parent_fallback_calls.clear();
        self.vgui_parent_chain_calls.clear();
    }

    pub fn get_flow_cache(
        &self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
    ) -> Option<&CacheEntry<LuaType>> {
        self.get_flow_cache_with_origin(var_ref_id, flow_id, query_realm, FlowOrigin::Real)
    }

    pub fn get_flow_cache_with_origin(
        &self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
    ) -> Option<&CacheEntry<LuaType>> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.flow_node_cache
            .get(&cache_key)
            .and_then(|by_flow| by_flow.get(&(flow_id, query_realm, origin)))
    }

    pub fn set_flow_cache(
        &mut self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
        entry: CacheEntry<LuaType>,
    ) {
        self.set_flow_cache_with_origin(var_ref_id, flow_id, query_realm, FlowOrigin::Real, entry);
    }

    pub fn set_flow_cache_with_origin(
        &mut self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
        entry: CacheEntry<LuaType>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.flow_node_cache
            .entry(cache_key)
            .or_default()
            .insert((flow_id, query_realm, origin), entry);
    }

    pub fn take_flow_cache_for_var_ref(
        &mut self,
        var_ref_id: &VarRefId,
    ) -> Option<FxHashMap<FlowCacheInnerKey, CacheEntry<LuaType>>> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.flow_node_cache.remove(&cache_key)
    }

    pub fn restore_flow_cache_for_var_ref(
        &mut self,
        var_ref_id: &VarRefId,
        previous: Option<FxHashMap<FlowCacheInnerKey, CacheEntry<LuaType>>>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        if let Some(previous) = previous {
            self.flow_node_cache.insert(cache_key, previous);
        } else {
            self.flow_node_cache.remove(&cache_key);
        }
    }

    pub fn get_index_ref_origin_type_cache(
        &self,
        var_ref_id: &VarRefId,
    ) -> Option<&CacheEntry<LuaType>> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.index_ref_origin_type_cache.get(&cache_key)
    }

    pub fn set_index_ref_origin_type_cache(
        &mut self,
        var_ref_id: &VarRefId,
        entry: CacheEntry<LuaType>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.index_ref_origin_type_cache.insert(cache_key, entry);
    }

    pub fn replace_index_ref_origin_type_cache(
        &mut self,
        var_ref_id: &VarRefId,
        entry: CacheEntry<LuaType>,
    ) -> Option<CacheEntry<LuaType>> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.index_ref_origin_type_cache.insert(cache_key, entry)
    }

    pub fn restore_index_ref_origin_type_cache(
        &mut self,
        var_ref_id: &VarRefId,
        previous: Option<CacheEntry<LuaType>>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        if let Some(previous) = previous {
            self.index_ref_origin_type_cache.insert(cache_key, previous);
        } else {
            self.index_ref_origin_type_cache.remove(&cache_key);
        }
    }

    pub fn flow_cache_entry_count(&self) -> usize {
        self.flow_node_cache
            .values()
            .map(|entries| entries.len())
            .sum()
    }
}

fn var_ref_is_phase_stable(db: &DbIndex, syntax_id: &LuaSyntaxId, var_ref_id: &VarRefId) -> bool {
    let local_decl_is_stable = |decl_id: &LuaDeclId| {
        db.get_decl_index()
            .get_decl(decl_id)
            .is_some_and(|decl| decl.is_local())
    };

    match var_ref_id {
        VarRefId::VarRef(decl_id) => local_decl_is_stable(decl_id),
        VarRefId::IndexRef(VarRefRootId::Decl(decl_id), _) => {
            // A call annotated as rawget can also produce an IndexRef rooted at
            // a local, but that identity depends on its resolved signature.
            // Only a syntactic index expression is structural across unresolve.
            syntax_id.get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
                && local_decl_is_stable(decl_id)
        }
        VarRefId::SelfRef(_)
        | VarRefId::IndexRef(VarRefRootId::Member(_) | VarRefRootId::SelfRef(_), _)
        | VarRefId::GlobalName(_, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use internment::ArcIntern;
    use rowan::{TextRange, TextSize};
    use smol_str::SmolStr;

    use super::{CacheEntry, LuaInferCache};
    use crate::{
        DbIndex, FileId, FlowId, GmodRealm, LuaDecl, LuaDeclExtra, LuaDeclId, LuaDeclarationTree,
        LuaType, VarRefId, VarRefRootId,
    };

    #[test]
    fn clear_for_unresolve_preserves_only_structural_lookup_caches() {
        let file_id = FileId { id: 7 };
        let syntax_id = LuaSyntaxId::new(
            LuaSyntaxKind::NameExpr.into(),
            TextRange::new(TextSize::from(3), TextSize::from(4)),
        );
        let decl_id = LuaDeclId::new(file_id, TextSize::from(3));
        let mut cache = LuaInferCache::new(file_id, Default::default());

        cache
            .expr_cache
            .insert(syntax_id, CacheEntry::Cache(LuaType::Integer));
        cache
            .expr_var_ref_id_cache
            .insert(syntax_id, VarRefId::VarRef(decl_id));
        cache
            .flow_node_realm_cache
            .insert(FlowId(1), GmodRealm::Server);
        cache
            .narrow_by_literal_stop_position_cache
            .insert(syntax_id);

        cache.clear_for_unresolve(&DbIndex::new());

        assert!(cache.expr_cache.is_empty());
        assert!(cache.expr_var_ref_id_cache.is_empty());
        assert_eq!(
            cache.flow_node_realm_cache.get(&FlowId(1)),
            Some(&GmodRealm::Server)
        );
        assert!(cache.narrow_by_literal_stop_position_cache.is_empty());
    }

    #[test]
    fn clear_discards_literal_narrowing_stop_markers() {
        let file_id = FileId { id: 7 };
        let syntax_id = LuaSyntaxId::new(
            LuaSyntaxKind::IndexExpr.into(),
            TextRange::new(TextSize::from(3), TextSize::from(9)),
        );
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache
            .narrow_by_literal_stop_position_cache
            .insert(syntax_id);

        cache.clear();

        assert!(cache.narrow_by_literal_stop_position_cache.is_empty());
    }

    #[test]
    fn clear_for_unresolve_discards_semantic_call_expression_var_refs() {
        let file_id = FileId { id: 7 };
        let local_range = TextRange::new(TextSize::from(3), TextSize::from(4));
        let local_decl = LuaDecl::new(
            "value",
            file_id,
            local_range,
            LuaDeclExtra::Local {
                kind: LuaSyntaxKind::LocalName.into(),
                attrib: None,
            },
            None,
        );
        let decl_id = local_decl.get_id();
        let mut decl_tree = LuaDeclarationTree::new(file_id);
        decl_tree.add_decl(local_decl);
        let mut db = DbIndex::new();
        db.get_decl_index_mut().add_decl_tree(decl_tree);

        let call_expr_id = LuaSyntaxId::new(
            LuaSyntaxKind::CallExpr.into(),
            TextRange::new(TextSize::from(10), TextSize::from(20)),
        );
        let index_expr_id = LuaSyntaxId::new(
            LuaSyntaxKind::IndexExpr.into(),
            TextRange::new(TextSize::from(21), TextSize::from(30)),
        );
        let index_ref = VarRefId::IndexRef(
            VarRefRootId::Decl(decl_id),
            ArcIntern::new(SmolStr::new("value.key")),
        );
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache
            .expr_var_ref_id_cache
            .insert(call_expr_id, index_ref.clone());
        cache
            .expr_var_ref_id_cache
            .insert(index_expr_id, index_ref.clone());

        cache.clear_for_unresolve(&db);

        assert!(!cache.expr_var_ref_id_cache.contains_key(&call_expr_id));
        assert_eq!(
            cache.expr_var_ref_id_cache.get(&index_expr_id),
            Some(&index_ref)
        );
    }
}
