use rustc_hash::FxHashMap;
use std::collections::HashSet;

use super::{LuaMemberId, LuaMemberKey, LuaMemberOwner};
use crate::{FileId, LuaType};

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
