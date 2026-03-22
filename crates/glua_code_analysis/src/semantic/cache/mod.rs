mod cache_options;

pub use cache_options::{CacheOptions, LuaAnalysisPhase};
use glua_parser::LuaSyntaxId;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    FileId, FlowId, GmodRealm, LuaFunctionType, LuaSemanticDeclId,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::infer::{InferFailReason, VarRefId},
};

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
    pub expr_cache: HashMap<LuaSyntaxId, CacheEntry<LuaType>>,
    pub call_cache:
        HashMap<(LuaSyntaxId, Option<usize>, LuaType), CacheEntry<Arc<LuaFunctionType>>>,
    pub flow_node_cache: HashMap<(VarRefId, FlowId, GmodRealm), CacheEntry<LuaType>>,
    pub flow_query_realm: Option<GmodRealm>,
    pub index_ref_origin_type_cache: HashMap<VarRefId, CacheEntry<LuaType>>,
    pub expr_var_ref_id_cache: HashMap<LuaSyntaxId, VarRefId>,
    pub narrow_by_literal_stop_position_cache: HashSet<LuaSyntaxId>,
    pub scoped_scripted_global_cache: Option<Option<(String, String)>>,
    pub pending_str_tpl_type_decls: Vec<PendingStrTplTypeDecl>,
    /// Cache for `self` type per enclosing method (keyed by LuaFuncStat syntax_id).
    /// Avoids repeated ancestor walks and type resolution for each `self` reference
    /// within the same method body.
    pub self_type_cache: HashMap<LuaSyntaxId, Option<LuaType>>,
    /// Cache for `find_decl` results so that multiple diagnostic checkers
    /// processing the same file don't redo the full member-resolution chain.
    pub decl_cache: HashMap<LuaSyntaxId, Option<LuaSemanticDeclId>>,
    /// Tracks total flow nodes visited during flow analysis. When this exceeds
    /// `flow_node_budget`, subsequent flow walks return the base type without
    /// narrowing. This prevents pathologically large files from dominating
    /// diagnostic time.
    pub flow_nodes_visited: u32,
    /// Maximum number of flow nodes to visit before skipping narrowing.
    /// 0 means unlimited.
    pub flow_node_budget: u32,
    // Diagnostic profiling counters (zero-cost when not read)
    pub prof_infer_expr_calls: u32,
    pub prof_infer_expr_hits: u32,
    pub prof_flow_calls: u32,
    pub prof_flow_hits: u32,
    pub prof_flow_nodes_walked: u32,
    // Detailed flow profiling
    pub prof_merge_calls: u32,
    pub prof_merge_total_antecedents: u32,
    pub prof_condition_errors_caught: u32,
    pub prof_condition_errors_none: u32,
    pub prof_condition_errors_recursive: u32,
    pub prof_condition_errors_unresolved: u32,
    pub prof_multi_ante_from_condition: u32,
}

impl LuaInferCache {
    pub fn new(file_id: FileId, config: CacheOptions) -> Self {
        Self {
            file_id,
            config,
            expr_cache: HashMap::new(),
            call_cache: HashMap::new(),
            flow_node_cache: HashMap::new(),
            flow_query_realm: None,
            index_ref_origin_type_cache: HashMap::new(),
            expr_var_ref_id_cache: HashMap::new(),
            narrow_by_literal_stop_position_cache: HashSet::new(),
            scoped_scripted_global_cache: None,
            pending_str_tpl_type_decls: Vec::new(),
            self_type_cache: HashMap::new(),
            decl_cache: HashMap::new(),
            flow_nodes_visited: 0,
            flow_node_budget: 0,
            prof_infer_expr_calls: 0,
            prof_infer_expr_hits: 0,
            prof_flow_calls: 0,
            prof_flow_hits: 0,
            prof_flow_nodes_walked: 0,
            prof_merge_calls: 0,
            prof_merge_total_antecedents: 0,
            prof_condition_errors_caught: 0,
            prof_condition_errors_none: 0,
            prof_condition_errors_recursive: 0,
            prof_condition_errors_unresolved: 0,
            prof_multi_ante_from_condition: 0,
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
        self.flow_node_cache.clear();
        self.flow_query_realm = None;
        self.index_ref_origin_type_cache.clear();
        self.expr_var_ref_id_cache.clear();
        self.scoped_scripted_global_cache = None;
        self.pending_str_tpl_type_decls.clear();
        self.self_type_cache.clear();
        self.decl_cache.clear();
        self.flow_nodes_visited = 0;
    }

    /// Clears all caches. Used before the unresolve phase.
    pub fn clear_for_unresolve(&mut self) {
        self.clear();
    }

    /// Returns true if the flow analysis budget has been exhausted.
    pub fn flow_budget_exhausted(&self) -> bool {
        self.flow_node_budget > 0 && self.flow_nodes_visited >= self.flow_node_budget
    }
}
