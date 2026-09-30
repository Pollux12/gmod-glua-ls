mod collection;

pub(in crate::compilation::analyzer) use collection::alias_target_global_path;
#[cfg(test)]
pub(in crate::compilation::analyzer::lua) use collection::record_pending_dynamic_key_collection_widening;
pub(in crate::compilation::analyzer) use collection::resolve_index_expr_member_owner_for_file;
pub(in crate::compilation::analyzer) use collection::{
    CollectionWidening, widen_member_collections, widened_member_assignment_collection_type,
};
pub(in crate::compilation::analyzer::lua) use collection::{
    DynamicKeyCollectionWideningKey, direct_local_prefix_has_declared_type,
    direct_local_table_prefix_member_owner,
    flush_pending_dynamic_key_collection_widening_for_members, is_collection_append_write,
    widen_existing_member_collection_type,
};
