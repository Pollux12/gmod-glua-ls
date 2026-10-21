use std::sync::OnceLock;

use rustc_hash::FxHashMap as HashMap;

use super::member_id_sort_key;
use crate::db_index::cow_shards::{Merged, merge_list};
use crate::db_index::derivation::{Fingerprint, fingerprint_of, mapping_back};
use crate::{LuaMemberId, LuaMemberIndexItem, LuaMemberKey};

#[allow(unused)]
#[derive(Debug, Clone)]
pub struct LuaOwnerMembers {
    members: HashMap<LuaMemberKey, LuaMemberIndexItem>,
    // `members` is private and these four mutators are the complete id-set
    // invalidation surface: `add_member`, `get_member_mut`, `iter_mut`, and
    // `remove_member`.
    sorted_ids_cache: OnceLock<Vec<LuaMemberId>>,
    sorted_expr_ids_cache: OnceLock<Vec<LuaMemberId>>,
    /// The subset of `members` keyed by an expression rather than a name.
    ///
    /// Only `add_member` and `remove_member` change the key set; the mutators
    /// that hand out an item leave keys alone.
    expr_keys: Vec<LuaMemberKey>,
    resolve_state: OwnerMemberStatus,
    /// Wrapping sum of [`listed_id_fingerprint`] over every listed
    /// `(key, id)`. `add_member`, `remove_member` and `remove_id` keep it;
    /// callers of `get_member_mut`/`iter_mut` report their edits through
    /// `note_id_added`/`note_id_removed`.
    fingerprint: Fingerprint,
}

/// Two owners' members are equal when they list the same members, whatever
/// each has cached of them.
impl PartialEq for LuaOwnerMembers {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.resolve_state == other.resolve_state
            && self.expr_keys == other.expr_keys
            && self.members == other.members
    }
}

/// The ids listed under one key, which the index keeps as a set: the
/// index's, less those the fork unlisted, with those it listed.
fn merge_item(
    current: &mut Option<LuaMemberIndexItem>,
    frozen: Option<&LuaMemberIndexItem>,
    fork: Option<&LuaMemberIndexItem>,
) -> Merged {
    fn ids(item: Option<&LuaMemberIndexItem>) -> Vec<LuaMemberId> {
        match item {
            None => Vec::new(),
            Some(LuaMemberIndexItem::One(id)) => vec![*id],
            Some(LuaMemberIndexItem::Many(ids)) => ids.clone(),
        }
    }
    let (frozen, fork) = (ids(frozen), ids(fork));
    let mut merged = ids(current.as_ref());
    merged.retain(|id| !frozen.contains(id) || fork.contains(id));
    for id in fork {
        if !frozen.contains(&id) && !merged.contains(&id) {
            merged.push(id);
        }
    }
    merged.sort_by_key(|id| member_id_sort_key(*id));
    *current = match merged.as_slice() {
        [] => None,
        [id] => Some(LuaMemberIndexItem::One(*id)),
        _ => Some(LuaMemberIndexItem::Many(merged)),
    };
    Ok(())
}

fn listed_id_fingerprint(key: &LuaMemberKey, id: LuaMemberId) -> Fingerprint {
    fingerprint_of(&(key, id))
}

fn item_fingerprint(key: &LuaMemberKey, item: &LuaMemberIndexItem) -> Fingerprint {
    match item {
        LuaMemberIndexItem::One(id) => listed_id_fingerprint(key, *id),
        LuaMemberIndexItem::Many(ids) => ids.iter().fold(0, |sum: Fingerprint, id| {
            sum.wrapping_add(listed_id_fingerprint(key, *id))
        }),
    }
}

#[allow(unused)]
impl LuaOwnerMembers {
    pub fn new() -> Self {
        Self {
            members: HashMap::default(),
            sorted_ids_cache: OnceLock::new(),
            sorted_expr_ids_cache: OnceLock::new(),
            expr_keys: Vec::new(),
            resolve_state: OwnerMemberStatus::UnResolved,
            fingerprint: 0,
        }
    }

    pub fn add_member(&mut self, key: LuaMemberKey, item: LuaMemberIndexItem) {
        self.invalidate_sorted_member_ids();
        match self.members.get(&key) {
            Some(old) => {
                self.fingerprint = self.fingerprint.wrapping_sub(item_fingerprint(&key, old));
            }
            None if key.is_expr() => self.expr_keys.push(key.clone()),
            None => {}
        }
        self.fingerprint = self.fingerprint.wrapping_add(item_fingerprint(&key, &item));
        self.members.insert(key, item);
    }

    /// Merges two tasks' changes to one owner's members key by key. An absent
    /// owner has no members, and an emptied one is absent, as the index
    /// prunes it.
    pub(crate) fn merge(
        current: &mut Option<Self>,
        frozen: Option<&Self>,
        fork: Option<&Self>,
    ) -> Merged {
        let empty = Self::new();
        let frozen = frozen.unwrap_or(&empty);
        let fork = fork.unwrap_or(&empty);
        let merged = current.get_or_insert_with(Self::new);
        let changed = fork
            .members
            .keys()
            .filter(|key| fork.members.get(*key) != frozen.members.get(*key))
            .chain(
                frozen
                    .members
                    .keys()
                    .filter(|key| !fork.members.contains_key(*key)),
            );
        for key in changed {
            let (before, value) = (frozen.members.get(key), fork.members.get(key));
            let now = merged.members.get(key);
            if now == value {
                continue;
            }
            let mut item = merged.members.remove(key);
            if let Some(old) = &item {
                merged.fingerprint = merged.fingerprint.wrapping_sub(item_fingerprint(key, old));
            }
            if item.as_ref() == before {
                item = value.cloned();
            } else {
                merge_item(&mut item, before, value)?;
            }
            if let Some(item) = item {
                merged.fingerprint = merged
                    .fingerprint
                    .wrapping_add(item_fingerprint(key, &item));
                merged.members.insert(key.clone(), item);
            }
        }
        let mut expr_keys = Some(std::mem::take(&mut merged.expr_keys));
        merge_list(
            &mut expr_keys,
            Some(&frozen.expr_keys),
            Some(&fork.expr_keys),
        )?;
        merged.expr_keys = expr_keys.unwrap_or_default();
        if fork.resolve_state != frozen.resolve_state {
            merged.resolve_state = fork.resolve_state.clone();
        }
        merged.invalidate_sorted_member_ids();
        if merged.members.is_empty() {
            *current = None;
        }
        Ok(())
    }

    /// The listed `(key, id)` pairs as an order-independent fingerprint.
    pub fn fingerprint(&self) -> Fingerprint {
        if !mapping_back() {
            return self.fingerprint;
        }
        self.members
            .iter()
            .fold(0, |sum: Fingerprint, (key, item)| {
                sum.wrapping_add(item_fingerprint(key, item))
            })
    }

    /// Records that `id` was listed under `key` through `get_member_mut`.
    pub fn note_id_added(&mut self, key: &LuaMemberKey, id: LuaMemberId) {
        self.fingerprint = self
            .fingerprint
            .wrapping_add(listed_id_fingerprint(key, id));
    }

    /// Records that `id` was unlisted from `key` through `iter_mut`.
    pub fn note_id_removed(&mut self, key: &LuaMemberKey, id: LuaMemberId) {
        self.fingerprint = self
            .fingerprint
            .wrapping_sub(listed_id_fingerprint(key, id));
    }

    /// Unlists `id` from the item under `key`, dropping the key once its item
    /// is empty. Returns whether `id` was listed.
    pub fn remove_id(&mut self, key: &LuaMemberKey, id: LuaMemberId) -> bool {
        let many = match self.get_member(key) {
            Some(LuaMemberIndexItem::One(existing)) if *existing == id => false,
            Some(LuaMemberIndexItem::Many(ids)) if ids.contains(&id) => true,
            _ => return false,
        };
        let drop_key = match self.get_member_mut(key) {
            Some(item @ LuaMemberIndexItem::Many(_)) => !item.retain_ids(|held| held != id),
            _ => true,
        };
        if many {
            self.note_id_removed(key, id);
        }
        if drop_key {
            self.remove_member(key);
        }
        true
    }

    pub fn expr_keys(&self) -> impl Iterator<Item = &LuaMemberKey> {
        self.expr_keys.iter()
    }

    pub fn get_member(&self, key: &LuaMemberKey) -> Option<&LuaMemberIndexItem> {
        self.members.get(key)
    }

    pub fn contains_member(&self, key: &LuaMemberKey) -> bool {
        self.members.contains_key(key)
    }

    pub fn get_member_len(&self) -> usize {
        self.members.len()
    }

    pub fn get_member_mut(&mut self, key: &LuaMemberKey) -> Option<&mut LuaMemberIndexItem> {
        self.invalidate_sorted_member_ids();
        self.members.get_mut(key)
    }

    pub fn get_member_items(&self) -> impl Iterator<Item = &LuaMemberIndexItem> {
        self.members.values()
    }

    pub fn get_member_keys(&self) -> impl Iterator<Item = &LuaMemberKey> {
        self.members.keys()
    }

    pub fn sorted_member_ids(&self) -> &[LuaMemberId] {
        self.sorted_ids_cache.get_or_init(|| {
            let mut member_ids = Vec::new();
            for item in self.members.values() {
                match item {
                    LuaMemberIndexItem::One(id) => member_ids.push(*id),
                    LuaMemberIndexItem::Many(ids) => member_ids.extend(ids.iter().copied()),
                }
            }
            member_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));
            member_ids
        })
    }

    pub fn sorted_expr_key_member_ids(&self) -> &[LuaMemberId] {
        self.sorted_expr_ids_cache.get_or_init(|| {
            let mut member_ids = Vec::with_capacity(self.expr_keys.len());
            for key in &self.expr_keys {
                match self.members.get(key) {
                    Some(LuaMemberIndexItem::One(id)) => member_ids.push(*id),
                    Some(LuaMemberIndexItem::Many(ids)) => member_ids.extend(ids.iter().copied()),
                    None => {}
                }
            }
            member_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));
            member_ids
        })
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&LuaMemberKey, &mut LuaMemberIndexItem)> {
        self.invalidate_sorted_member_ids();
        self.members.iter_mut()
    }

    pub fn remove_member(&mut self, key: &LuaMemberKey) -> Option<LuaMemberIndexItem> {
        self.invalidate_sorted_member_ids();
        if key.is_expr() {
            self.expr_keys.retain(|expr_key| expr_key != key);
        }
        let removed = self.members.remove(key);
        if let Some(item) = &removed {
            self.fingerprint = self.fingerprint.wrapping_sub(item_fingerprint(key, item));
        }
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn set_resolved(&mut self) {
        self.resolve_state = OwnerMemberStatus::Resolved;
    }

    pub fn set_unresolved(&mut self) {
        self.resolve_state = OwnerMemberStatus::UnResolved;
    }

    pub fn is_resolved(&self) -> bool {
        matches!(self.resolve_state, OwnerMemberStatus::Resolved)
    }

    fn invalidate_sorted_member_ids(&mut self) {
        self.sorted_ids_cache = OnceLock::new();
        self.sorted_expr_ids_cache = OnceLock::new();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerMemberStatus {
    UnResolved,
    Resolved,
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::*;
    use crate::FileId;
    use crate::db_index::cow_shards::Conflict;

    fn member_id(file_id: u32, start: u32) -> LuaMemberId {
        LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::NameExpr.into(),
                TextRange::new(TextSize::new(start), TextSize::new(start + 1)),
            ),
            FileId::new(file_id),
        )
    }

    fn owner_with(entries: &[(&str, LuaMemberId)]) -> LuaOwnerMembers {
        let mut members = LuaOwnerMembers::new();
        for (name, id) in entries {
            members.add_member(
                LuaMemberKey::Name((*name).into()),
                LuaMemberIndexItem::One(*id),
            );
        }
        members
    }

    /// What merging the fork into `current` makes of it, or its conflict.
    fn merge_owner(
        current: Option<&LuaOwnerMembers>,
        frozen: Option<&LuaOwnerMembers>,
        fork: Option<&LuaOwnerMembers>,
    ) -> Result<Option<LuaOwnerMembers>, Conflict> {
        let mut merged = current.cloned();
        LuaOwnerMembers::merge(&mut merged, frozen, fork).map(|()| merged)
    }

    #[test]
    fn tasks_adding_other_keys_merge_into_the_owner_a_serial_run_builds() {
        let frozen = owner_with(&[("a", member_id(1, 1))]);
        let current = owner_with(&[("a", member_id(1, 1)), ("b", member_id(2, 2))]);
        let fork = owner_with(&[("a", member_id(1, 1)), ("c", member_id(3, 3))]);
        let serial = owner_with(&[
            ("a", member_id(1, 1)),
            ("b", member_id(2, 2)),
            ("c", member_id(3, 3)),
        ]);

        let merged = merge_owner(Some(&current), Some(&frozen), Some(&fork))
            .unwrap()
            .unwrap();
        assert_eq!(merged, serial);
    }

    #[test]
    fn tasks_changing_one_key_merge_the_ids_listed_under_it() {
        let key = LuaMemberKey::Name("a".into());
        let item = |ids: &[LuaMemberId]| {
            let mut ids = ids.to_vec();
            ids.sort_by_key(|id| member_id_sort_key(*id));
            match ids.as_slice() {
                [id] => LuaMemberIndexItem::One(*id),
                _ => LuaMemberIndexItem::Many(ids),
            }
        };
        let owner = |ids: &[LuaMemberId]| {
            let mut members = LuaOwnerMembers::new();
            members.add_member(key.clone(), item(ids));
            members
        };
        let (one, two, three) = (member_id(1, 1), member_id(2, 2), member_id(3, 3));

        let merged = merge_owner(
            Some(&owner(&[one, two])),
            Some(&owner(&[one])),
            Some(&owner(&[one, three])),
        )
        .unwrap()
        .unwrap();
        assert_eq!(merged.get_member(&key), Some(&item(&[one, two, three])));

        let merged = merge_owner(
            Some(&owner(&[one, two])),
            Some(&owner(&[one])),
            Some(&owner(&[three])),
        )
        .unwrap()
        .unwrap();
        assert_eq!(merged.get_member(&key), Some(&item(&[two, three])));
    }

    #[test]
    fn a_task_removing_the_owner_keeps_what_another_task_added() {
        let frozen = owner_with(&[("a", member_id(1, 1))]);
        let current = owner_with(&[("a", member_id(1, 1)), ("b", member_id(2, 2))]);

        let merged = merge_owner(Some(&current), Some(&frozen), None)
            .unwrap()
            .unwrap();
        assert_eq!(merged, owner_with(&[("b", member_id(2, 2))]));
    }

    #[test]
    fn tasks_each_removing_their_own_members_leave_the_owner_absent() {
        let frozen = owner_with(&[("a", member_id(1, 1)), ("b", member_id(2, 2))]);
        let current = owner_with(&[("b", member_id(2, 2))]);
        let fork = owner_with(&[("a", member_id(1, 1))]);

        assert_eq!(
            merge_owner(Some(&current), Some(&frozen), Some(&fork)),
            Ok(None)
        );
    }

    #[test]
    fn sorted_expression_member_ids_follow_every_mutation_path() {
        let expr_key = LuaMemberKey::ExprType(crate::LuaType::String);
        let second_expr_key = LuaMemberKey::ExprType(crate::LuaType::Integer);
        let first = member_id(2, 20);
        let second = member_id(1, 10);
        let third = member_id(3, 30);
        let mut members = LuaOwnerMembers::new();

        members.add_member(expr_key.clone(), LuaMemberIndexItem::One(first));
        assert_eq!(members.sorted_expr_key_member_ids(), &[first]);

        members.add_member(
            expr_key.clone(),
            LuaMemberIndexItem::Many(vec![first, second]),
        );
        assert_eq!(members.sorted_expr_key_member_ids(), &[second, first]);

        if let Some(item) = members.get_member_mut(&expr_key) {
            *item = LuaMemberIndexItem::One(first);
        }
        assert_eq!(members.sorted_expr_key_member_ids(), &[first]);

        members.add_member(second_expr_key.clone(), LuaMemberIndexItem::One(third));
        assert_eq!(members.sorted_expr_key_member_ids(), &[first, third]);

        for (key, item) in members.iter_mut() {
            if key == &expr_key {
                *item = LuaMemberIndexItem::One(second);
            }
        }
        assert_eq!(members.sorted_expr_key_member_ids(), &[second, third]);

        members.remove_member(&second_expr_key);
        assert_eq!(members.sorted_expr_key_member_ids(), &[second]);
    }
}
