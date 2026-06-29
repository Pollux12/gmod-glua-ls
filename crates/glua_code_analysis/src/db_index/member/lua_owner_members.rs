use std::{collections::HashMap, sync::OnceLock};

use super::member_id_sort_key;
use crate::{LuaMemberId, LuaMemberIndexItem, LuaMemberKey};

#[allow(unused)]
#[derive(Debug, Clone)]
pub struct LuaOwnerMembers {
    members: HashMap<LuaMemberKey, LuaMemberIndexItem>,
    // `members` is private and these four mutators are the complete id-set
    // invalidation surface: `add_member`, `get_member_mut`, `iter_mut`, and
    // `remove_member`.
    sorted_ids_cache: OnceLock<Vec<LuaMemberId>>,
    resolve_state: OwnerMemberStatus,
}

#[allow(unused)]
impl LuaOwnerMembers {
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
            sorted_ids_cache: OnceLock::new(),
            resolve_state: OwnerMemberStatus::UnResolved,
        }
    }

    pub fn add_member(&mut self, key: LuaMemberKey, item: LuaMemberIndexItem) {
        self.invalidate_sorted_member_ids();
        self.members.insert(key, item);
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

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&LuaMemberKey, &mut LuaMemberIndexItem)> {
        self.invalidate_sorted_member_ids();
        self.members.iter_mut()
    }

    pub fn remove_member(&mut self, key: &LuaMemberKey) -> Option<LuaMemberIndexItem> {
        self.invalidate_sorted_member_ids();
        self.members.remove(key)
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
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerMemberStatus {
    UnResolved,
    Resolved,
}
