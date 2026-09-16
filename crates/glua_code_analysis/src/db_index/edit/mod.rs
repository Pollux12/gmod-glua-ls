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

use std::collections::HashSet;

use super::{DbIndex, FileId};

/// Rewrites every reference into `remap.file_id` that another file holds.
///
/// Runs after the edited file has been re-indexed: its own entries are rebuilt
/// by that, and what survives on an old position is what some *other* file
/// recorded about it. Returns the files whose reference could not be
/// rewritten - they have to be re-analysed instead.
pub fn remap_into_file(db: &mut DbIndex, remap: &FileRemap) -> HashSet<FileId> {
    let mut dirty = HashSet::default();
    if remap.map.is_identity() {
        return dirty;
    }
    let trace = std::env::var_os("GLUALS_SETTLE_TRACE").is_some();
    // The member index drives the assignment-contribution store, so its
    // signature half is threaded through it.
    let (forgotten, member_dirty) = db.get_member_index_mut().remap_file_element_owners(remap);
    if trace && !member_dirty.is_empty() {
        eprintln!("[settle-trace]   remap pass member_element_owners: {member_dirty:?}");
    }
    dirty.extend(member_dirty);
    if !forgotten.is_empty() {
        db.get_type_index_mut()
            .remove_member_type_caches(&forgotten);
    }

    let pass = db
        .get_call_site_param_index_mut()
        .remap_file_signatures(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass call_site_signatures: {pass:?}");
    }
    dirty.extend(pass);
    let pass = db.get_signature_index_mut().remap_settled_reads(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass signature_settled_reads: {pass:?}");
    }
    dirty.extend(pass);
    let pass = db.get_signature_index_mut().remap_payload_types(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass signature_payload_types: {pass:?}");
    }
    dirty.extend(pass);
    let pass = db.get_type_index_mut().remap_file_identities(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass type_identities: {pass:?}");
    }
    dirty.extend(pass);
    let pass = db
        .get_dynamic_field_index_mut()
        .remap_file_table_owners(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass dynamic_field_owners: {pass:?}");
    }
    dirty.extend(pass);
    let pass = db.get_metatable_index_mut().remap_file_ranges(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass metatable_ranges: {pass:?}");
    }
    dirty.extend(pass);
    let pass = db
        .get_flow_index_mut()
        .remap_special_call_effect_types(remap);
    if trace && !pass.is_empty() {
        eprintln!("[settle-trace]   remap pass flow_effect_types: {pass:?}");
    }
    dirty.extend(pass);

    if trace {
        let mut all: Vec<_> = dirty.iter().collect();
        all.sort_unstable();
        eprintln!(
            "[settle-trace]   remap total: {} file(s): {all:?}",
            all.len()
        );
    }
    dirty.remove(&remap.file_id);
    dirty
}
