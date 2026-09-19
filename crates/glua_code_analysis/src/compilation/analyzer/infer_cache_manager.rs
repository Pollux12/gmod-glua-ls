use rayon::prelude::*;
use rustc_hash::FxHashSet;

use rustc_hash::FxHashMap;

use crate::{
    DbIndex, FileId, LuaAnalysisPhase, LuaInferredGuardOwner,
    semantic::{LuaInferCache, PendingStrTplTypeDecl},
};

#[derive(Debug, Default)]
pub struct InferCacheManager {
    infer_map: FxHashMap<FileId, LuaInferCache>,
    current_phase: LuaAnalysisPhase,
    dynamic_fields_visible: bool,
    call_site_params_visible: bool,
}

impl InferCacheManager {
    pub fn new() -> Self {
        InferCacheManager {
            infer_map: FxHashMap::default(),
            current_phase: LuaAnalysisPhase::Ordered,
            dynamic_fields_visible: false,
            call_site_params_visible: false,
        }
    }

    pub fn get_infer_cache(&mut self, file_id: FileId) -> &mut LuaInferCache {
        let phase = self.current_phase;
        let dynamic_fields_visible = self.dynamic_fields_visible;
        let call_site_params_visible = self.call_site_params_visible;
        self.infer_map.entry(file_id).or_insert_with(|| {
            LuaInferCache::new(
                file_id,
                crate::CacheOptions {
                    analysis_phase: phase,
                    dynamic_fields_visible,
                    building_dynamic_field_index: false,
                    call_site_params_visible,
                },
            )
        })
    }

    /// A cache built the way [`get_infer_cache`](Self::get_infer_cache) would
    /// build this file's, for worker threads that cannot borrow the map.
    /// Deterministic inference results match because the config matches; the
    /// memo entries it accumulates can be merged back or discarded.
    pub fn fresh_cache(&self, file_id: FileId) -> crate::LuaInferCache {
        crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase: self.current_phase,
                dynamic_fields_visible: self.dynamic_fields_visible,
                building_dynamic_field_index: false,
                call_site_params_visible: self.call_site_params_visible,
            },
        )
    }

    /// The file's cache without creating it — worker seeding clones only
    /// what the serial walk would have found already present.
    pub fn peek_infer_cache(&self, file_id: FileId) -> Option<&crate::LuaInferCache> {
        self.infer_map.get(&file_id)
    }

    /// An empty manager whose visibility flags match this one's, for worker
    /// threads that walk a file against a forked index. `get_infer_cache` on
    /// it builds the same `LuaInferCache::new` the shared manager would.
    pub fn worker_shadow(&self) -> Self {
        InferCacheManager {
            infer_map: FxHashMap::default(),
            current_phase: self.current_phase,
            dynamic_fields_visible: self.dynamic_fields_visible,
            call_site_params_visible: self.call_site_params_visible,
        }
    }

    /// Called once the dynamic-field index has been populated for this batch.
    /// Until then inference must not read it — see [`crate::CacheOptions`].
    pub fn set_dynamic_fields_visible(&mut self) {
        self.dynamic_fields_visible = true;
        for infer_cache in self.infer_map.values_mut() {
            infer_cache.config_mut().dynamic_fields_visible = true;
            infer_cache.dynamic_field_resolution_cache.clear();
        }
    }

    pub fn current_phase(&self) -> LuaAnalysisPhase {
        self.current_phase
    }

    pub fn dynamic_fields_visible(&self) -> bool {
        self.dynamic_fields_visible
    }

    /// Called once the call-site parameter index has been populated for this batch.
    /// Until then early syntactic and body passes must not read it.
    pub fn set_call_site_params_visible(&mut self) {
        self.call_site_params_visible = true;
        for infer_cache in self.infer_map.values_mut() {
            infer_cache.config_mut().call_site_params_visible = true;
        }
    }

    pub fn call_site_params_visible(&self) -> bool {
        self.call_site_params_visible
    }

    pub fn merge_inference_side_effects(
        &mut self,
        file_id: FileId,
        pending_type_decls: Vec<PendingStrTplTypeDecl>,
        guard_dependencies: FxHashSet<LuaInferredGuardOwner>,
    ) {
        let infer_cache = self.get_infer_cache(file_id);
        for pending in pending_type_decls {
            debug_assert_eq!(pending.file_id, file_id);
            infer_cache.add_pending_str_tpl_type_decl(
                pending.source_range,
                pending.type_decl_id,
                pending.super_type,
            );
        }
        for dependency in guard_dependencies {
            infer_cache.add_inferred_guard_dependency(dependency);
        }
    }

    pub fn set_force(&mut self) {
        self.current_phase = LuaAnalysisPhase::Force;
        for infer_cache in self.infer_map.values_mut() {
            infer_cache.set_phase(LuaAnalysisPhase::Force);
            // The force phase answers a failed inference with a floor instead
            // of an error, so a failure recorded under the previous phase is
            // not the answer this one would give. Replaying it lets whichever
            // expression in a chain happened to be walked first decide the
            // result, which is the batch's settling order leaking into a type.
            infer_cache.clear_deferred_inference_results();
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

    /// Clears inference results between floored settle rounds while retaining
    /// the structural lookups that remain valid for this analysis context.
    /// Branch special-call answers are derived from a mutable effect index, so
    /// they must not cross the boundary with the structural caches.
    ///
    /// Each cache clears independently, so the sweep runs on workers when the
    /// map is large enough to pay the dispatch.
    pub fn clear_for_settle(&mut self, db: &DbIndex) {
        let clear = |infer_cache: &mut LuaInferCache| {
            infer_cache.clear_for_unresolve(db);
            infer_cache.branch_special_call_effect_cache.clear();
        };
        if super::parallel::runs_inline(self.infer_map.len()) {
            self.infer_map.values_mut().for_each(clear);
        } else {
            super::parallel::init_pool();
            let mut caches: Vec<&mut LuaInferCache> = self.infer_map.values_mut().collect();
            rayon::in_place_scope(|_| {
                caches.par_iter_mut().for_each(|cache| clear(cache));
            });
        }
    }

    pub fn drain_pending_str_tpl_type_decls(&mut self) -> Vec<PendingStrTplTypeDecl> {
        let mut pending = Vec::new();

        for infer_cache in self.infer_map.values_mut() {
            pending.extend(infer_cache.take_pending_str_tpl_type_decls());
        }

        pending
    }

    pub fn clear_files(&mut self, file_ids: &FxHashSet<FileId>) {
        for file_id in file_ids {
            if let Some(infer_cache) = self.infer_map.get_mut(file_id) {
                infer_cache.clear();
            }
        }
    }

    pub fn clear_file_deferred_results(&mut self, file_id: FileId) {
        if let Some(infer_cache) = self.infer_map.get_mut(&file_id) {
            infer_cache.clear_deferred_inference_results();
        }
    }

    pub fn clear_files_iter_var_results(&mut self, file_ids: &FxHashSet<FileId>) {
        for file_id in file_ids {
            if let Some(infer_cache) = self.infer_map.get_mut(file_id) {
                infer_cache.clear_iter_var_results();
            }
        }
    }

    pub fn clear_file_undetermined_flow_results(&mut self, file_id: FileId) {
        if let Some(infer_cache) = self.infer_map.get_mut(&file_id) {
            infer_cache.clear_undetermined_flow_results();
        }
    }

    pub fn clear_files_deferred_results(&mut self, file_ids: &FxHashSet<FileId>) {
        for file_id in file_ids {
            if let Some(infer_cache) = self.infer_map.get_mut(file_id) {
                infer_cache.clear_deferred_inference_results();
            }
        }
    }

    pub fn take_file(&mut self, file_id: FileId) -> Option<LuaInferCache> {
        self.infer_map.remove(&file_id)
    }

    pub fn restore_file(&mut self, file_id: FileId, infer_cache: LuaInferCache) {
        debug_assert_eq!(infer_cache.get_file_id(), file_id);
        let previous = self.infer_map.insert(file_id, infer_cache);
        debug_assert!(previous.is_none());
    }

    pub fn drain_inferred_guard_dependencies(
        &mut self,
    ) -> Vec<(FileId, FxHashSet<LuaInferredGuardOwner>)> {
        self.infer_map
            .iter_mut()
            .map(|(file_id, cache)| (*file_id, cache.take_inferred_guard_dependencies()))
            .collect()
    }

    pub fn drain_inferred_return_reads(
        &mut self,
    ) -> Vec<(
        crate::LuaSignatureId,
        crate::db_index::read_set::InferredReturnReadRecord,
    )> {
        self.infer_map
            .values_mut()
            .flat_map(|cache| cache.take_inferred_return_reads())
            .collect()
    }

    pub fn drain_missed_member_reads(
        &mut self,
    ) -> Vec<(
        FileId,
        FxHashSet<(crate::LuaMemberOwner, crate::LuaMemberKey)>,
    )> {
        self.infer_map
            .iter_mut()
            .map(|(file_id, cache)| (*file_id, cache.take_missed_member_reads()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of the dynamic-field gate pull in opposite directions and
    /// only work together: a batch starts with the index invisible and opens it
    /// once populated, while everything outside a batch (hover, completion,
    /// diagnostics) must see it. Flipping either default silently changes what a
    /// cold build infers.
    #[test]
    fn dynamic_field_visibility_defaults_are_opposite() {
        assert!(crate::CacheOptions::default().dynamic_fields_visible);
        assert!(!InferCacheManager::new().dynamic_fields_visible());
        assert!(crate::CacheOptions::default().call_site_params_visible);
        assert!(!InferCacheManager::new().call_site_params_visible());
    }

    #[test]
    fn taking_and_restoring_a_file_preserves_cache_and_config() {
        let file_id = FileId::new(7);
        let decl_id = crate::LuaDeclId::new(file_id, 4.into());
        let mut manager = InferCacheManager::new();
        let mut cache = LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase: crate::LuaAnalysisPhase::Force,
                dynamic_fields_visible: false,
                building_dynamic_field_index: true,
                call_site_params_visible: false,
            },
        );
        cache
            .for_range_iter_var_type_cache
            .insert(decl_id, crate::CacheEntry::Cache(crate::LuaType::String));

        manager.restore_file(file_id, cache);
        let cache = manager.take_file(file_id).expect("cache was restored");
        assert_eq!(cache.get_file_id(), file_id);
        assert!(cache.get_config().analysis_phase.is_force());
        assert!(!cache.get_config().dynamic_fields_visible);
        assert!(cache.get_config().building_dynamic_field_index);
        assert!(matches!(
            cache.for_range_iter_var_type_cache.get(&decl_id),
            Some(crate::CacheEntry::Cache(crate::LuaType::String))
        ));

        manager.restore_file(file_id, cache);
        let restored = manager
            .take_file(file_id)
            .expect("cache was restored again");
        assert!(matches!(
            restored.for_range_iter_var_type_cache.get(&decl_id),
            Some(crate::CacheEntry::Cache(crate::LuaType::String))
        ));
        assert!(restored.get_config().analysis_phase.is_force());
    }

    #[test]
    fn clear_for_settle_retains_structural_lookup_but_clears_results() {
        let file_id = FileId::new(7);
        let syntax_id = glua_parser::LuaSyntaxId::new(
            glua_parser::LuaSyntaxKind::NameExpr.into(),
            rowan::TextRange::new(3.into(), 4.into()),
        );
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache.decl_name_index = Some(rustc_hash::FxHashMap::default());
        cache
            .flow_node_realm_cache
            .insert(crate::FlowId(1), crate::GmodRealm::Server);
        cache
            .expr_cache
            .insert(syntax_id, crate::CacheEntry::Cache(crate::LuaType::Integer));
        cache.branch_special_call_effect_cache.insert(
            (
                crate::FlowId(1),
                crate::FlowId(2),
                (&crate::VarRefId::VarRef(crate::LuaDeclId::new(file_id, 3.into()))).into(),
            ),
            true,
        );

        let mut manager = InferCacheManager::new();
        manager.restore_file(file_id, cache);
        manager.clear_for_settle(&DbIndex::new());

        let cache = manager.take_file(file_id).expect("cache was retained");
        assert!(cache.decl_name_index.is_some());
        assert_eq!(
            cache.flow_node_realm_cache.get(&crate::FlowId(1)),
            Some(&crate::GmodRealm::Server)
        );
        assert!(cache.expr_cache.is_empty());
        assert!(cache.branch_special_call_effect_cache.is_empty());
    }
}
