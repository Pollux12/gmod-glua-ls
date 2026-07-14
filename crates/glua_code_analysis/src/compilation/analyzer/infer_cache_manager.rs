use std::collections::HashSet;

use rustc_hash::FxHashMap;

use crate::{
    DbIndex, FileId, LuaAnalysisPhase, LuaInferredGuardOwner,
    semantic::{LuaInferCache, PendingStrTplTypeDecl},
};

#[derive(Debug, Default)]
pub struct InferCacheManager {
    infer_map: FxHashMap<FileId, LuaInferCache>,
    current_phase: LuaAnalysisPhase,
}

impl InferCacheManager {
    pub fn new() -> Self {
        InferCacheManager {
            infer_map: FxHashMap::default(),
            current_phase: LuaAnalysisPhase::Ordered,
        }
    }

    pub fn get_infer_cache(&mut self, file_id: FileId) -> &mut LuaInferCache {
        let phase = self.current_phase;
        self.infer_map.entry(file_id).or_insert_with(|| {
            LuaInferCache::new(
                file_id,
                crate::CacheOptions {
                    analysis_phase: phase,
                },
            )
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

    pub fn clear_for_unresolve(&mut self, db: &DbIndex) {
        for infer_cache in self.infer_map.values_mut() {
            infer_cache.clear_for_unresolve(db);
        }
    }

    pub fn drain_pending_str_tpl_type_decls(&mut self) -> Vec<PendingStrTplTypeDecl> {
        let mut pending = Vec::new();

        for infer_cache in self.infer_map.values_mut() {
            pending.extend(infer_cache.take_pending_str_tpl_type_decls());
        }

        pending
    }

    pub fn clear_files(&mut self, file_ids: &HashSet<FileId>) {
        for file_id in file_ids {
            if let Some(infer_cache) = self.infer_map.get_mut(file_id) {
                infer_cache.clear();
            }
        }
    }

    pub fn drain_inferred_guard_dependencies(
        &mut self,
    ) -> Vec<(FileId, HashSet<LuaInferredGuardOwner>)> {
        self.infer_map
            .iter_mut()
            .map(|(file_id, cache)| (*file_id, cache.take_inferred_guard_dependencies()))
            .collect()
    }
}
