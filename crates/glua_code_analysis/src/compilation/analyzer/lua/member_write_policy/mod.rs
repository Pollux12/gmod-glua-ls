mod cache;
mod collection;
mod scalar;

pub(in crate::compilation::analyzer::lua) use cache::{
    DynamicKeyCollectionWideningKey, MemberAssignmentWideningCacheKey, MemberWideningCache,
    WideningCacheLookup, lookup_widening_cache, member_assignment_state_mask,
    member_assignment_state_masks_compatible, record_widening_cache,
};
pub(in crate::compilation::analyzer::lua) use collection::{
    direct_local_table_prefix_member_owner,
    flush_pending_dynamic_key_collection_widening_for_members,
    get_widened_member_assignment_collection_type, is_collection_append_write,
    is_member_realm_compatible, record_member_collection_assignment_widening_cache,
    resolve_index_expr_member_owner_for_file, widen_existing_member_collection_type,
};
#[cfg(test)]
pub(in crate::compilation::analyzer::lua) use collection::{
    get_cached_widened_member_collection_assignment_type,
    record_pending_dynamic_key_collection_widening,
};
pub(in crate::compilation::analyzer::lua) use scalar::{
    MemberAssignmentWideningDecision, MemberAssignmentWideningState,
    decide_member_assignment_widening, merge_member_assignment_widening_state,
    union_member_assignment_widening, widen_related_assignment_type,
};
