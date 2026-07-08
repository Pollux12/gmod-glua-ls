use rustc_hash::FxHashMap;

use crate::{
    GmodStateMask, LuaMemberKey,
    db_index::{LuaMemberId, LuaMemberOwner},
};

use super::super::LuaAnalyzer;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::compilation::analyzer::lua) struct MemberAssignmentWideningCacheKey {
    pub(in crate::compilation::analyzer::lua) owner: LuaMemberOwner,
    pub(in crate::compilation::analyzer::lua) key: LuaMemberKey,
}

#[derive(Debug)]
pub(in crate::compilation::analyzer::lua) struct MemberWideningCache<S> {
    pub(in crate::compilation::analyzer::lua) seen_count: usize,
    pub(in crate::compilation::analyzer::lua) by_state_mask: FxHashMap<GmodStateMask, S>,
    pub(in crate::compilation::analyzer::lua) disabled: bool,
}

impl<S> Default for MemberWideningCache<S> {
    fn default() -> Self {
        Self {
            seen_count: 0,
            by_state_mask: FxHashMap::default(),
            disabled: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::compilation::analyzer::lua) struct DynamicKeyCollectionWideningKey {
    pub(in crate::compilation::analyzer::lua) owner: LuaMemberOwner,
    pub(in crate::compilation::analyzer::lua) key: LuaMemberKey,
}

pub(in crate::compilation::analyzer::lua) enum WideningCacheLookup<'a, S> {
    FirstSighting,
    Fallback,
    Hit(&'a MemberWideningCache<S>),
}

pub(in crate::compilation::analyzer::lua) fn lookup_widening_cache<'a, S>(
    cache_map: &'a FxHashMap<MemberAssignmentWideningCacheKey, MemberWideningCache<S>>,
    cache_key: &MemberAssignmentWideningCacheKey,
    visible_count: usize,
) -> WideningCacheLookup<'a, S> {
    let Some(cache) = cache_map.get(cache_key) else {
        return if visible_count == 1 {
            WideningCacheLookup::FirstSighting
        } else {
            WideningCacheLookup::Fallback
        };
    };

    if cache.disabled || cache.seen_count + 1 != visible_count {
        return WideningCacheLookup::Fallback;
    }

    WideningCacheLookup::Hit(cache)
}

pub(in crate::compilation::analyzer::lua) fn record_widening_cache<S>(
    cache_map: &mut FxHashMap<MemberAssignmentWideningCacheKey, MemberWideningCache<S>>,
    cache_key: MemberAssignmentWideningCacheKey,
    visible_count: usize,
    state_mask: GmodStateMask,
    new_state: S,
    merge: impl FnOnce(&mut S, S),
) {
    let mut cache = cache_map.remove(&cache_key).unwrap_or_default();
    if cache.disabled {
        cache_map.insert(cache_key, cache);
        return;
    }
    if cache.seen_count + 1 != visible_count {
        cache.disabled = true;
        cache_map.insert(cache_key, cache);
        return;
    }

    cache.seen_count = visible_count;
    match cache.by_state_mask.get_mut(&state_mask) {
        Some(state) => merge(state, new_state),
        None => {
            cache.by_state_mask.insert(state_mask, new_state);
        }
    }

    cache_map.insert(cache_key, cache);
}

pub(in crate::compilation::analyzer::lua) fn member_assignment_state_mask(
    analyzer: &LuaAnalyzer,
    member_id: LuaMemberId,
) -> GmodStateMask {
    if !analyzer.gmod_enabled {
        return GmodStateMask::empty();
    }

    analyzer
        .db
        .get_gmod_infer_index()
        .get_state_mask_at_offset(&member_id.file_id, member_id.get_position())
}

pub(in crate::compilation::analyzer::lua) fn member_assignment_state_masks_compatible(
    analyzer: &LuaAnalyzer,
    left: GmodStateMask,
    right: GmodStateMask,
) -> bool {
    !analyzer.gmod_enabled || left.is_compatible_with(right)
}
