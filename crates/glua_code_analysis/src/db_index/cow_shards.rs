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

/// The slot table grows in steps of this many slots.
const SLOT_CHUNK: usize = 256;

/// Which shard owns a key.
pub(crate) enum ShardOwner {
    File(FileId),
    Workspace,
}

pub(crate) trait ShardKey: Eq + Hash {
    /// Whether a key may be workspace-owned and so need the hash pages.
    const PAGED: bool = true;

    fn shard_owner(&self) -> ShardOwner;
}

impl ShardKey for FileId {
    const PAGED: bool = false;

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
    const PAGED: bool = false;

    fn shard_owner(&self) -> ShardOwner {
        ShardOwner::File(self.get_file_id())
    }
}

impl ShardKey for LuaMemberId {
    const PAGED: bool = false;

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
            const PAGED: bool = false;

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
    const PAGED: bool = false;

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

/// A map whose keys are all file-owned has no hash pages in front of its
/// file shards.
fn file_slot<K: ShardKey + ?Sized>(file_id: FileId) -> usize {
    let pages = if K::PAGED { PAGES } else { 0 };
    pages + file_id.id as usize
}

fn slot_of<K: ShardKey + ?Sized>(key: &K) -> usize {
    match key.shard_owner() {
        ShardOwner::File(file_id) => file_slot::<K>(file_id),
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
/// its writes back in time proportional to the shards it touched. An install
/// into a map whose slot table is shared keeps its shards in `dirty` too,
/// until [`TaskFork::flatten`] folds them into the table.
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
    pub(crate) fn file_entries(&self, file_id: FileId) -> impl Iterator<Item = (&K, &V)>
    where
        K: ShardKey,
    {
        self.shard(file_slot::<K>(file_id))
            .into_iter()
            .flat_map(|shard| shard.iter())
    }

    pub(crate) fn clear(&mut self) {
        self.slots = Arc::from(Vec::new());
        self.dirty.clear();
    }

    /// A copy that shares every shard and keeps its own writes in a dirty list.
    pub(crate) fn fork(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            dirty: self.dirty.clone(),
            forked: true,
        }
    }

    /// The shard at `at` as it is held, behind its `Arc`.
    fn held_shard(&self, at: usize) -> Option<&Arc<Shard<K, V>>> {
        self.dirty
            .get(&(at as u32))
            .or_else(|| self.slots.get(at)?.as_ref())
    }

    /// The slot table, grown to hold `at` and owned by this map alone.
    fn slots_mut(&mut self, at: usize) -> &mut [Option<Arc<Shard<K, V>>>] {
        if at >= self.slots.len() {
            let len = (at + 1).next_multiple_of(SLOT_CHUNK);
            let mut grown = Vec::with_capacity(len);
            grown.extend(self.slots.iter().cloned());
            grown.resize(len, None);
            self.slots = Arc::from(grown);
        }
        Arc::make_mut(&mut self.slots)
    }
}

/// What an install replaced or left over: the shards and values a superstep
/// no longer holds, freed on the drop thread.
struct Replaced<K, V> {
    shards: Vec<Arc<Shard<K, V>>>,
    left: Vec<Shard<K, V>>,
    values: Vec<V>,
}

impl<K, V> Replaced<K, V> {
    fn is_empty(&self) -> bool {
        self.shards.is_empty() && self.left.is_empty() && self.values.is_empty()
    }
}

impl<K, V> Default for Replaced<K, V> {
    fn default() -> Self {
        Self {
            shards: Vec::new(),
            left: Vec::new(),
            values: Vec::new(),
        }
    }
}

/// Drops `garbage` on a thread kept for it, off the path of the barrier
/// that let go of it.
pub(crate) fn drop_apart<T: Send + 'static>(garbage: T) {
    type Garbage = Box<dyn Send>;
    static DROPS: std::sync::OnceLock<Option<std::sync::mpsc::Sender<Garbage>>> =
        std::sync::OnceLock::new();
    let drops = DROPS.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Garbage>();
        std::thread::Builder::new()
            .name("index-drops".into())
            .spawn(move || receiver.into_iter().for_each(drop))
            .ok()
            .map(|_| sender)
    });
    if let Some(drops) = drops {
        let _ = drops.send(Box::new(garbage));
    }
}

impl<K: ShardKey + Clone + Send + Sync + 'static, V: Clone + PartialEq + Send + Sync + 'static>
    CowShards<K, V>
{
    /// The slots `fork`, a fork of `frozen`, may have changed: the shards it
    /// wrote, or every slot when it was replaced or cleared whole.
    fn forked_slots(frozen: &Self, fork: &Self) -> Vec<usize> {
        if Arc::ptr_eq(&fork.slots, &frozen.slots) {
            return fork.dirty.keys().map(|at| *at as usize).collect();
        }
        (0..frozen.end().max(fork.end())).collect()
    }

    /// Takes in what `fork`, a fork of `frozen`, changed: a shard nothing
    /// else changed since is taken whole, any other key by key. Returns
    /// whether `self` had changed a key since that `fork` changed too, to
    /// another value; the fork's value replaces it.
    pub(crate) fn install(&mut self, frozen: &Self, fork: Self) -> bool {
        self.install_with(frozen, fork, None)
    }

    /// [`Self::install`], merging with `merge` each key both `self` and
    /// `fork` changed; only a key `merge` refuses conflicts. The fork's
    /// shards are taken, not copied.
    fn install_with(&mut self, frozen: &Self, mut fork: Self, merge: Option<Merge<V>>) -> bool {
        let mut conflicted = false;
        let mut replaced = Replaced::default();
        for at in Self::forked_slots(frozen, &fork) {
            let was = frozen.held_shard(at);
            if same_shard(was, fork.held_shard(at)) {
                continue;
            }
            let forked = fork.take_held_shard(at);
            if same_shard(was, self.held_shard(at)) {
                replaced.shards.extend(self.place_shard(at, forked));
                continue;
            }
            let was = was.map(|was| &**was);
            let mut forked = forked.map(Arc::unwrap_or_clone).unwrap_or_default();
            let changed = forked
                .iter()
                .filter(|(key, value)| was.and_then(|was| was.get(*key)) != Some(*value))
                .map(|(key, _)| key.clone())
                .chain(
                    was.into_iter()
                        .flat_map(|was| was.keys())
                        .filter(|key| !forked.contains_key(*key))
                        .cloned(),
                )
                .collect::<Vec<_>>();
            let target = self.install_shard_mut(at);
            for key in changed {
                let before = was.and_then(|was| was.get(&key));
                let value = forked.remove(&key);
                let now = target.get(&key);
                let identical = now == value.as_ref();
                let value = if now == before {
                    value
                } else if let Some(merge) = merge {
                    // Merged even when both wrote the same value: two tasks'
                    // equal sums are two changes.
                    let mut merged = target.remove(&key);
                    match merge(&mut merged, before, value.as_ref()) {
                        Ok(()) => merged,
                        Err(Conflict) => {
                            conflicted |= !identical;
                            replaced.values.extend(merged);
                            value
                        }
                    }
                } else if identical {
                    continue;
                } else {
                    conflicted = true;
                    value
                };
                replaced.values.extend(match value {
                    Some(value) => target.insert(key, value),
                    None => target.remove(&key),
                });
            }
            replaced.left.push(forked);
        }
        if !replaced.is_empty() {
            drop_apart(replaced);
        }
        conflicted
    }

    /// [`Self::install_with`] of each of `forks`, a fork of `frozen` each
    /// with its place, in order. While installs write apart, the shards run
    /// on worker threads, each taking the forks that changed it in order, as
    /// installing the forks one after another does: no shard's install reads
    /// another's. Returns the places of the forks that conflicted.
    fn install_all(
        &mut self,
        frozen: &Self,
        forks: Vec<(usize, Self)>,
        merge: Option<Merge<V>>,
    ) -> Vec<usize> {
        use rayon::prelude::*;
        if forks.len() < 2 || !self.writes_apart() {
            return forks
                .into_iter()
                .filter_map(|(place, fork)| self.install_with(frozen, fork, merge).then_some(place))
                .collect();
        }
        let mut by_shard = FxHashMap::<usize, Vec<_>>::default();
        for (place, mut fork) in forks {
            for at in Self::forked_slots(frozen, &fork) {
                if !same_shard(frozen.held_shard(at), fork.held_shard(at)) {
                    by_shard
                        .entry(at)
                        .or_default()
                        .push((place, fork.take_held_shard(at)));
                }
            }
        }
        let shards = by_shard
            .into_iter()
            .map(|(at, forked)| (at, self.take_held_shard(at), forked))
            .collect::<Vec<_>>();
        let installed = shards
            .into_par_iter()
            .map(|(at, held, forked)| {
                (
                    at,
                    Self::install_shard(frozen.held_shard(at), held, forked, merge),
                )
            })
            .collect::<Vec<_>>();
        let mut conflicted = Vec::new();
        let mut replaced = Replaced::default();
        for (at, (shard, conflicts, shard_replaced)) in installed {
            self.dirty.insert(at as u32, shard);
            conflicted.extend(conflicts);
            replaced.shards.extend(shard_replaced.shards);
            replaced.left.extend(shard_replaced.left);
            replaced.values.extend(shard_replaced.values);
        }
        if !replaced.is_empty() {
            drop_apart(replaced);
        }
        conflicted.sort_unstable();
        conflicted.dedup();
        conflicted
    }

    /// Installs each of `forked`, the shards forks of `frozen` left at one
    /// slot with their places, in order, into `held`, the shard the slot
    /// holds, as [`Self::install_with`] does while installs write apart.
    /// Returns the shard left held, the places that conflicted and what the
    /// installs replaced.
    #[allow(clippy::type_complexity)]
    fn install_shard(
        was: Option<&Arc<Shard<K, V>>>,
        mut held: Option<Arc<Shard<K, V>>>,
        forked: Vec<(usize, Option<Arc<Shard<K, V>>>)>,
        merge: Option<Merge<V>>,
    ) -> (Arc<Shard<K, V>>, Vec<usize>, Replaced<K, V>) {
        let mut conflicted = Vec::new();
        let mut replaced = Replaced::default();
        for (place, forked) in forked {
            if same_shard(was, held.as_ref()) {
                replaced
                    .shards
                    .extend(held.replace(forked.unwrap_or_default()));
                continue;
            }
            let was = was.map(|was| &**was);
            let mut forked = forked.map(Arc::unwrap_or_clone).unwrap_or_default();
            let changed = forked
                .iter()
                .filter(|(key, value)| was.and_then(|was| was.get(*key)) != Some(*value))
                .map(|(key, _)| key.clone())
                .chain(
                    was.into_iter()
                        .flat_map(|was| was.keys())
                        .filter(|key| !forked.contains_key(*key))
                        .cloned(),
                )
                .collect::<Vec<_>>();
            let target = Arc::make_mut(held.get_or_insert_with(Default::default));
            let mut fork_conflicted = false;
            for key in changed {
                let before = was.and_then(|was| was.get(&key));
                let value = forked.remove(&key);
                let now = target.get(&key);
                let identical = now == value.as_ref();
                let value = if now == before {
                    value
                } else if let Some(merge) = merge {
                    // Merged even when both wrote the same value: two tasks'
                    // equal sums are two changes.
                    let mut merged = target.remove(&key);
                    match merge(&mut merged, before, value.as_ref()) {
                        Ok(()) => merged,
                        Err(Conflict) => {
                            fork_conflicted |= !identical;
                            replaced.values.extend(merged);
                            value
                        }
                    }
                } else if identical {
                    continue;
                } else {
                    fork_conflicted = true;
                    value
                };
                replaced.values.extend(match value {
                    Some(value) => target.insert(key, value),
                    None => target.remove(&key),
                });
            }
            if fork_conflicted {
                conflicted.push(place);
            }
            replaced.left.push(forked);
        }
        (held.unwrap_or_default(), conflicted, replaced)
    }

    /// Takes the shard at `at` out of a fork that is being installed.
    fn take_held_shard(&mut self, at: usize) -> Option<Arc<Shard<K, V>>> {
        self.dirty
            .remove(&(at as u32))
            .or_else(|| self.slots.get(at).cloned().flatten())
    }

    /// Whether writes go to `dirty`: a fork's always, and an installing
    /// map's while its slot table is shared, so the table is not copied.
    fn writes_apart(&self) -> bool {
        self.forked || Arc::strong_count(&self.slots) > 1
    }

    /// Puts `shard` at `at` for an install, returning the shard it replaces.
    fn place_shard(
        &mut self,
        at: usize,
        shard: Option<Arc<Shard<K, V>>>,
    ) -> Option<Arc<Shard<K, V>>> {
        if self.writes_apart() {
            self.dirty.insert(at as u32, shard.unwrap_or_default())
        } else {
            let kept = self.dirty.remove(&(at as u32));
            std::mem::replace(&mut self.slots_mut(at)[at], shard).or(kept)
        }
    }

    /// The shard at `at` for an install to write.
    fn install_shard_mut(&mut self, at: usize) -> &mut Shard<K, V> {
        if !self.writes_apart() {
            return self.shard_mut(at);
        }
        let slots = &self.slots;
        let shard = self
            .dirty
            .entry(at as u32)
            .or_insert_with(|| slots.get(at).cloned().flatten().unwrap_or_default());
        Arc::make_mut(shard)
    }
}

impl<K: ShardKey + Clone + Send + Sync + 'static, V: Clone + PartialEq + Send + Sync + 'static>
    CowShards<K, V>
{
    /// The keys `fork`, a fork of `frozen`, changed.
    pub(crate) fn changed_keys(frozen: &Self, fork: &Self) -> Vec<K> {
        Self::forked_slots(frozen, fork)
            .into_iter()
            .flat_map(|at| {
                changed_entries(frozen.held_shard(at), fork.held_shard(at))
                    .map(|(key, _)| key.clone())
            })
            .collect()
    }

    /// The keys `fork`, a fork of `frozen`, changed that `self` changed too,
    /// to another value.
    pub(crate) fn both_changed_keys(&self, frozen: &Self, fork: &Self) -> Vec<K> {
        Self::changed_keys(frozen, fork)
            .into_iter()
            .filter(|key| {
                let now = self.get(key);
                now != frozen.get(key) && now != fork.get(key)
            })
            .collect()
    }
}

fn same_shard<K, V>(a: Option<&Arc<Shard<K, V>>>, b: Option<&Arc<Shard<K, V>>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// The entries of `shard` that differ from `frozen`, with `None` for a key
/// `shard` dropped.
fn changed_entries<'a, K: Eq + Hash, V: PartialEq>(
    frozen: Option<&'a Arc<Shard<K, V>>>,
    shard: Option<&'a Arc<Shard<K, V>>>,
) -> impl Iterator<Item = (&'a K, Option<&'a V>)> {
    let changed = shard
        .into_iter()
        .flat_map(|shard| shard.iter())
        .filter(move |(key, value)| frozen.and_then(|frozen| frozen.get(*key)) != Some(*value))
        .map(|(key, value)| (key, Some(value)));
    let dropped = frozen
        .into_iter()
        .flat_map(|frozen| frozen.keys())
        .filter(move |key| !shard.is_some_and(|shard| shard.contains_key(*key)))
        .map(|key| (key, None));
    changed.chain(dropped)
}

impl<K: Clone, V: Clone> CowShards<K, V> {
    fn shard_mut(&mut self, at: usize) -> &mut Shard<K, V> {
        if !self.forked && !self.dirty.contains_key(&(at as u32)) {
            return Arc::make_mut(self.slots_mut(at)[at].get_or_insert_with(Default::default));
        }
        let slots = &self.slots;
        let shard = self
            .dirty
            .entry(at as u32)
            .or_insert_with(|| slots.get(at).cloned().flatten().unwrap_or_default());
        Arc::make_mut(shard)
    }

    /// Folds the shards an install kept in `dirty` into the slot table,
    /// returning the shards they replace.
    fn fold_dirty(&mut self) -> Vec<Arc<Shard<K, V>>> {
        if self.forked || self.dirty.is_empty() {
            return Vec::new();
        }
        let mut replaced = Vec::with_capacity(self.dirty.len());
        for (at, shard) in std::mem::take(&mut self.dirty) {
            let at = at as usize;
            let shard = (!shard.is_empty()).then_some(shard);
            replaced.extend(std::mem::replace(&mut self.slots_mut(at)[at], shard));
        }
        replaced
    }

    /// Makes every non-empty shard writable and returns their slots in order.
    fn writable_shards(&mut self) -> Vec<&mut Shard<K, V>> {
        drop(self.fold_dirty());
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

impl<K: ShardKey + Clone + Send + Sync + 'static> TaskFork for CowShardSet<K> {
    fn task_fork(&self) -> Self {
        Self(self.0.task_fork())
    }

    fn wrote(&self, frozen: &Self) -> bool {
        self.0.wrote(&frozen.0)
    }

    fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
        self.0.install_task(&frozen.0, fork.0)
    }

    fn install_tasks(&mut self, frozen: &Self, forks: Vec<(usize, Self)>) -> Vec<usize> {
        let forks = forks
            .into_iter()
            .map(|(place, fork)| (place, fork.0))
            .collect();
        self.0.install_tasks(&frozen.0, forks)
    }

    fn flatten(&mut self) {
        self.0.flatten();
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

/// Storage a superstep task forks: the task writes its fork apart from the
/// index, and the barrier installs what it wrote.
pub(crate) trait TaskFork: Sized {
    /// A copy whose writes stay apart from `self`.
    fn task_fork(&self) -> Self;

    /// Whether this fork of `frozen` wrote anything.
    fn wrote(&self, frozen: &Self) -> bool;

    /// Takes in what `fork` changed since `frozen`. Returns whether `self`,
    /// which held `frozen` when `fork` was forked from it, had changed a value
    /// since that `fork` changed too, to another value, with no merge of the
    /// two: the fork's value replaces it.
    #[must_use]
    fn install_task(&mut self, frozen: &Self, fork: Self) -> bool;

    /// [`Self::install_task`] of each of `forks`, a fork of `frozen` each
    /// with its place, in order. Returns the places of those that conflicted.
    fn install_tasks(&mut self, frozen: &Self, forks: Vec<(usize, Self)>) -> Vec<usize> {
        forks
            .into_iter()
            .filter_map(|(place, fork)| self.install_task(frozen, fork).then_some(place))
            .collect()
    }

    /// Folds what installs kept apart while storage was shared back into
    /// it, once nothing else shares it.
    fn flatten(&mut self) {}
}

impl<K: ShardKey + Clone + Send + Sync + 'static, V: Clone + PartialEq + Send + Sync + 'static>
    TaskFork for CowShards<K, V>
{
    fn task_fork(&self) -> Self {
        self.fork()
    }

    fn wrote(&self, frozen: &Self) -> bool {
        !Arc::ptr_eq(&self.slots, &frozen.slots)
            || self.dirty.iter().any(|(at, shard)| {
                !frozen
                    .dirty
                    .get(at)
                    .is_some_and(|held| Arc::ptr_eq(held, shard))
            })
    }

    fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
        self.install(frozen, fork)
    }

    fn install_tasks(&mut self, frozen: &Self, forks: Vec<(usize, Self)>) -> Vec<usize> {
        self.install_all(frozen, forks, None)
    }

    fn flatten(&mut self) {
        let replaced = self.fold_dirty();
        if !replaced.is_empty() {
            drop_apart(replaced);
        }
    }
}

/// Installs a value a task replaces whole. Returns whether another task had
/// replaced it with a different one.
fn install_whole<T>(current: &mut T, frozen: &T, fork: T, same: impl Fn(&T, &T) -> bool) -> bool {
    if same(&fork, frozen) {
        return false;
    }
    let conflicted = !same(current, frozen) && !same(current, &fork);
    *current = fork;
    conflicted
}

impl<T: Clone> TaskFork for CowCell<T> {
    fn task_fork(&self) -> Self {
        self.clone()
    }

    fn wrote(&self, frozen: &Self) -> bool {
        !Arc::ptr_eq(&self.0, &frozen.0)
    }

    fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
        install_whole(self, frozen, fork, |a, b| Arc::ptr_eq(&a.0, &b.0))
    }
}

impl<T: ?Sized> TaskFork for Arc<T> {
    fn task_fork(&self) -> Self {
        Arc::clone(self)
    }

    fn wrote(&self, frozen: &Self) -> bool {
        !Arc::ptr_eq(self, frozen)
    }

    fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
        install_whole(self, frozen, fork, Arc::ptr_eq)
    }
}

/// A cache a task may fill: the index keeps its own, which its owner clears
/// when what it caches moves.
impl<T> TaskFork for std::sync::OnceLock<Arc<T>> {
    fn task_fork(&self) -> Self {
        self.clone()
    }

    fn wrote(&self, _frozen: &Self) -> bool {
        false
    }

    fn install_task(&mut self, _frozen: &Self, _fork: Self) -> bool {
        false
    }
}

macro_rules! task_fork_scalars {
    ($($ty:ty),* $(,)?) => {
        $(impl TaskFork for $ty {
            fn task_fork(&self) -> Self {
                *self
            }

            fn wrote(&self, frozen: &Self) -> bool {
                self != frozen
            }

            fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
                install_whole(self, frozen, fork, |a, b| a == b)
            }
        })*
    };
}

task_fork_scalars!(u32, u64, usize, bool, crate::db_index::ModuleNodeId);

/// How a field takes in a value that both the index and a task fork changed
/// since the fork was taken: it turns the index's value, in place, into the
/// value both changes leave, from the value the fork found and the value it
/// left, or reports [`Conflict`] when they do not compose, and the fork's
/// value replaces it. `None` is an absent entry, or a scalar's default.
pub(crate) type Merge<V> = fn(&mut Option<V>, Option<&V>, Option<&V>) -> Merged;

/// Whether a merge composed both changes.
pub(crate) type Merged = Result<(), Conflict>;

/// Two tasks changed a value in ways no merge composes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Conflict;

/// Storage whose fields name a [`Merge`] in [`task_fork_fields!`]: the
/// values two tasks changed apart are merged instead of conflicting.
pub(crate) trait MergeFork: TaskFork {
    type Value;

    /// [`TaskFork::install_task`], merging what `self` changed too; only a
    /// value `merge` refuses conflicts.
    #[must_use]
    fn install_merging(&mut self, frozen: &Self, fork: Self, merge: Merge<Self::Value>) -> bool;

    /// [`Self::install_merging`] of each of `forks` in order, as
    /// [`TaskFork::install_tasks`] installs them.
    fn install_merging_tasks(
        &mut self,
        frozen: &Self,
        forks: Vec<(usize, Self)>,
        merge: Merge<Self::Value>,
    ) -> Vec<usize> {
        forks
            .into_iter()
            .filter_map(|(place, fork)| self.install_merging(frozen, fork, merge).then_some(place))
            .collect()
    }
}

impl<K: ShardKey + Clone + Send + Sync + 'static, V: Clone + PartialEq + Send + Sync + 'static>
    MergeFork for CowShards<K, V>
{
    type Value = V;

    fn install_merging(&mut self, frozen: &Self, fork: Self, merge: Merge<V>) -> bool {
        self.install_with(frozen, fork, Some(merge))
    }

    fn install_merging_tasks(
        &mut self,
        frozen: &Self,
        forks: Vec<(usize, Self)>,
        merge: Merge<V>,
    ) -> Vec<usize> {
        self.install_all(frozen, forks, Some(merge))
    }
}

impl<T: Clone + Default + PartialEq> MergeFork for CowCell<T> {
    type Value = T;

    fn install_merging(&mut self, frozen: &Self, fork: Self, merge: Merge<T>) -> bool {
        if Arc::ptr_eq(&fork.0, &frozen.0) || fork == *frozen {
            return false;
        }
        if Arc::ptr_eq(&self.0, &frozen.0) || *self == *frozen {
            *self = fork;
            return false;
        }
        let identical = *self == fork;
        let mut merged = Some(std::mem::take(&mut **self));
        match merge(&mut merged, Some(&**frozen), Some(&*fork)) {
            Ok(()) => {
                *self = merged.unwrap_or_default().into();
                false
            }
            Err(Conflict) => {
                *self = fork;
                !identical
            }
        }
    }
}

macro_rules! merge_fork_scalars {
    ($($ty:ty),* $(,)?) => {
        $(impl MergeFork for $ty {
            type Value = $ty;

            fn install_merging(&mut self, frozen: &Self, fork: Self, merge: Merge<$ty>) -> bool {
                if fork == *frozen {
                    return false;
                }
                if *self == *frozen {
                    *self = fork;
                    return false;
                }
                let identical = *self == fork;
                let mut merged = Some(*self);
                match merge(&mut merged, Some(frozen), Some(&fork)) {
                    Ok(()) => {
                        *self = merged.unwrap_or_default();
                        false
                    }
                    Err(Conflict) => {
                        *self = fork;
                        !identical
                    }
                }
            }
        })*
    };
}

merge_fork_scalars!(u32, u64);

/// A wrapping sum that each task adds its own terms to and takes its own
/// terms from, so two tasks' changes add up: the index's value plus what the
/// fork added since. Zero is absent.
pub(crate) fn merge_sum<T: Summand>(
    current: &mut Option<T>,
    frozen: Option<&T>,
    fork: Option<&T>,
) -> Merged {
    let value = |value: Option<&T>| value.copied().unwrap_or_default();
    let sum = value(current.as_ref()).add(value(fork).sub(value(frozen)));
    *current = (sum != T::default()).then_some(sum);
    Ok(())
}

/// Makes an emptied value absent, as its owners prune it.
fn prune<T>(merged: &mut Option<T>, is_empty: impl Fn(&T) -> bool) {
    if merged.as_ref().is_some_and(is_empty) {
        *merged = None;
    }
}

/// A value [`merge_sum`] adds up.
pub(crate) trait Summand: Copy + Default + PartialEq {
    fn add(self, other: Self) -> Self;
    fn sub(self, other: Self) -> Self;
}

macro_rules! summands {
    ($($ty:ty),* $(,)?) => {
        $(impl Summand for $ty {
            fn add(self, other: Self) -> Self {
                self.wrapping_add(other)
            }

            fn sub(self, other: Self) -> Self {
                self.wrapping_sub(other)
            }
        })*
    };
}

summands!(u32, u64, usize);

/// A set each task adds its own elements to and drops its own from: the
/// index's elements, less those the fork dropped, plus those it added. An
/// emptied set is absent, as its owners prune it.
pub(crate) fn merge_set<S: SetOf>(
    current: &mut Option<S>,
    frozen: Option<&S>,
    fork: Option<&S>,
) -> Merged {
    let had = |set: Option<&S>, element: &S::Element| set.is_some_and(|set| set.has(element));
    let merged = current.get_or_insert_with(S::default);
    for element in fork.into_iter().flat_map(SetOf::elements) {
        if !had(frozen, element) {
            merged.put(element.clone());
        }
    }
    for element in frozen.into_iter().flat_map(SetOf::elements) {
        if !had(fork, element) {
            merged.drop_element(element);
        }
    }
    prune(current, S::is_empty);
    Ok(())
}

/// A set [`merge_set`] merges element by element.
pub(crate) trait SetOf: Clone + Default {
    type Element: Clone;
    fn elements(&self) -> impl Iterator<Item = &Self::Element>;
    fn has(&self, element: &Self::Element) -> bool;
    fn put(&mut self, element: Self::Element);
    fn drop_element(&mut self, element: &Self::Element);
    fn is_empty(&self) -> bool;
}

impl<T: Eq + Hash + Clone, S: BuildHasher + Default + Clone> SetOf
    for std::collections::HashSet<T, S>
{
    type Element = T;

    fn elements(&self) -> impl Iterator<Item = &T> {
        self.iter()
    }

    fn has(&self, element: &T) -> bool {
        self.contains(element)
    }

    fn put(&mut self, element: T) {
        self.insert(element);
    }

    fn drop_element(&mut self, element: &T) {
        self.remove(element);
    }

    fn is_empty(&self) -> bool {
        std::collections::HashSet::is_empty(self)
    }
}

impl<T: Ord + Clone> SetOf for std::collections::BTreeSet<T> {
    type Element = T;

    fn elements(&self) -> impl Iterator<Item = &T> {
        self.iter()
    }

    fn has(&self, element: &T) -> bool {
        self.contains(element)
    }

    fn put(&mut self, element: T) {
        self.insert(element);
    }

    fn drop_element(&mut self, element: &T) {
        self.remove(element);
    }

    fn is_empty(&self) -> bool {
        std::collections::BTreeSet::is_empty(self)
    }
}

/// A list used as a set whose readers do not depend on its order: the
/// index's elements, less those the fork dropped, with those it added
/// appended in the fork's order. An emptied list is absent, as its owners
/// prune it.
pub(crate) fn merge_list<T: Clone + Eq + Hash>(
    current: &mut Option<Vec<T>>,
    frozen: Option<&Vec<T>>,
    fork: Option<&Vec<T>>,
) -> Merged {
    let frozen = frozen
        .into_iter()
        .flatten()
        .collect::<rustc_hash::FxHashSet<_>>();
    let fork = fork.map_or(&[][..], Vec::as_slice);
    let kept = fork.iter().collect::<rustc_hash::FxHashSet<_>>();
    let merged = current.get_or_insert_with(Vec::new);
    merged.retain(|element| !frozen.contains(element) || kept.contains(element));
    let added = {
        let mut present = merged.iter().collect::<rustc_hash::FxHashSet<_>>();
        fork.iter()
            .filter(|element| !frozen.contains(element) && present.insert(element))
            .cloned()
            .collect::<Vec<_>>()
    };
    merged.extend(added);
    prune(current, Vec::is_empty);
    Ok(())
}

/// [`merge_list`] for a list its owners keep sorted by `key` without
/// duplicates: the merged list is sorted the same way.
pub(crate) fn merge_sorted_by<T: Clone + Eq + Hash, K: Ord>(
    current: &mut Option<Vec<T>>,
    frozen: Option<&Vec<T>>,
    fork: Option<&Vec<T>>,
    key: impl Fn(&T) -> K,
) -> Merged {
    merge_list(current, frozen, fork)?;
    if let Some(merged) = current {
        merged.sort_by_key(key);
    }
    Ok(())
}

/// No merge: for a value only one task of a superstep should change, so two
/// tasks changing it apart conflict.
pub(crate) fn refuse<V>(_: &mut Option<V>, _: Option<&V>, _: Option<&V>) -> Merged {
    Err(Conflict)
}

/// The index's value, for a value the domain's `prepare … then …` hook
/// rebuilds from the merged state after the install.
pub(crate) fn rebuilt<V>(_: &mut Option<V>, _: Option<&V>, _: Option<&V>) -> Merged {
    Ok(())
}

/// A map whose keys each task changes apart: the index's entries with each
/// entry the fork changed taken in, merged by `inner` where the index changed
/// it too. An emptied map is absent, as its owners prune it.
pub(crate) fn merge_map<M: MapOf>(
    current: &mut Option<M>,
    frozen: Option<&M>,
    fork: Option<&M>,
    inner: Merge<M::Value>,
) -> Merged {
    fn entry<'a, M: MapOf>(map: Option<&'a M>, key: &M::Key) -> Option<&'a M::Value> {
        map.and_then(|map| map.entry_of(key))
    }
    let changed = fork
        .into_iter()
        .flat_map(MapOf::keys_of)
        .filter(|key| entry(fork, key) != entry(frozen, key))
        .chain(
            frozen
                .into_iter()
                .flat_map(MapOf::keys_of)
                .filter(|key| entry(fork, key).is_none()),
        );
    let merged = current.get_or_insert_with(M::default);
    for key in changed {
        let (before, value) = (entry(frozen, key), entry(fork, key));
        let now = merged.entry_of(key);
        if now == before {
            match value {
                Some(value) => merged.put_entry(key.clone(), value.clone()),
                None => merged.drop_entry(key),
            }
            continue;
        }
        let identical = now == value;
        let mut slot = merged.take_entry(key);
        match inner(&mut slot, before, value) {
            Ok(()) => {
                if let Some(slot) = slot {
                    merged.put_entry(key.clone(), slot);
                }
            }
            Err(Conflict) if identical => {
                if let Some(value) = value {
                    merged.put_entry(key.clone(), value.clone());
                }
            }
            Err(conflict) => return Err(conflict),
        }
    }
    prune(current, M::is_empty);
    Ok(())
}

/// A map [`merge_map`] merges key by key.
pub(crate) trait MapOf: Clone + Default {
    type Key: Clone;
    type Value: Clone + PartialEq;
    fn keys_of(&self) -> impl Iterator<Item = &Self::Key>;
    fn entry_of(&self, key: &Self::Key) -> Option<&Self::Value>;
    fn put_entry(&mut self, key: Self::Key, value: Self::Value);
    fn take_entry(&mut self, key: &Self::Key) -> Option<Self::Value>;
    fn drop_entry(&mut self, key: &Self::Key);
    fn is_empty(&self) -> bool;
}

impl<K: Eq + Hash + Clone, V: Clone + PartialEq, S: BuildHasher + Default + Clone> MapOf
    for std::collections::HashMap<K, V, S>
{
    type Key = K;
    type Value = V;

    fn keys_of(&self) -> impl Iterator<Item = &K> {
        self.keys()
    }

    fn entry_of(&self, key: &K) -> Option<&V> {
        self.get(key)
    }

    fn put_entry(&mut self, key: K, value: V) {
        self.insert(key, value);
    }

    fn take_entry(&mut self, key: &K) -> Option<V> {
        self.remove(key)
    }

    fn drop_entry(&mut self, key: &K) {
        self.remove(key);
    }

    fn is_empty(&self) -> bool {
        std::collections::HashMap::is_empty(self)
    }
}

impl<K: Ord + Clone, V: Clone + PartialEq> MapOf for std::collections::BTreeMap<K, V> {
    type Key = K;
    type Value = V;

    fn keys_of(&self) -> impl Iterator<Item = &K> {
        self.keys()
    }

    fn entry_of(&self, key: &K) -> Option<&V> {
        self.get(key)
    }

    fn put_entry(&mut self, key: K, value: V) {
        self.insert(key, value);
    }

    fn take_entry(&mut self, key: &K) -> Option<V> {
        self.remove(key)
    }

    fn drop_entry(&mut self, key: &K) {
        self.remove(key);
    }

    fn is_empty(&self) -> bool {
        std::collections::BTreeMap::is_empty(self)
    }
}

/// Installs a field, merging by the field's [`Merge`] when it names one.
/// Returns whether it conflicted.
macro_rules! install_field {
    ($current:expr, $frozen:expr, $fork:expr) => {
        $crate::db_index::cow_shards::TaskFork::install_task(&mut $current, &$frozen, $fork)
    };
    ($current:expr, $frozen:expr, $fork:expr, $merge:expr) => {
        $crate::db_index::cow_shards::MergeFork::install_merging(
            &mut $current,
            &$frozen,
            $fork,
            $merge,
        )
    };
}
pub(crate) use install_field;

/// [`install_field!`] of a field's value in each fork of a column, in order:
/// the places of the forks that conflicted.
macro_rules! install_column {
    ($current:expr, $frozen:expr, $forks:expr) => {
        $crate::db_index::cow_shards::TaskFork::install_tasks(&mut $current, &$frozen, $forks)
    };
    ($current:expr, $frozen:expr, $forks:expr, $merge:expr) => {
        $crate::db_index::cow_shards::MergeFork::install_merging_tasks(
            &mut $current,
            &$frozen,
            $forks,
            $merge,
        )
    };
}
pub(crate) use install_column;

/// What a domain's `prepare` hook takes before its fields are installed.
macro_rules! prepare_install {
    ($index:ident, $frozen:ident, $fork:ident) => {
        ()
    };
    ($index:ident, $frozen:ident, $fork:ident, $before:ident) => {
        $index.$before($frozen, &$fork)
    };
}
pub(crate) use prepare_install;

/// Runs a domain's `then` hook, handing it what `prepare` took.
macro_rules! finish_install {
    ($index:ident, $plan:ident, [], []) => {
        let () = $plan;
    };
    ($index:ident, $plan:ident, [], [$after:ident]) => {{
        let () = $plan;
        $index.$after();
    }};
    ($index:ident, $plan:ident, [$before:ident], [$after:ident]) => {
        $index.$after($plan)
    };
}
pub(crate) use finish_install;

/// Forks and installs an index field by field. Every field is listed, so a
/// new field has to say how a task forks it; `field => merge` names the
/// [`Merge`] for values two tasks change apart. `then` runs after an install,
/// with what `prepare` took from the index and the fork before it.
/// `by_field` installs a superstep's forks field by field in parallel
/// instead, each field taking the forks in order; with `plan => settle`,
/// `plan` first installs in fork order the fields the hooks read, and
/// `settle` runs once every fork is in.
macro_rules! task_fork_fields {
    ($ty:ty { $($(#[$attr:meta])* $field:ident $(=> $merge:expr)?),* $(,)? } $(prepare $before:ident)? $(then $after:ident)? $(by_field $($plan:ident => $settle:ident)?)?) => {
        impl $crate::db_index::cow_shards::TaskFork for $ty {
            fn task_fork(&self) -> Self {
                Self {
                    $($(#[$attr])* $field: $crate::db_index::cow_shards::TaskFork::task_fork(&self.$field),)*
                }
            }

            fn wrote(&self, frozen: &Self) -> bool {
                $($(#[$attr])* if $crate::db_index::cow_shards::TaskFork::wrote(&self.$field, &frozen.$field) {
                    return true;
                })*
                false
            }

            fn install_task(&mut self, frozen: &Self, fork: Self) -> bool {
                let mut conflicts = Vec::new();
                self.install_task_reporting(frozen, fork, &mut conflicts);
                !conflicts.is_empty()
            }

            fn flatten(&mut self) {
                $($(#[$attr])* $crate::db_index::cow_shards::TaskFork::flatten(&mut self.$field);)*
            }
        }

        impl $ty {
            /// [`TaskFork::install_task`](crate::db_index::cow_shards::TaskFork::install_task),
            /// adding to `conflicts` every field that conflicted.
            pub(crate) fn install_task_reporting(
                &mut self,
                frozen: &Self,
                fork: Self,
                conflicts: &mut Vec<&'static str>,
            ) {
                let plan = $crate::db_index::cow_shards::prepare_install!(self, frozen, fork $(, $before)?);
                let Self { $($(#[$attr])* $field),* } = fork;
                $($(#[$attr])* if $crate::db_index::cow_shards::install_field!(self.$field, frozen.$field, $field $(, $merge)?) {
                    conflicts.push(concat!(stringify!($ty), ".", stringify!($field)));
                })*
                $crate::db_index::cow_shards::finish_install!(self, plan, [$($before)?], [$($after)?]);
            }

            /// Installs a superstep's `forks`, each with its place, adding to
            /// `conflicts` at that place every field that conflicted.
            #[allow(dead_code, reason = "an index nested in another installs through it")]
            pub(crate) fn install_tasks_reporting(
                &mut self,
                frozen: &Self,
                forks: Vec<(usize, Self)>,
                conflicts: &mut [Vec<&'static str>],
            ) {
                $crate::db_index::cow_shards::install_tasks_body!(
                    $ty, self, frozen, forks, conflicts,
                    [$(by_field $($plan => $settle)?)?],
                    { $($(#[$attr])* $field $(=> $merge)?),* }
                );
            }
        }
    };
}
pub(crate) use task_fork_fields;

/// The body of a domain's superstep install: fork by fork, or field by field
/// in parallel under `by_field`.
macro_rules! install_tasks_body {
    ($ty:ty, $index:ident, $frozen:ident, $forks:ident, $conflicts:ident, [], { $($(#[$attr:meta])* $field:ident $(=> $merge:expr)?),* }) => {
        for (at, fork) in $forks {
            $index.install_task_reporting($frozen, fork, &mut $conflicts[at]);
        }
    };
    ($ty:ty, $index:ident, $frozen:ident, $forks:ident, $conflicts:ident, [by_field $($plan:ident => $settle:ident)?], { $($(#[$attr:meta])* $field:ident $(=> $merge:expr)?),* }) => {{
        let mut forks = $forks;
        $(let plan = $index.$plan($frozen, &mut forks);)?
        let mut reported: Vec<Vec<usize>> = Vec::new();
        $($(#[$attr])* {
            let _ = stringify!($field);
            reported.push(Vec::new());
        })*
        rayon::scope(|scope| {
            let mut out = reported.iter_mut();
            $($(#[$attr])* {
                let target = &mut $index.$field;
                let base = &$frozen.$field;
                let column = forks
                    .iter_mut()
                    .map(|(at, fork)| (*at, std::mem::replace(&mut fork.$field, base.clone())))
                    .collect::<Vec<_>>();
                let out = out.next().expect("a report per field");
                scope.spawn(move |_| {
                    out.extend($crate::db_index::cow_shards::install_column!(*target, *base, column $(, $merge)?));
                });
            })*
        });
        let mut reported = reported.into_iter();
        $($(#[$attr])* for at in reported.next().expect("a report per field") {
            $conflicts[at].push(concat!(stringify!($ty), ".", stringify!($field)));
        })*
        $($index.$settle(plan);)?
    }};
}
pub(crate) use install_tasks_body;

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

        let frozen = base.clone();
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

        base.install(&frozen, fork);
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
    fn a_map_a_fork_replaces_whole_is_installed_whole() {
        let mut base: CowShards<LuaTypeOwner, u32> = CowShards::default();
        base.insert(owner(1, 0), 10);
        base.insert(owner(2, 0), 20);
        let frozen = base.clone();
        let fork: CowShards<LuaTypeOwner, u32> = [(owner(3, 0), 30)].into_iter().collect();

        assert!(TaskFork::wrote(&fork, &frozen));
        base.install(&frozen, fork);
        assert_eq!(sorted(&base), rendered(vec![(owner(3, 0), 30)]));
    }

    #[test]
    fn forks_writing_other_keys_of_one_page_both_land() {
        let mut base: CowShards<LuaMemberKey, u32> = CowShards::default();
        let frozen = base.clone();
        let mut first = frozen.fork();
        first.insert(path_key("a"), 1);
        let mut second = frozen.fork();
        second.insert(path_key("b"), 2);

        assert!(!base.install(&frozen, first));
        assert!(!base.install(&frozen, second));
        assert_eq!(
            sorted(&base),
            rendered(vec![(path_key("a"), 1), (path_key("b"), 2)])
        );
    }

    #[test]
    fn forks_writing_one_key_apart_conflict_unless_they_agree() {
        let mut base: CowShards<LuaMemberKey, u32> = CowShards::default();
        base.insert(path_key("a"), 0);
        let frozen = base.clone();
        let mut first = frozen.fork();
        first.insert(path_key("a"), 1);
        let mut agreeing = frozen.fork();
        agreeing.insert(path_key("a"), 1);
        let mut other = frozen.fork();
        other.insert(path_key("a"), 2);

        assert!(!base.install(&frozen, first));
        assert!(!base.clone().install(&frozen, agreeing));
        assert!(base.install(&frozen, other));
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

    type Set = std::collections::BTreeSet<u32>;
    type Map = std::collections::BTreeMap<u32, u32>;

    fn set(elements: &[u32]) -> Set {
        elements.iter().copied().collect()
    }

    fn map(entries: &[(u32, u32)]) -> Map {
        entries.iter().copied().collect()
    }

    /// What `merge` makes of `current`, or its conflict.
    fn merged<V: Clone>(
        merge: Merge<V>,
        current: Option<&V>,
        frozen: Option<&V>,
        fork: Option<&V>,
    ) -> Result<Option<V>, Conflict> {
        let mut value = current.cloned();
        merge(&mut value, frozen, fork).map(|()| value)
    }

    #[test]
    fn sums_take_in_both_tasks_changes() {
        assert_eq!(
            merged(merge_sum, Some(&15u64), Some(&10), Some(&13)),
            Ok(Some(18))
        );
    }

    #[test]
    fn sums_that_cancel_out_are_absent() {
        assert_eq!(
            merged(merge_sum, Some(&7u64), Some(&10), Some(&3)),
            Ok(None)
        );
    }

    #[test]
    fn sums_wrap_like_the_fingerprints_they_merge() {
        assert_eq!(
            merged(merge_sum, Some(&1u64), Some(&u64::MAX), Some(&0)),
            Ok(Some(2))
        );
    }

    #[test]
    fn sets_keep_the_index_additions_and_take_the_fork_removals_and_additions() {
        let merged = merged(
            merge_set,
            Some(&set(&[1, 2, 4])),
            Some(&set(&[1, 2])),
            Some(&set(&[2, 5])),
        );
        assert_eq!(merged, Ok(Some(set(&[2, 4, 5]))));
    }

    #[test]
    fn sets_both_tasks_emptied_are_absent() {
        assert_eq!(
            merged(
                merge_set,
                Some(&set(&[2])),
                Some(&set(&[1, 2])),
                Some(&set(&[1]))
            ),
            Ok(None)
        );
    }

    #[test]
    fn sets_both_tasks_created_are_their_union() {
        assert_eq!(
            merged(merge_set, Some(&set(&[1])), None, Some(&set(&[2]))),
            Ok(Some(set(&[1, 2])))
        );
    }

    #[test]
    fn maps_merge_a_key_both_tasks_changed_once() {
        let merged = merged(
            |current, frozen, fork| merge_map(current, frozen, fork, merge_sum),
            Some(&map(&[(1, 12), (2, 2)])),
            Some(&map(&[(1, 10), (2, 2)])),
            Some(&map(&[(1, 11), (2, 2), (3, 3)])),
        );
        assert_eq!(merged, Ok(Some(map(&[(1, 13), (2, 2), (3, 3)]))));
    }

    #[test]
    fn maps_take_a_removal_only_the_fork_made() {
        let merged = merged(
            |current, frozen, fork| merge_map(current, frozen, fork, refuse),
            Some(&map(&[(1, 1), (2, 2), (4, 4)])),
            Some(&map(&[(1, 1), (2, 2)])),
            Some(&map(&[(1, 1)])),
        );
        assert_eq!(merged, Ok(Some(map(&[(1, 1), (4, 4)]))));
    }

    #[test]
    fn maps_conflict_where_their_values_conflict() {
        let merged = merged(
            |current, frozen, fork| merge_map(current, frozen, fork, refuse),
            Some(&map(&[(1, 2)])),
            Some(&map(&[(1, 1)])),
            Some(&map(&[(1, 3)])),
        );
        assert_eq!(merged, Err(Conflict));
    }

    #[test]
    fn lists_keep_the_index_order_and_append_the_fork_additions() {
        let merged = merged(
            merge_list,
            Some(&vec![3, 1, 7]),
            Some(&vec![3, 1]),
            Some(&vec![9, 3, 8]),
        );
        assert_eq!(merged, Ok(Some(vec![3, 7, 9, 8])));
    }

    #[test]
    fn sorted_lists_stay_sorted() {
        let merged = merged(
            |current, frozen, fork| {
                merge_sorted_by(current, frozen, fork, |element: &u32| *element)
            },
            Some(&vec![1, 5]),
            Some(&vec![1]),
            Some(&vec![1, 3]),
        );
        assert_eq!(merged, Ok(Some(vec![1, 3, 5])));
    }

    /// Forks of `frozen` that overlap: two change one key apart, one agrees
    /// with the first, one removes a key, and two write shards no other does.
    fn overlapping_forks(
        frozen: &CowShards<LuaTypeOwner, u64>,
    ) -> Vec<(usize, CowShards<LuaTypeOwner, u64>)> {
        let mut forks = vec![frozen.fork(); 5];
        forks[0].insert(owner(1, 0), 11);
        forks[0].insert(owner(3, 0), 30);
        forks[1].insert(owner(1, 0), 12);
        forks[1].remove(&owner(2, 0));
        forks[2].insert(owner(1, 0), 11);
        forks[2].insert(owner(4, 1), 41);
        forks[3].insert(owner(5, 0), 50);
        forks[4].insert(owner(1, 1), 7);
        forks.into_iter().enumerate().collect()
    }

    #[test]
    fn forks_installed_together_leave_what_installing_them_one_by_one_leaves() {
        let mut base: CowShards<LuaTypeOwner, u64> = CowShards::default();
        for file in 1..=4 {
            base.insert(owner(file, 0), u64::from(file) * 10);
        }
        let frozen = base.clone();
        for merge in [None, Some(merge_sum as Merge<u64>)] {
            let mut one_by_one = base.clone();
            let one_by_one_conflicts = overlapping_forks(&frozen)
                .into_iter()
                .filter_map(|(place, fork)| {
                    one_by_one
                        .install_with(&frozen, fork, merge)
                        .then_some(place)
                })
                .collect::<Vec<_>>();
            let mut together = base.clone();
            let together_conflicts =
                together.install_all(&frozen, overlapping_forks(&frozen), merge);
            assert_eq!(together_conflicts, one_by_one_conflicts);
            assert_eq!(sorted(&together), sorted(&one_by_one));
        }
    }

    #[test]
    fn installing_with_a_merge_takes_in_both_tasks_changes_to_a_key() {
        let mut base: CowShards<LuaMemberKey, u64> = CowShards::default();
        base.insert(path_key("a"), 10);
        let frozen = base.clone();
        let mut first = frozen.fork();
        *first.get_mut(&path_key("a")).unwrap() += 2;
        let mut second = frozen.fork();
        *second.get_mut(&path_key("a")).unwrap() += 3;

        assert!(!base.install_merging(&frozen, first, merge_sum));
        assert!(!base.install_merging(&frozen, second, merge_sum));
        assert_eq!(base.get(&path_key("a")), Some(&15));
    }

    #[test]
    fn an_install_into_a_shared_table_reads_back_takes_writes_and_flattens() {
        let mut base: CowShards<LuaTypeOwner, u32> = CowShards::default();
        base.insert(owner(1, 0), 10);
        let frozen = base.fork();
        let mut fork = frozen.fork();
        fork.insert(owner(2, 0), 20);

        assert!(!base.install(&frozen, fork));
        *base.get_mut(&owner(2, 0)).unwrap() += 1;
        base.insert(owner(1, 5), 15);
        drop(frozen);
        TaskFork::flatten(&mut base);
        assert_eq!(
            (base.dirty.is_empty(), sorted(&base)),
            (
                true,
                rendered(vec![
                    (owner(1, 0), 10),
                    (owner(1, 5), 15),
                    (owner(2, 0), 21)
                ])
            )
        );
    }

    #[test]
    fn a_refused_merge_conflicts_and_takes_the_fork_value() {
        let mut base: CowShards<LuaMemberKey, u64> = CowShards::default();
        base.insert(path_key("a"), 10);
        let frozen = base.clone();
        let mut first = frozen.fork();
        first.insert(path_key("a"), 11);
        let mut second = frozen.fork();
        second.insert(path_key("a"), 12);

        assert!(!base.install_merging(&frozen, first, refuse));
        assert!(base.install_merging(&frozen, second, refuse));
        assert_eq!(base.get(&path_key("a")), Some(&12));
    }

    #[test]
    fn scalars_merge_by_their_rule() {
        let (mut base, frozen) = (13u64, 10u64);
        assert!(!base.install_merging(&frozen, 11, merge_sum));
        assert_eq!(base, 14);
    }

    #[test]
    fn two_tasks_making_the_same_change_to_a_sum_both_count() {
        let mut base: CowShards<LuaMemberKey, u64> = CowShards::default();
        base.insert(path_key("a"), 10);
        let frozen = base.clone();
        let forks = [1, 2].map(|_| {
            let mut fork = frozen.fork();
            *fork.get_mut(&path_key("a")).unwrap() += 1;
            fork
        });
        let mut one_by_one = base.clone();
        for fork in forks.clone() {
            assert!(!one_by_one.install_merging(&frozen, fork, merge_sum));
        }
        let mut together = base.clone();
        let conflicts = together.install_all(
            &frozen,
            forks.into_iter().enumerate().collect(),
            Some(merge_sum),
        );
        let (mut scalar, scalar_frozen) = (10u64, 10u64);
        for fork in [11, 11] {
            assert!(!scalar.install_merging(&scalar_frozen, fork, merge_sum));
        }

        assert_eq!(
            (
                one_by_one.get(&path_key("a")),
                together.get(&path_key("a")),
                conflicts,
                scalar
            ),
            (Some(&12), Some(&12), Vec::new(), 12)
        );
    }

    #[test]
    fn a_cell_a_fork_copied_without_changing_leaves_the_index_value() {
        let base_cell: CowCell<Set> = set(&[1]).into();
        let frozen = base_cell.clone();
        let mut base = base_cell;
        base.insert(2);
        let mut fork = frozen.clone();
        fork.remove(&9);

        assert!(!base.install_merging(&frozen, fork, merge_set));
        assert_eq!(*base, set(&[1, 2]));
    }

    #[test]
    fn a_cell_merges_what_both_tasks_changed() {
        let frozen: CowCell<Set> = set(&[1]).into();
        let mut base = frozen.clone();
        base.insert(2);
        let mut fork = frozen.clone();
        fork.remove(&1);
        fork.insert(3);

        assert!(!base.install_merging(&frozen, fork, merge_set));
        assert_eq!(*base, set(&[2, 3]));
    }
}
