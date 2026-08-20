mod assignment_contribution;
mod lua_member;
mod lua_member_feature;
mod lua_member_item;
mod lua_member_owner;
mod lua_owner_members;

use glua_parser::LuaSyntaxKind;
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeMap;

use super::traits::LuaIndex;
use crate::{FileId, GlobalId, db_index::member::lua_owner_members::LuaOwnerMembers};
pub use assignment_contribution::{
    MemberAssignmentContribution, MemberAssignmentContributionKey,
    MemberAssignmentContributionStore,
};
pub use lua_member::{LuaMember, LuaMemberId, LuaMemberKey};
pub use lua_member_feature::LuaMemberFeature;
pub use lua_member_item::LuaMemberIndexItem;
pub use lua_member_owner::LuaMemberOwner;

#[derive(Debug)]
pub struct LuaMemberIndex {
    members: HashMap<LuaMemberId, LuaMember>,
    in_filed: HashMap<FileId, HashSet<MemberOrOwner>>,
    owner_members: HashMap<LuaMemberOwner, LuaOwnerMembers>,
    member_current_owner: HashMap<LuaMemberId, LuaMemberOwner>,
    member_owner_key_index: HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
    member_owner_key_history_index:
        HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
    current_owner_member_history:
        HashMap<LuaMemberOwner, BTreeMap<(u32, u32, u32, u16), LuaMemberId>>,
    current_members_by_key: HashMap<LuaMemberKey, BTreeMap<(u32, u32, u32, u16), LuaMemberId>>,
    non_overwriting_assignment_members: HashSet<LuaMemberId>,
    /// Assignment members written inside a conditional construct (`if c
    /// then t.k = v end`).
    conditional_branch_assignment_members: HashSet<LuaMemberId>,
    /// Members whose owner was decided by scripted-class synthesis rather
    /// than by name resolution.
    synthesized_owner_members: HashSet<LuaMemberId>,
    /// Members `try_resolve_member` had to create itself, from a prefix
    /// type read mid-fixpoint.
    deferred_index_expr_members: HashSet<LuaMemberId>,
    function_scope_ranges: HashMap<FileId, Vec<TextRange>>,
    member_function_scope_ranges: HashMap<LuaMemberId, TextRange>,
    /// Per-writer evidence for the member assignment widening merge. See
    /// [`MemberAssignmentContribution`].
    assignment_contributions: MemberAssignmentContributionStore,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MemberOrOwner {
    Member(LuaMemberId),
    Owner(LuaMemberOwner),
}

#[derive(Debug)]
enum MemberInsertAction {
    Noop,
    Store(LuaMemberIndexItem),
    StoreRemovingVisibleOldIds {
        item: LuaMemberIndexItem,
        old_ids: Vec<LuaMemberId>,
    },
    PushPreservedAssignment,
}

impl Default for LuaMemberIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaMemberIndex {
    pub fn new() -> Self {
        Self {
            members: HashMap::default(),
            in_filed: HashMap::default(),
            owner_members: HashMap::default(),
            member_current_owner: HashMap::default(),
            member_owner_key_index: HashMap::default(),
            member_owner_key_history_index: HashMap::default(),
            current_owner_member_history: HashMap::default(),
            current_members_by_key: HashMap::default(),
            non_overwriting_assignment_members: HashSet::default(),
            conditional_branch_assignment_members: HashSet::default(),
            synthesized_owner_members: HashSet::default(),
            deferred_index_expr_members: HashSet::default(),
            function_scope_ranges: HashMap::default(),
            member_function_scope_ranges: HashMap::default(),
            assignment_contributions: MemberAssignmentContributionStore::default(),
        }
    }

    /// Records this write's own evidence for the settled widening re-derivation.
    pub fn record_member_assignment_contribution(
        &mut self,
        member_id: LuaMemberId,
        contribution: MemberAssignmentContribution,
    ) -> Option<()> {
        let owner = self.member_current_owner.get(&member_id)?.clone();
        let key = self.get_member(&member_id)?.get_key().clone();
        self.assignment_contributions
            .record(owner, key, member_id, contribution);
        Some(())
    }

    pub fn member_assignment_contributions(&self) -> &MemberAssignmentContributionStore {
        &self.assignment_contributions
    }

    pub fn add_member(&mut self, owner: LuaMemberOwner, member: LuaMember) -> LuaMemberId {
        let id = member.get_id();
        let file_id = member.get_file_id();
        let function_scope = self.assignment_file_define_scope_for_member(&member);
        self.members.insert(id, member);
        self.set_member_function_scope_range(id, function_scope);
        self.add_in_file_object(file_id, MemberOrOwner::Member(id));
        if !owner.is_unknown() {
            self.member_current_owner.insert(id, owner.clone());
            self.add_current_member_key(id);
            self.current_owner_member_history
                .entry(owner.clone())
                .or_default()
                .insert(member_id_sort_key(id), id);
            self.add_in_file_object(file_id, MemberOrOwner::Owner(owner.clone()));
            self.add_new_member_to_owner_key_index(owner.clone(), id);
            self.add_new_member_to_owner_key_history_index(owner.clone(), id);
            self.add_member_to_owner(owner.clone(), id);
        }
        id
    }

    fn add_in_file_object(&mut self, file_id: FileId, member_or_owner: MemberOrOwner) {
        self.in_filed
            .entry(file_id)
            .or_default()
            .insert(member_or_owner);
    }

    pub fn add_member_to_owner(&mut self, owner: LuaMemberOwner, id: LuaMemberId) -> Option<()> {
        let member = self.get_member(&id)?;
        let key = member.get_key().clone();
        let is_decl = member.get_feature().is_decl();
        if self.member_current_owner.get(&id) != Some(&owner) {
            self.add_member_to_owner_key_index(owner.clone(), id);
            self.add_member_to_owner_key_history_index(owner.clone(), id);
        }

        self.owner_members
            .entry(owner.clone())
            .or_insert_with(LuaOwnerMembers::new);

        let current_item = self
            .owner_members
            .get(&owner)
            .and_then(|owner_members| owner_members.get_member(&key));
        let action = self.classify_member_insert(&owner, &key, id, is_decl, current_item);
        self.apply_member_insert_action(owner, key, id, action);

        Some(())
    }

    fn classify_member_insert(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
        id: LuaMemberId,
        is_decl: bool,
        current_item: Option<&LuaMemberIndexItem>,
    ) -> MemberInsertAction {
        let Some(item) = current_item else {
            return MemberInsertAction::Store(LuaMemberIndexItem::One(id));
        };

        if is_decl {
            return match item {
                LuaMemberIndexItem::One(old_id) if *old_id == id => MemberInsertAction::Noop,
                LuaMemberIndexItem::One(old_id) => {
                    MemberInsertAction::Store(LuaMemberIndexItem::Many(vec![*old_id, id]))
                }
                LuaMemberIndexItem::Many(ids) if ids.contains(&id) => MemberInsertAction::Noop,
                LuaMemberIndexItem::Many(ids) => {
                    let mut ids = ids.clone();
                    ids.push(id);
                    MemberInsertAction::Store(LuaMemberIndexItem::Many(ids))
                }
            };
        }

        if let Some(action) = self.classify_conditional_branch_insert(id, item) {
            return action;
        }

        if self.should_preserve_assignment_file_define_member(owner, key, id) {
            return MemberInsertAction::PushPreservedAssignment;
        }

        if self.is_item_only_meta(item) {
            return match item {
                LuaMemberIndexItem::One(old_id) if *old_id == id => MemberInsertAction::Noop,
                LuaMemberIndexItem::One(old_id) => {
                    MemberInsertAction::Store(LuaMemberIndexItem::Many(vec![id, *old_id]))
                }
                LuaMemberIndexItem::Many(ids) if ids.contains(&id) => MemberInsertAction::Noop,
                LuaMemberIndexItem::Many(ids) => {
                    let mut ids = ids.clone();
                    ids.push(id);
                    MemberInsertAction::Store(LuaMemberIndexItem::Many(ids))
                }
            };
        }

        if !self.is_item_only_file_define(item) {
            return MemberInsertAction::Noop;
        }

        let old_member_ids = member_ids_from_item(item);
        let all_assignment_file_defines = self.is_assignment_file_define_member(id)
            && old_member_ids
                .iter()
                .all(|old_id| self.is_assignment_file_define_member(*old_id));

        if all_assignment_file_defines {
            let should_preserve_members = self.non_overwriting_assignment_members.contains(&id)
                && old_member_ids
                    .iter()
                    .all(|old_id| self.non_overwriting_assignment_members.contains(old_id));
            if should_preserve_members {
                let mut ids = old_member_ids;
                if !ids.contains(&id) {
                    ids.push(id);
                }
                let item = match ids.as_slice() {
                    [id] => LuaMemberIndexItem::One(*id),
                    _ => LuaMemberIndexItem::Many(ids),
                };
                return MemberInsertAction::Store(item);
            }

            // A guarded self-assignment (`t.k = t.k or {}`) is a
            // placeholder: its `{}` carries no member information of its
            // own, so it must not take the visible slot from a real writer
            // in another file -- which one survived would then depend on
            // load order. Only across files: statements in one file run in
            // source order, so a later write there genuinely supersedes.
            if self.non_overwriting_assignment_members.contains(&id)
                && old_member_ids.iter().any(|old_id| {
                    old_id.file_id != id.file_id
                        && !self.non_overwriting_assignment_members.contains(old_id)
                })
            {
                return MemberInsertAction::Noop;
            }

            // The surviving write is the *latest defined* one, not the one
            // that arrived last -- the rule the mixed-feature fall-through
            // below already applies. Within a file the two agree, because
            // the sort key leads with source position.
            let candidates = || old_member_ids.iter().copied().chain(std::iter::once(id));
            let winner = candidates()
                .filter(|candidate| {
                    !self.non_overwriting_assignment_members.contains(candidate)
                        || !candidates().any(|other| {
                            other.file_id != candidate.file_id
                                && !self.non_overwriting_assignment_members.contains(&other)
                        })
                })
                .max_by_key(|candidate| member_id_sort_key(*candidate))
                .unwrap_or_else(|| latest_defined_member(&old_member_ids, id));

            return match item {
                LuaMemberIndexItem::One(old_id) if *old_id == id => MemberInsertAction::Noop,
                LuaMemberIndexItem::Many(ids) if ids.contains(&id) => MemberInsertAction::Noop,
                _ => MemberInsertAction::Store(LuaMemberIndexItem::One(winner)),
            };
        }

        match item {
            LuaMemberIndexItem::One(old_id) if *old_id == id => MemberInsertAction::Noop,
            _ => {
                let winner = latest_defined_member(&old_member_ids, id);
                if matches!(item, LuaMemberIndexItem::One(current) if *current == winner) {
                    return MemberInsertAction::Noop;
                }
                MemberInsertAction::StoreRemovingVisibleOldIds {
                    item: LuaMemberIndexItem::One(winner),
                    old_ids: old_member_ids
                        .into_iter()
                        .chain(std::iter::once(id))
                        .filter(|candidate| {
                            *candidate != winner
                                && !self.is_assignment_file_define_member(*candidate)
                        })
                        .collect(),
                }
            }
        }
    }

    /// Resolves an owner/key slot that any conditional-branch write
    /// contributes to, as a function of the members involved rather than of
    /// their arrival order.
    fn classify_conditional_branch_insert(
        &self,
        id: LuaMemberId,
        item: &LuaMemberIndexItem,
    ) -> Option<MemberInsertAction> {
        if !self.is_item_only_file_define(item)
            || !self.is_item_only_file_define(&LuaMemberIndexItem::One(id))
        {
            return None;
        }

        let mut candidates = member_ids_from_item(item);
        if !candidates.contains(&id) {
            candidates.push(id);
        }
        let new_item = self.conditional_branch_item(&candidates)?;
        Some(if &new_item == item {
            MemberInsertAction::Noop
        } else {
            MemberInsertAction::Store(new_item)
        })
    }

    /// The visible item for a slot that `candidates` write to, when at least one
    /// of them is a conditional-branch write: every conditional writer, plus the
    /// latest plain one because plain writers do dominate each other. Ordered by
    /// [`member_id_sort_key`], so it is a pure function of the candidate set.
    fn conditional_branch_item(&self, candidates: &[LuaMemberId]) -> Option<LuaMemberIndexItem> {
        let (mut kept, plain): (Vec<_>, Vec<_>) =
            candidates.iter().copied().partition(|candidate| {
                self.conditional_branch_assignment_members
                    .contains(candidate)
            });
        if kept.is_empty() {
            return None;
        }
        if let Some(latest_plain) = plain.into_iter().max_by_key(|id| member_id_sort_key(*id)) {
            kept.push(latest_plain);
        }
        kept.sort_by_key(|id| member_id_sort_key(*id));

        Some(match kept.as_slice() {
            [only] => LuaMemberIndexItem::One(*only),
            _ => LuaMemberIndexItem::Many(kept),
        })
    }

    /// Re-resolves the slot `member_id` writes to, now that it is known to
    /// be a conditional-branch write.
    fn resolve_conditional_branch_owner_key_item(&mut self, member_id: LuaMemberId) -> Option<()> {
        let owner = self.member_current_owner.get(&member_id)?.clone();
        if matches!(owner, LuaMemberOwner::GlobalPath(_)) {
            return None;
        }
        let key = self.get_member(&member_id)?.get_key().clone();
        let candidates = self
            .get_current_owner_members_for_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        if !candidates
            .iter()
            .all(|candidate| self.is_assignment_file_define_member(*candidate))
        {
            return None;
        }

        let item = self.conditional_branch_item(&candidates)?;
        let owner_members = self.owner_members.get_mut(&owner)?;
        if owner_members.get_member(&key) != Some(&item) {
            owner_members.add_member(key, item);
        }
        Some(())
    }

    fn apply_member_insert_action(
        &mut self,
        owner: LuaMemberOwner,
        key: LuaMemberKey,
        id: LuaMemberId,
        action: MemberInsertAction,
    ) {
        match action {
            MemberInsertAction::Noop => {}
            MemberInsertAction::Store(item) => {
                self.owner_members
                    .entry(owner)
                    .or_insert_with(LuaOwnerMembers::new)
                    .add_member(key, item);
            }
            MemberInsertAction::StoreRemovingVisibleOldIds { item, old_ids } => {
                for old_id in old_ids {
                    self.remove_member_from_visible_owner_key_index(&owner, old_id);
                }
                self.owner_members
                    .entry(owner)
                    .or_insert_with(LuaOwnerMembers::new)
                    .add_member(key, item);
            }
            MemberInsertAction::PushPreservedAssignment => {
                self.merge_member_into_owner_item(owner, key, id);
            }
        }
    }

    /// Records that `id`'s owner was decided by scripted-class synthesis, so the
    /// global-member migration must leave it alone.
    pub fn pin_synthesized_owner(&mut self, id: LuaMemberId) {
        self.synthesized_owner_members.insert(id);
    }

    pub fn has_synthesized_owner(&self, id: &LuaMemberId) -> bool {
        self.synthesized_owner_members.contains(id)
    }

    /// Records that `id` was created by deferred resolution, so its first home
    /// was decided from a mid-fixpoint prefix type. See
    /// [`Self::deferred_index_expr_members`].
    pub fn mark_deferred_index_expr_member(&mut self, id: LuaMemberId) {
        self.deferred_index_expr_members.insert(id);
    }

    pub fn is_deferred_index_expr_member(&self, id: &LuaMemberId) -> bool {
        self.deferred_index_expr_members.contains(id)
    }

    /// Removes `id` from `owner` entirely, including the item
    /// `set_member_owner` leaves behind.
    pub fn detach_member_from_owner(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        self.remove_member_from_all_owner_key_indexes(owner, id);
        self.remove_current_owner_member(owner, id);

        let Some(owner_members) = self.owner_members.get_mut(owner) else {
            return;
        };
        let drop_key = match owner_members.get_member_mut(&key) {
            Some(LuaMemberIndexItem::One(existing)) => *existing == id,
            Some(LuaMemberIndexItem::Many(ids)) => {
                ids.retain(|member_id| *member_id != id);
                ids.is_empty()
            }
            None => false,
        };
        if drop_key {
            owner_members.remove_member(&key);
        }
        if owner_members.is_empty() {
            self.owner_members.remove(owner);
        }
    }

    /// Makes `id` *also* reachable through `owner`, without ever displacing
    /// what is already there.
    pub fn add_member_alias_to_owner(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
    ) -> Option<()> {
        let member = self.get_member(&id)?;
        let file_id = member.get_file_id();
        let key = member.get_key().clone();

        if self.member_current_owner.get(&id) != Some(&owner) {
            self.add_member_to_owner_key_index(owner.clone(), id);
            self.add_member_to_owner_key_history_index(owner.clone(), id);
        }

        let owner_members = self
            .owner_members
            .entry(owner.clone())
            .or_insert_with(LuaOwnerMembers::new);
        if owner_members.contains_member(&key) {
            self.merge_member_into_owner_item(owner.clone(), key, id);
        } else {
            owner_members.add_member(key, LuaMemberIndexItem::One(id));
        }

        self.add_in_file_object(file_id, MemberOrOwner::Owner(owner));
        Some(())
    }

    fn should_preserve_assignment_file_define_member(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
        id: LuaMemberId,
    ) -> bool {
        if !self.non_overwriting_assignment_members.contains(&id)
            || !self.is_assignment_file_define_member(id)
        {
            return false;
        }

        self.owner_members
            .get(owner)
            .and_then(|owner_members| owner_members.get_member(key))
            .is_some_and(|item| self.item_can_append_preserved_assignment_member(item))
    }

    fn item_can_append_preserved_assignment_member(&self, item: &LuaMemberIndexItem) -> bool {
        match item {
            LuaMemberIndexItem::One(id) => {
                self.is_assignment_file_define_member(*id)
                    && self.non_overwriting_assignment_members.contains(id)
            }
            LuaMemberIndexItem::Many(ids) => ids.last().is_some_and(|id| {
                self.is_assignment_file_define_member(*id)
                    && self.non_overwriting_assignment_members.contains(id)
            }),
        }
    }

    /// Adds `id` to the item already stored at `owner`/`key`, keeping the item a
    /// set ordered by `member_id_sort_key`. Never removes an existing id, and is
    /// a no-op when `id` is already present.
    fn merge_member_into_owner_item(
        &mut self,
        owner: LuaMemberOwner,
        key: LuaMemberKey,
        id: LuaMemberId,
    ) {
        let Some(item) = self
            .owner_members
            .entry(owner)
            .or_insert_with(LuaOwnerMembers::new)
            .get_member_mut(&key)
        else {
            return;
        };

        match item {
            LuaMemberIndexItem::One(old_id) => {
                if *old_id != id {
                    *item = LuaMemberIndexItem::Many(sorted_member_pair(*old_id, id));
                }
            }
            LuaMemberIndexItem::Many(ids) => {
                // `Many` is not guaranteed sorted — `classify_member_insert`
                // appends in arrival order — so the ordered fast paths below
                // cannot themselves rule out a duplicate. Enumerating a member
                // twice is worse than the linear scan; these lists are short.
                if ids.contains(&id) {
                    return;
                }
                if ids
                    .last()
                    .is_none_or(|last_id| member_id_sort_key(*last_id) < member_id_sort_key(id))
                {
                    ids.push(id);
                    return;
                }

                match ids.binary_search_by_key(&member_id_sort_key(id), |existing_id| {
                    member_id_sort_key(*existing_id)
                }) {
                    Ok(_) => {}
                    Err(index) => ids.insert(index, id),
                }
            }
        }
    }

    fn add_member_to_owner_key_index(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        self.add_member_id_to_owner_key_map(owner, id, false);
    }

    fn add_member_to_owner_key_history_index(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        self.add_member_id_to_owner_key_map(owner, id, true);
    }

    fn add_new_member_to_owner_key_index(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        self.add_new_member_id_to_owner_key_map(owner, id, false);
    }

    fn add_new_member_to_owner_key_history_index(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
    ) {
        self.add_new_member_id_to_owner_key_map(owner, id, true);
    }

    fn add_member_id_to_owner_key_map(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
        history: bool,
    ) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        {
            let target_index = if history {
                &mut self.member_owner_key_history_index
            } else {
                &mut self.member_owner_key_index
            };
            let member_ids = target_index
                .entry(owner)
                .or_default()
                .entry(key.clone())
                .or_default();
            if !member_ids.contains(&id) {
                member_ids.push(id);
            }
        }
    }

    fn add_new_member_id_to_owner_key_map(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
        history: bool,
    ) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        let target_index = if history {
            &mut self.member_owner_key_history_index
        } else {
            &mut self.member_owner_key_index
        };
        target_index
            .entry(owner)
            .or_default()
            .entry(key)
            .or_default()
            .push(id);
    }

    fn remove_member_from_visible_owner_key_index(
        &mut self,
        owner: &LuaMemberOwner,
        id: LuaMemberId,
    ) {
        self.remove_member_from_owner_key_map(owner, id, false);
    }

    fn remove_member_from_all_owner_key_indexes(
        &mut self,
        owner: &LuaMemberOwner,
        id: LuaMemberId,
    ) {
        self.remove_member_from_visible_owner_key_index(owner, id);
        self.remove_member_from_owner_key_map(owner, id, true);
    }

    fn remove_member_from_owner_key_map(
        &mut self,
        owner: &LuaMemberOwner,
        id: LuaMemberId,
        history: bool,
    ) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        let mut remove_owner_entry = false;
        let target_index = if history {
            &mut self.member_owner_key_history_index
        } else {
            &mut self.member_owner_key_index
        };
        if let Some(owner_items) = target_index.get_mut(owner) {
            if let Some(member_ids) = owner_items.get_mut(&key) {
                member_ids.retain(|member_id| *member_id != id);
                if member_ids.is_empty() {
                    owner_items.remove(&key);
                }
            }
            remove_owner_entry = owner_items.is_empty();
        }

        if remove_owner_entry {
            target_index.remove(owner);
        }
    }

    fn remove_files_members_from_owner_key_indexes(&mut self, removed: &HashSet<FileId>) {
        Self::remove_files_members_from_owner_key_map(&mut self.member_owner_key_index, removed);
        Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_history_index,
            removed,
        );
    }

    fn remove_files_members_from_owner_key_map(
        owner_key_index: &mut HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
        removed: &HashSet<FileId>,
    ) {
        owner_key_index.retain(|_, key_members| {
            key_members.retain(|_, member_ids| {
                member_ids.retain(|member_id| !removed.contains(&member_id.file_id));
                !member_ids.is_empty()
            });
            !key_members.is_empty()
        });
    }

    fn is_item_only_meta(&self, item: &LuaMemberIndexItem) -> bool {
        match item {
            LuaMemberIndexItem::One(id) => {
                if let Some(member) = self.get_member(id) {
                    return member.get_feature().is_meta_decl();
                }
            }
            LuaMemberIndexItem::Many(ids) => {
                for id in ids {
                    if let Some(member) = self.get_member(id)
                        && !member.get_feature().is_meta_decl()
                    {
                        return false;
                    }
                }
                return true;
            }
        }

        false
    }

    fn is_item_only_file_define(&self, item: &LuaMemberIndexItem) -> bool {
        match item {
            LuaMemberIndexItem::One(id) => self
                .get_member(id)
                .is_some_and(|member| member.get_feature().is_file_define()),
            LuaMemberIndexItem::Many(ids) => ids.iter().all(|id| {
                self.get_member(id)
                    .is_some_and(|member| member.get_feature().is_file_define())
            }),
        }
    }

    fn is_assignment_file_define_member(&self, id: LuaMemberId) -> bool {
        self.get_member(&id).is_some_and(|member| {
            member.get_feature().is_file_define()
                && member.get_syntax_id().get_kind() == LuaSyntaxKind::IndexExpr
        })
    }

    fn assignment_file_define_scope_for_member(&self, member: &LuaMember) -> Option<TextRange> {
        if !member.get_feature().is_file_define()
            || member.get_syntax_id().get_kind() != LuaSyntaxKind::IndexExpr
        {
            return None;
        }

        self.enclosing_function_scope_range(member.get_file_id(), member.get_id().get_position())
    }

    pub fn set_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
    ) -> Option<()> {
        let previous_owner = self.member_current_owner.insert(id, owner.clone());
        if previous_owner.is_none() {
            self.add_current_member_key(id);
        }
        if let Some(previous_owner) = previous_owner
            .as_ref()
            .filter(|previous_owner| *previous_owner != &owner)
        {
            self.remove_member_from_visible_owner_key_index(previous_owner, id);
            self.remove_current_owner_member(previous_owner, id);
        }

        self.current_owner_member_history
            .entry(owner.clone())
            .or_default()
            .insert(member_id_sort_key(id), id);

        self.add_member_to_owner_key_index(owner.clone(), id);
        self.add_member_to_owner_key_history_index(owner.clone(), id);
        if self.member_function_scope_range(id).is_none()
            && let Some(member) = self.get_member(&id)
        {
            let function_scope = self.assignment_file_define_scope_for_member(member);
            self.set_member_function_scope_range(id, function_scope);
        }
        self.add_in_file_object(file_id, MemberOrOwner::Owner(owner));

        Some(())
    }

    pub fn get_member(&self, id: &LuaMemberId) -> Option<&LuaMember> {
        self.members.get(id)
    }

    pub fn get_member_mut(&mut self, id: &LuaMemberId) -> Option<&mut LuaMember> {
        self.members.get_mut(id)
    }

    /// Every global path that currently has members parked on it, in a
    /// stable order.
    pub fn sorted_global_path_owners(&self) -> Vec<GlobalId> {
        let mut global_ids = self
            .owner_members
            .keys()
            .filter_map(|owner| match owner {
                LuaMemberOwner::GlobalPath(global_id) => Some(global_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        global_ids.sort_unstable_by(|left, right| left.get_name().cmp(right.get_name()));
        global_ids
    }

    pub fn get_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let owner_members = self.owner_members.get(owner)?;

        if owner_members.get_member_len() == 0 {
            return Some(Vec::new());
        }

        if owner_members.get_member_len() == 1 {
            match owner_members.get_member_items().next()? {
                LuaMemberIndexItem::One(id) => {
                    return Some(self.get_member(id).into_iter().collect());
                }
                LuaMemberIndexItem::Many(_) => {}
            }
        }

        Some(
            owner_members
                .sorted_member_ids()
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    /// The owner's members whose key is an expression rather than a name, in
    /// the same order [`Self::get_members`] would yield them.
    pub fn get_expr_key_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let owner_members = self.owner_members.get(owner)?;
        let mut member_ids = Vec::new();
        for key in owner_members.expr_keys() {
            match owner_members.get_member(key) {
                Some(LuaMemberIndexItem::One(id)) => member_ids.push(*id),
                Some(LuaMemberIndexItem::Many(ids)) => member_ids.extend(ids.iter().copied()),
                None => {}
            }
        }
        member_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));
        Some(
            member_ids
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    /// The owner's members under one key, in the same order
    /// [`Self::get_members`] would yield them.
    ///
    /// Returns `None` only when the owner has no member map at all, so a
    /// caller can still tell "no such owner" from "owner without that key".
    pub fn get_members_with_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Option<Vec<&LuaMember>> {
        let owner_members = self.owner_members.get(owner)?;
        let mut member_ids = match owner_members.get_member(key) {
            Some(LuaMemberIndexItem::One(id)) => vec![*id],
            Some(LuaMemberIndexItem::Many(ids)) => ids.clone(),
            None => return Some(Vec::new()),
        };
        member_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));
        Some(
            member_ids
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    pub fn get_member_keys<'a>(
        &'a self,
        owner: &LuaMemberOwner,
    ) -> impl Iterator<Item = &'a LuaMemberKey> + 'a {
        self.owner_members
            .get(owner)
            .into_iter()
            .flat_map(LuaOwnerMembers::get_member_keys)
    }

    pub fn get_member_item_by_member_id(
        &self,
        member_id: LuaMemberId,
    ) -> Option<&LuaMemberIndexItem> {
        let owner = self.member_current_owner.get(&member_id)?;
        let member_key = self.members.get(&member_id)?.get_key();
        let member_items = self.owner_members.get(owner)?;
        let item = member_items.get_member(member_key)?;
        Some(item)
    }

    pub fn get_sorted_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        self.get_members(owner)
    }

    pub fn get_member_item(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Option<&LuaMemberIndexItem> {
        self.owner_members
            .get(owner)
            .and_then(|map| map.get_member(key))
    }

    pub fn get_member_len(&self, owner: &LuaMemberOwner) -> usize {
        self.owner_members
            .get(owner)
            .map_or(0, |map| map.get_member_len())
    }

    pub fn get_current_owner(&self, id: &LuaMemberId) -> Option<&LuaMemberOwner> {
        self.member_current_owner.get(id)
    }

    pub fn get_member_owner(&self, id: &LuaMemberId) -> Option<&LuaMemberOwner> {
        self.member_current_owner.get(id)
    }

    pub fn visible_member_count_for_owner_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> usize {
        self.member_owner_key_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
            .map(|member_ids| member_ids.len())
            .unwrap_or(0)
    }

    pub fn has_visible_member_for_owner_key_other_than(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
        excluded_member_id: LuaMemberId,
    ) -> bool {
        let Some(member_ids) = self
            .member_owner_key_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
        else {
            return false;
        };

        match member_ids.as_slice() {
            [] => false,
            [member_id] => *member_id != excluded_member_id,
            _ => true,
        }
    }

    pub fn get_members_for_owner_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Vec<&LuaMember> {
        let Some(owner_items) = self.member_owner_key_index.get(owner) else {
            return Vec::new();
        };
        let Some(member_ids) = owner_items.get(key) else {
            return Vec::new();
        };

        member_ids
            .iter()
            .copied()
            .filter_map(|member_id| {
                self.member_current_owner
                    .get(&member_id)
                    .filter(|current_owner| *current_owner == owner)?;
                self.get_member(&member_id)
            })
            .collect()
    }

    /// Every member ever keyed under `owner`, including ones hidden by the
    /// latest-assignment view and ones since re-homed to a concrete owner.
    pub fn get_member_history(&self, owner: &LuaMemberOwner) -> Vec<&LuaMember> {
        let Some(owner_items) = self.member_owner_key_history_index.get(owner) else {
            return Vec::new();
        };

        let mut member_ids = owner_items.values().flatten().copied().collect::<Vec<_>>();
        member_ids.sort_unstable_by_key(|member_id| member_id_sort_key(*member_id));
        member_ids.dedup();
        member_ids
            .into_iter()
            .filter_map(|member_id| self.get_member(&member_id))
            .collect()
    }

    /// The subset of [`get_member_history`](Self::get_member_history) whose
    /// own global path is `global_id`, in the same order.
    pub fn get_member_history_for_global_path(
        &self,
        owner: &LuaMemberOwner,
        global_id: &GlobalId,
    ) -> Vec<LuaMemberId> {
        // A global path's last segment is the member key it is stored under, so
        // the members declaring it are one bucket of the owner's history rather
        // than all of it. Reading the whole history to filter it built and
        // sorted every member the owner has ever held, once per resolution
        // event, against paths that gain a member per assignment — on a
        // workspace with 24k members under one path that was the single most
        // expensive thing the unresolve phase did.
        let Some(owner_items) = self.member_owner_key_history_index.get(owner) else {
            return Vec::new();
        };
        let name = global_id.get_name();
        let last_segment = name.rsplit_once('.').map_or(name, |(_, last)| last);

        let mut matched = Vec::new();
        let collect = |key: &LuaMemberKey, matched: &mut Vec<LuaMemberId>| {
            let Some(member_ids) = owner_items.get(key) else {
                return;
            };
            matched.extend(member_ids.iter().copied().filter(|member_id| {
                self.get_member(member_id)
                    .and_then(|member| member.get_global_id())
                    == Some(global_id)
            }));
        };
        collect(&LuaMemberKey::Name(last_segment.into()), &mut matched);
        // A numeric field is keyed by its integer, not by its spelling.
        if let Ok(index) = last_segment.parse::<i64>() {
            collect(&LuaMemberKey::Integer(index), &mut matched);
        }

        matched.sort_unstable_by_key(|member_id| member_id_sort_key(*member_id));
        matched.dedup();
        matched
    }

    pub(crate) fn iter_current_owner_keys(
        &self,
    ) -> impl Iterator<Item = (&LuaMemberOwner, &LuaMemberKey)> {
        self.member_owner_key_index
            .iter()
            .flat_map(|(owner, members_by_key)| members_by_key.keys().map(move |key| (owner, key)))
    }

    pub fn add_function_scope_range(&mut self, file_id: FileId, range: TextRange) {
        let ranges = self.function_scope_ranges.entry(file_id).or_default();
        match ranges.binary_search_by_key(&range.start(), |range| range.start()) {
            Ok(index) | Err(index) => ranges.insert(index, range),
        }
    }

    pub fn enclosing_function_scope_range(
        &self,
        file_id: FileId,
        position: TextSize,
    ) -> Option<TextRange> {
        let ranges = self.function_scope_ranges.get(&file_id)?;
        let mut index = ranges.partition_point(|range| range.start() <= position);
        while index > 0 {
            index -= 1;
            let range = ranges[index];
            if range.contains(position) {
                return Some(range);
            }
        }
        None
    }

    pub fn set_member_function_scope_range(
        &mut self,
        member_id: LuaMemberId,
        range: Option<TextRange>,
    ) {
        if let Some(range) = range {
            self.member_function_scope_ranges.insert(member_id, range);
        } else {
            self.member_function_scope_ranges.remove(&member_id);
        }
    }

    pub fn member_function_scope_range(&self, member_id: LuaMemberId) -> Option<TextRange> {
        self.member_function_scope_ranges.get(&member_id).copied()
    }

    pub fn mark_non_overwriting_assignment_member(&mut self, member_id: LuaMemberId) {
        self.non_overwriting_assignment_members.insert(member_id);
    }

    pub fn is_non_overwriting_assignment_member(&self, member_id: LuaMemberId) -> bool {
        self.non_overwriting_assignment_members.contains(&member_id)
    }

    pub fn mark_conditional_branch_assignment_member(&mut self, member_id: LuaMemberId) {
        self.non_overwriting_assignment_members.insert(member_id);
        self.conditional_branch_assignment_members.insert(member_id);
        self.resolve_conditional_branch_owner_key_item(member_id);
    }

    pub fn get_current_owner_members_for_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Vec<&LuaMember> {
        let Some(member_ids) = self
            .member_owner_key_history_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
        else {
            return Vec::new();
        };

        let mut members = member_ids
            .iter()
            .copied()
            .filter_map(|member_id| {
                self.member_current_owner
                    .get(&member_id)
                    .filter(|current_owner| *current_owner == owner)?;
                self.get_member(&member_id)
                    .filter(|member| member.get_key() == key)
            })
            .collect::<Vec<_>>();
        members.sort_by_key(|member| stable_member_sort_key(member));
        members
    }

    /// Returns every still-current member ever indexed under `owner`, including
    /// assignment history entries hidden by the normal latest-value view.
    ///
    /// This is intended for metadata synthesis that must migrate a bounded
    /// owner region wholesale. Ordinary semantic lookup should continue using
    /// `get_members`, which applies runtime overwrite visibility.
    pub fn get_current_owner_member_history(&self, owner: &LuaMemberOwner) -> Vec<&LuaMember> {
        let Some(member_ids) = self.current_owner_member_history.get(owner) else {
            return Vec::new();
        };
        member_ids
            .values()
            .filter_map(|member_id| self.get_member(member_id))
            .collect()
    }

    fn remove_current_owner_member(&mut self, owner: &LuaMemberOwner, member_id: LuaMemberId) {
        let Some(members) = self.current_owner_member_history.get_mut(owner) else {
            return;
        };
        members.remove(&member_id_sort_key(member_id));
        if members.is_empty() {
            self.current_owner_member_history.remove(owner);
        }
    }

    pub fn get_current_members_for_key(&self, key: &LuaMemberKey) -> Vec<&LuaMember> {
        let Some(member_ids) = self.current_members_by_key.get(key) else {
            return Vec::new();
        };

        member_ids
            .values()
            .filter_map(|member_id| self.get_member(member_id))
            .collect()
    }

    fn add_current_member_key(&mut self, member_id: LuaMemberId) {
        let Some(key) = self
            .get_member(&member_id)
            .map(|member| member.get_key().clone())
        else {
            return;
        };
        self.current_members_by_key
            .entry(key)
            .or_default()
            .insert(member_id_sort_key(member_id), member_id);
    }

    fn remove_current_member_key(&mut self, member_id: LuaMemberId) {
        let Some(key) = self
            .get_member(&member_id)
            .map(|member| member.get_key().clone())
        else {
            return;
        };
        let Some(members) = self.current_members_by_key.get_mut(&key) else {
            return;
        };
        members.remove(&member_id_sort_key(member_id));
        if members.is_empty() {
            self.current_members_by_key.remove(&key);
        }
    }

    pub fn get_file_members(&self, file_id: FileId) -> Vec<&LuaMember> {
        let Some(member_or_owners) = self.in_filed.get(&file_id) else {
            return Vec::new();
        };

        member_or_owners
            .iter()
            .filter_map(|entry| match entry {
                MemberOrOwner::Member(member_id) => self.get_member(member_id),
                MemberOrOwner::Owner(_) => None,
            })
            .collect()
    }

    pub fn retain_only_member_for_owner_key(&mut self, member_id: LuaMemberId) -> Option<()> {
        let owner = self.member_current_owner.get(&member_id)?.clone();
        let key = self.get_member(&member_id)?.get_key().clone();
        let member_ids = self.member_owner_key_index.get(&owner)?.get(&key)?;
        if !member_ids
            .iter()
            .copied()
            .all(|id| self.is_assignment_file_define_member(id))
        {
            return Some(());
        }

        let member_ids = self.member_owner_key_index.get_mut(&owner)?.get_mut(&key)?;
        member_ids.retain(|id| *id == member_id);
        Some(())
    }

    pub fn preserve_members_for_owner_key(
        &mut self,
        member_id: LuaMemberId,
        member_ids: Vec<LuaMemberId>,
    ) -> Option<()> {
        let owner = self.member_current_owner.get(&member_id)?.clone();
        let key = self.get_member(&member_id)?.get_key().clone();
        let mut preserved_member_ids = Vec::with_capacity(member_ids.len());

        for id in member_ids {
            if self.member_current_owner.get(&id) != Some(&owner) {
                continue;
            }
            let Some(member) = self.get_member(&id) else {
                continue;
            };
            if member.get_key() != &key || preserved_member_ids.contains(&id) {
                continue;
            }

            preserved_member_ids.push(id);
        }

        let item = match preserved_member_ids.as_slice() {
            [] => return Some(()),
            [id] => LuaMemberIndexItem::One(*id),
            _ => LuaMemberIndexItem::Many(preserved_member_ids.clone()),
        };

        self.member_owner_key_index
            .entry(owner.clone())
            .or_default()
            .insert(key.clone(), preserved_member_ids);
        self.owner_members
            .entry(owner)
            .or_insert_with(LuaOwnerMembers::new)
            .add_member(key, item);

        Some(())
    }
}

fn stable_member_sort_key(member: &LuaMember) -> (u32, u32, u32, u16) {
    let member_id = member.get_id();
    member_id_sort_key(member_id)
}

// The owner-level sorted member-id cache depends on these file id, position,
// range end, and kind components remaining immutable for a member's lifetime.
pub(crate) fn member_id_sort_key(member_id: LuaMemberId) -> (u32, u32, u32, u16) {
    let syntax_id = member_id.get_syntax_id();
    (
        member_id.file_id.id,
        u32::from(member_id.get_position()),
        u32::from(syntax_id.get_range().end()),
        syntax_id.get_kind() as u16,
    )
}

fn sorted_member_pair(first: LuaMemberId, second: LuaMemberId) -> Vec<LuaMemberId> {
    if member_id_sort_key(first) <= member_id_sort_key(second) {
        vec![first, second]
    } else {
        vec![second, first]
    }
}

impl LuaMemberIndex {
    /// The per-file half of removal: erase every entry keyed or owned by
    /// `file_id`. The whole-index owner-key sweeps are done once per batch in
    /// `remove_files`, not here — running them per file made removal cost
    /// O(files × index) and dominated incremental edits of high fan-in files.
    fn remove_file_owned_entries(&mut self, file_id: FileId) {
        if let Some(member_ids) = self.in_filed.remove(&file_id) {
            let mut owners = HashSet::default();
            for member_id_or_owner in member_ids {
                match member_id_or_owner {
                    MemberOrOwner::Member(member_id) => {
                        if let Some(owner) = self.member_current_owner.get(&member_id).cloned() {
                            self.remove_member_from_all_owner_key_indexes(&owner, member_id);
                            self.remove_current_owner_member(&owner, member_id);
                            self.remove_current_member_key(member_id);
                        }
                        self.members.remove(&member_id);
                        self.member_current_owner.remove(&member_id);
                        self.non_overwriting_assignment_members.remove(&member_id);
                        self.conditional_branch_assignment_members
                            .remove(&member_id);
                        self.synthesized_owner_members.remove(&member_id);
                        self.deferred_index_expr_members.remove(&member_id);
                        self.member_function_scope_ranges.remove(&member_id);
                    }
                    MemberOrOwner::Owner(owner) => {
                        owners.insert(owner);
                    }
                }
            }

            let mut need_removed_owner = Vec::new();
            for owner in owners {
                if let Some(member_items) = self.owner_members.get_mut(&owner) {
                    let mut need_removed_key = Vec::new();
                    for (key, item) in member_items.iter_mut() {
                        match item {
                            LuaMemberIndexItem::One(id) => {
                                if id.file_id == file_id {
                                    need_removed_key.push(key.clone());
                                }
                            }
                            LuaMemberIndexItem::Many(ids) => {
                                ids.retain(|id| id.file_id != file_id);
                                if ids.is_empty() {
                                    need_removed_key.push(key.clone());
                                }
                            }
                        }
                    }

                    for key in need_removed_key {
                        member_items.remove_member(&key);
                    }

                    if member_items.is_empty() {
                        need_removed_owner.push(owner);
                    }
                }
            }

            for owner in need_removed_owner {
                self.owner_members.remove(&owner);
            }
        }
        self.function_scope_ranges.remove(&file_id);
    }
}

impl LuaIndex for LuaMemberIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(&[file_id]);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            self.remove_file_owned_entries(file_id);
        }
        let removed: HashSet<FileId> = file_ids.iter().copied().collect();
        self.remove_files_members_from_owner_key_indexes(&removed);
        self.assignment_contributions.remove_files(&removed);
        self.member_function_scope_ranges
            .retain(|member_id, _| !removed.contains(&member_id.file_id));
    }

    fn clear(&mut self) {
        self.members.clear();
        self.in_filed.clear();
        self.owner_members.clear();
        self.member_current_owner.clear();
        self.member_owner_key_index.clear();
        self.member_owner_key_history_index.clear();
        self.current_owner_member_history.clear();
        self.current_members_by_key.clear();
        self.non_overwriting_assignment_members.clear();
        self.conditional_branch_assignment_members.clear();
        self.synthesized_owner_members.clear();
        self.deferred_index_expr_members.clear();
        self.function_scope_ranges.clear();
        self.member_function_scope_ranges.clear();
        self.assignment_contributions.clear();
    }
}

/// Picks the definition that wins when a key is redefined without a guard.
fn latest_defined_member(existing_ids: &[LuaMemberId], incoming_id: LuaMemberId) -> LuaMemberId {
    existing_ids
        .iter()
        .copied()
        .chain(std::iter::once(incoming_id))
        .max_by_key(|candidate| member_id_sort_key(*candidate))
        .unwrap_or(incoming_id)
}

fn member_ids_from_item(item: &LuaMemberIndexItem) -> Vec<LuaMemberId> {
    match item {
        LuaMemberIndexItem::One(id) => vec![*id],
        LuaMemberIndexItem::Many(ids) => ids.clone(),
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::*;
    use crate::{FileId, LuaTypeDeclId};

    fn make_member(member_id: LuaMemberId, key: &str) -> LuaMember {
        make_member_with_feature(member_id, key, LuaMemberFeature::FileFieldDecl)
    }

    fn make_member_with_feature(
        member_id: LuaMemberId,
        key: &str,
        feature: LuaMemberFeature,
    ) -> LuaMember {
        LuaMember::new(member_id, LuaMemberKey::Name(key.into()), feature, None)
    }

    fn make_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range),
            file_id,
        )
    }

    fn make_index_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range),
            file_id,
        )
    }

    fn make_member_id_with_kind_and_end(
        file_id: FileId,
        kind: LuaSyntaxKind,
        start: u32,
        end: u32,
    ) -> LuaMemberId {
        LuaMemberId::new(
            LuaSyntaxId::new(
                kind.into(),
                TextRange::new(TextSize::new(start), TextSize::new(end)),
            ),
            file_id,
        )
    }

    fn owner_member_ids(index: &LuaMemberIndex, owner: &LuaMemberOwner) -> Vec<LuaMemberId> {
        index
            .get_members(owner)
            .expect("owner should exist")
            .into_iter()
            .map(|member| member.get_id())
            .collect()
    }

    #[test]
    fn get_members_multi_key_owner_matches_member_id_sort_order() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let member_specs = [
            (
                make_member_id_with_kind_and_end(FileId::new(3), LuaSyntaxKind::NameExpr, 20, 24),
                "gamma",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(1), LuaSyntaxKind::IndexExpr, 40, 45),
                "alpha",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(1), LuaSyntaxKind::NameExpr, 10, 12),
                "beta",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(2), LuaSyntaxKind::NameExpr, 5, 6),
                "delta",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(1), LuaSyntaxKind::IndexExpr, 10, 11),
                "epsilon",
            ),
        ];

        let mut index = LuaMemberIndex::new();
        for (member_id, key) in member_specs {
            index.add_member(owner.clone(), make_member(member_id, key));
        }

        let mut expected_ids = member_specs.map(|(member_id, _)| member_id).to_vec();
        expected_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));

        assert_eq!(owner_member_ids(&index, &owner), expected_ids);
    }

    #[test]
    fn batch_removal_matches_sequential_removal_for_surviving_members() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("BatchRemoveType"));
        let first = FileId::new(1);
        let second = FileId::new(2);
        let survivor = FileId::new(3);

        let populate = || {
            let mut index = LuaMemberIndex::new();
            index.add_member(
                owner.clone(),
                make_member(make_member_id(first, 10), "alpha"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(second, 20), "alpha"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(second, 30), "beta"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(survivor, 40), "alpha"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(survivor, 50), "gamma"),
            );
            index
        };

        let mut sequential = populate();
        sequential.remove(first);
        sequential.remove(second);

        let mut batched = populate();
        batched.remove_files(&[second, first, second]);

        assert_eq!(
            owner_member_ids(&sequential, &owner),
            owner_member_ids(&batched, &owner)
        );
        assert_eq!(
            owner_member_ids(&batched, &owner),
            vec![make_member_id(survivor, 40), make_member_id(survivor, 50)]
        );
        for index in [&sequential, &batched] {
            assert!(index.get_file_members(first).is_empty());
            assert!(index.get_file_members(second).is_empty());
            assert!(index.get_member(&make_member_id(first, 10)).is_none());
            assert!(index.get_member(&make_member_id(second, 20)).is_none());
            assert!(index.get_member(&make_member_id(survivor, 40)).is_some());
        }
        assert_eq!(
            sequential.member_owner_key_index,
            batched.member_owner_key_index
        );
        assert_eq!(
            sequential.member_owner_key_history_index,
            batched.member_owner_key_history_index
        );
    }

    #[test]
    fn get_members_cache_invalidates_when_adding_earlier_member_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let first_member_id = make_member_id(FileId::new(4), 20);
        let second_member_id = make_member_id(FileId::new(5), 30);
        let earlier_member_id = make_member_id(FileId::new(1), 5);
        let mut index = LuaMemberIndex::new();

        index.add_member(owner.clone(), make_member(first_member_id, "first"));
        index.add_member(owner.clone(), make_member(second_member_id, "second"));

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![first_member_id, second_member_id]
        );

        index.add_member(owner.clone(), make_member(earlier_member_id, "third"));

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![earlier_member_id, first_member_id, second_member_id]
        );
    }

    #[test]
    fn set_member_owner_moves_member_between_owner_indexes() {
        let file_id = FileId::new(1);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OldOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("NewOwner"));
        let key = LuaMemberKey::Name("field".into());
        let member_id = make_member_id(file_id, 1);

        let mut index = LuaMemberIndex::new();
        index.add_member(old_owner.clone(), make_member(member_id, "field"));
        assert!(index.get_member_item(&old_owner, &key).is_some());

        index
            .set_member_owner(new_owner.clone(), file_id, member_id)
            .expect("owner reassignment should succeed");

        assert!(index.get_member_item(&old_owner, &key).is_some());
        assert!(index.get_member_item(&new_owner, &key).is_none());
        assert!(index.get_members_for_owner_key(&old_owner, &key).is_empty());
        assert_eq!(index.get_members_for_owner_key(&new_owner, &key).len(), 1);
        assert!(
            index
                .get_current_owner_member_history(&old_owner)
                .is_empty()
        );
        assert_eq!(
            index
                .get_current_owner_member_history(&new_owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
    }

    #[test]
    fn set_member_owner_keeps_other_old_owner_members() {
        let file_id = FileId::new(2);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OriginalOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("ReassignedOwner"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_member_id(file_id, 1);
        let second_member_id = make_member_id(file_id, 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(old_owner.clone(), make_member(first_member_id, "field"));
        index.add_member(old_owner.clone(), make_member(second_member_id, "field"));

        index
            .set_member_owner(new_owner.clone(), file_id, first_member_id)
            .expect("owner reassignment should succeed");

        assert_eq!(index.get_members_for_owner_key(&old_owner, &key).len(), 1);
        assert_eq!(index.get_members_for_owner_key(&new_owner, &key).len(), 1);

        let old_owner_member_ids = index
            .get_members_for_owner_key(&old_owner, &key)
            .iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(old_owner_member_ids, vec![second_member_id]);

        let new_owner_member_ids = index
            .get_members_for_owner_key(&new_owner, &key)
            .iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(new_owner_member_ids, vec![first_member_id]);
        assert_eq!(
            index
                .get_current_owner_member_history(&old_owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![second_member_id]
        );
        assert_eq!(
            index
                .get_current_owner_member_history(&new_owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id]
        );

        let new_owner_history_member_ids = index
            .get_current_owner_members_for_key(&new_owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(new_owner_history_member_ids, vec![first_member_id]);

        let key_history_member_ids = index
            .get_current_members_for_key(&key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(
            key_history_member_ids,
            vec![first_member_id, second_member_id]
        );
    }

    #[test]
    fn get_members_cache_invalidates_when_removing_file_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let removed_member_id = make_member_id(FileId::new(4), 10);
        let retained_member_id = make_member_id(FileId::new(5), 5);
        let mut index = LuaMemberIndex::new();

        index.add_member(owner.clone(), make_member(removed_member_id, "removed"));
        index.add_member(owner.clone(), make_member(retained_member_id, "retained"));

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![removed_member_id, retained_member_id]
        );

        index.remove(FileId::new(4));

        assert_eq!(owner_member_ids(&index, &owner), vec![retained_member_id]);
    }

    #[test]
    fn warming_get_members_does_not_break_owner_move_visibility() {
        let file_id = FileId::new(6);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OldOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("NewOwner"));
        let key = LuaMemberKey::Name("field".into());
        let member_id = make_member_id(file_id, 1);
        let mut index = LuaMemberIndex::new();

        index.add_member(old_owner.clone(), make_member(member_id, "field"));
        assert_eq!(owner_member_ids(&index, &old_owner), vec![member_id]);
        assert!(index.get_members(&new_owner).is_none());

        index
            .set_member_owner(new_owner.clone(), file_id, member_id)
            .expect("owner reassignment should succeed");

        assert!(index.get_members_for_owner_key(&old_owner, &key).is_empty());
        assert_eq!(
            index
                .get_members_for_owner_key(&new_owner, &key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
    }

    #[test]
    fn get_members_cache_invalidates_when_one_promotes_to_many_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_member_id(FileId::new(4), 20);
        let second_member_id = make_member_id(FileId::new(4), 10);
        let mut index = LuaMemberIndex::new();

        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        assert_eq!(owner_member_ids(&index, &owner), vec![first_member_id]);

        index.add_member(
            owner.clone(),
            LuaMember::new(second_member_id, key, LuaMemberFeature::FileFieldDecl, None),
        );

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![second_member_id, first_member_id]
        );
    }

    #[test]
    fn clear_resets_member_owner_tracking() {
        let file_id = FileId::new(3);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let member_id = make_member_id(file_id, 7);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner, make_member(member_id, "field"));
        assert!(index.get_member_owner(&member_id).is_some());
        assert!(
            !index
                .get_current_members_for_key(&LuaMemberKey::Name("field".into()))
                .is_empty()
        );

        index.clear();

        assert!(index.get_member_owner(&member_id).is_none());
        assert!(
            index
                .get_current_members_for_key(&LuaMemberKey::Name("field".into()))
                .is_empty()
        );
    }

    #[test]
    fn key_lookup_cache_invalidates_after_member_mutation() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_member_id(FileId::new(9), 1);
        let second_member_id = make_member_id(FileId::new(10), 3);
        let mut index = LuaMemberIndex::new();

        index.add_member(owner.clone(), make_member(first_member_id, "field"));
        assert_eq!(
            index
                .get_current_members_for_key(&key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id]
        );

        index.add_member(owner.clone(), make_member(second_member_id, "field"));
        assert_eq!(
            index
                .get_current_members_for_key(&key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id, second_member_id]
        );

        index.remove(FileId::new(9));
        assert_eq!(
            index
                .get_current_members_for_key(&key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![second_member_id]
        );
    }

    #[test]
    fn file_define_assignment_history_stays_visible_for_owner_key_queries() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(1), TextSize::new(2)),
            ),
            FileId::new(4),
        );
        let second_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(3), TextSize::new(4)),
            ),
            FileId::new(5),
        );

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::One(second_member_id))
        );
        let member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(member_ids, vec![first_member_id, second_member_id]);
    }

    #[test]
    fn retained_file_define_keeps_owner_key_history() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(1), TextSize::new(2)),
            ),
            FileId::new(4),
        );
        let second_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(3), TextSize::new(4)),
            ),
            FileId::new(4),
        );

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        index
            .retain_only_member_for_owner_key(second_member_id)
            .expect("retain should succeed");

        let visible_member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(visible_member_ids, vec![second_member_id]);

        let history_member_ids = index
            .get_current_owner_members_for_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(history_member_ids, vec![first_member_id, second_member_id]);
    }

    #[test]
    fn visible_owner_key_other_member_check_uses_current_visible_members() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_index_member_id(FileId::new(4), 1);
        let second_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert!(!index.has_visible_member_for_owner_key_other_than(&owner, &key, first_member_id));

        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert!(index.has_visible_member_for_owner_key_other_than(&owner, &key, second_member_id));
    }

    #[test]
    fn meta_only_member_item_preserves_meta_when_assignment_is_added() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let meta_member_id = make_member_id(FileId::new(4), 1);
        let assignment_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(meta_member_id, "field", LuaMemberFeature::MetaFieldDecl),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(assignment_member_id, "field", LuaMemberFeature::FileDefine),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                assignment_member_id,
                meta_member_id,
            ]))
        );
    }

    #[test]
    fn retain_only_member_for_owner_key_keeps_mixed_visible_members() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let declaration_member_id = make_member_id(FileId::new(4), 1);
        let assignment_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(
                declaration_member_id,
                "field",
                LuaMemberFeature::FileFieldDecl,
            ),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(assignment_member_id, "field", LuaMemberFeature::FileDefine),
        );

        index
            .retain_only_member_for_owner_key(assignment_member_id)
            .expect("retain should no-op for mixed visible members");

        let visible_member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(
            visible_member_ids,
            vec![declaration_member_id, assignment_member_id]
        );
    }

    #[test]
    fn preserve_members_for_owner_key_filters_dedups_and_updates_visible_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let other_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OtherOwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_index_member_id(FileId::new(4), 1);
        let second_member_id = make_index_member_id(FileId::new(4), 3);
        let other_owner_member_id = make_index_member_id(FileId::new(5), 1);
        let other_key_member_id = make_index_member_id(FileId::new(4), 5);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_member_id, second_member_id] {
            index.add_member(
                owner.clone(),
                make_member_with_feature(member_id, "field", LuaMemberFeature::FileDefine),
            );
        }
        index.add_member(
            other_owner,
            make_member_with_feature(other_owner_member_id, "field", LuaMemberFeature::FileDefine),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(other_key_member_id, "other", LuaMemberFeature::FileDefine),
        );

        index
            .preserve_members_for_owner_key(
                first_member_id,
                vec![
                    second_member_id,
                    other_owner_member_id,
                    first_member_id,
                    second_member_id,
                    other_key_member_id,
                ],
            )
            .expect("preserve should succeed");

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                second_member_id,
                first_member_id,
            ]))
        );
        let visible_member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(visible_member_ids, vec![second_member_id, first_member_id]);
    }

    #[test]
    fn preserved_assignment_duplicate_insertions_keep_existing_item_order() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_index_member_id(FileId::new(4), 1);
        let second_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_member_id, second_member_id] {
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                make_member_with_feature(member_id, "field", LuaMemberFeature::FileDefine),
            );
        }

        index.merge_member_into_owner_item(owner.clone(), key.clone(), second_member_id);
        index.merge_member_into_owner_item(owner.clone(), key.clone(), first_member_id);

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                first_member_id,
                second_member_id,
            ]))
        );
    }

    #[test]
    fn alias_merge_does_not_duplicate_an_id_already_in_an_unsorted_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let earlier_member_id = make_member_id(FileId::new(1), 10);
        let later_member_id = make_member_id(FileId::new(2), 20);

        // Decl inserts append in arrival order, so adding the later id first
        // leaves the stored item unsorted.
        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_member(later_member_id, "field"));
        index.add_member(owner.clone(), make_member(earlier_member_id, "field"));
        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                later_member_id,
                earlier_member_id,
            ]))
        );

        index.add_member_alias_to_owner(owner.clone(), later_member_id);

        let Some(LuaMemberIndexItem::Many(member_ids)) = index.get_member_item(&owner, &key) else {
            panic!("the item should still hold both members");
        };
        assert_eq!(
            member_ids.len(),
            2,
            "aliasing an id already in the item must not add it again"
        );
        assert_eq!(owner_member_ids(&index, &owner).len(), 2);
    }

    #[test]
    fn alias_adds_to_an_existing_file_define_without_displacing_it() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("SharedTable"));
        let other_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OtherTable"));
        let key = LuaMemberKey::Name("field".into());
        let own_member_id = make_index_member_id(FileId::new(1), 10);
        let aliased_member_id = make_index_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(own_member_id, "field", LuaMemberFeature::FileDefine),
        );
        index.add_member(
            other_owner,
            make_member_with_feature(aliased_member_id, "field", LuaMemberFeature::FileDefine),
        );

        index.add_member_alias_to_owner(owner.clone(), aliased_member_id);

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                own_member_id,
                aliased_member_id,
            ])),
            "an alias only ever adds; it must never replace the owner's own writer"
        );
    }

    #[test]
    fn global_path_key_keeps_every_writer_in_history_while_one_wins_the_visible_slot() {
        let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new("cityrp"));
        let first_member_id = make_member_id(FileId::new(1), 10);
        let second_member_id = make_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_member_id, second_member_id] {
            index.add_member(
                owner.clone(),
                make_member_with_feature(member_id, "menu", LuaMemberFeature::FileDefine),
            );
        }

        assert_eq!(
            index
                .get_member_history(&owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id, second_member_id],
            "history must enumerate every file that wrote the key"
        );
        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![second_member_id],
            "the visible slot is still last-writer-wins"
        );
    }

    /// `cityrp.progresshud = {}` is written by both `cl_progress_hud.lua` and
    /// `sv_progress_hud.lua`. Only one can hold the visible slot, and the
    /// global-path reconciliation re-homes whichever one that is onto the
    /// elected table — so if arrival decided the survivor, a re-index moved the
    /// member's owner on unchanged source.
    #[test]
    fn cross_file_assignment_writers_elect_the_visible_slot_by_source_order() {
        let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new("cityrp"));
        let earlier_member_id = make_index_member_id(FileId::new(1), 10);
        let later_member_id = make_index_member_id(FileId::new(2), 20);

        for arrival in [
            [earlier_member_id, later_member_id],
            [later_member_id, earlier_member_id],
        ] {
            let mut index = LuaMemberIndex::new();
            for member_id in arrival {
                index.add_member(
                    owner.clone(),
                    make_member_with_feature(
                        member_id,
                        "progresshud",
                        LuaMemberFeature::FileDefine,
                    ),
                );
            }

            assert_eq!(
                owner_member_ids(&index, &owner),
                vec![later_member_id],
                "the visible writer must not depend on which file was analysed first"
            );
        }
    }

    #[test]
    fn marked_non_overwriting_file_defines_share_lookup_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(1), TextSize::new(2)),
            ),
            FileId::new(4),
        );
        let second_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(3), TextSize::new(4)),
            ),
            FileId::new(4),
        );

        let mut index = LuaMemberIndex::new();
        index.mark_non_overwriting_assignment_member(first_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.mark_non_overwriting_assignment_member(second_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                first_member_id,
                second_member_id
            ]))
        );
    }

    #[test]
    fn preserved_assignment_insertion_invalidates_get_members_cache_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let later_member_id = make_index_member_id(FileId::new(4), 30);
        let earlier_member_id = make_index_member_id(FileId::new(4), 10);
        let mut index = LuaMemberIndex::new();

        index.mark_non_overwriting_assignment_member(later_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                later_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        assert_eq!(owner_member_ids(&index, &owner), vec![later_member_id]);

        index.mark_non_overwriting_assignment_member(earlier_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(earlier_member_id, key, LuaMemberFeature::FileDefine, None),
        );

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![earlier_member_id, later_member_id]
        );
    }

    #[test]
    fn many_marked_non_overwriting_file_defines_share_lookup_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let mut index = LuaMemberIndex::new();

        for i in 0..128 {
            let member_id = LuaMemberId::new(
                LuaSyntaxId::new(
                    LuaSyntaxKind::IndexExpr.into(),
                    TextRange::new(TextSize::new(i), TextSize::new(i + 1)),
                ),
                FileId::new(4),
            );
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
            );
        }

        let Some(LuaMemberIndexItem::Many(member_ids)) = index.get_member_item(&owner, &key) else {
            panic!("marked assignments should be preserved as a shared lookup item");
        };

        assert_eq!(member_ids.len(), 128);
        assert_eq!(
            member_ids.first().copied(),
            Some(make_index_member_id(FileId::new(4), 0))
        );
        assert_eq!(
            member_ids.last().copied(),
            Some(make_index_member_id(FileId::new(4), 127))
        );
    }

    #[test]
    fn marked_non_overwriting_file_defines_keep_stable_order_when_added_out_of_order() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let mut index = LuaMemberIndex::new();

        for start in [30, 10, 20] {
            let member_id = make_index_member_id(FileId::new(4), start);
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
            );
        }

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                make_index_member_id(FileId::new(4), 10),
                make_index_member_id(FileId::new(4), 20),
                make_index_member_id(FileId::new(4), 30),
            ]))
        );
    }

    #[test]
    fn marked_non_overwriting_file_define_does_not_preserve_unmarked_assignment() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let class_assignment_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(1), TextSize::new(2)),
            ),
            FileId::new(4),
        );
        let guarded_assignment_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(3), TextSize::new(4)),
            ),
            FileId::new(4),
        );

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                class_assignment_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.mark_non_overwriting_assignment_member(guarded_assignment_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                guarded_assignment_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::One(guarded_assignment_id))
        );
    }

    /// A guarded bootstrap in one file and a plain assignment in another is
    /// the order-dependent case: whichever was processed last used to take the
    /// visible slot, so the surviving writer followed load order. The
    /// bootstrap contributes no type of its own, so the plain writer must win
    /// either way.
    #[test]
    fn cross_file_bootstrap_never_displaces_a_plain_writer() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("stock".into());
        let plain_id = make_index_member_id(FileId::new(1), 10);
        let bootstrap_id = make_index_member_id(FileId::new(2), 20);

        let visible_after = |bootstrap_first: bool| {
            let mut index = LuaMemberIndex::new();
            let order = if bootstrap_first {
                [bootstrap_id, plain_id]
            } else {
                [plain_id, bootstrap_id]
            };
            for member_id in order {
                if member_id == bootstrap_id {
                    index.mark_non_overwriting_assignment_member(member_id);
                }
                index.add_member(
                    owner.clone(),
                    LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
                );
            }
            owner_member_ids(&index, &owner)
        };

        assert_eq!(
            visible_after(false),
            vec![plain_id],
            "plain writer analysed first"
        );
        assert_eq!(
            visible_after(true),
            vec![plain_id],
            "bootstrap analysed first"
        );
    }

    /// Transparency only applies when a real writer exists. With nothing but
    /// bootstraps across files there is no placeholder to see through, so the
    /// existing merge still has to keep every writer visible.
    #[test]
    fn cross_file_all_bootstrap_writers_still_merge() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("stock".into());
        let first_id = make_index_member_id(FileId::new(1), 10);
        let second_id = make_index_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_id, second_id] {
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
            );
        }

        assert_eq!(owner_member_ids(&index, &owner), vec![first_id, second_id]);
    }

    #[test]
    fn file_removal_clears_previous_owner_history_entries() {
        let file_id = FileId::new(6);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OldOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("NewOwner"));
        let old_key = LuaMemberKey::Name("old_field".into());
        let new_key = LuaMemberKey::Name("new_field".into());
        let member_id = make_member_id(file_id, 10);
        let mut index = LuaMemberIndex::new();

        index.add_member(old_owner.clone(), make_member(member_id, "old_field"));
        index
            .set_member_owner(new_owner, file_id, member_id)
            .expect("owner reassignment should succeed");
        index.remove(file_id);
        index.add_member(old_owner.clone(), make_member(member_id, "new_field"));

        assert!(
            index
                .get_current_owner_members_for_key(&old_owner, &old_key)
                .is_empty()
        );
        assert!(index.get_current_members_for_key(&old_key).is_empty());
        assert_eq!(
            index
                .get_current_owner_members_for_key(&old_owner, &new_key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
        assert_eq!(
            index
                .get_current_members_for_key(&new_key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
    }

    #[test]
    fn function_scope_lookup_returns_innermost_range() {
        let file_id = FileId::new(7);
        let outer = TextRange::new(TextSize::new(10), TextSize::new(100));
        let inner = TextRange::new(TextSize::new(30), TextSize::new(60));
        let mut index = LuaMemberIndex::new();

        index.add_function_scope_range(file_id, outer);
        index.add_function_scope_range(file_id, inner);

        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(40)),
            Some(inner)
        );
        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(80)),
            Some(outer)
        );
        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(5)),
            None
        );
    }

    #[test]
    fn file_removal_clears_function_scope_metadata() {
        let file_id = FileId::new(8);
        let range = TextRange::new(TextSize::new(10), TextSize::new(100));
        let member_id = make_member_id(file_id, 20);
        let mut index = LuaMemberIndex::new();

        index.add_function_scope_range(file_id, range);
        index.set_member_function_scope_range(member_id, Some(range));
        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(20)),
            Some(range)
        );
        assert_eq!(index.member_function_scope_range(member_id), Some(range));

        index.remove(file_id);

        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(20)),
            None
        );
        assert_eq!(index.member_function_scope_range(member_id), None);

        index.add_function_scope_range(file_id, range);
        index.set_member_function_scope_range(member_id, Some(range));
        index.clear();

        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(20)),
            None
        );
        assert_eq!(index.member_function_scope_range(member_id), None);
    }
}
