//! Incremental-edit machinery: where an edit moved things, what a file
//! exports, and who reads it.

mod export_map;
mod position_map;
#[cfg(test)]
mod test;

pub use export_map::{
    ChangedExports, ExportKey, ExportMap, NetSide, RealmSubject, dependents_of, diff_exports,
    export_map,
};
pub use position_map::{FileFacts, FileRemap, PositionMap, Remap};

use rustc_hash::FxHashSet;

use super::{DbIndex, FileId};

/// Rewrites every reference into `remap.file_id` that another file holds.
///
/// Runs after the edited file has been re-indexed: its own entries are rebuilt
/// by that, and what survives on an old position is what some *other* file
/// recorded about it. Returns the files whose reference could not be
/// rewritten - they have to be re-analysed instead.
pub fn remap_into_file(db: &mut DbIndex, remap: &FileRemap) -> FxHashSet<FileId> {
    let mut dirty = FxHashSet::default();
    if remap.map.is_identity() {
        return dirty;
    }

    // The member index drives the assignment-contribution store, so its
    // signature half is threaded through it.
    let (forgotten, member_dirty) = db.get_member_index_mut().remap_file_element_owners(remap);
    dirty.extend(member_dirty);
    if !forgotten.is_empty() {
        db.get_type_index_mut()
            .remove_member_type_caches(&forgotten);
    }

    dirty.extend(
        db.get_call_site_param_index_mut()
            .remap_file_signatures(remap),
    );
    dirty.extend(db.get_signature_index_mut().remap_settled_reads(remap));
    dirty.extend(db.get_signature_index_mut().remap_payload_types(remap));
    dirty.extend(db.get_type_index_mut().remap_file_identities(remap));
    dirty.extend(
        db.get_dynamic_field_index_mut()
            .remap_file_table_owners(remap),
    );
    dirty.extend(db.get_metatable_index_mut().remap_file_ranges(remap));
    dirty.extend(
        db.get_flow_index_mut()
            .remap_special_call_effect_types(remap),
    );

    dirty.remove(&remap.file_id);
    dirty
}
