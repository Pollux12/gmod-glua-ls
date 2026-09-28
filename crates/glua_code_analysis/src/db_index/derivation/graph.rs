use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use hashbrown::HashTable;
use rustc_hash::{FxHashMap, FxHashSet};

use super::paged::Paged;
use super::{DerivationId, EditMaps, FactKey, Fingerprint, HashedKey};
use crate::FileId;

/// A fact key's slot in a [`KeyTable`].
pub type KeyIdx = u32;
/// A derivation's slot in [`ReaderGraph::derivations`].
type DerivIdx = u32;

/// What one derivation's last evaluation read, with the fingerprint each read
/// saw, and wrote, as sorted key slots.
#[derive(Debug, Clone, Default)]
struct Record {
    reads: Box<[(KeyIdx, Fingerprint)]>,
    outputs: Box<[KeyIdx]>,
}

/// A fact key and the derivations that read or produced it. `key` is `None`
/// while the slot is free.
#[derive(Debug)]
struct KeySlot {
    key: Option<HashedKey>,
    readers: Readers,
    producers: Vec<DerivIdx>,
    /// The key's fingerprint as last taken, and the generation it was taken in
    /// ([`NO_GENERATION`]: none); see [`ReaderGraph::current_fingerprint`].
    /// Only the analysis thread takes fingerprints, so relaxed order suffices.
    current: AtomicU64,
    current_generation: AtomicU32,
    /// The stamp the key was last logged under; see [`KeyTable::log`].
    stamp: u64,
    /// The stamp current when the key was last written.
    written: u64,
    /// Whether an evaluation took the value away to compute it again; see
    /// [`KeyTable::open_window`].
    windowed: bool,
}

impl Clone for KeySlot {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            readers: self.readers.clone(),
            producers: self.producers.clone(),
            current: AtomicU64::new(self.current.load(Ordering::Relaxed)),
            current_generation: AtomicU32::new(self.current_generation.load(Ordering::Relaxed)),
            stamp: self.stamp,
            written: self.written,
            windowed: self.windowed,
        }
    }
}

impl Default for KeySlot {
    fn default() -> Self {
        Self {
            key: None,
            readers: Readers::default(),
            producers: Vec::new(),
            current: AtomicU64::new(0),
            current_generation: AtomicU32::new(NO_GENERATION),
            stamp: 0,
            written: 0,
            windowed: false,
        }
    }
}

const NO_GENERATION: u32 = u32::MAX;

/// A key's readers, grouped by the fingerprint each saw, each group sorted.
/// Nearly every key has one group, so checking its readers against a new
/// value is one comparison per group rather than one per reader.
#[derive(Debug, Clone, Default)]
struct Readers(Vec<(Fingerprint, Vec<DerivIdx>)>);

impl Readers {
    fn insert(&mut self, deriv: DerivIdx, seen: Fingerprint) {
        match self.0.iter_mut().find(|(group, _)| *group == seen) {
            Some((_, derivs)) => {
                if let Err(at) = derivs.binary_search(&deriv) {
                    derivs.insert(at, deriv);
                }
            }
            None => self.0.push((seen, vec![deriv])),
        }
    }

    fn remove(&mut self, deriv: DerivIdx, seen: Fingerprint) {
        if let Some(at) = self.0.iter().position(|(group, _)| *group == seen) {
            if let Ok(index) = self.0[at].1.binary_search(&deriv) {
                self.0[at].1.remove(index);
            }
            if self.0[at].1.is_empty() {
                self.0.swap_remove(at);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (Fingerprint, DerivIdx)> + '_ {
        self.0
            .iter()
            .flat_map(|(seen, derivs)| derivs.iter().map(move |deriv| (*seen, *deriv)))
    }
}

#[derive(Debug, Clone)]
struct DerivSlot {
    id: DerivationId,
    record: Option<Record>,
}

/// The reader graph: for every derivation the facts its last evaluation read
/// (with the fingerprint it saw) and the facts it produced, plus the reverse
/// edges from a fact to its readers and producers.
///
/// Keys and derivations are interned to slots, so recording an evaluation
/// again diffs two sorted slot lists and only the edges that changed touch a
/// map. A slot nothing refers to any more is freed for reuse.
/// Fact keys interned to slots. While the engine evaluates a group, the table
/// is lent to the recording thread so a read is interned where it is made.
#[derive(Debug, Clone, Default)]
pub struct KeyTable {
    /// Each live slot, found by its key's hash: the key itself is kept only
    /// in its slot.
    key_slots: HashTable<KeyIdx>,
    keys: Paged<KeySlot>,
    free_keys: Vec<KeyIdx>,
    /// The keys read out of each base key ([`FactKey::base`]).
    derived: FxHashMap<KeyIdx, DerivedKeys>,
    /// The stamp of the latest write or release of a live slot.
    last_written: u64,
    /// The keys an evaluation took away to compute again, each with the stamp
    /// it was written at before; see [`Self::open_window`].
    windows: FxHashMap<KeyIdx, u64>,
}

/// Whether what a memo read still holds; see [`KeyTable::written_since`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freshness {
    Unwritten,
    /// Only keys an evaluation is computing again were written since: the
    /// memo holds again if they come back as they were.
    Recomputing,
    Written,
}

/// The keys read out of one base key, and the base's fingerprint when their
/// readers were last checked ([`UNCHECKED`]: never).
#[derive(Debug)]
struct DerivedKeys {
    slots: Vec<KeyIdx>,
    checked_at: AtomicU64,
}

const UNCHECKED: Fingerprint = Fingerprint::MAX;

impl Default for DerivedKeys {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            checked_at: AtomicU64::new(UNCHECKED),
        }
    }
}

impl Clone for DerivedKeys {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            checked_at: AtomicU64::new(self.checked_at.load(Ordering::Relaxed)),
        }
    }
}

impl KeyTable {
    /// The slot `key` is interned to.
    fn find(&self, key: &HashedKey) -> Option<KeyIdx> {
        self.key_slots
            .find(key.hash, |slot| {
                self.keys[*slot as usize].key.as_ref() == Some(key)
            })
            .copied()
    }

    /// Unfiles the live `slot`, whose key is `key`.
    fn unfile(&mut self, key: &HashedKey, slot: KeyIdx) {
        if let Ok(entry) = self.key_slots.find_entry(key.hash, |filed| *filed == slot) {
            entry.remove();
        }
    }

    /// Interns `key`. A new slot counts as written at stamp `now`: older
    /// memos cannot have read the key it now names.
    fn intern(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        if let Some(slot) = self.find(&key) {
            return slot;
        }
        let slot = match self.free_keys.pop() {
            Some(slot) => slot,
            None => KeyIdx::try_from(self.keys.push(KeySlot::default()))
                .expect("fewer than 2^32 fact keys"),
        };
        let base = key.base();
        let hash = key.hash;
        let entry = &mut self.keys[slot as usize];
        entry.key = Some(key);
        entry.stamp = 0;
        entry.written = now;
        entry
            .current_generation
            .store(NO_GENERATION, Ordering::Relaxed);
        let keys = &self.keys;
        self.key_slots.insert_unique(hash, slot, |filed| {
            keys[*filed as usize]
                .key
                .as_ref()
                .expect("a filed key slot is live")
                .hash
        });
        if let Some(base) = base {
            let base = self.intern(HashedKey::new(base), now);
            self.derived.entry(base).or_default().slots.push(slot);
        }
        slot
    }

    /// Whether `key` was logged at or after stamp `since`.
    pub(super) fn logged_since(&self, key: &HashedKey, since: u64) -> bool {
        self.find(key)
            .is_some_and(|slot| self.keys[slot as usize].stamp >= since)
    }

    pub(super) fn intern_read(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        self.intern(key, now)
    }

    /// The key in `slot` and the stamp it was last written, for diagnostics.
    pub(super) fn describe(&self, slot: KeyIdx) -> (Option<HashedKey>, u64) {
        let entry = &self.keys[slot as usize];
        (entry.key.clone(), entry.written)
    }

    /// Marks `key` written at stamp `now`, if anything ever read it, and every
    /// key read out of it.
    pub(super) fn mark_written(&mut self, key: &HashedKey, now: u64) {
        if let Some(slot) = self.find(key) {
            self.last_written = self.last_written.max(now);
            self.keys[slot as usize].written = now;
            for derived in self.derived.get(&slot).into_iter().flat_map(|d| &d.slots) {
                self.keys[*derived as usize].written = now;
            }
        }
    }

    /// Marks `key`, and every key read out of it, written at stamp `now` by an
    /// evaluation that computes it again: a memo that read it before then
    /// holds again if [`Self::close_windows`] finds it came back unchanged,
    /// and nothing read from it until then is shared.
    pub(super) fn open_window(&mut self, key: &HashedKey, now: u64) {
        let Some(slot) = self.find(key) else {
            return;
        };
        self.last_written = self.last_written.max(now);
        let derived = self.derived.get(&slot).into_iter().flat_map(|d| &d.slots);
        for slot in std::iter::once(&slot).chain(derived) {
            let entry = &mut self.keys[*slot as usize];
            self.windows.entry(*slot).or_insert(entry.written);
            entry.written = now;
            entry.windowed = true;
        }
    }

    /// Closes every window: a key `unchanged` accepts, and each key read out
    /// of it, is written as it was before its window opened.
    pub(super) fn close_windows(&mut self, unchanged: impl Fn(&HashedKey) -> bool) {
        let windows = std::mem::take(&mut self.windows);
        for (slot, before) in &windows {
            let entry = &mut self.keys[*slot as usize];
            entry.windowed = false;
            let Some(key) = entry.key.as_ref() else {
                continue;
            };
            if key.base().is_some() || !unchanged(key) {
                continue;
            }
            entry.written = *before;
            let derived = self.derived.get(slot).into_iter().flat_map(|d| &d.slots);
            for derived in derived {
                if let Some(before) = windows.get(derived) {
                    self.keys[*derived as usize].written = *before;
                }
            }
        }
    }

    /// Whether `slot`'s value is being computed again.
    pub(super) fn windowed(&self, slot: KeyIdx) -> bool {
        self.keys[slot as usize].windowed
    }

    /// Whether a slot was written or released after stamp `stamp`.
    pub(super) fn written_after(&self, stamp: u64) -> bool {
        self.last_written > stamp
    }

    /// The stamp of the latest write or release of a live slot.
    pub(super) fn last_written(&self) -> u64 {
        self.last_written
    }

    /// Whether any of `slots` was written after stamp `since`.
    pub(super) fn written_since(&self, slots: &[KeyIdx], since: u64) -> Freshness {
        let mut freshness = Freshness::Unwritten;
        for slot in slots {
            let Some(entry) = self.keys.get(*slot as usize) else {
                return Freshness::Written;
            };
            if entry.written <= since {
                continue;
            }
            match self.windows.get(slot) {
                Some(before) if *before <= since => freshness = Freshness::Recomputing,
                _ => return Freshness::Written,
            }
        }
        freshness
    }

    /// Whether a read of `slot` goes into a log whose innermost open frame
    /// began at stamp `frame`: not if it was logged since. Marks it logged at
    /// `latest`, the newest stamp handed out.
    pub(super) fn log(&mut self, slot: KeyIdx, frame: u64, latest: u64) -> bool {
        let entry = &mut self.keys[slot as usize];
        if entry.stamp >= frame {
            return false;
        }
        entry.stamp = latest;
        true
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReaderGraph {
    table: KeyTable,
    deriv_slots: FxHashMap<DerivationId, DerivIdx>,
    derivations: Vec<DerivSlot>,
    free_derivations: Vec<DerivIdx>,
    by_file: FxHashMap<FileId, FxHashSet<DerivIdx>>,
    /// Derivations that produced a key owned by another file, by that file.
    /// Removing the file's facts removes their outputs too, so they must run
    /// again when it is re-analysed.
    foreign_producers: FxHashMap<FileId, FxHashSet<DerivIdx>>,
    /// Cached fingerprints taken in an older generation are forgotten.
    generation: u32,
}

impl ReaderGraph {
    /// Counts and estimated heap sizes of the graph's parts, for the profile.
    pub fn census(&self) -> String {
        use crate::profile::{hash_table_bytes, megabytes};
        use std::mem::size_of;
        let table = &self.table;
        let live = table.keys.iter().filter(|slot| slot.key.is_some()).count();
        let groups: usize = table.keys.iter().map(|slot| slot.readers.0.len()).sum();
        let group_bytes: usize = table
            .keys
            .iter()
            .map(|slot| slot.readers.0.capacity())
            .sum::<usize>()
            * size_of::<(Fingerprint, Vec<DerivIdx>)>();
        let edges: usize = table
            .keys
            .iter()
            .flat_map(|slot| slot.readers.0.iter())
            .map(|(_, readers)| readers.len())
            .sum();
        let set_bytes: usize = table
            .keys
            .iter()
            .flat_map(|slot| slot.readers.0.iter())
            .map(|(_, readers)| readers.capacity() * size_of::<DerivIdx>())
            .sum();
        let producer_bytes: usize = table
            .keys
            .iter()
            .map(|slot| slot.producers.capacity())
            .sum::<usize>()
            * size_of::<DerivIdx>();
        let (reads, outputs) = self
            .derivations
            .iter()
            .filter_map(|slot| slot.record.as_ref())
            .fold((0, 0), |(reads, outputs), record| {
                (reads + record.reads.len(), outputs + record.outputs.len())
            });
        let key_map = hash_table_bytes(table.key_slots.capacity(), size_of::<KeyIdx>());
        let key_slots = table.keys.capacity() * size_of::<KeySlot>();
        let records = reads * size_of::<(KeyIdx, Fingerprint)>() + outputs * size_of::<KeyIdx>();
        let derivation_maps = hash_table_bytes(
            self.deriv_slots.capacity(),
            size_of::<DerivationId>() + size_of::<DerivIdx>(),
        ) + self.derivations.capacity() * size_of::<DerivSlot>();
        let total = key_map
            + key_slots
            + group_bytes
            + set_bytes
            + producer_bytes
            + records
            + derivation_maps;
        let mut by_variant =
            FxHashMap::<std::mem::Discriminant<FactKey>, (usize, &FactKey)>::default();
        for key in table.keys.iter().filter_map(|slot| slot.key.as_deref()) {
            by_variant
                .entry(std::mem::discriminant(key))
                .or_insert((0, key))
                .0 += 1;
        }
        let mut by_variant = by_variant
            .into_values()
            .map(|(count, sample)| {
                let name = format!("{sample:?}");
                let name = name.split(['(', ' ']).next().unwrap_or_default().to_owned();
                (count, name)
            })
            .collect::<Vec<_>>();
        by_variant.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let top = by_variant
            .iter()
            .take(12)
            .map(|(count, variant)| format!("{variant} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "keys {} (live {live}; {top}), reader groups {groups}, edges {edges}, derivations {}, record reads {reads}, outputs {outputs}; MB: key map {}, key slots {}, reader groups {}, reader sets {}, producers {}, records {}, derivation maps {}, total ~{}",
            table.keys.len(),
            self.derivations.len(),
            megabytes(key_map),
            megabytes(key_slots),
            megabytes(group_bytes),
            megabytes(set_bytes),
            megabytes(producer_bytes),
            megabytes(records),
            megabytes(derivation_maps),
            megabytes(total)
        )
    }

    /// Lends the key table out, until [`Self::restore_keys`].
    pub fn lend_keys(&mut self) -> KeyTable {
        std::mem::take(&mut self.table)
    }

    pub fn restore_keys(&mut self, table: KeyTable) {
        debug_assert!(self.table.keys.is_empty(), "keys restored twice");
        self.table = table;
    }

    /// `key`'s fingerprint, taken by `fingerprint` once per value: a read
    /// key's slot keeps it until [`Self::forget_fingerprint`] or
    /// [`Self::forget_fingerprints`].
    pub fn current_fingerprint(
        &self,
        key: &HashedKey,
        fingerprint: impl FnOnce(&HashedKey) -> Fingerprint,
    ) -> Fingerprint {
        match self.table.find(key) {
            Some(slot) => self.slot_fingerprint(slot, fingerprint),
            None => fingerprint(key),
        }
    }

    fn slot_fingerprint(
        &self,
        slot: KeyIdx,
        fingerprint: impl FnOnce(&HashedKey) -> Fingerprint,
    ) -> Fingerprint {
        let entry = &self.table.keys[slot as usize];
        if entry.current_generation.load(Ordering::Relaxed) == self.generation {
            return entry.current.load(Ordering::Relaxed);
        }
        let current = fingerprint(self.key(slot));
        entry.current.store(current, Ordering::Relaxed);
        entry
            .current_generation
            .store(self.generation, Ordering::Relaxed);
        current
    }

    /// Forgets the cached fingerprints of `key` and of every key read out of
    /// it: their values may have moved.
    pub fn forget_fingerprint(&self, key: &HashedKey) {
        if let Some(slot) = self.table.find(key) {
            for slot in std::iter::once(&slot).chain(
                self.table
                    .derived
                    .get(&slot)
                    .into_iter()
                    .flat_map(|derived| &derived.slots),
            ) {
                self.table.keys[*slot as usize]
                    .current_generation
                    .store(NO_GENERATION, Ordering::Relaxed);
            }
        }
    }

    /// The keys read out of `key` ([`FactKey::base`]) that something reads.
    pub fn derived_keys(&self, key: &HashedKey) -> Vec<HashedKey> {
        self.table
            .find(key)
            .and_then(|slot| self.table.derived.get(&slot))
            .into_iter()
            .flat_map(|derived| &derived.slots)
            .map(|slot| self.key(*slot).clone())
            .collect()
    }

    /// The keys read out of `key` whose readers have to be checked now that
    /// `key` fingerprints as `now`: none when they were last checked at that
    /// value, since each is a function of it.
    pub fn derived_keys_to_check(&self, key: &HashedKey, now: Fingerprint) -> Vec<HashedKey> {
        let Some(derived) = self
            .table
            .find(key)
            .and_then(|slot| self.table.derived.get(&slot))
        else {
            return Vec::new();
        };
        if derived.checked_at.swap(now, Ordering::Relaxed) == now {
            return Vec::new();
        }
        derived
            .slots
            .iter()
            .map(|slot| self.key(*slot).clone())
            .collect()
    }

    /// Forgets every cached fingerprint.
    pub fn forget_fingerprints(&mut self) {
        self.generation = (self.generation + 1) % NO_GENERATION;
    }

    /// Every key whose fingerprint is cached, with it.
    pub fn cached_fingerprints(&self) -> impl Iterator<Item = (&HashedKey, Fingerprint)> {
        self.table.keys.iter().filter_map(|slot| {
            (slot.current_generation.load(Ordering::Relaxed) == self.generation)
                .then(|| Some((slot.key.as_ref()?, slot.current.load(Ordering::Relaxed))))?
        })
    }

    /// Replaces `id`'s record with the reads and outputs of its latest
    /// evaluation. Returns the outputs the previous evaluation produced and
    /// this one did not.
    pub fn replace(
        &mut self,
        id: DerivationId,
        reads: Vec<(HashedKey, Option<Fingerprint>)>,
        outputs: Vec<HashedKey>,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
    ) -> Vec<HashedKey> {
        let slots = reads
            .into_iter()
            .map(|(key, seen)| (self.intern_key(key), seen))
            .collect();
        let mut released = Vec::new();
        let dropped = self.replace_slots(id, slots, outputs, fingerprint, &mut released);
        self.release_keys(released);
        dropped
    }

    /// Frees each slot of `slots` nothing refers to any more.
    pub fn release_keys(&mut self, slots: Vec<KeyIdx>) {
        for slot in slots {
            self.release_key(slot);
        }
    }

    /// [`Self::replace`] with the reads already interned. The slots of reads
    /// the record no longer keeps (see [`super::records_read`]) go to
    /// `released`, to free with [`Self::release_keys`] once every record of
    /// the group sharing the lent table is replaced: another member may hold
    /// the same slot.
    pub fn replace_slots(
        &mut self,
        id: DerivationId,
        slots: Vec<(KeyIdx, Option<Fingerprint>)>,
        outputs: Vec<HashedKey>,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
        released: &mut Vec<KeyIdx>,
    ) -> Vec<HashedKey> {
        let deriv = self.intern_derivation(id);
        let output_set = outputs.iter().collect::<FxHashSet<_>>();
        let mut dropped_reads = Vec::new();
        let mut reads = Vec::with_capacity(slots.len());
        for (slot, seen) in slots {
            if !super::records_read(id, self.key(slot), |key| output_set.contains(key)) {
                dropped_reads.push(slot);
                continue;
            }
            let seen = seen.unwrap_or_else(|| self.slot_fingerprint(slot, &fingerprint));
            reads.push((slot, seen));
        }
        drop(output_set);
        reads.sort_unstable_by_key(|(key, _)| *key);
        let mut outputs = outputs
            .into_iter()
            .map(|key| self.intern_key(key))
            .collect::<Vec<_>>();
        outputs.sort_unstable();
        outputs.dedup();
        let old = self.derivations[deriv as usize]
            .record
            .take()
            .unwrap_or_default();

        let mut unread = Vec::new();
        let (mut old_reads, mut new_reads) = (old.reads.iter().peekable(), reads.iter().peekable());
        loop {
            match (old_reads.peek().copied(), new_reads.peek().copied()) {
                (Some((old_key, old_seen)), Some((new_key, new_seen))) if old_key == new_key => {
                    if old_seen != new_seen {
                        let readers = &mut self.table.keys[*new_key as usize].readers;
                        readers.remove(deriv, *old_seen);
                        readers.insert(deriv, *new_seen);
                    }
                    old_reads.next();
                    new_reads.next();
                }
                (Some((old_key, old_seen)), next)
                    if next.is_none_or(|(new_key, _)| old_key < new_key) =>
                {
                    self.table.keys[*old_key as usize]
                        .readers
                        .remove(deriv, *old_seen);
                    unread.push(*old_key);
                    old_reads.next();
                }
                (_, Some((new_key, new_seen))) => {
                    self.table.keys[*new_key as usize]
                        .readers
                        .insert(deriv, *new_seen);
                    new_reads.next();
                }
                (None, None) => break,
                (Some(_), None) => unreachable!("handled above"),
            }
        }

        let mut dropped = Vec::new();
        let mut added = Vec::new();
        merge_sorted(
            old.outputs.iter().copied(),
            outputs.iter().copied(),
            |key| dropped.push(key),
            |key| added.push(key),
        );
        for key in added {
            self.table.keys[key as usize].producers.push(deriv);
            if let Some(owner) = self.key(key).owning_file()
                && owner != id.file_id
            {
                self.foreign_producers
                    .entry(owner)
                    .or_default()
                    .insert(deriv);
            }
        }
        let dropped = dropped
            .into_iter()
            .map(|key| {
                let fact = self.key(key).clone();
                self.unproduce(key, deriv, id.file_id);
                released.push(key);
                fact
            })
            .collect();
        // Only now: a key this evaluation stopped reading may be one it
        // started producing.
        released.extend(unread);
        released.extend(dropped_reads);

        self.by_file.entry(id.file_id).or_default().insert(deriv);
        self.derivations[deriv as usize].record = Some(Record {
            reads: reads.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
        });
        dropped
    }

    /// Forgets every derivation of `file_id`, returning each with the facts
    /// it produced.
    pub fn remove_file(&mut self, file_id: FileId) -> Vec<(DerivationId, Box<[HashedKey]>)> {
        let Some(derivs) = self.by_file.remove(&file_id) else {
            return Vec::new();
        };
        let mut removed: Vec<_> = derivs
            .into_iter()
            .map(|deriv| {
                let id = self.derivations[deriv as usize].id;
                (id, self.detach(deriv))
            })
            .collect();
        removed.sort_by_key(|entry| entry.0);
        removed
    }

    /// The derivations of other files that produced a key owned by `file_id`.
    pub fn foreign_producers(&self, file_id: FileId) -> Vec<DerivationId> {
        let mut ids: Vec<_> = self
            .foreign_producers
            .get(&file_id)
            .into_iter()
            .flatten()
            .map(|deriv| self.derivations[*deriv as usize].id)
            .collect();
        ids.sort();
        ids
    }

    /// The derivations whose recorded read of `key` no longer holds:
    /// `holds(seen)` says whether the value a reader saw is still current.
    pub fn stale_readers<'a>(
        &'a self,
        key: &HashedKey,
        holds: impl Fn(Fingerprint) -> bool + 'a,
    ) -> impl Iterator<Item = DerivationId> + 'a {
        self.table
            .find(key)
            .map(|slot| &self.table.keys[slot as usize].readers.0)
            .into_iter()
            .flatten()
            .filter(move |(seen, _)| !holds(*seen))
            .flat_map(|(_, derivs)| derivs.iter())
            .map(|deriv| self.derivations[*deriv as usize].id)
    }

    /// Whether any read `id` last made no longer holds: it saw another
    /// fingerprint than the key's current one, and `holds_moved` does not
    /// excuse the difference.
    pub fn is_stale(
        &self,
        id: DerivationId,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
        holds_moved: impl Fn(&HashedKey, Fingerprint) -> bool,
    ) -> bool {
        let Some((_, record)) = self.record(id) else {
            return true;
        };
        record.reads.iter().any(|(slot, seen)| {
            *seen != self.slot_fingerprint(*slot, &fingerprint)
                && !holds_moved(self.key(*slot), *seen)
        })
    }

    /// For `GLUALS_CHECK_CACHE`: `id`'s recorded reads by slot.
    pub fn recorded_slots(&self, id: DerivationId) -> Vec<(KeyIdx, Fingerprint)> {
        self.record(id)
            .map(|(_, record)| record.reads.to_vec())
            .unwrap_or_default()
    }

    /// The keys `id` last read.
    pub fn reads_of(&self, id: DerivationId) -> impl Iterator<Item = &HashedKey> + '_ {
        self.record(id)
            .into_iter()
            .flat_map(|(_, record)| record.reads.iter())
            .map(|(key, _)| self.key(*key))
    }

    /// Replaces `id`'s recorded fingerprints, in [`Self::reads_of`] order.
    /// Returns whether any differed.
    pub fn set_fingerprints(&mut self, id: DerivationId, fingerprints: Vec<Fingerprint>) -> bool {
        let Some(deriv) = self.deriv_slots.get(&id).copied() else {
            return false;
        };
        let Some(record) = self.derivations[deriv as usize].record.as_mut() else {
            return false;
        };
        let mut moved = false;
        for ((key, seen), fingerprint) in record.reads.iter_mut().zip(fingerprints) {
            // A stage rule a reader saw is the stage it ran at, which only a
            // stage barrier re-runs it for.
            if *seen != fingerprint
                && !self.table.keys[*key as usize]
                    .key
                    .as_ref()
                    .is_some_and(|key| key.is_stage_rule())
            {
                moved = true;
                let readers = &mut self.table.keys[*key as usize].readers;
                readers.remove(deriv, *seen);
                readers.insert(deriv, fingerprint);
                *seen = fingerprint;
            }
        }
        moved
    }

    /// Whether some reader of `key` saw a value other than the one
    /// `current` fingerprints, taken only if something reads `key`.
    pub fn read_other_than(&self, key: &HashedKey, current: impl FnOnce() -> Fingerprint) -> bool {
        let Some(slot) = self.table.find(key) else {
            return false;
        };
        let readers = &self.table.keys[slot as usize].readers;
        if readers.is_empty() {
            return false;
        }
        let current = current();
        readers.0.iter().any(|(seen, _)| *seen != current)
    }

    /// Relabels the reads of `key` that saw `from` as having seen `to`: after
    /// an edit that only moved the key's positions, what they read is the
    /// value as it stands now, and the next edit maps back from here.
    pub fn rebase_readers(&mut self, key: &HashedKey, from: Fingerprint, to: Fingerprint) {
        let Some(slot) = self.table.find(key) else {
            return;
        };
        let readers = &mut self.table.keys[slot as usize].readers;
        let Some(at) = readers.0.iter().position(|(seen, _)| *seen == from) else {
            return;
        };
        let (_, derivs) = readers.0.swap_remove(at);
        for deriv in &derivs {
            if let Some(record) = self.derivations[*deriv as usize].record.as_mut()
                && let Ok(read) = record.reads.binary_search_by_key(&slot, |(read, _)| *read)
            {
                record.reads[read].1 = to;
            }
        }
        let readers = &mut self.table.keys[slot as usize].readers;
        for deriv in derivs {
            readers.insert(deriv, to);
        }
    }

    /// Renames every recorded read of a key `edits` moved to the key's new
    /// position, keeping the fingerprint the reader saw. Returns the readers
    /// of keys the edits destroyed.
    pub fn apply_edits(&mut self, edits: &EditMaps) -> Vec<DerivationId> {
        let files: FxHashSet<FileId> = edits.files().collect();
        let mut renames = Vec::new();
        let mut lost = Vec::new();
        for (slot, entry) in self.table.keys.iter().enumerate() {
            let Some(key) = entry.key.as_ref() else {
                continue;
            };
            let slot = KeyIdx::try_from(slot).expect("fewer than 2^32 fact keys");
            let readers = &entry.readers;
            if readers.is_empty()
                || !(key
                    .positional_file()
                    .is_some_and(|file_id| files.contains(&file_id))
                    || key.expr_key_type().is_some_and(|typ| {
                        crate::db_index::r#type::type_referenced_files(typ)
                            .iter()
                            .any(|file_id| files.contains(file_id))
                    }))
            {
                continue;
            }
            match edits.map_key(key) {
                Some(moved) if moved == **key => {}
                Some(moved) => renames.push((slot, HashedKey::new(moved))),
                None => lost.extend(
                    readers
                        .iter()
                        .map(|(_, deriv)| self.derivations[deriv as usize].id),
                ),
            }
        }
        // Every moved key gives up its readers before any takes new ones:
        // positions shift in chains, where one key's new place is another's
        // old one.
        let moved = renames
            .into_iter()
            .map(|(slot, new_key)| {
                let readers = std::mem::take(&mut self.table.keys[slot as usize].readers);
                (slot, new_key, readers)
            })
            .collect::<Vec<_>>();
        let mut rewrites: FxHashMap<DerivIdx, FxHashMap<KeyIdx, KeyIdx>> = FxHashMap::default();
        let mut vacated = Vec::with_capacity(moved.len());
        for (slot, new_key, readers) in moved {
            let target = self.intern_key(new_key);
            for (seen, deriv) in readers.iter() {
                self.table.keys[target as usize].readers.insert(deriv, seen);
                rewrites.entry(deriv).or_default().insert(slot, target);
            }
            vacated.push(slot);
        }
        for (deriv, rewrite) in rewrites {
            if let Some(record) = self.derivations[deriv as usize].record.as_mut() {
                let mut reads = record
                    .reads
                    .iter()
                    .map(|(key, seen)| (rewrite.get(key).copied().unwrap_or(*key), *seen))
                    .collect::<Vec<_>>();
                reads.sort_by_key(|(key, _)| *key);
                reads.dedup_by_key(|(key, _)| *key);
                record.reads = reads.into_boxed_slice();
            }
        }
        for slot in vacated {
            self.release_key(slot);
        }
        lost.sort();
        lost.dedup();
        lost
    }

    /// Every key some derivation read.
    #[cfg(test)]
    pub fn read_keys(&self) -> impl Iterator<Item = &FactKey> {
        self.table
            .keys
            .iter()
            .filter(|slot| !slot.readers.is_empty())
            .filter_map(|slot| slot.key.as_deref())
    }

    /// Every derivation with a record.
    #[cfg(test)]
    pub fn derivations(&self) -> impl Iterator<Item = DerivationId> + '_ {
        self.derivations
            .iter()
            .filter(|slot| slot.record.is_some())
            .map(|slot| slot.id)
    }

    pub fn contains(&self, id: DerivationId) -> bool {
        self.record(id).is_some()
    }

    pub fn outputs_of(&self, id: DerivationId) -> impl Iterator<Item = &HashedKey> + '_ {
        self.record(id)
            .into_iter()
            .flat_map(|(_, record)| record.outputs.iter())
            .map(|key| self.key(*key))
    }

    pub fn producers(&self, key: &HashedKey) -> impl Iterator<Item = DerivationId> + '_ {
        self.table
            .find(key)
            .map(|slot| &self.table.keys[slot as usize].producers)
            .into_iter()
            .flatten()
            .map(|deriv| self.derivations[*deriv as usize].id)
    }

    pub fn derivations_of_file(&self, file_id: FileId) -> impl Iterator<Item = DerivationId> + '_ {
        self.by_file
            .get(&file_id)
            .into_iter()
            .flatten()
            .map(|deriv| self.derivations[*deriv as usize].id)
    }

    /// For every file, the other files whose derivations read a fact it
    /// owns.
    pub fn dependent_files(&self) -> FxHashMap<FileId, FxHashSet<FileId>> {
        let mut dependents: FxHashMap<FileId, FxHashSet<FileId>> = FxHashMap::default();
        for slot in self.table.keys.iter() {
            let Some(owner) = slot.key.as_deref().and_then(FactKey::owning_file) else {
                continue;
            };
            for (_, deriv) in slot.readers.iter() {
                let reader = self.derivations[deriv as usize].id.file_id;
                if reader != owner && reader != FileId::VIRTUAL {
                    dependents.entry(owner).or_default().insert(reader);
                }
            }
        }
        dependents
    }

    fn record(&self, id: DerivationId) -> Option<(DerivIdx, &Record)> {
        let deriv = *self.deriv_slots.get(&id)?;
        Some((deriv, self.derivations[deriv as usize].record.as_ref()?))
    }

    fn key(&self, slot: KeyIdx) -> &HashedKey {
        self.table.keys[slot as usize]
            .key
            .as_ref()
            .expect("a referenced key slot is live")
    }

    fn intern_key(&mut self, key: HashedKey) -> KeyIdx {
        self.table.intern(key, super::new_stamp())
    }

    /// Interns a read made while no key table was lent.
    pub fn intern_read(&mut self, key: HashedKey) -> KeyIdx {
        self.table.intern(key, super::new_stamp())
    }

    /// Frees `slot` once no derivation reads or produces its key.
    /// Frees `slot` if no record reads or produces it. A freed slot reads as
    /// written after every memo, so none replays it.
    fn release_key(&mut self, slot: KeyIdx) {
        let entry = &mut self.table.keys[slot as usize];
        if !entry.readers.is_empty()
            || !entry.producers.is_empty()
            || self.table.derived.contains_key(&slot)
        {
            return;
        }
        let Some(key) = entry.key.take() else {
            return;
        };
        entry.written = u64::MAX;
        self.table.last_written = super::new_stamp();
        self.table.unfile(&key, slot);
        self.table.free_keys.push(slot);
        let Some(base) = key
            .base()
            .and_then(|base| self.table.find(&HashedKey::new(base)))
        else {
            return;
        };
        if let Some(derived) = self.table.derived.get_mut(&base) {
            derived.slots.retain(|derived| *derived != slot);
            if derived.slots.is_empty() {
                self.table.derived.remove(&base);
                self.release_key(base);
            }
        }
    }

    fn intern_derivation(&mut self, id: DerivationId) -> DerivIdx {
        if let Some(slot) = self.deriv_slots.get(&id) {
            return *slot;
        }
        let entry = DerivSlot { id, record: None };
        let slot = match self.free_derivations.pop() {
            Some(slot) => {
                self.derivations[slot as usize] = entry;
                slot
            }
            None => {
                self.derivations.push(entry);
                DerivIdx::try_from(self.derivations.len() - 1).expect("fewer than 2^32 derivations")
            }
        };
        self.deriv_slots.insert(id, slot);
        slot
    }

    fn drop_producer(&mut self, key: KeyIdx, deriv: DerivIdx, file_id: FileId) {
        self.unproduce(key, deriv, file_id);
        self.release_key(key);
    }

    /// Removes `deriv` from `key`'s producers, leaving the slot allocated.
    fn unproduce(&mut self, key: KeyIdx, deriv: DerivIdx, file_id: FileId) {
        let producers = &mut self.table.keys[key as usize].producers;
        if let Some(at) = producers.iter().position(|producer| *producer == deriv) {
            producers.swap_remove(at);
        }
        if let Some(owner) = self.key(key).owning_file()
            && owner != file_id
            && let Some(derivs) = self.foreign_producers.get_mut(&owner)
        {
            derivs.remove(&deriv);
            if derivs.is_empty() {
                self.foreign_producers.remove(&owner);
            }
        }
    }

    /// Removes `deriv`'s record and edges and frees its slot. Returns the
    /// facts it produced.
    fn detach(&mut self, deriv: DerivIdx) -> Box<[HashedKey]> {
        let id = self.derivations[deriv as usize].id;
        let record = self.derivations[deriv as usize]
            .record
            .take()
            .unwrap_or_default();
        for (key, seen) in record.reads.iter() {
            self.table.keys[*key as usize].readers.remove(deriv, *seen);
            self.release_key(*key);
        }
        let outputs = record
            .outputs
            .iter()
            .map(|key| {
                let fact = self.key(*key).clone();
                self.drop_producer(*key, deriv, id.file_id);
                fact
            })
            .collect();
        if let Some(derivs) = self.by_file.get_mut(&id.file_id) {
            derivs.remove(&deriv);
            if derivs.is_empty() {
                self.by_file.remove(&id.file_id);
            }
        }
        self.deriv_slots.remove(&id);
        self.free_derivations.push(deriv);
        outputs
    }
}

/// Walks two sorted slot lists, calling `only_old` for each slot only `old`
/// has and `only_new` for each slot only `new` has.
fn merge_sorted(
    old: impl Iterator<Item = KeyIdx>,
    new: impl Iterator<Item = KeyIdx>,
    mut only_old: impl FnMut(KeyIdx),
    mut only_new: impl FnMut(KeyIdx),
) {
    let mut old = old.peekable();
    let mut new = new.peekable();
    loop {
        match (old.peek().copied(), new.peek().copied()) {
            (Some(left), Some(right)) if left == right => {
                old.next();
                new.next();
            }
            (Some(left), Some(right)) if left < right => {
                only_old(left);
                old.next();
            }
            (_, Some(right)) => {
                only_new(right);
                new.next();
            }
            (Some(left), None) => {
                only_old(left);
                old.next();
            }
            (None, None) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::*;
    use crate::db_index::derivation::DerivationKind;

    fn id(file: u32, start: u32) -> DerivationId {
        DerivationId::new(
            DerivationKind::LocalStat,
            FileId::new(file),
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(start), TextSize::new(1)),
            ),
        )
    }

    fn key(file: u32) -> HashedKey {
        HashedKey::new(FactKey::DeclTree(FileId::new(file)))
    }

    #[test]
    fn stale_readers_are_those_that_saw_another_fingerprint() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![key(1)], |_| 0);
        graph.replace(id(2, 0), vec![(key(9), Some(6))], vec![key(2)], |_| 0);

        let stale: Vec<_> = graph.stale_readers(&key(9), |seen| seen == 6).collect();

        assert_eq!(stale, vec![id(1, 0)]);
    }

    #[test]
    fn replace_drops_old_edges_and_reports_outputs_no_longer_produced() {
        let mut graph = ReaderGraph::default();
        graph.replace(
            id(1, 0),
            vec![(key(9), Some(5))],
            vec![key(1), key(2)],
            |_| 0,
        );

        let dropped = graph.replace(id(1, 0), vec![(key(8), Some(1))], vec![key(2)], |_| 0);

        assert_eq!(dropped, vec![key(1)]);
        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 0).count(), 0);
        assert_eq!(graph.producers(&key(1)).count(), 0);
        assert_eq!(graph.producers(&key(2)).collect::<Vec<_>>(), vec![id(1, 0)]);
    }

    #[test]
    fn a_read_kept_across_evaluations_holds_the_latest_fingerprint() {
        let mut graph = ReaderGraph::default();
        graph.replace(
            id(1, 0),
            vec![(key(9), Some(5)), (key(8), Some(1))],
            vec![],
            |_| 0,
        );

        graph.replace(id(1, 0), vec![(key(9), Some(7))], vec![], |_| 0);

        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 7).count(), 0);
        assert_eq!(
            graph
                .stale_readers(&key(9), |seen| seen == 5)
                .collect::<Vec<_>>(),
            vec![id(1, 0)]
        );
        assert_eq!(graph.stale_readers(&key(8), |_| false).count(), 0);
        assert_eq!(graph.reads_of(id(1, 0)).collect::<Vec<_>>(), vec![&key(9)]);
    }

    #[test]
    fn a_key_nothing_refers_to_is_forgotten_and_its_slot_reused() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![key(1)], |_| 0);
        graph.replace(id(1, 0), vec![], vec![], |_| 0);

        assert_eq!(graph.read_keys().count(), 0);
        assert_eq!(graph.producers(&key(1)).count(), 0);

        graph.replace(id(1, 0), vec![(key(7), Some(1))], vec![key(6)], |_| 0);

        assert_eq!(graph.table.keys.len(), 2);
    }

    #[test]
    fn remove_file_returns_outputs_and_forgets_its_readers() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![key(1)], |_| 0);
        graph.replace(id(2, 0), vec![(key(1), Some(5))], vec![key(2)], |_| 0);

        let removed = graph.remove_file(FileId::new(1));

        assert_eq!(removed.len(), 1);
        assert_eq!(&*removed[0].1, &[key(1)]);
        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 0).count(), 0);
        assert_eq!(
            graph
                .stale_readers(&key(1), |seen| seen == 0)
                .collect::<Vec<_>>(),
            vec![id(2, 0)]
        );
        assert_eq!(graph.derivations_of_file(FileId::new(1)).count(), 0);
    }

    #[test]
    fn a_derivation_writing_another_files_key_is_its_foreign_producer() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(2, 0), vec![], vec![key(1), key(2)], |_| 0);

        assert_eq!(graph.foreign_producers(FileId::new(1)), vec![id(2, 0)]);
        assert!(graph.foreign_producers(FileId::new(2)).is_empty());

        graph.replace(id(2, 0), vec![], vec![key(2)], |_| 0);

        assert!(graph.foreign_producers(FileId::new(1)).is_empty());
    }

    #[test]
    fn a_slot_one_member_drops_stays_live_for_another_member_of_its_group() {
        let mut graph = ReaderGraph::default();
        let mut table = graph.lend_keys();
        let slot = table.intern_read(key(9), 0);
        assert!(table.log(slot, 1, 1));
        assert_eq!(table.intern_read(key(9), 0), slot);
        assert!(table.log(slot, 2, 2));
        graph.restore_keys(table);
        let producer = DerivationId::new(
            DerivationKind::LocalStat,
            FileId::new(9),
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(0), TextSize::new(1)),
            ),
        );

        let mut released = Vec::new();
        graph.replace_slots(
            producer,
            vec![(slot, None)],
            vec![key(9)],
            |_| 0,
            &mut released,
        );
        graph.replace_slots(id(2, 0), vec![(slot, None)], vec![], |_| 5, &mut released);
        graph.release_keys(released);

        assert_eq!(graph.reads_of(id(2, 0)).collect::<Vec<_>>(), vec![&key(9)]);
        assert_eq!(graph.reads_of(producer).count(), 0);
    }

    #[test]
    fn a_slot_one_member_stops_producing_stays_live_for_another_member_of_its_group() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![], vec![key(9)], |_| 0);
        let mut table = graph.lend_keys();
        let slot = table.intern_read(key(9), 1);
        graph.restore_keys(table);

        let mut released = Vec::new();
        graph.replace_slots(id(1, 0), vec![], vec![], |_| 0, &mut released);
        graph.replace_slots(id(2, 0), vec![(slot, None)], vec![], |_| 5, &mut released);
        graph.release_keys(released);

        assert_eq!(graph.reads_of(id(2, 0)).collect::<Vec<_>>(), vec![&key(9)]);
        assert_eq!(graph.producers(&key(9)).count(), 0);
    }

    #[test]
    fn an_evaluation_begun_inside_another_suspends_it_until_it_ends() {
        use super::super::{begin, end};

        begin(id(1, 0));
        begin(id(2, 0));

        assert_eq!(end().id, id(2, 0));
        assert_eq!(end().id, id(1, 0));
    }

    #[test]
    fn a_key_read_before_and_produced_now_stays_live() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![], |_| 0);

        graph.replace(id(1, 0), vec![], vec![key(9)], |_| 0);

        assert_eq!(graph.producers(&key(9)).collect::<Vec<_>>(), vec![id(1, 0)]);
        assert_eq!(
            graph.outputs_of(id(1, 0)).collect::<Vec<_>>(),
            vec![&key(9)]
        );
    }
}
