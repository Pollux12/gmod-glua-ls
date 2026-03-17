mod cache_options;

pub use cache_options::{CacheOptions, LuaAnalysisPhase};
use glua_parser::LuaSyntaxId;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    FileId, FlowId, LuaFunctionType,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::infer::VarRefId,
};

#[derive(Debug)]
pub enum CacheEntry<T> {
    Ready,
    Cache(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStrTplTypeDecl {
    pub file_id: FileId,
    pub type_decl_id: LuaTypeDeclId,
    pub super_type: LuaType,
}

#[derive(Debug)]
pub struct LuaInferCache {
    file_id: FileId,
    config: CacheOptions,
    pub expr_cache: HashMap<LuaSyntaxId, CacheEntry<LuaType>>,
    pub call_cache:
        HashMap<(LuaSyntaxId, Option<usize>, LuaType), CacheEntry<Arc<LuaFunctionType>>>,
    pub flow_node_cache: HashMap<(VarRefId, FlowId), CacheEntry<LuaType>>,
    pub index_ref_origin_type_cache: HashMap<VarRefId, CacheEntry<LuaType>>,
    pub expr_var_ref_id_cache: HashMap<LuaSyntaxId, VarRefId>,
    pub narrow_by_literal_stop_position_cache: HashSet<LuaSyntaxId>,
    pub scoped_scripted_global_cache: Option<Option<(String, String)>>,
    pub pending_str_tpl_type_decls: Vec<PendingStrTplTypeDecl>,
    /// Cache for `self` type per enclosing method (keyed by LuaFuncStat syntax_id).
    /// Avoids repeated ancestor walks and type resolution for each `self` reference
    /// within the same method body.
    pub self_type_cache: HashMap<LuaSyntaxId, Option<LuaType>>,
}

impl LuaInferCache {
    pub fn new(file_id: FileId, config: CacheOptions) -> Self {
        Self {
            file_id,
            config,
            expr_cache: HashMap::new(),
            call_cache: HashMap::new(),
            flow_node_cache: HashMap::new(),
            index_ref_origin_type_cache: HashMap::new(),
            expr_var_ref_id_cache: HashMap::new(),
            narrow_by_literal_stop_position_cache: HashSet::new(),
            scoped_scripted_global_cache: None,
            pending_str_tpl_type_decls: Vec::new(),
            self_type_cache: HashMap::new(),
        }
    }

    pub fn get_config(&self) -> &CacheOptions {
        &self.config
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
        self.index_ref_origin_type_cache.clear();
        self.expr_var_ref_id_cache.clear();
        self.scoped_scripted_global_cache = None;
        self.pending_str_tpl_type_decls.clear();
        self.self_type_cache.clear();
    }
}
