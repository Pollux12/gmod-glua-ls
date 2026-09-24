use rustc_hash::FxHashMap;

use crate::{FileId, LuaAnalysisPhase, semantic::LuaInferCache};

#[derive(Debug, Default)]
pub struct InferCacheManager {
    infer_map: FxHashMap<FileId, LuaInferCache>,
    /// The write generation the cached inference was computed against.
    write_generation: u64,
    current_phase: LuaAnalysisPhase,
    dynamic_fields_visible: bool,
    call_site_params_visible: bool,
}

impl InferCacheManager {
    pub fn new() -> Self {
        InferCacheManager {
            infer_map: FxHashMap::default(),
            write_generation: 0,
            current_phase: LuaAnalysisPhase::Ordered,
            dynamic_fields_visible: false,
            call_site_params_visible: false,
        }
    }

    pub fn get_infer_cache(&mut self, file_id: FileId) -> &mut LuaInferCache {
        let generation = crate::db_index::derivation::write_generation();
        if generation != self.write_generation {
            self.write_generation = generation;
            self.clear();
        }
        let phase = self.current_phase;
        let dynamic_fields_visible = self.dynamic_fields_visible;
        let call_site_params_visible = self.call_site_params_visible;
        self.infer_map.entry(file_id).or_insert_with(|| {
            LuaInferCache::new(
                file_id,
                crate::CacheOptions {
                    analysis_phase: phase,
                    dynamic_fields_visible,
                    call_site_params_visible,
                },
            )
        })
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

    pub fn take_file(&mut self, file_id: FileId) -> Option<LuaInferCache> {
        self.infer_map.remove(&file_id)
    }

    pub fn restore_file(&mut self, file_id: FileId, infer_cache: LuaInferCache) {
        debug_assert_eq!(infer_cache.get_file_id(), file_id);
        let previous = self.infer_map.insert(file_id, infer_cache);
        debug_assert!(previous.is_none());
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
}
