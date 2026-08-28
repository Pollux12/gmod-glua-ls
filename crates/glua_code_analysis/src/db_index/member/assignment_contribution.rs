use rustc_hash::FxHashMap;
use std::collections::HashSet;

use super::{LuaMemberId, LuaMemberKey, LuaMemberOwner};
use crate::{FileId, InFiled, LuaType};
use rowan::TextRange;

/// The group a member assignment contributes its evidence to.
pub type MemberAssignmentContributionKey = (LuaMemberOwner, LuaMemberKey);

/// What one writer of `owner.key = value` knows on its own.
#[derive(Debug, Clone)]
pub struct MemberAssignmentContribution {
    /// What this write bound — what a sibling reads out of the type cache.
    pub bound_type: LuaType,
    /// What this write carried before it was merged with any sibling.
    pub source_type: LuaType,
    pub doc_type: Option<LuaType>,
    /// Taken from syntax at the write, so it does not change with batch phase.
    pub guarded_bootstrap: bool,
    /// Whether the write asked the merge to keep table literals unwidened.
    pub preserve_table_literals: bool,
}

#[derive(Debug, Default)]
pub struct MemberAssignmentContributionStore {
    by_owner_key: FxHashMap<
        MemberAssignmentContributionKey,
        FxHashMap<LuaMemberId, MemberAssignmentContribution>,
    >,
    /// Reverse index used to sweep a file's entries without scanning the store.
    by_file: FxHashMap<FileId, FxHashMap<LuaMemberId, MemberAssignmentContributionKey>>,
}

impl MemberAssignmentContributionStore {
    pub fn record(
        &mut self,
        owner: LuaMemberOwner,
        key: LuaMemberKey,
        member_id: LuaMemberId,
        contribution: MemberAssignmentContribution,
    ) {
        let store_key = (owner, key);
        let previous = self
            .by_file
            .entry(member_id.file_id)
            .or_default()
            .insert(member_id, store_key.clone());
        if let Some(previous) = previous
            && previous != store_key
        {
            self.detach(&previous, member_id);
        }
        self.by_owner_key
            .entry(store_key)
            .or_default()
            .insert(member_id, contribution);
    }
    /// Drops every entry the removed files contributed, in one sweep keyed by
    /// file rather than a whole-store scan per file.
    pub fn remove_files<S: std::hash::BuildHasher>(&mut self, removed: &HashSet<FileId, S>) {
        for file_id in removed {
            let Some(entries) = self.by_file.remove(file_id) else {
                continue;
            };
            for (member_id, store_key) in entries {
                self.detach(&store_key, member_id);
            }
        }
    }

    pub fn contributions(
        &self,
        store_key: &MemberAssignmentContributionKey,
    ) -> Option<&FxHashMap<LuaMemberId, MemberAssignmentContribution>> {
        self.by_owner_key.get(store_key)
    }

    /// The contribution this member recorded, wherever its writer group
    /// currently sits.
    pub fn contribution_of(
        &self,
        member_id: &LuaMemberId,
    ) -> Option<&MemberAssignmentContribution> {
        let store_key = self.by_file.get(&member_id.file_id)?.get(member_id)?;
        self.by_owner_key.get(store_key)?.get(member_id)
    }

    /// The `(owner, key)` group this member's write currently contributes to.
    pub fn contribution_group_of(
        &self,
        member_id: &LuaMemberId,
    ) -> Option<(LuaMemberOwner, LuaMemberKey)> {
        self.by_file
            .get(&member_id.file_id)?
            .get(member_id)
            .cloned()
    }

    /// The distinct groups the given files wrote to.
    pub fn keys_for_files(
        &self,
        files: &HashSet<FileId>,
    ) -> HashSet<MemberAssignmentContributionKey> {
        let mut keys = HashSet::new();
        for file_id in files {
            let Some(entries) = self.by_file.get(file_id) else {
                continue;
            };
            keys.extend(entries.values().cloned());
        }
        keys
    }

    /// Moves writer groups whose owner is a table literal that shifted offset
    /// onto the literal's new range.
    ///
    /// Without this a group stays filed under the pre-edit range while the
    /// member index has already re-homed the owner, so the widening merge for
    /// the new range cannot see the writers other files contributed.
    pub fn remap_element_owners(
        &mut self,
        map: &FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
    ) {
        let moved: Vec<(
            MemberAssignmentContributionKey,
            MemberAssignmentContributionKey,
        )> = self
            .by_owner_key
            .keys()
            .filter_map(|store_key| {
                let LuaMemberOwner::Element(old) = &store_key.0 else {
                    return None;
                };
                let new = map.get(old)?;
                Some((
                    store_key.clone(),
                    (LuaMemberOwner::Element(new.clone()), store_key.1.clone()),
                ))
            })
            .collect();

        // Every group is detached before any is re-filed. One literal's new
        // range can be another's old range, and a remove-then-insert loop
        // would then merge the first group into the second's key and carry
        // both along on the second move.
        let detached: Vec<(
            MemberAssignmentContributionKey,
            MemberAssignmentContributionKey,
            _,
        )> = moved
            .into_iter()
            .filter_map(|(old_key, new_key)| {
                let group = self.by_owner_key.remove(&old_key)?;
                Some((old_key, new_key, group))
            })
            .collect();

        for (_old_key, new_key, group) in detached {
            for member_id in group.keys() {
                if let Some(entries) = self.by_file.get_mut(&member_id.file_id) {
                    entries.insert(*member_id, new_key.clone());
                }
            }
            self.by_owner_key.entry(new_key).or_default().extend(group);
        }
    }

    /// Drops one member's writer entry, for a member removed on its own
    /// rather than as part of a file sweep.
    pub fn remove_member(&mut self, member_id: LuaMemberId) {
        let Some(entries) = self.by_file.get_mut(&member_id.file_id) else {
            return;
        };
        let Some(store_key) = entries.remove(&member_id) else {
            return;
        };
        if entries.is_empty() {
            self.by_file.remove(&member_id.file_id);
        }
        self.detach(&store_key, member_id);
    }

    /// Rewrites table-literal ranges inside stored writer evidence.
    ///
    /// A contribution's `bound_type`/`source_type`/`doc_type` can name a table
    /// literal in the edited file. The member index re-homes the *owner*, but
    /// the evidence the widening merge reads is here, and a stale range in it
    /// resolves to a literal that no longer exists.
    pub fn remap_table_ranges(&mut self, map: &FxHashMap<InFiled<TextRange>, InFiled<TextRange>>) {
        let keys: Vec<MemberAssignmentContributionKey> =
            self.by_owner_key.keys().cloned().collect();
        for key in keys {
            let Some(group) = self.by_owner_key.get_mut(&key) else {
                continue;
            };
            for contribution in group.values_mut() {
                if let Some(bound) =
                    crate::db_index::remap_table_ranges_in_type(&contribution.bound_type, map)
                {
                    contribution.bound_type = bound;
                }
                if let Some(source) =
                    crate::db_index::remap_table_ranges_in_type(&contribution.source_type, map)
                {
                    contribution.source_type = source;
                }
                if let Some(doc) = contribution
                    .doc_type
                    .as_ref()
                    .and_then(|doc| crate::db_index::remap_table_ranges_in_type(doc, map))
                {
                    contribution.doc_type = Some(doc);
                }
            }
        }
    }

    /// Every table-literal range this store is keyed by or whose evidence
    /// names one.
    #[cfg(test)]
    pub(crate) fn table_ranges(&self) -> Vec<InFiled<TextRange>> {
        self.by_owner_key
            .iter()
            .flat_map(|(store_key, group)| {
                let owner = match &store_key.0 {
                    LuaMemberOwner::Element(range) => Some(range.clone()),
                    _ => None,
                };
                owner
                    .into_iter()
                    .chain(group.values().flat_map(|contribution| {
                        crate::db_index::table_ranges_in_type(&contribution.bound_type)
                            .into_iter()
                            .chain(crate::db_index::table_ranges_in_type(
                                &contribution.source_type,
                            ))
                            .chain(
                                contribution
                                    .doc_type
                                    .iter()
                                    .flat_map(crate::db_index::table_ranges_in_type),
                            )
                    }))
            })
            .collect()
    }

    /// Number of stored writer entries, for the profile report.
    pub fn entry_count(&self) -> usize {
        self.by_owner_key.values().map(FxHashMap::len).sum()
    }

    pub fn clear(&mut self) {
        self.by_owner_key.clear();
        self.by_file.clear();
    }

    fn detach(&mut self, store_key: &MemberAssignmentContributionKey, member_id: LuaMemberId) {
        let Some(group) = self.by_owner_key.get_mut(store_key) else {
            return;
        };
        group.remove(&member_id);
        if group.is_empty() {
            self.by_owner_key.remove(store_key);
        }
    }
}
