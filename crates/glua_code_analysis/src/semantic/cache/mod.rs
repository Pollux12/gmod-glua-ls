mod cache_options;

pub use cache_options::{CacheOptions, LuaAnalysisPhase};
use glua_parser::LuaSyntaxId;
use internment::ArcIntern;
use rowan::{TextRange, TextSize};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::{collections::HashSet, sync::Arc};

use crate::{
    FileId, FlowId, GmodRealm, LuaDeclId, LuaFunctionType, LuaMemberId, LuaMemberKey,
    LuaSemanticDeclId, VarRefId, VarRefRootId,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::infer::InferFailReason,
};

type FlowCacheInnerKey = (FlowId, GmodRealm, FlowOrigin);

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
    pub type_decl_id: LuaTypeDeclId,
    pub super_type: LuaType,
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
    pub flow_node_cache:
        FxHashMap<VarRefCacheKey, FxHashMap<FlowCacheInnerKey, CacheEntry<LuaType>>>,
    pub flow_query_realm: Option<GmodRealm>,
    pub flow_node_realm_cache: FxHashMap<FlowId, GmodRealm>,
    pub index_ref_origin_type_cache: FxHashMap<VarRefCacheKey, CacheEntry<LuaType>>,
    pub param_type_cache: FxHashMap<LuaDeclId, CacheEntry<LuaType>>,
    pub expr_var_ref_id_cache: FxHashMap<LuaSyntaxId, VarRefId>,
    pub narrow_by_literal_stop_position_cache: HashSet<LuaSyntaxId>,
    pub scoped_scripted_global_cache: Option<Option<(String, String)>>,
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
    pub local_reassignment_positions_cache: FxHashMap<LuaDeclId, Vec<TextSize>>,
    pub local_reassignments_indexed: bool,
    pub dynamic_field_scope_metatable_cache:
        FxHashMap<TextRange, FxHashMap<VarRefId, Vec<(TextRange, LuaType)>>>,
    pub dynamic_field_resolution_cache: FxHashMap<
        (LuaType, LuaMemberKey, Option<TextSize>),
        Option<(LuaType, Option<LuaSemanticDeclId>)>,
    >,
    pub dynamic_field_type_cache: FxHashMap<LuaMemberId, Option<LuaType>>,
    pub dynamic_field_resolving: HashSet<LuaMemberId>,
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
            flow_node_realm_cache: FxHashMap::default(),
            index_ref_origin_type_cache: FxHashMap::default(),
            param_type_cache: FxHashMap::default(),
            expr_var_ref_id_cache: FxHashMap::default(),
            narrow_by_literal_stop_position_cache: HashSet::new(),
            scoped_scripted_global_cache: None,
            pending_str_tpl_type_decls: Vec::new(),
            self_type_cache: FxHashMap::default(),
            self_base_seed: None,
            decl_cache: FxHashMap::default(),
            for_range_iter_var_type_cache: FxHashMap::default(),
            local_reassignment_positions_cache: FxHashMap::default(),
            local_reassignments_indexed: false,
            dynamic_field_scope_metatable_cache: FxHashMap::default(),
            dynamic_field_resolution_cache: FxHashMap::default(),
            dynamic_field_type_cache: FxHashMap::default(),
            dynamic_field_resolving: HashSet::new(),
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

    pub fn add_pending_str_tpl_type_decl(
        &mut self,
        type_decl_id: LuaTypeDeclId,
        super_type: LuaType,
    ) {
        let pending = PendingStrTplTypeDecl {
            file_id: self.file_id,
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

    pub fn clear(&mut self) {
        self.expr_cache.clear();
        self.call_cache.clear();
        self.call_arg_types_cache.clear();
        self.flow_node_cache.clear();
        self.flow_query_realm = None;
        self.flow_node_realm_cache.clear();
        self.index_ref_origin_type_cache.clear();
        self.param_type_cache.clear();
        self.expr_var_ref_id_cache.clear();
        self.scoped_scripted_global_cache = None;
        self.pending_str_tpl_type_decls.clear();
        self.self_type_cache.clear();
        self.self_base_seed = None;
        self.decl_cache.clear();
        self.for_range_iter_var_type_cache.clear();
        self.local_reassignment_positions_cache.clear();
        self.local_reassignments_indexed = false;
        self.dynamic_field_scope_metatable_cache.clear();
        self.dynamic_field_resolution_cache.clear();
        self.dynamic_field_type_cache.clear();
        self.dynamic_field_resolving.clear();
    }

    /// Clears all caches. Used before the unresolve phase.
    pub fn clear_for_unresolve(&mut self) {
        self.clear();
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
