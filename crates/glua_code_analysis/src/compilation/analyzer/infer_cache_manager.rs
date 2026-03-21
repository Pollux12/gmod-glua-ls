use std::collections::HashMap;

use crate::{
    FileId, LuaAnalysisPhase,
    semantic::{LuaInferCache, PendingStrTplTypeDecl},
};

#[derive(Debug, Default)]
pub struct InferCacheManager {
    infer_map: HashMap<FileId, LuaInferCache>,
    current_phase: LuaAnalysisPhase,
    /// Default flow budget for newly created caches. 0 = unlimited.
    default_flow_budget: u32,
}

impl InferCacheManager {
    pub fn new() -> Self {
        InferCacheManager {
            infer_map: HashMap::new(),
            current_phase: LuaAnalysisPhase::Ordered,
            default_flow_budget: 0,
        }
    }

    pub fn set_default_flow_budget(&mut self, budget: u32) {
        self.default_flow_budget = budget;
    }

    pub fn get_infer_cache(&mut self, file_id: FileId) -> &mut LuaInferCache {
        let phase = self.current_phase;
        let budget = self.default_flow_budget;
        self.infer_map.entry(file_id).or_insert_with(|| {
            let mut cache = LuaInferCache::new(
                file_id,
                crate::CacheOptions {
                    analysis_phase: phase,
                    skip_flow_narrowing: false,
                },
            );
            cache.flow_node_budget = budget;
            cache
        })
    }

    pub fn set_force(&mut self) {
        self.current_phase = LuaAnalysisPhase::Force;
        for (_, infer_cache) in self.infer_map.iter_mut() {
            infer_cache.set_phase(LuaAnalysisPhase::Force);
        }
    }

    pub fn clear(&mut self) {
        for (_, infer_cache) in self.infer_map.iter_mut() {
            infer_cache.clear();
        }
    }

    /// Clears all caches before the unresolve phase.
    pub fn clear_for_unresolve(&mut self) {
        self.clear();
    }

    pub fn drain_pending_str_tpl_type_decls(&mut self) -> Vec<PendingStrTplTypeDecl> {
        let mut pending = Vec::new();

        for infer_cache in self.infer_map.values_mut() {
            pending.extend(infer_cache.take_pending_str_tpl_type_decls());
        }

        pending
    }
}
