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

use rustc_hash::{FxHashMap, FxHashSet};

use super::{DbIndex, FileId, LuaSignatureId, TypeCacheRef, TypeIdentityRemap};

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

    let (tables, signatures) = identity_maps(db, remap);

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
    dirty.extend(db.get_type_index_mut().remap_file_identities(remap));
    dirty.extend(
        db.get_dynamic_field_index_mut()
            .remap_file_table_owners(remap),
    );
    dirty.extend(db.get_metatable_index_mut().remap_file_ranges(remap));
    db.get_flow_index_mut()
        .remap_special_call_effect_types(TypeIdentityRemap {
            tables: &tables,
            signatures: &signatures,
        });

    dirty.remove(&remap.file_id);
    dirty
}

/// The identities the edited file moved, as old -> new.
///
/// Collected from the stores that can enumerate what they hold about the file.
/// A payload naming an identity no store enumerates is unreachable from any
/// map; the file that holds it is dirtied by whichever store *did* see the
/// identity, which is the same population in every case observed so far.
fn identity_maps(
    db: &DbIndex,
    remap: &FileRemap,
) -> (
    FxHashMap<crate::InFiled<rowan::TextRange>, crate::InFiled<rowan::TextRange>>,
    FxHashMap<LuaSignatureId, LuaSignatureId>,
) {
    let mut tables = FxHashMap::default();
    let mut signatures = FxHashMap::default();

    let mut add_table = |range: &crate::InFiled<rowan::TextRange>| {
        if let Remap::Moved(new) = remap.table_range(range)
            && &new != range
        {
            tables.insert(range.clone(), new);
        }
    };
    for range in db
        .get_member_index()
        .element_owner_ranges_in_file(remap.file_id)
    {
        add_table(&range);
    }
    let mut cache_signatures = Vec::new();
    for type_ref in db.get_type_index().type_cache_refs_into_file(remap.file_id) {
        match type_ref {
            TypeCacheRef::Table(range) | TypeCacheRef::Instance(range) => add_table(range),
            TypeCacheRef::Signature(signature_id) => cache_signatures.push(*signature_id),
            TypeCacheRef::Module(_) | TypeCacheRef::Decl(_) => {}
        }
    }

    let stored = db
        .get_call_site_param_index()
        .stored_signature_ids_for_file(remap.file_id);
    for signature_id in cache_signatures.into_iter().chain(stored) {
        if let Remap::Moved(new) = remap.signature_id(signature_id)
            && new != signature_id
        {
            signatures.insert(signature_id, new);
        }
    }

    (tables, signatures)
}
