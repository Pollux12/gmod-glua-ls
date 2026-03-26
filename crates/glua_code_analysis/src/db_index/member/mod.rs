mod lua_member;
mod lua_member_feature;
mod lua_member_item;
mod lua_member_owner;
mod lua_owner_members;

use glua_parser::LuaSyntaxKind;
use std::collections::{HashMap, HashSet};

use super::traits::LuaIndex;
use crate::{FileId, db_index::member::lua_owner_members::LuaOwnerMembers};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MemberOrOwner {
    Member(LuaMemberId),
    Owner(LuaMemberOwner),
}

impl Default for LuaMemberIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaMemberIndex {
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
            in_filed: HashMap::new(),
            owner_members: HashMap::new(),
            member_current_owner: HashMap::new(),
            member_owner_key_index: HashMap::new(),
        }
    }

    pub fn add_member(&mut self, owner: LuaMemberOwner, member: LuaMember) -> LuaMemberId {
        let id = member.get_id();
        let file_id = member.get_file_id();
        self.members.insert(id, member);
        self.add_in_file_object(file_id, MemberOrOwner::Member(id));
        if !owner.is_unknown() {
            self.member_current_owner.insert(id, owner.clone());
            self.add_in_file_object(file_id, MemberOrOwner::Owner(owner.clone()));
            self.add_member_to_owner_key_index(owner.clone(), id);
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
        let feature = member.get_feature();
        self.add_member_to_owner_key_index(owner.clone(), id);
        let member_map = self
            .owner_members
            .entry(owner.clone())
            .or_insert_with(LuaOwnerMembers::new);
        if feature.is_decl() {
            if let Some(item) = member_map.get_member_mut(&key) {
                match item {
                    LuaMemberIndexItem::One(old_id) => {
                        if old_id != &id {
                            let ids = vec![*old_id, id];
                            *item = LuaMemberIndexItem::Many(ids);
                        }
                    }
                    LuaMemberIndexItem::Many(ids) => {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                }
            } else {
                member_map.add_member(key.clone(), LuaMemberIndexItem::One(id));
            }
        } else {
            if !self
                .owner_members
                .get(&owner)
                .is_some_and(|owner_members| owner_members.contains_member(&key))
            {
                self.owner_members
                    .entry(owner.clone())
                    .or_insert_with(LuaOwnerMembers::new)
                    .add_member(key, LuaMemberIndexItem::One(id));
                return Some(());
            }

            let item = self.owner_members.get(&owner)?.get_member(&key)?.clone();
            let (new_items, removed_member_ids) = if self.is_item_only_meta(&item) {
                let new_items = match item {
                    LuaMemberIndexItem::One(old_id) => {
                        if old_id == id {
                            return Some(());
                        }
                        LuaMemberIndexItem::Many(vec![id, old_id])
                    }
                    LuaMemberIndexItem::Many(mut ids) => {
                        if ids.contains(&id) {
                            return Some(());
                        }

                        ids.push(id);
                        LuaMemberIndexItem::Many(ids)
                    }
                };
                (new_items, Vec::new())
            } else if self.is_item_only_file_define(&item) {
                let old_member_ids = member_ids_from_item(&item);
                let should_accumulate_assignments = self.is_assignment_file_define_member(id)
                    && old_member_ids
                        .iter()
                        .all(|old_id| self.is_assignment_file_define_member(*old_id));

                if should_accumulate_assignments {
                    match item {
                        LuaMemberIndexItem::One(old_id) if old_id == id => return Some(()),
                        LuaMemberIndexItem::Many(ids) if ids.contains(&id) => return Some(()),
                        _ => (LuaMemberIndexItem::One(id), Vec::new()),
                    }
                } else {
                    match item {
                        LuaMemberIndexItem::One(old_id) if old_id == id => return Some(()),
                        _ => (
                            LuaMemberIndexItem::One(id),
                            old_member_ids
                                .into_iter()
                                .filter(|old_id| *old_id != id)
                                .collect(),
                        ),
                    }
                }
            } else {
                return Some(());
            };

            for old_member_id in removed_member_ids {
                self.remove_member_from_owner_key_index(&owner, old_member_id);
            }

            self.owner_members
                .entry(owner.clone())
                .or_insert_with(LuaOwnerMembers::new)
                .add_member(key.clone(), new_items);
        }

        Some(())
    }

    fn add_member_to_owner_key_index(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        let member_ids = self
            .member_owner_key_index
            .entry(owner)
            .or_default()
            .entry(key)
            .or_default();
        if !member_ids.contains(&id) {
            member_ids.push(id);
        }
    }

    fn remove_member_from_owner_key_index(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        let mut remove_owner_entry = false;
        if let Some(owner_items) = self.member_owner_key_index.get_mut(owner) {
            if let Some(member_ids) = owner_items.get_mut(&key) {
                member_ids.retain(|member_id| *member_id != id);
                if member_ids.is_empty() {
                    owner_items.remove(&key);
                }
            }
            remove_owner_entry = owner_items.is_empty();
        }

        if remove_owner_entry {
            self.member_owner_key_index.remove(owner);
        }
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
                && member.get_syntax_id().get_kind() == LuaSyntaxKind::IndexExpr.into()
        })
    }

    pub fn set_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
    ) -> Option<()> {
        let previous_owner = self.member_current_owner.insert(id, owner.clone());
        if let Some(previous_owner) =
            previous_owner.filter(|previous_owner| previous_owner != &owner)
        {
            self.remove_member_from_owner_key_index(&previous_owner, id);
        }

        self.add_member_to_owner_key_index(owner.clone(), id);
        self.add_in_file_object(file_id, MemberOrOwner::Owner(owner));

        Some(())
    }

    pub fn get_member(&self, id: &LuaMemberId) -> Option<&LuaMember> {
        self.members.get(id)
    }

    pub fn get_member_mut(&mut self, id: &LuaMemberId) -> Option<&mut LuaMember> {
        self.members.get_mut(id)
    }

    pub fn get_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let member_items = self.owner_members.get(owner)?;
        let mut members = Vec::new();
        for item in member_items.get_member_items() {
            match item {
                LuaMemberIndexItem::One(id) => {
                    if let Some(member) = self.get_member(id) {
                        members.push(member);
                    }
                }
                LuaMemberIndexItem::Many(ids) => {
                    for id in ids {
                        if let Some(member) = self.get_member(id) {
                            members.push(member);
                        }
                    }
                }
            }
        }

        Some(members)
    }

    #[allow(unused)]
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
        let mut members = self.get_members(owner)?;
        members.sort_by_key(|member| member.get_sort_key());
        Some(members)
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
}

impl LuaIndex for LuaMemberIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(member_ids) = self.in_filed.remove(&file_id) {
            let mut owners = HashSet::new();
            for member_id_or_owner in member_ids {
                match member_id_or_owner {
                    MemberOrOwner::Member(member_id) => {
                        if let Some(owner) = self.member_current_owner.get(&member_id).cloned() {
                            self.remove_member_from_owner_key_index(&owner, member_id);
                        }
                        self.members.remove(&member_id);
                        self.member_current_owner.remove(&member_id);
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
    }

    fn clear(&mut self) {
        self.members.clear();
        self.in_filed.clear();
        self.owner_members.clear();
        self.member_current_owner.clear();
        self.member_owner_key_index.clear();
    }
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
        LuaMember::new(
            member_id,
            LuaMemberKey::Name(key.into()),
            LuaMemberFeature::FileFieldDecl,
            None,
        )
    }

    fn make_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range),
            file_id,
        )
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
    }

    #[test]
    fn clear_resets_member_owner_tracking() {
        let file_id = FileId::new(3);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let member_id = make_member_id(file_id, 7);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner, make_member(member_id, "field"));
        assert!(index.get_member_owner(&member_id).is_some());

        index.clear();

        assert!(index.get_member_owner(&member_id).is_none());
    }
}
