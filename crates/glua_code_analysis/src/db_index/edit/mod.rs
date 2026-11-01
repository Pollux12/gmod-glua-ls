//! Incremental-edit machinery: where an edit moved things.

mod position_map;
#[cfg(test)]
mod test;

pub use position_map::{FileFacts, FileRemap, PositionMap, Remap};

use super::derivation::FactKey;
use super::{DbIndex, FileId};

/// A fact one file stores whose value named a position an edit destroyed.
/// The value is left as it was, for the fact's producers to compute again.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LostFact {
    /// The file storing the fact, re-analysed whole when no derivation
    /// produced it.
    pub file_id: FileId,
    pub key: FactKey,
}

impl LostFact {
    pub fn new(file_id: FileId, key: FactKey) -> Self {
        Self { file_id, key }
    }
}

/// Rewrites every reference into `remap.file_id` that another file holds.
///
/// Runs after the edited file has been re-indexed: its own entries are rebuilt
/// by that, and what survives on an old position is what some *other* file
/// recorded about it. Returns the other files' facts whose reference could
/// not be rewritten - they have to be computed again instead.
pub fn remap_into_file(db: &mut DbIndex, remap: &FileRemap) -> Vec<LostFact> {
    let mut lost = Vec::new();
    if remap.map.is_identity() {
        return lost;
    }
    // The member index drives the assignment-contribution store, so its
    // signature half is threaded through it.
    let forgotten = db.get_member_index_mut().remap_file_element_owners(remap);
    if !forgotten.is_empty() {
        let typed = db
            .get_type_index_mut()
            .remove_member_type_caches(&forgotten);
        lost.extend(
            forgotten
                .iter()
                .map(|member_id| LostFact::new(member_id.file_id, FactKey::Member(*member_id))),
        );
        lost.extend(
            typed
                .into_iter()
                .map(|owner| LostFact::new(owner.get_file_id(), FactKey::TypeCache(owner))),
        );
    }

    lost.extend(db.get_signature_index_mut().remap_payload_types(remap));
    // The edited file's own types, written by its re-indexed source layer,
    // already name its new positions.
    let rebuilt = db
        .reader_graph()
        .derivations_of_file(remap.file_id)
        .flat_map(|id| db.reader_graph().outputs_of(id))
        .filter_map(|key| match key.key() {
            FactKey::TypeCache(owner) => Some(owner.clone()),
            _ => None,
        })
        .collect::<rustc_hash::FxHashSet<_>>();
    lost.extend(
        db.get_type_index_mut()
            .remap_file_identities(remap, &rebuilt),
    );
    lost.extend(
        db.get_dynamic_field_index_mut()
            .remap_file_table_owners(remap),
    );
    lost.extend(db.get_metatable_index_mut().remap_file_ranges(remap));
    lost.extend(
        db.get_accessor_func_call_index_mut()
            .remap_file_table_owners(remap),
    );
    lost.extend(db.get_module_index_mut().remap_file_exports(remap));
    lost.extend(
        db.get_flow_index_mut()
            .remap_special_call_effect_types(remap),
    );
    lost.extend(db.get_call_target_index_mut().remap_file_identities(remap));
    lost.extend(
        db.get_call_site_param_index_mut()
            .remap_file_identities(remap),
    );
    lost.extend(db.remap_net_sites(remap));
    lost.extend(db.get_gmod_network_index_mut().remap_file_positions(remap));
    db.get_gmod_load_index_mut().remap_edge_positions(remap);
    db.get_gmod_infer_index_mut().remap_realm_ranges(remap);

    lost.retain(|fact| fact.file_id != remap.file_id);
    lost
}
