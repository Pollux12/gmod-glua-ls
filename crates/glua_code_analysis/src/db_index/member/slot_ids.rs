use std::ops::Deref;

use super::{LuaMemberId, member_id_sort_key};

/// The ids filed under one owner-key slot, in the slot's sort order. Most
/// slots hold one id, which then lives inline, without a list.
///
/// A list of one id is always `One`, so equal slots have equal shapes.
#[derive(Clone)]
pub(super) enum SlotIds {
    One(LuaMemberId),
    Many(Vec<LuaMemberId>),
}

impl Default for SlotIds {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl Deref for SlotIds {
    type Target = [LuaMemberId];

    fn deref(&self) -> &[LuaMemberId] {
        match self {
            Self::One(id) => std::slice::from_ref(id),
            Self::Many(ids) => ids,
        }
    }
}

impl SlotIds {
    pub(super) fn from_vec(mut ids: Vec<LuaMemberId>) -> Self {
        match ids.len() {
            1 => Self::One(ids[0]),
            0 => Self::default(),
            _ => {
                ids.shrink_to_fit();
                Self::Many(ids)
            }
        }
    }

    pub(super) fn as_slice(&self) -> &[LuaMemberId] {
        self
    }

    /// Files `id` at `at`, which keeps the slot's order.
    pub(super) fn insert(&mut self, at: usize, id: LuaMemberId) {
        match self {
            Self::One(existing) => {
                let existing = *existing;
                *self = Self::Many(if at == 0 {
                    vec![id, existing]
                } else {
                    vec![existing, id]
                });
            }
            Self::Many(ids) if ids.is_empty() => *self = Self::One(id),
            Self::Many(ids) => ids.insert(at, id),
        }
    }

    /// Files `id` in [`member_id_sort_key`] order unless the slot holds it.
    /// Returns whether it was new.
    pub(super) fn insert_sorted(&mut self, id: LuaMemberId) -> bool {
        match self.binary_search_by_key(&member_id_sort_key(id), |existing| {
            member_id_sort_key(*existing)
        }) {
            Ok(_) => false,
            Err(at) => {
                self.insert(at, id);
                true
            }
        }
    }

    /// Drops `id` from a slot kept in [`member_id_sort_key`] order. Returns
    /// whether it was there.
    pub(super) fn remove_sorted(&mut self, id: LuaMemberId) -> bool {
        let Ok(at) = self.binary_search_by_key(&member_id_sort_key(id), |existing| {
            member_id_sort_key(*existing)
        }) else {
            return false;
        };
        match self {
            Self::One(_) => *self = Self::default(),
            Self::Many(ids) => {
                ids.remove(at);
                if ids.len() < 2 {
                    *self = Self::from_vec(std::mem::take(ids));
                }
            }
        }
        true
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&LuaMemberId) -> bool) {
        match self {
            Self::One(id) => {
                if !keep(id) {
                    *self = Self::default();
                }
            }
            Self::Many(ids) => {
                ids.retain(|id| keep(id));
                if ids.len() < 2 {
                    *self = Self::from_vec(std::mem::take(ids));
                }
            }
        }
    }
}

impl PartialEq for SlotIds {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for SlotIds {}

impl std::fmt::Debug for SlotIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for &'a SlotIds {
    type Item = &'a LuaMemberId;
    type IntoIter = std::slice::Iter<'a, LuaMemberId>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
