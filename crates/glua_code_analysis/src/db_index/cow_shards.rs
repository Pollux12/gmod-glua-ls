//! Copy-on-write sharded maps for statement-written index state.
//!
//! A [`CowShards`] keeps a file-owned key in its file's shard and a
//! workspace-owned key in one of [`PAGES`] hash pages. Shards sit behind
//! `Arc`s, so a clone or a [`CowShards::fork`] costs reference-count bumps,
//! and a write copies only the one shard it touches.

use std::collections::hash_map::Entry;
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;

use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::{FileId, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaTypeOwner};

/// The number of hash pages workspace-owned keys are spread over.
const PAGES: usize = 4096;

/// Which shard owns a key.
pub(crate) enum ShardOwner {
    File(FileId),
    Workspace,
}

pub(crate) trait ShardKey: Eq + Hash {
    fn shard_owner(&self) -> ShardOwner;
}

impl ShardKey for LuaTypeOwner {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::File(self.get_file_id())
    }
}

impl ShardKey for LuaMemberId {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::File(self.file_id)
    }
}

impl ShardKey for LuaMemberOwner {
    fn shard_owner(&self) -> ShardOwner {
        match self {
            LuaMemberOwner::Element(site) => ShardOwner::File(site.file_id),
            _ => ShardOwner::Workspace,
        }
    }
}

impl ShardKey for LuaMemberKey {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::Workspace
    }
}

fn file_slot(file_id: FileId) -> usize {
    PAGES + file_id.id as usize
}

fn slot_of<K: ShardKey>(key: &K) -> usize {
    match key.shard_owner() {
        ShardOwner::File(file_id) => file_slot(file_id),
        // The low bits pick the bucket inside a shard's table and the top
        // seven its control byte, so the page comes from bits in between.
        ShardOwner::Workspace => (FxBuildHasher.hash_one(key) >> 40) as usize % PAGES,
    }
}

type Shard<K, V> = FxHashMap<K, V>;

/// A map split into copy-on-write shards.
///
/// Iteration visits shards in slot order (hash pages, then files by id) and
/// each shard in its own table order; callers that need a stable order sort.
///
/// A fork records each shard it writes in `dirty`, so [`Self::install`] takes
/// its writes back in time proportional to the shards it touched.
pub(crate) struct CowShards<K, V> {
    slots: Arc<[Option<Arc<Shard<K, V>>>]>,
    dirty: Vec<(u32, Arc<Shard<K, V>>)>,
    forked: bool,
}

impl<K, V> Default for CowShards<K, V> {
    fn default() -> Self {
        Self {
            slots: Arc::from(Vec::new()),
            dirty: Vec::new(),
            forked: false,
        }
    }
}

impl<K, V> Clone for CowShards<K, V> {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            dirty: self.dirty.clone(),
            forked: self.forked,
        }
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for CowShards<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.shards().flat_map(|shard| shard.iter()))
            .finish()
    }
}

impl<K: ShardKey + Clone, V: Clone + PartialEq> PartialEq for CowShards<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(key) == Some(value))
    }
}

impl<K, V> CowShards<K, V> {
    fn shard(&self, at: usize) -> Option<&Shard<K, V>> {
        if !self.dirty.is_empty()
            && let Some((_, shard)) = self.dirty.iter().find(|(slot, _)| *slot as usize == at)
        {
            return Some(shard);
        }
        self.slots.get(at)?.as_deref()
    }

    fn shards(&self) -> impl Iterator<Item = &Shard<K, V>> {
        let end = self
            .dirty
            .iter()
            .map(|(slot, _)| *slot as usize + 1)
            .fold(self.slots.len(), usize::max);
        (0..end).filter_map(|at| self.shard(at))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.shards().flat_map(|shard| shard.iter())
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(key, _)| key)
    }

    pub(crate) fn len(&self) -> usize {
        self.shards().map(|shard| shard.len()).sum()
    }

    /// The entries held in `file_id`'s shard: every file-owned key of the file.
    pub(crate) fn file_entries(&self, file_id: FileId) -> impl Iterator<Item = (&K, &V)> {
        self.shard(file_slot(file_id))
            .into_iter()
            .flat_map(|shard| shard.iter())
    }

    pub(crate) fn clear(&mut self) {
        self.slots = Arc::from(Vec::new());
        self.dirty.clear();
    }

    /// A copy that shares every shard and keeps its own writes in a dirty list.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "superstep tasks fork the index")
    )]
    pub(crate) fn fork(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            dirty: self.dirty.clone(),
            forked: true,
        }
    }

    /// Takes back the shards `fork` wrote. `fork` must come from
    /// [`Self::fork`] on this map, with no write to the shards it touched since.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "superstep barriers install forks")
    )]
    pub(crate) fn install(&mut self, fork: Self) {
        let Self { slots, dirty, .. } = fork;
        drop(slots);
        for (at, shard) in dirty {
            let at = at as usize;
            self.slots_mut(at)[at] = Some(shard);
        }
    }

    /// The slot table, grown to hold `at` and owned by this map alone.
    fn slots_mut(&mut self, at: usize) -> &mut [Option<Arc<Shard<K, V>>>] {
        if at >= self.slots.len() {
            let len = (at + 1).next_power_of_two();
            let mut grown = Vec::with_capacity(len);
            grown.extend(self.slots.iter().cloned());
            grown.resize(len, None);
            self.slots = Arc::from(grown);
        }
        Arc::make_mut(&mut self.slots)
    }
}

impl<K: Clone, V: Clone> CowShards<K, V> {
    fn shard_mut(&mut self, at: usize) -> &mut Shard<K, V> {
        if !self.forked {
            return Arc::make_mut(self.slots_mut(at)[at].get_or_insert_with(Default::default));
        }
        let position = match self.dirty.iter().position(|(slot, _)| *slot as usize == at) {
            Some(position) => position,
            None => {
                let base = self.slots.get(at).cloned().flatten().unwrap_or_default();
                self.dirty.push((at as u32, base));
                self.dirty.len() - 1
            }
        };
        Arc::make_mut(&mut self.dirty[position].1)
    }
}

impl<K: ShardKey + Clone, V: Clone> CowShards<K, V> {
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.shard(slot_of(key))?.get(key)
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.shard(slot_of(key))
            .is_some_and(|shard| shard.contains_key(key))
    }

    /// The shard holding `key` for writing, or `None` when it cannot hold it.
    /// A fork copies the shard only when the key is there.
    fn holding_shard_mut(&mut self, key: &K) -> Option<&mut Shard<K, V>> {
        let at = slot_of(key);
        let shard = self.shard(at)?;
        if self.forked && !shard.contains_key(key) {
            return None;
        }
        Some(self.shard_mut(at))
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.holding_shard_mut(key)?.get_mut(key)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.shard_mut(slot_of(&key)).insert(key, value)
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        self.holding_shard_mut(key)?.remove(key)
    }

    pub(crate) fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        self.shard_mut(slot_of(&key)).entry(key)
    }
}

impl<'a, K, V> IntoIterator for &'a CowShards<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (&'a K, &'a V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LuaDeclId;

    fn owner(file: u32, position: u32) -> LuaTypeOwner {
        LuaTypeOwner::Decl(LuaDeclId::new(FileId { id: file }, position.into()))
    }

    fn path_key(name: &str) -> LuaMemberKey {
        LuaMemberKey::Name(name.into())
    }

    fn sorted<K: std::fmt::Debug, V: std::fmt::Debug>(map: &CowShards<K, V>) -> Vec<String> {
        let mut entries: Vec<String> = map.iter().map(|entry| format!("{entry:?}")).collect();
        entries.sort();
        entries
    }

    fn rendered<K: std::fmt::Debug, V: std::fmt::Debug>(entries: Vec<(K, V)>) -> Vec<String> {
        let mut entries: Vec<String> = entries
            .iter()
            .map(|(k, v)| format!("{:?}", (k, v)))
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn fork_writes_stay_out_of_the_base_until_installed() {
        let mut base: CowShards<LuaTypeOwner, u32> = CowShards::default();
        base.insert(owner(1, 0), 10);
        base.insert(owner(2, 0), 20);

        let mut fork = base.fork();
        fork.insert(owner(1, 4), 14);
        *fork.get_mut(&owner(2, 0)).unwrap() = 21;
        fork.insert(owner(9, 0), 90);

        assert_eq!(
            (
                base.get(&owner(1, 4)),
                base.get(&owner(2, 0)),
                base.get(&owner(9, 0))
            ),
            (None, Some(&20), None)
        );
        assert_eq!(
            (
                fork.get(&owner(1, 4)),
                fork.get(&owner(2, 0)),
                fork.get(&owner(9, 0))
            ),
            (Some(&14), Some(&21), Some(&90))
        );

        base.install(fork);
        assert_eq!(
            (
                base.get(&owner(1, 4)),
                base.get(&owner(2, 0)),
                base.get(&owner(9, 0))
            ),
            (Some(&14), Some(&21), Some(&90))
        );
    }

    #[test]
    fn fork_copies_only_the_shards_it_changes() {
        let mut base: CowShards<LuaTypeOwner, u32> = CowShards::default();
        base.insert(owner(1, 0), 10);
        let mut fork = base.fork();
        assert_eq!(fork.remove(&owner(1, 8)), None);
        assert_eq!(fork.get_mut(&owner(1, 8)), None);
        assert!(fork.dirty.is_empty());
    }

    #[test]
    fn a_clone_is_independent_of_its_source() {
        let mut base: CowShards<LuaMemberKey, u32> = CowShards::default();
        for (at, name) in ["a", "b", "c"].into_iter().enumerate() {
            base.insert(path_key(name), at as u32);
        }
        let mut copy = base.clone();
        copy.remove(&path_key("a"));
        copy.insert(path_key("d"), 3);
        *copy.entry(path_key("b")).or_default() += 10;

        assert_eq!(
            sorted(&base),
            rendered(vec![
                (path_key("a"), 0),
                (path_key("b"), 1),
                (path_key("c"), 2)
            ])
        );
        assert_eq!(
            sorted(&copy),
            rendered(vec![
                (path_key("b"), 11),
                (path_key("c"), 2),
                (path_key("d"), 3)
            ])
        );
    }

    #[test]
    fn iteration_sees_base_and_dirty_shards_once_each() {
        let mut base: CowShards<LuaTypeOwner, u32> = CowShards::default();
        base.insert(owner(1, 0), 10);
        base.insert(owner(3, 0), 30);
        let mut fork = base.fork();
        fork.insert(owner(1, 2), 12);
        fork.insert(owner(40, 0), 400);

        assert_eq!(fork.len(), 4);
        assert_eq!(
            sorted(&fork),
            rendered(vec![
                (owner(1, 0), 10),
                (owner(1, 2), 12),
                (owner(3, 0), 30),
                (owner(40, 0), 400)
            ])
        );
        assert_eq!(
            sorted(&base),
            rendered(vec![(owner(1, 0), 10), (owner(3, 0), 30)])
        );
    }

    #[test]
    fn file_entries_hold_exactly_that_files_keys() {
        let mut map: CowShards<LuaTypeOwner, u32> = CowShards::default();
        map.insert(owner(1, 0), 10);
        map.insert(owner(1, 5), 15);
        map.insert(owner(2, 0), 20);
        let mut entries: Vec<u32> = map
            .file_entries(FileId { id: 1 })
            .map(|(_, v)| *v)
            .collect();
        entries.sort_unstable();
        assert_eq!(entries, vec![10, 15]);
    }
}
