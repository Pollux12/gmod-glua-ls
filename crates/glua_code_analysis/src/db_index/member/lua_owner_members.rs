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
    revision: u64,
    sorted_ids_cache: OnceLock<Vec<LuaMemberId>>,
    sorted_expr_ids_cache: OnceLock<Vec<LuaMemberId>>,
    /// The subset of `members` keyed by an expression rather than a name.
    ///
    /// Only `add_member` and `remove_member` change the key set; the mutators
    /// that hand out an item leave keys alone.
    expr_keys: Vec<LuaMemberKey>,
    resolve_state: OwnerMemberStatus,
}

#[allow(unused)]
impl LuaOwnerMembers {
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
            revision: 0,
            sorted_ids_cache: OnceLock::new(),
            sorted_expr_ids_cache: OnceLock::new(),
            expr_keys: Vec::new(),
            resolve_state: OwnerMemberStatus::UnResolved,
        }
    }

    pub fn add_member(&mut self, key: LuaMemberKey, item: LuaMemberIndexItem) {
        self.invalidate_sorted_member_ids();
        if key.is_expr() && !self.members.contains_key(&key) {
            self.expr_keys.push(key.clone());
        }
        self.members.insert(key, item);
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

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    fn invalidate_sorted_member_ids(&mut self) {
        self.revision += 1;
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

    fn member_id(file_id: u32, start: u32) -> LuaMemberId {
        LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::NameExpr.into(),
                TextRange::new(TextSize::new(start), TextSize::new(start + 1)),
            ),
            FileId::new(file_id),
        )
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
