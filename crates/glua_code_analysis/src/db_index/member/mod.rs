mod lua_member;
mod lua_member_feature;
mod lua_member_item;
mod lua_member_owner;
mod lua_owner_members;

use glua_parser::LuaSyntaxKind;
use rowan::{TextRange, TextSize};
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

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
    member_owner_key_history_index:
        HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
    /// Lazy diagnostics-phase memo for global key lookups. Reset after every
    /// member history/current-owner mutation so it rebuilds from the owner-key
    /// indexes on demand.
    member_key_current_cache: OnceLock<HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
    non_overwriting_assignment_members: HashSet<LuaMemberId>,
    function_scope_ranges: HashMap<FileId, Vec<TextRange>>,
    member_function_scope_ranges: HashMap<LuaMemberId, TextRange>,
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
            member_owner_key_history_index: HashMap::new(),
            member_key_current_cache: OnceLock::new(),
            non_overwriting_assignment_members: HashSet::new(),
            function_scope_ranges: HashMap::new(),
            member_function_scope_ranges: HashMap::new(),
        }
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
            self.add_in_file_object(file_id, MemberOrOwner::Owner(owner.clone()));
            self.add_member_to_owner_key_index(owner.clone(), id);
            self.add_member_to_owner_key_history_index(owner.clone(), id);
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
        self.add_member_to_owner_key_history_index(owner.clone(), id);
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
                let all_assignment_file_defines = self.is_assignment_file_define_member(id)
                    && old_member_ids
                        .iter()
                        .all(|old_id| self.is_assignment_file_define_member(*old_id));

                if all_assignment_file_defines {
                    let should_preserve_members =
                        self.non_overwriting_assignment_members.contains(&id)
                            && old_member_ids.iter().all(|old_id| {
                                self.non_overwriting_assignment_members.contains(old_id)
                            });
                    if should_preserve_members {
                        let mut ids = old_member_ids;
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                        let item = match ids.as_slice() {
                            [id] => LuaMemberIndexItem::One(*id),
                            _ => LuaMemberIndexItem::Many(ids),
                        };
                        self.owner_members
                            .entry(owner.clone())
                            .or_insert_with(LuaOwnerMembers::new)
                            .add_member(key.clone(), item);
                        return Some(());
                    }
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
                                .filter(|old_id| {
                                    *old_id != id && !self.is_assignment_file_define_member(*old_id)
                                })
                                .collect(),
                        ),
                    }
                }
            } else {
                return Some(());
            };

            for old_member_id in removed_member_ids {
                self.remove_member_from_visible_owner_key_index(&owner, old_member_id);
            }

            self.owner_members
                .entry(owner.clone())
                .or_insert_with(LuaOwnerMembers::new)
                .add_member(key.clone(), new_items);
        }

        Some(())
    }

    fn add_member_to_owner_key_index(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        self.add_member_id_to_owner_key_map(owner, id, false);
    }

    fn add_member_to_owner_key_history_index(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        self.add_member_id_to_owner_key_map(owner, id, true);
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

        if history {
            self.invalidate_member_key_history_cache();
        }
    }

    fn invalidate_member_key_history_cache(&mut self) {
        self.member_key_current_cache = OnceLock::new();
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

        if history {
            self.invalidate_member_key_history_cache();
        }
    }

    fn remove_file_members_from_owner_key_indexes(&mut self, file_id: FileId) {
        Self::remove_file_members_from_owner_key_map(&mut self.member_owner_key_index, file_id);
        Self::remove_file_members_from_owner_key_map(
            &mut self.member_owner_key_history_index,
            file_id,
        );
        self.invalidate_member_key_history_cache();
    }

    fn remove_file_members_from_owner_key_map(
        owner_key_index: &mut HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
        file_id: FileId,
    ) {
        owner_key_index.retain(|_, key_members| {
            key_members.retain(|_, member_ids| {
                member_ids.retain(|member_id| member_id.file_id != file_id);
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
        if let Some(previous_owner) =
            previous_owner.filter(|previous_owner| previous_owner != &owner)
        {
            self.remove_member_from_visible_owner_key_index(&previous_owner, id);
        }

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

        members.sort_by_key(|member| stable_member_sort_key(member));
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

    /// Number of historical assignment members recorded under `(owner, key)`.
    /// O(1) — used to bound the otherwise O(N) per-assignment widening/preserve
    /// scans so a field assigned a pathological number of times (generated code,
    /// huge dispatch tables) cannot drive `lua analyze` into O(N²) behaviour.
    pub fn count_members_for_owner_key(
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

    pub fn get_current_members_for_key(&self, key: &LuaMemberKey) -> Vec<&LuaMember> {
        let key_current_index = self
            .member_key_current_cache
            .get_or_init(|| self.build_member_key_current_index());
        let Some(member_ids) = key_current_index.get(key) else {
            return Vec::new();
        };

        member_ids
            .iter()
            .copied()
            .filter_map(|member_id| self.get_member(&member_id))
            .collect()
    }

    fn build_member_key_current_index(&self) -> HashMap<LuaMemberKey, Vec<LuaMemberId>> {
        let mut key_history_index: HashMap<LuaMemberKey, HashSet<LuaMemberId>> = HashMap::new();
        for owner_items in self.member_owner_key_history_index.values() {
            for (key, ids) in owner_items {
                key_history_index
                    .entry(key.clone())
                    .or_default()
                    .extend(ids.iter().copied());
            }
        }

        key_history_index
            .into_iter()
            .filter_map(|(key, ids)| {
                let mut members = ids
                    .into_iter()
                    .filter_map(|member_id| {
                        self.member_current_owner.get(&member_id)?;
                        self.get_member(&member_id)
                            .filter(|member| member.get_key() == &key)
                    })
                    .collect::<Vec<_>>();
                if members.is_empty() {
                    return None;
                }

                members.sort_by_key(|member| stable_member_sort_key(member));
                Some((
                    key,
                    members
                        .into_iter()
                        .map(|member| member.get_id())
                        .collect::<Vec<_>>(),
                ))
            })
            .collect()
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
    let syntax_id = member_id.get_syntax_id();
    (
        member_id.file_id.id,
        u32::from(member_id.get_position()),
        u32::from(syntax_id.get_range().end()),
        syntax_id.get_kind() as u16,
    )
}

impl LuaIndex for LuaMemberIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(member_ids) = self.in_filed.remove(&file_id) {
            let mut owners = HashSet::new();
            for member_id_or_owner in member_ids {
                match member_id_or_owner {
                    MemberOrOwner::Member(member_id) => {
                        if let Some(owner) = self.member_current_owner.get(&member_id).cloned() {
                            self.remove_member_from_all_owner_key_indexes(&owner, member_id);
                        }
                        self.members.remove(&member_id);
                        self.member_current_owner.remove(&member_id);
                        self.non_overwriting_assignment_members.remove(&member_id);
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
        self.remove_file_members_from_owner_key_indexes(file_id);
        self.function_scope_ranges.remove(&file_id);
        self.member_function_scope_ranges
            .retain(|member_id, _| member_id.file_id != file_id);
    }

    fn clear(&mut self) {
        self.members.clear();
        self.in_filed.clear();
        self.owner_members.clear();
        self.member_current_owner.clear();
        self.member_owner_key_index.clear();
        self.member_owner_key_history_index.clear();
        self.member_key_current_cache = OnceLock::new();
        self.non_overwriting_assignment_members.clear();
        self.function_scope_ranges.clear();
        self.member_function_scope_ranges.clear();
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
