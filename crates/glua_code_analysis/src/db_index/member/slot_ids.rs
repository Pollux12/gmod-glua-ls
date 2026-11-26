use std::ops::Deref;

use super::{LuaMemberId, member_id_sort_key};

/// The ids filed under one owner-key slot, in the slot's sort order. Most
/// slots hold one id, which then lives inline, without a list.
///
/// A list of one id is always `One`, so equal slots have equal shapes.
#[derive(Clone, Default)]
pub(super) enum SlotIds {
    #[default]
    Empty,
    One(LuaMemberId),
    Many(Box<[LuaMemberId]>),
}

const _: () = assert!(std::mem::size_of::<SlotIds>() == 24);

impl Deref for SlotIds {
    type Target = [LuaMemberId];

    fn deref(&self) -> &[LuaMemberId] {
        match self {
            Self::Empty => &[],
            Self::One(id) => std::slice::from_ref(id),
            Self::Many(ids) => ids,
        }
    }
}

impl SlotIds {
    pub(super) fn from_vec(ids: Vec<LuaMemberId>) -> Self {
        match ids.len() {
            0 => Self::Empty,
            1 => Self::One(ids[0]),
            _ => Self::Many(ids.into_boxed_slice()),
        }
    }

    pub(super) fn as_slice(&self) -> &[LuaMemberId] {
        self
    }

    /// Files `id` at `at`, which keeps the slot's order.
    pub(super) fn insert(&mut self, at: usize, id: LuaMemberId) {
        match self {
            Self::Empty => *self = Self::One(id),
            Self::One(existing) => {
                let existing = *existing;
                let pair = if at == 0 {
                    [id, existing]
                } else {
                    [existing, id]
                };
                *self = Self::Many(Box::new(pair));
            }
            Self::Many(ids) => {
                let mut vec = Vec::with_capacity(ids.len() + 1);
                vec.extend_from_slice(&ids[..at]);
                vec.push(id);
                vec.extend_from_slice(&ids[at..]);
                *self = Self::Many(vec.into_boxed_slice());
            }
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
            Self::Empty => return false,
            Self::One(_) => *self = Self::Empty,
            Self::Many(ids) => {
                if ids.len() == 2 {
                    *self = Self::One(ids[1 - at]);
                } else {
                    let mut vec = Vec::with_capacity(ids.len() - 1);
                    vec.extend_from_slice(&ids[..at]);
                    vec.extend_from_slice(&ids[at + 1..]);
                    *self = Self::Many(vec.into_boxed_slice());
                }
            }
        }
        true
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&LuaMemberId) -> bool) {
        match self {
            Self::Empty => {}
            Self::One(id) => {
                if !keep(id) {
                    *self = Self::Empty;
                }
            }
            Self::Many(ids) => {
                let mut kept = Vec::with_capacity(ids.len());
                for id in ids.iter() {
                    if keep(id) {
                        kept.push(*id);
                    }
                }
                if kept.len() != ids.len() {
                    *self = Self::from_vec(kept);
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
