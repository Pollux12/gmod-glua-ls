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
}

impl<K, V> Default for TrackedMap<K, V> {
    fn default() -> Self {
        Self {
            shared: FxHashMap::default(),
            local: FxHashMap::default(),
        }
    }
}

impl<K: Hash + Eq, V> TrackedMap<K, V> {
    /// The value for `key` if it holds under `lookup` with the references
    /// `keyed` walking from a keyed origin, recording what it was computed
    /// from into the open frames. A value computed under other context stays
    /// for where it holds.
    #[track_caller]
    pub fn get(&mut self, key: &K, lookup: Lookup, keyed: &[u64]) -> Option<&V> {
        if self.local.contains_key(key) {
            derivation::taint();
            return self.local.get(key);
        }
        let (_, provenance) = self.shared.get(key)?;
        match derivation::reuse(provenance, lookup.gates, keyed, lookup.realm) {
            Reuse::Held => self.shared.get(key).map(|(value, _)| value),
            Reuse::Stale => {
                self.shared.remove(key);
                None
            }
            Reuse::Elsewhere => None,
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
                self.shared.insert(key, (value, provenance));
            }
            None => {
                self.local.insert(key, value);
            }
        }
    }

    /// Stores `value` for this evaluation only.
    pub fn insert_local(&mut self, key: K, value: V) {
        self.local.insert(key, value);
    }

    pub fn remove(&mut self, key: &K) {
        self.local.remove(key);
        self.shared.remove(key);
    }

    /// Drops what only this evaluation may use.
    pub fn clear_local(&mut self) {
        if !self.local.is_empty() {
            self.local.clear();
        }
    }

    pub fn clear(&mut self) {
        self.local.clear();
        self.shared.clear();
    }

    /// Keeps the entries `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.local.retain(|key, value| keep(key, value));
        self.shared.retain(|key, (value, _)| keep(key, value));
    }

    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.shared.is_empty()
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
