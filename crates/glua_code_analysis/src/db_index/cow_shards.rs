//! Copy-on-write storage for index state, so an index clones by bumping
//! reference counts.
//!
//! A [`CowShards`] keeps a file-owned key in its file's shard and a
//! workspace-owned key in one of [`PAGES`] hash pages. Shards sit behind
//! `Arc`s, so a clone or a [`CowShards::fork`] costs reference-count bumps,
//! and a write copies only the one shard it touches. A [`CowCell`] holds a
//! whole value behind one `Arc`, for state small enough to copy whole or
//! written only outside the tasks that fork the index.

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;

use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::db_index::derivation::DerivationId;
use crate::{
    DynamicFieldOwner, FileId, GlobalId, InFiled, LuaDeclId, LuaDefinitionId, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaOperatorId, LuaOperatorOwner, LuaSignatureId, LuaTypeDeclId,
    LuaTypeOwner, SpecialCallFact,
};

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

impl ShardKey for FileId {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::File(*self)
    }
}

impl ShardKey for str {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::Workspace
    }
}

impl ShardKey for String {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::Workspace
    }
}

impl ShardKey for smol_str::SmolStr {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::Workspace
    }
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

/// Keys that name a file's position, or are owned by one file, live in that
/// file's shard.
macro_rules! file_keys {
    ($($ty:ty => |$key:ident| $file:expr;)*) => {
        $(impl ShardKey for $ty {
            fn shard_owner(&self) -> ShardOwner {
                let $key = self;
                ShardOwner::File($file)
            }
        })*
    };
}

/// Keys that several files contribute to live in a hash page.
macro_rules! workspace_keys {
    ($($ty:ty),* $(,)?) => {
        $(impl ShardKey for $ty {
            fn shard_owner(&self) -> ShardOwner {
                ShardOwner::Workspace
            }
        })*
    };
}

file_keys! {
    LuaDeclId => |key| key.file_id;
    LuaSignatureId => |key| key.get_file_id();
    LuaOperatorId => |key| key.file_id;
    LuaDefinitionId => |key| key.file_id();
    DerivationId => |key| key.file_id;
    (LuaSignatureId, usize) => |key| key.0.get_file_id();
    (FileId, rowan::TextSize) => |key| key.0;
    (FileId, rowan::TextRange) => |key| key.0;
    (LuaTypeDeclId, FileId, rowan::TextRange) => |key| key.1;
    (LuaTypeDeclId, InFiled<crate::LuaSuperType>) => |key| key.1.file_id;
    SpecialCallFact => |key| key.site_file();
}

workspace_keys!(
    LuaTypeDeclId,
    GlobalId,
    (GlobalId, crate::db_index::member::GlobalPathClassCandidate),
);

impl ShardKey for InFiled<rowan::TextRange> {
    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::File(self.file_id)
    }
}

impl ShardKey for crate::TypeCacheRef {
    fn shard_owner(&self) -> ShardOwner {
        self.file_id()
            .map_or(ShardOwner::Workspace, ShardOwner::File)
    }
}

impl ShardKey for DynamicFieldOwner {
    fn shard_owner(&self) -> ShardOwner {
        match self {
            DynamicFieldOwner::Table(site) => ShardOwner::File(site.file_id),
            _ => ShardOwner::Workspace,
        }
    }
}

impl ShardKey for LuaOperatorOwner {
    fn shard_owner(&self) -> ShardOwner {
        match self {
            LuaOperatorOwner::Table(site) => ShardOwner::File(site.file_id),
            LuaOperatorOwner::Type(_) => ShardOwner::Workspace,
        }
    }
}

impl ShardKey for (LuaMemberOwner, LuaMemberKey) {
    fn shard_owner(&self) -> ShardOwner {
        self.0.shard_owner()
    }
}

fn file_slot(file_id: FileId) -> usize {
    PAGES + file_id.id as usize
}

fn slot_of<K: ShardKey + ?Sized>(key: &K) -> usize {
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
    dirty: FxHashMap<u32, Arc<Shard<K, V>>>,
    forked: bool,
}

impl<K, V> Default for CowShards<K, V> {
    fn default() -> Self {
        Self {
            slots: Arc::from(Vec::new()),
            dirty: FxHashMap::default(),
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
            && let Some(shard) = self.dirty.get(&(at as u32))
        {
            return Some(shard);
        }
        self.slots.get(at)?.as_deref()
    }

    fn shards(&self) -> impl Iterator<Item = &Shard<K, V>> {
        (0..self.end()).filter_map(|at| self.shard(at))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.shards().flat_map(|shard| shard.iter())
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(key, _)| key)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, value)| value)
    }

    fn end(&self) -> usize {
        self.dirty
            .keys()
            .map(|slot| *slot as usize + 1)
            .fold(self.slots.len(), usize::max)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.shards().all(|shard| shard.is_empty())
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
        let slots = &self.slots;
        let shard = self
            .dirty
            .entry(at as u32)
            .or_insert_with(|| slots.get(at).cloned().flatten().unwrap_or_default());
        Arc::make_mut(shard)
    }

    /// Makes every non-empty shard writable and returns their slots in order.
    fn writable_shards(&mut self) -> Vec<&mut Shard<K, V>> {
        let populated: Vec<usize> = (0..self.end())
            .filter(|at| self.shard(*at).is_some_and(|shard| !shard.is_empty()))
            .collect();
        for &at in &populated {
            self.shard_mut(at);
        }
        let Self {
            slots,
            dirty,
            forked,
        } = self;
        let mut shards: Vec<(usize, &mut Shard<K, V>)> = if *forked {
            dirty
                .iter_mut()
                .filter_map(|(at, shard)| Some((*at as usize, Arc::get_mut(shard)?)))
                .collect()
        } else {
            Arc::get_mut(slots)
                .into_iter()
                .flat_map(|slots| slots.iter_mut().enumerate())
                .filter_map(|(at, shard)| Some((at, Arc::get_mut(shard.as_mut()?)?)))
                .collect()
        };
        shards.sort_unstable_by_key(|(at, _)| *at);
        shards.into_iter().map(|(_, shard)| shard).collect()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.writable_shards()
            .into_iter()
            .flat_map(|shard| shard.iter_mut())
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.iter_mut().map(|(_, value)| value)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        for shard in self.writable_shards() {
            shard.retain(|key, value| keep(key, value));
        }
    }
}

/// A borrowed form of a key must land in the key's own shard, which holds for
/// the identity borrow and for workspace keys hashed by value.
impl<K: ShardKey + Clone, V: Clone> CowShards<K, V> {
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        self.shard(slot_of(key))?.get(key)
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        self.shard(slot_of(key))
            .is_some_and(|shard| shard.contains_key(key))
    }

    /// The shard holding `key` for writing, or `None` when it cannot hold it.
    /// A fork copies the shard only when the key is there.
    fn holding_shard_mut<Q>(&mut self, key: &Q) -> Option<&mut Shard<K, V>>
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        let at = slot_of(key);
        let shard = self.shard(at)?;
        if self.forked && !shard.contains_key(key) {
            return None;
        }
        Some(self.shard_mut(at))
    }

    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        self.holding_shard_mut(key)?.get_mut(key)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.shard_mut(slot_of(&key)).insert(key, value)
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        self.holding_shard_mut(key)?.remove(key)
    }

    pub(crate) fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        self.shard_mut(slot_of(&key)).entry(key)
    }
}

impl<K: ShardKey + Clone, V: Clone + Eq> Eq for CowShards<K, V> {}

impl<K: ShardKey + Clone, V: Clone> Extend<(K, V)> for CowShards<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, entries: I) {
        for (key, value) in entries {
            self.insert(key, value);
        }
    }
}

impl<K: ShardKey + Clone, V: Clone> FromIterator<(K, V)> for CowShards<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(entries: I) -> Self {
        let mut map = Self::default();
        map.extend(entries);
        map
    }
}

impl<K: Clone, V: Clone> IntoIterator for CowShards<K, V> {
    type Item = (K, V);
    type IntoIter = std::iter::Flatten<std::vec::IntoIter<Shard<K, V>>>;

    fn into_iter(mut self) -> Self::IntoIter {
        let end = self.end();
        let shards: Vec<Shard<K, V>> = (0..end)
            .filter_map(|at| {
                self.dirty
                    .remove(&(at as u32))
                    .or_else(|| self.slots.get(at).cloned().flatten())
            })
            .map(Arc::unwrap_or_clone)
            .collect();
        shards.into_iter().flatten()
    }
}

impl<K: ShardKey + Clone, V: Clone, S> From<std::collections::HashMap<K, V, S>>
    for CowShards<K, V>
{
    fn from(map: std::collections::HashMap<K, V, S>) -> Self {
        map.into_iter().collect()
    }
}

impl<K, V, Q> std::ops::Index<&Q> for CowShards<K, V>
where
    K: ShardKey + Clone + Borrow<Q>,
    V: Clone,
    Q: ShardKey + ?Sized,
{
    type Output = V;

    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("no entry for the key")
    }
}

impl<'a, K: Clone, V: Clone> IntoIterator for &'a mut CowShards<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = Box<dyn Iterator<Item = (&'a K, &'a mut V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter_mut())
    }
}

impl<'a, K, V> IntoIterator for &'a CowShards<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (&'a K, &'a V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// A set split into copy-on-write shards the way [`CowShards`] splits a map.
#[derive(Clone)]
pub(crate) struct CowShardSet<K>(CowShards<K, ()>);

impl<K: ShardKey + Clone> PartialEq for CowShardSet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<K> Default for CowShardSet<K> {
    fn default() -> Self {
        Self(CowShards::default())
    }
}

impl<K: std::fmt::Debug> std::fmt::Debug for CowShardSet<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K> CowShardSet<K> {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &K> {
        self.0.keys()
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

impl<K: ShardKey + Clone> CowShardSet<K> {
    pub(crate) fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        self.0.contains_key(key)
    }

    pub(crate) fn insert(&mut self, key: K) -> bool {
        self.0.insert(key, ()).is_none()
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ShardKey + ?Sized,
    {
        self.0.remove(key).is_some()
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.0.retain(|key, _| keep(key));
    }
}

impl<K: ShardKey + Clone> Extend<K> for CowShardSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, keys: I) {
        for key in keys {
            self.insert(key);
        }
    }
}

impl<'a, K> IntoIterator for &'a CowShardSet<K> {
    type Item = &'a K;
    type IntoIter = Box<dyn Iterator<Item = &'a K> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// A value behind one `Arc`: a clone shares it, and the first write through a
/// shared cell copies the whole value.
#[derive(Default, Clone, PartialEq, Eq)]
pub(crate) struct CowCell<T>(Arc<T>);

impl<T: std::fmt::Debug> std::fmt::Debug for CowCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> std::ops::Deref for CowCell<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for CowCell<T> {
    fn deref_mut(&mut self) -> &mut T {
        Arc::make_mut(&mut self.0)
    }
}

impl<T> From<T> for CowCell<T> {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

impl<A, T: FromIterator<A>> FromIterator<A> for CowCell<T> {
    fn from_iter<I: IntoIterator<Item = A>>(iter: I) -> Self {
        Self(Arc::new(T::from_iter(iter)))
    }
}

impl<T: Clone + IntoIterator> IntoIterator for CowCell<T> {
    type Item = T::Item;
    type IntoIter = T::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        Arc::unwrap_or_clone(self.0).into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut CowCell<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        Arc::make_mut(&mut self.0).into_iter()
    }
}

impl<'a, T> IntoIterator for &'a CowCell<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (*self.0).into_iter()
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

    #[test]
    fn a_dirty_shard_answers_reads_and_writes_without_touching_the_base() {
        let mut base: CowShards<LuaTypeOwner, u32> = CowShards::default();
        for file in 0..64 {
            base.insert(owner(file, 0), file);
        }
        let mut fork = base.fork();
        for file in 0..64 {
            *fork.get_mut(&owner(file, 0)).unwrap() += 100;
        }
        assert_eq!(fork.dirty.len(), 64);
        assert!((0..64).all(|file| fork.get(&owner(file, 0)) == Some(&(file + 100))));
        assert!((0..64).all(|file| base.get(&owner(file, 0)) == Some(&file)));
    }

    #[test]
    fn a_cow_cell_copies_on_the_first_write_through_a_shared_value() {
        let mut base: CowCell<Vec<u32>> = CowCell::default();
        base.push(1);
        let mut copy = base.clone();
        copy.push(2);
        assert_eq!((&*base, &*copy), (&vec![1], &vec![1, 2]));
    }
}
