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

    pub fn current_phase(&self) -> LuaAnalysisPhase {
        self.current_phase
    }

    pub fn merge_inference_side_effects(
        &mut self,
        file_id: FileId,
        pending_type_decls: Vec<PendingStrTplTypeDecl>,
        guard_dependencies: HashSet<LuaInferredGuardOwner>,
    ) {
        let infer_cache = self.get_infer_cache(file_id);
        for pending in pending_type_decls {
            debug_assert_eq!(pending.file_id, file_id);
            infer_cache.add_pending_str_tpl_type_decl(pending.type_decl_id, pending.super_type);
        }
        for dependency in guard_dependencies {
            infer_cache.add_inferred_guard_dependency(dependency);
        }
    }

    pub fn set_force(&mut self) {
        self.current_phase = LuaAnalysisPhase::Force;
        for infer_cache in self.infer_map.values_mut() {
            infer_cache.set_phase(LuaAnalysisPhase::Force);
        }
    }

    pub fn clear(&mut self) {
        for infer_cache in self.infer_map.values_mut() {
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

    pub fn clear_files_deferred_results(&mut self, file_ids: &HashSet<FileId>) {
        for file_id in file_ids {
            if let Some(infer_cache) = self.infer_map.get_mut(file_id) {
                infer_cache.clear_deferred_inference_results();
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
