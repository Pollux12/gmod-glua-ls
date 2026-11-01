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

    /// Lets the caches this manager creates read the dynamic-field index — see
    /// [`crate::CacheOptions`].
    pub fn set_dynamic_fields_visible(&mut self) {
        self.dynamic_fields_visible = true;
    }

    /// Lets the caches this manager creates read the call-site parameter index.
    pub fn set_call_site_params_visible(&mut self) {
        self.call_site_params_visible = true;
    }

    /// Has the caches this manager creates floor what has not resolved.
    pub fn set_force(&mut self) {
        self.current_phase = LuaAnalysisPhase::Force;
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
    /// only work together: an analysis context's manager starts with the index
    /// invisible until it is opened, while everything outside one (hover,
    /// completion, diagnostics) must see it. Flipping either default silently
    /// changes what a cold build infers.
    #[test]
    fn dynamic_field_visibility_defaults_are_opposite() {
        assert!(crate::CacheOptions::default().dynamic_fields_visible);
        assert!(!InferCacheManager::new().dynamic_fields_visible);
        assert!(crate::CacheOptions::default().call_site_params_visible);
        assert!(!InferCacheManager::new().call_site_params_visible);
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
