//! Memo maps whose entries outlive the evaluation that computed them.
//!
//! A [`TrackedMap`] entry carries the [`Provenance`] of its value: any later
//! evaluation may reuse it once its reads still hold, recording them as its
//! own. A value its frame could not vouch for stays local to the evaluation
//! that computed it. An [`Untracked`] map keeps no provenance, so a hit taints
//! the open frames.

use std::hash::Hash;
use std::ops::{Deref, DerefMut};

use rustc_hash::FxHashMap;

use crate::GmodRealm;
use crate::db_index::derivation::{self, Gates, Provenance, Reuse};

/// What a lookup is made under: the view's gates, and the inherited query
/// realm with the frame depth its scope began at.
#[derive(Debug, Clone, Copy)]
pub struct Lookup {
    pub(super) gates: Gates,
    pub(super) realm: Option<(GmodRealm, u32)>,
}

#[derive(Debug, Clone)]
pub struct TrackedMap<K, V> {
    shared: FxHashMap<K, (V, Provenance)>,
    local: FxHashMap<K, V>,
    undo: Option<Box<SharedUndo<K, V>>>,
}

#[derive(Debug, Clone)]
struct SharedUndo<K, V> {
    transaction: u64,
    entries: FxHashMap<K, Option<(V, Provenance)>>,
}

impl<K, V> Default for TrackedMap<K, V> {
    fn default() -> Self {
        Self {
            shared: FxHashMap::default(),
            local: FxHashMap::default(),
            undo: None,
        }
    }
}

impl<K, V> TrackedMap<K, V> {
    /// The shared and local tables, for measuring them.
    pub(super) fn tables(&self) -> (&FxHashMap<K, (V, Provenance)>, &FxHashMap<K, V>) {
        (&self.shared, &self.local)
    }
}

impl<K: Hash + Eq + Clone, V> TrackedMap<K, V> {
    /// The value for `key` if it holds under `lookup` with the references
    /// `keyed` walking from a keyed origin, recording what it was computed
    /// from into the open frames. A value computed under other context stays
    /// for where it holds.
    #[track_caller]
    pub fn get(&mut self, key: &K, lookup: Lookup, keyed: &[u64]) -> Option<&V> {
        self.get_with_changes(key, lookup, keyed).0
    }

    #[track_caller]
    pub(super) fn get_with_changes(
        &mut self,
        key: &K,
        lookup: Lookup,
        keyed: &[u64],
    ) -> (Option<&V>, bool) {
        if self.local.contains_key(key) {
            derivation::taint();
            return (self.local.get(key), false);
        }
        let Some((_, provenance)) = self.shared.get(key) else {
            return (None, false);
        };
        match derivation::reuse(provenance, lookup.gates, keyed, lookup.realm) {
            Reuse::Held => (self.shared.get(key).map(|(value, _)| value), false),
            Reuse::Stale => {
                self.remove_shared(key);
                (None, true)
            }
            Reuse::Elsewhere => (None, false),
        }
    }

    /// The value stored for `key`, without checking or recording it: for
    /// deciding whether to drop the entry.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.local
            .get(key)
            .or_else(|| self.shared.get(key).map(|(value, _)| value))
    }

    /// What the shared value for `key` was computed from.
    pub fn provenance(&self, key: &K) -> Option<&Provenance> {
        self.shared.get(key).map(|(_, provenance)| provenance)
    }

    /// Stores `value`, shared when `provenance` vouches for it.
    pub fn insert(&mut self, key: K, value: V, provenance: Option<Provenance>) {
        match provenance {
            Some(provenance) => {
                self.local.remove(&key);
                let key = self.capture_shared(&key).unwrap_or(key);
                self.shared.insert(key, (value, provenance));
            }
            None => {
                self.local.insert(key, value);
            }
        }
    }

    pub fn remove(&mut self, key: &K) {
        self.local.remove(key);
        self.remove_shared(key);
    }

    /// Drops what only this evaluation may use.
    pub fn clear_local(&mut self) {
        if !self.local.is_empty() {
            self.local.clear();
        }
    }

    /// Frees the room local values took once none is left.
    pub(super) fn release_local(&mut self) {
        if self.local.is_empty() && self.local.capacity() != 0 {
            self.local = FxHashMap::default();
        }
    }

    pub fn clear(&mut self) {
        self.local.clear();
        if self.shared.is_empty() {
            return;
        }
        if let Some(transaction) = super::transaction::current() {
            let undo = Self::prepare_undo(&mut self.undo, transaction);
            for (key, value) in self.shared.drain() {
                undo.entry(key).or_insert(Some(value));
            }
        } else {
            self.shared.clear();
        }
    }

    /// Keeps the entries `keep` accepts.
    pub fn retain(&mut self, keep: impl FnMut(&K, &V) -> bool) {
        self.retain_with_changes(keep);
    }

    pub(super) fn retain_with_changes(&mut self, mut keep: impl FnMut(&K, &V) -> bool) -> bool {
        let before = self.shared.len();
        self.local.retain(|key, value| keep(key, value));
        if self.shared.is_empty() {
            return false;
        }
        if let Some(transaction) = super::transaction::current() {
            let undo = &mut self.undo;
            for (key, value) in self.shared.extract_if(|key, (value, _)| !keep(key, value)) {
                let undo = undo.get_or_insert_with(|| {
                    Box::new(SharedUndo {
                        transaction,
                        entries: FxHashMap::default(),
                    })
                });
                assert_eq!(undo.transaction, transaction);
                undo.entries.entry(key).or_insert(Some(value));
            }
        } else {
            self.shared.retain(|key, (value, _)| keep(key, value));
        }
        self.shared.len() != before
    }

    fn prepare_undo(
        undo: &mut Option<Box<SharedUndo<K, V>>>,
        transaction: u64,
    ) -> &mut FxHashMap<K, Option<(V, Provenance)>> {
        let undo = undo.get_or_insert_with(|| {
            Box::new(SharedUndo {
                transaction,
                entries: FxHashMap::default(),
            })
        });
        assert_eq!(undo.transaction, transaction);
        &mut undo.entries
    }

    #[inline]
    fn capture_shared(&mut self, key: &K) -> Option<K> {
        let transaction = super::transaction::current()?;
        self.capture_shared_active(key, transaction)
    }

    #[cold]
    #[inline(never)]
    fn capture_shared_active(&mut self, key: &K, transaction: u64) -> Option<K> {
        let undo = Self::prepare_undo(&mut self.undo, transaction);
        if undo.contains_key(key) {
            return None;
        }
        match self.shared.remove_entry(key) {
            Some((original, value)) => {
                let replacement = original.clone();
                undo.insert(original, Some(value));
                Some(replacement)
            }
            None => {
                undo.insert(key.clone(), None);
                None
            }
        }
    }

    fn remove_shared(&mut self, key: &K) {
        if super::transaction::current().is_some() && self.shared.contains_key(key) {
            self.capture_shared(key);
        }
        self.shared.remove(key);
    }

    pub(super) fn commit_transaction(&mut self) {
        self.undo = None;
    }

    pub(super) fn rollback_transaction(&mut self) {
        if let Some(undo) = self.undo.take() {
            for (key, original) in undo.entries {
                self.shared.remove(&key);
                if let Some(value) = original {
                    self.shared.insert(key, value);
                }
            }
        }
        self.local.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.shared.is_empty()
    }

    /// Whether it holds entries only this evaluation may use.
    pub fn has_local(&self) -> bool {
        !self.local.is_empty()
    }

    pub fn len(&self) -> usize {
        self.local.len() + self.shared.len()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.shared.reserve(additional);
    }
}

/// A memo whose entries record no provenance: a hit taints the open frames.
#[derive(Debug, Clone, Default)]
pub struct Untracked<M>(pub(super) M);

impl<M> Deref for Untracked<M> {
    type Target = M;

    fn deref(&self) -> &M {
        &self.0
    }
}

impl<M> DerefMut for Untracked<M> {
    fn deref_mut(&mut self) -> &mut M {
        &mut self.0
    }
}

impl<K: Hash + Eq, V, S: std::hash::BuildHasher> Untracked<std::collections::HashMap<K, V, S>> {
    #[track_caller]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let value = self.0.get(key);
        if value.is_some() {
            derivation::taint();
        }
        value
    }

    #[track_caller]
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let value = self.0.get_mut(key);
        if value.is_some() {
            derivation::taint();
        }
        value
    }

    #[track_caller]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let found = self.0.contains_key(key);
        if found {
            derivation::taint();
        }
        found
    }

    #[track_caller]
    pub fn entry(&mut self, key: K) -> std::collections::hash_map::Entry<'_, K, V> {
        let entry = self.0.entry(key);
        if matches!(entry, std::collections::hash_map::Entry::Occupied(_)) {
            derivation::taint();
        }
        entry
    }
}

impl<K: Hash + Eq, S: std::hash::BuildHasher> Untracked<std::collections::HashSet<K, S>> {
    #[track_caller]
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let found = self.0.contains(key);
        if found {
            derivation::taint();
        }
        found
    }
}
