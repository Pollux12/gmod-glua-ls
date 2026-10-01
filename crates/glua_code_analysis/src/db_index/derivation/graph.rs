use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use hashbrown::HashTable;
use rustc_hash::{FxHashMap, FxHashSet};

use super::paged::Paged;
use super::{DerivationId, DerivationKind, EditMaps, FactKey, Fingerprint, HashedKey};
use crate::FileId;

/// A fact key's slot in a [`KeyTable`].
pub type KeyIdx = u32;
/// A derivation's slot in [`ReaderGraph::derivations`].
type DerivIdx = u32;

/// What one derivation's last evaluation read and wrote, as sorted key slots.
/// The fingerprint a read saw is the reader group the derivation sits in on
/// the key ([`Readers`]).
#[derive(Debug, Clone, Default)]
struct Record {
    reads: Box<[KeyIdx]>,
    outputs: Box<[KeyIdx]>,
}

/// A fact key and the derivations that read or produced it. `key` is `None`
/// while the slot is free.
#[derive(Debug)]
struct KeySlot {
    key: Option<HashedKey>,
    readers: Readers,
    /// The key's first producer ([`NO_PRODUCER`]: none); the rest are in
    /// [`KeyTable::more_producers`].
    producer: DerivIdx,
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
    /// Whether [`KeyTable::more_producers`] holds producers of the key.
    more_producers: bool,
}

impl Clone for KeySlot {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            readers: self.readers.clone(),
            producer: self.producer,
            current: AtomicU64::new(self.current.load(Ordering::Relaxed)),
            current_generation: AtomicU32::new(self.current_generation.load(Ordering::Relaxed)),
            stamp: self.stamp,
            written: self.written,
            windowed: self.windowed,
            more_producers: self.more_producers,
        }
    }
}

impl Default for KeySlot {
    fn default() -> Self {
        Self {
            key: None,
            readers: Readers::default(),
            producer: NO_PRODUCER,
            current: AtomicU64::new(0),
            current_generation: AtomicU32::new(NO_GENERATION),
            stamp: 0,
            written: 0,
            windowed: false,
            more_producers: false,
        }
    }
}

const NO_GENERATION: u32 = u32::MAX;
const NO_PRODUCER: DerivIdx = DerivIdx::MAX;

/// A key's readers, grouped by the fingerprint each saw, each group sorted.
/// A derivation reads a key at one value, so it is in one group. Nearly every
/// key has one group, so checking its readers against a new value is one
/// comparison per group rather than one per reader, and one group is kept in
/// the slot.
#[derive(Debug, Clone, Default)]
enum Readers {
    #[default]
    None,
    One(Fingerprint, DerivIdx),
    /// Two or more readers.
    Group(Fingerprint, Vec<DerivIdx>),
    /// Two or more groups.
    Groups(Vec<(Fingerprint, Vec<DerivIdx>)>),
}

const _: () = assert!(std::mem::size_of::<Readers>() <= 32);

impl Readers {
    /// Files `deriv` as having seen `seen`, out of the group it was in.
    fn insert(&mut self, deriv: DerivIdx, seen: Fingerprint) {
        match self {
            Self::None => *self = Self::One(seen, deriv),
            Self::One(group, reader) if *reader == deriv => *group = seen,
            Self::One(group, reader) if *group == seen => {
                let pair = if *reader < deriv {
                    vec![*reader, deriv]
                } else {
                    vec![deriv, *reader]
                };
                *self = Self::Group(seen, pair);
            }
            Self::Group(group, derivs) if *group == seen => {
                if let Err(at) = derivs.binary_search(&deriv) {
                    derivs.insert(at, deriv);
                }
            }
            Self::Groups(groups) => {
                for (group, derivs) in groups.iter_mut() {
                    if *group == seen {
                        if let Err(at) = derivs.binary_search(&deriv) {
                            derivs.insert(at, deriv);
                        }
                    } else {
                        take_from(derivs, deriv);
                    }
                }
                if !groups.iter().any(|(group, _)| *group == seen) {
                    groups.push((seen, vec![deriv]));
                }
                self.collapse();
            }
            Self::One(group, reader) => {
                *self = Self::Groups(vec![(*group, vec![*reader]), (seen, vec![deriv])]);
            }
            Self::Group(group, derivs) => {
                take_from(derivs, deriv);
                let derivs = std::mem::take(derivs);
                *self = Self::Groups(vec![(*group, derivs), (seen, vec![deriv])]);
                self.collapse();
            }
        }
    }

    fn remove(&mut self, deriv: DerivIdx) {
        match self {
            Self::None => {}
            Self::One(_, reader) => {
                if *reader == deriv {
                    *self = Self::None;
                }
            }
            Self::Group(seen, derivs) => {
                take_from(derivs, deriv);
                if let [reader] = derivs.as_slice() {
                    *self = Self::One(*seen, *reader);
                }
            }
            Self::Groups(groups) => {
                for (_, derivs) in groups.iter_mut() {
                    take_from(derivs, deriv);
                }
                self.collapse();
            }
        }
    }

    /// The fingerprint `deriv`, one of the readers, saw.
    fn seen_by(&self, deriv: DerivIdx) -> Fingerprint {
        match self {
            Self::One(seen, reader) => {
                debug_assert_eq!(*reader, deriv);
                *seen
            }
            Self::Group(seen, derivs) => {
                debug_assert!(derivs.binary_search(&deriv).is_ok());
                *seen
            }
            _ => self
                .groups()
                .find(|(_, derivs)| derivs.binary_search(&deriv).is_ok())
                .map(|(seen, _)| seen)
                .expect("a recorded read is filed among the key's readers"),
        }
    }

    /// Relabels the readers that saw `from` as having seen `to`.
    fn relabel(&mut self, from: Fingerprint, to: Fingerprint) {
        match self {
            Self::None => {}
            Self::One(seen, _) | Self::Group(seen, _) => {
                if *seen == from {
                    *seen = to;
                }
            }
            Self::Groups(groups) => {
                if from == to {
                    return;
                }
                let Some(at) = groups.iter().position(|(seen, _)| *seen == from) else {
                    return;
                };
                let (_, derivs) = groups.swap_remove(at);
                match groups.iter_mut().find(|(seen, _)| *seen == to) {
                    Some((_, into)) => {
                        into.extend(derivs);
                        into.sort_unstable();
                    }
                    None => groups.push((to, derivs)),
                }
                self.collapse();
            }
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Each group's fingerprint and its readers.
    fn groups(&self) -> impl Iterator<Item = (Fingerprint, &[DerivIdx])> {
        let (single, many) = match self {
            Self::None => (None, &[][..]),
            Self::One(seen, reader) => (Some((*seen, std::slice::from_ref(reader))), &[][..]),
            Self::Group(seen, derivs) => (Some((*seen, derivs.as_slice())), &[][..]),
            Self::Groups(groups) => (None, &groups[..]),
        };
        single
            .into_iter()
            .chain(many.iter().map(|(seen, derivs)| (*seen, derivs.as_slice())))
    }

    fn iter(&self) -> impl Iterator<Item = (Fingerprint, DerivIdx)> + '_ {
        self.groups()
            .flat_map(|(seen, derivs)| derivs.iter().map(move |deriv| (seen, *deriv)))
    }

    /// The heap bytes of the group list and of the reader lists.
    fn heap_bytes(&self) -> (usize, usize) {
        let list = |derivs: &Vec<DerivIdx>| derivs.capacity() * std::mem::size_of::<DerivIdx>();
        match self {
            Self::None | Self::One(..) => (0, 0),
            Self::Group(_, derivs) => (0, list(derivs)),
            Self::Groups(groups) => (
                groups.capacity() * std::mem::size_of::<(Fingerprint, Vec<DerivIdx>)>(),
                groups.iter().map(|(_, derivs)| list(derivs)).sum(),
            ),
        }
    }

    /// Drops emptied groups, and keeps what is left in the smallest form.
    fn collapse(&mut self) {
        let Self::Groups(groups) = self else {
            return;
        };
        groups.retain(|(_, derivs)| !derivs.is_empty());
        if groups.len() > 1 {
            return;
        }
        *self = match groups.pop() {
            None => Self::None,
            Some((seen, derivs)) => match derivs.as_slice() {
                [reader] => Self::One(seen, *reader),
                _ => Self::Group(seen, derivs),
            },
        };
    }
}

/// Removes `deriv` from the sorted `derivs`, if there.
fn take_from(derivs: &mut Vec<DerivIdx>, deriv: DerivIdx) {
    if let Ok(index) = derivs.binary_search(&deriv) {
        derivs.remove(index);
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
    /// The producers of a key past the one its slot keeps: few keys have more
    /// than one.
    more_producers: FxHashMap<KeyIdx, Vec<DerivIdx>>,
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
    pub(super) fn find(&self, key: &HashedKey) -> Option<KeyIdx> {
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

    pub(super) fn key(&self, slot: KeyIdx) -> &HashedKey {
        self.keys[slot as usize]
            .key
            .as_ref()
            .expect("a referenced key slot is live")
    }

    /// Whether `key` was logged at or after stamp `since`.
    pub(super) fn logged_since(&self, key: &HashedKey, since: u64) -> bool {
        self.find(key)
            .is_some_and(|slot| self.keys[slot as usize].stamp >= since)
    }

    pub(super) fn intern_read(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        self.intern(key, now)
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

    fn add_producer(&mut self, slot: KeyIdx, deriv: DerivIdx) {
        let entry = &mut self.keys[slot as usize];
        if entry.producer == NO_PRODUCER {
            entry.producer = deriv;
        } else {
            entry.more_producers = true;
            self.more_producers.entry(slot).or_default().push(deriv);
        }
    }

    fn remove_producer(&mut self, slot: KeyIdx, deriv: DerivIdx) {
        let entry = &mut self.keys[slot as usize];
        if !entry.more_producers {
            if entry.producer == deriv {
                entry.producer = NO_PRODUCER;
            }
            return;
        }
        let more = self
            .more_producers
            .get_mut(&slot)
            .expect("a slot marked with more producers has them");
        if entry.producer == deriv {
            entry.producer = more.pop().expect("more producers are never empty");
        } else if let Some(at) = more.iter().position(|producer| *producer == deriv) {
            more.swap_remove(at);
        }
        if more.is_empty() {
            entry.more_producers = false;
            self.more_producers.remove(&slot);
        }
    }

    fn producers(&self, slot: KeyIdx) -> impl Iterator<Item = DerivIdx> + '_ {
        let entry = &self.keys[slot as usize];
        let more = entry
            .more_producers
            .then(|| self.more_producers.get(&slot))
            .flatten();
        (entry.producer != NO_PRODUCER)
            .then_some(entry.producer)
            .into_iter()
            .chain(more.into_iter().flatten().copied())
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
    /// began at stamp `frame`: not if it was logged since, else whether this
    /// is its first log since the evaluation began at stamp `evaluation`.
    /// Marks it logged at `latest`, the newest stamp handed out.
    pub(super) fn log(
        &mut self,
        slot: KeyIdx,
        frame: u64,
        evaluation: u64,
        latest: u64,
    ) -> Option<bool> {
        let entry = &mut self.keys[slot as usize];
        if entry.stamp >= frame {
            return None;
        }
        let first = entry.stamp < evaluation;
        entry.stamp = latest;
        Some(first)
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
        let groups: usize = table
            .keys
            .iter()
            .map(|slot| slot.readers.groups().count())
            .sum();
        let edges: usize = table
            .keys
            .iter()
            .map(|slot| slot.readers.iter().count())
            .sum();
        let (group_bytes, set_bytes) = table
            .keys
            .iter()
            .map(|slot| slot.readers.heap_bytes())
            .fold((0, 0), |(groups, sets), (group, set)| {
                (groups + group, sets + set)
            });
        let producer_bytes = hash_table_bytes(
            table.more_producers.capacity(),
            size_of::<(KeyIdx, Vec<DerivIdx>)>(),
        ) + table
            .more_producers
            .values()
            .map(|more| more.capacity() * size_of::<DerivIdx>())
            .sum::<usize>();
        let (reads, outputs) = self
            .derivations
            .iter()
            .filter_map(|slot| slot.record.as_ref())
            .fold((0, 0), |(reads, outputs), record| {
                (reads + record.reads.len(), outputs + record.outputs.len())
            });
        let key_map = hash_table_bytes(table.key_slots.capacity(), size_of::<KeyIdx>());
        let key_slots = table.keys.capacity() * size_of::<KeySlot>();
        let records = (reads + outputs) * size_of::<KeyIdx>();
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

    pub(super) fn slot_fingerprint(
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

    /// Files `reads`, made viewing this graph as it stands, as `id`'s record.
    /// The slots of reads its previous record no longer keeps go to
    /// `released`, to free with [`Self::release_keys`] once every record
    /// viewed alongside is filed: another may hold the same slot.
    pub fn file_viewed(
        &mut self,
        id: DerivationId,
        reads: super::ViewedReads,
        released: &mut Vec<KeyIdx>,
    ) {
        let mut slots = reads
            .slots
            .into_iter()
            .map(|(slot, seen)| (slot, Some(seen)))
            .collect::<Vec<_>>();
        slots.extend(
            reads
                .keys
                .into_iter()
                .map(|(key, seen)| (self.intern_key(key), Some(seen))),
        );
        self.replace_slots(id, slots, Vec::new(), |_| super::UNSEEN, released);
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
        let (mut old_reads, mut new_reads) = (
            old.reads.iter().copied().peekable(),
            reads.iter().copied().peekable(),
        );
        loop {
            match (old_reads.peek().copied(), new_reads.peek().copied()) {
                (Some(old_key), Some((new_key, new_seen))) if old_key == new_key => {
                    let readers = &mut self.table.keys[new_key as usize].readers;
                    if readers.seen_by(deriv) != new_seen {
                        readers.insert(deriv, new_seen);
                    }
                    old_reads.next();
                    new_reads.next();
                }
                (Some(old_key), next) if next.is_none_or(|(new_key, _)| old_key < new_key) => {
                    self.table.keys[old_key as usize].readers.remove(deriv);
                    unread.push(old_key);
                    old_reads.next();
                }
                (_, Some((new_key, new_seen))) => {
                    self.table.keys[new_key as usize]
                        .readers
                        .insert(deriv, new_seen);
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
            self.table.add_producer(key, deriv);
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
            reads: reads.into_iter().map(|(key, _)| key).collect(),
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
        self.stale_readers_seen(key, holds).map(|(id, _)| id)
    }

    /// [`Self::stale_readers`], each with the fingerprint it saw.
    pub fn stale_readers_seen<'a>(
        &'a self,
        key: &HashedKey,
        holds: impl Fn(Fingerprint) -> bool + 'a,
    ) -> impl Iterator<Item = (DerivationId, Fingerprint)> + 'a {
        self.table
            .find(key)
            .map(|slot| self.table.keys[slot as usize].readers.groups())
            .into_iter()
            .flatten()
            .filter(move |(seen, _)| !holds(*seen))
            .flat_map(|(seen, derivs)| derivs.iter().map(move |deriv| (deriv, seen)))
            .map(|(deriv, seen)| (self.derivations[*deriv as usize].id, seen))
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
        let Some((deriv, record)) = self.record(id) else {
            return true;
        };
        record.reads.iter().any(|slot| {
            let seen = self.seen(*slot, deriv);
            seen != self.slot_fingerprint(*slot, &fingerprint)
                && !holds_moved(self.key(*slot), seen)
        })
    }

    /// For `GLUALS_CHECK_CACHE`: `id`'s recorded reads by slot.
    pub fn recorded_slots(&self, id: DerivationId) -> Vec<(KeyIdx, Fingerprint)> {
        self.record(id)
            .map(|(deriv, record)| {
                record
                    .reads
                    .iter()
                    .map(|slot| (*slot, self.seen(*slot, deriv)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The keys `id` last read.
    pub fn reads_of(&self, id: DerivationId) -> impl Iterator<Item = &HashedKey> + '_ {
        self.record(id)
            .into_iter()
            .flat_map(|(_, record)| record.reads.iter())
            .map(|key| self.key(*key))
    }

    /// Replaces `id`'s recorded fingerprints, in [`Self::reads_of`] order.
    /// Returns whether any differed.
    pub fn set_fingerprints(&mut self, id: DerivationId, fingerprints: Vec<Fingerprint>) -> bool {
        let Some(deriv) = self.deriv_slots.get(&id).copied() else {
            return false;
        };
        let Some(record) = self.derivations[deriv as usize].record.as_ref() else {
            return false;
        };
        let mut moved = false;
        for (key, fingerprint) in record.reads.iter().zip(fingerprints) {
            let entry = &mut self.table.keys[*key as usize];
            // A stage rule a reader saw is the stage it ran at, which only a
            // stage barrier re-runs it for.
            if entry.readers.seen_by(deriv) != fingerprint
                && !entry.key.as_ref().is_some_and(|key| key.is_stage_rule())
            {
                moved = true;
                entry.readers.insert(deriv, fingerprint);
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
        readers.groups().any(|(seen, _)| seen != current)
    }

    /// Relabels the reads of `key` that saw `from` as having seen `to`: after
    /// an edit that only moved the key's positions, what they read is the
    /// value as it stands now, and the next edit maps back from here.
    pub fn rebase_readers(&mut self, key: &HashedKey, from: Fingerprint, to: Fingerprint) {
        if let Some(slot) = self.table.find(key) {
            self.table.keys[slot as usize].readers.relabel(from, to);
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
                    .map(|key| rewrite.get(key).copied().unwrap_or(*key))
                    .collect::<Vec<_>>();
                reads.sort_unstable();
                reads.dedup();
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

    /// The derivations of the kinds `follows` accepts that read one of `keys`,
    /// or read, through what such derivations write, what one of them writes.
    pub fn readers_downstream<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a HashedKey>,
        follows: impl Fn(DerivationKind) -> bool,
    ) -> FxHashSet<DerivationId> {
        let mut marked = vec![false; self.derivations.len()];
        let mut pending = Vec::new();
        for slot in keys.into_iter().filter_map(|key| self.table.find(key)) {
            self.mark_readers(slot, &mut marked, &mut pending, &follows);
        }
        self.mark_downstream(&mut marked, pending, &follows);
        marked
            .iter()
            .enumerate()
            .filter(|(_, marked)| **marked)
            .map(|(deriv, _)| self.derivations[deriv].id)
            .collect()
    }

    /// Marks, from the marked derivations in `pending`, every derivation of
    /// the kinds `follows` accepts that reads what a marked one writes.
    fn mark_downstream(
        &self,
        marked: &mut [bool],
        mut pending: Vec<DerivIdx>,
        follows: &impl Fn(DerivationKind) -> bool,
    ) {
        while let Some(deriv) = pending.pop() {
            let Some(record) = self.derivations[deriv as usize].record.as_ref() else {
                continue;
            };
            for output in record.outputs.iter() {
                self.mark_readers(*output, marked, &mut pending, follows);
            }
        }
    }

    /// Marks the unmarked readers of `slot`, and of the keys read out of it,
    /// of the kinds `follows` accepts, and adds them to `pending`.
    fn mark_readers(
        &self,
        slot: KeyIdx,
        marked: &mut [bool],
        pending: &mut Vec<DerivIdx>,
        follows: &impl Fn(DerivationKind) -> bool,
    ) {
        let derived = self
            .table
            .derived
            .get(&slot)
            .map_or(&[][..], |derived| derived.slots.as_slice());
        for slot in std::iter::once(&slot).chain(derived) {
            for (_, readers) in self.table.keys[*slot as usize].readers.groups() {
                for reader in readers {
                    if !marked[*reader as usize]
                        && follows(self.derivations[*reader as usize].id.kind)
                    {
                        marked[*reader as usize] = true;
                        pending.push(*reader);
                    }
                }
            }
        }
    }

    /// The loops through `roots` among the derivations of the kinds `follows`
    /// accepts that `selects`, and that read, through what they read, from no
    /// other loop it selects: each set of two or more of them that read,
    /// through one another, what each writes, members sorted.
    pub fn read_loops(
        &self,
        roots: impl IntoIterator<Item = DerivationId>,
        follows: impl Fn(DerivationKind) -> bool,
        mut selects: impl FnMut(&[DerivationId]) -> bool,
    ) -> Vec<Vec<DerivationId>> {
        // Tarjan's algorithm along reads, from each root not yet visited:
        // `order` holds each visited derivation's index and lowest reachable
        // index.
        struct Visit {
            deriv: DerivIdx,
            producers: Vec<DerivIdx>,
            next: usize,
        }
        // Each key slot's base slot, found once per search: `NO_BASE` for a
        // key without one, `UNSEEN` until looked up.
        const UNSEEN: KeyIdx = KeyIdx::MAX;
        const NO_BASE: KeyIdx = KeyIdx::MAX - 1;
        let mut base_slots = vec![UNSEEN; self.table.keys.len()];
        const UNVISITED: u32 = u32::MAX;
        let count = self.derivations.len();
        let roots = roots
            .into_iter()
            .filter_map(|root| self.deriv_slots.get(&root).copied())
            .collect::<Vec<_>>();
        // A loop through a root, and whatever lies between two such loops,
        // is downstream of a root: the search keeps to what is.
        let mut downstream_of_roots = vec![false; count];
        let mut pending = Vec::new();
        for root in &roots {
            if !std::mem::replace(&mut downstream_of_roots[*root as usize], true) {
                pending.push(*root);
            }
        }
        self.mark_downstream(&mut downstream_of_roots, pending, &follows);
        let mut producers_read_by = |deriv: DerivIdx| {
            let mut producers = Vec::new();
            let Some(record) = self.derivations[deriv as usize].record.as_ref() else {
                return producers;
            };
            for slot in record.reads.iter() {
                let base_slot = &mut base_slots[*slot as usize];
                if *base_slot == UNSEEN {
                    *base_slot = self
                        .table
                        .key(*slot)
                        .base()
                        .and_then(|base| self.table.find(&HashedKey::new(base)))
                        .unwrap_or(NO_BASE);
                }
                let base = (*base_slot != NO_BASE).then_some(*base_slot);
                for slot in std::iter::once(*slot).chain(base) {
                    producers.extend(self.table.producers(slot).filter(|producer| {
                        *producer != deriv && downstream_of_roots[*producer as usize]
                    }));
                }
            }
            producers.sort_unstable();
            producers.dedup();
            producers
        };
        // Each visited derivation's index and lowest reachable index.
        let mut order = vec![(UNVISITED, UNVISITED); count];
        let mut next_index = 0;
        let mut stack = Vec::new();
        let mut on_stack = vec![false; count];
        let mut loops = Vec::new();
        // Each finished derivation's component, and whether a selected loop
        // is in or upstream of it. A component finishes after every one it
        // reads from, so theirs are known by then: a derivation that reads
        // from a finished component one is in or below is marked when it
        // does.
        let mut component_of = vec![usize::MAX; count];
        let mut downstream = Vec::<bool>::new();
        let mut reads_selected = vec![false; count];
        for root in roots {
            if order[root as usize].0 != UNVISITED {
                continue;
            }
            let mut visit = |deriv: DerivIdx,
                             order: &mut [(u32, u32)],
                             next_index: &mut u32,
                             stack: &mut Vec<DerivIdx>,
                             on_stack: &mut [bool]| {
                order[deriv as usize] = (*next_index, *next_index);
                *next_index += 1;
                stack.push(deriv);
                on_stack[deriv as usize] = true;
                Visit {
                    deriv,
                    producers: producers_read_by(deriv),
                    next: 0,
                }
            };
            let mut calls = vec![visit(
                root,
                &mut order,
                &mut next_index,
                &mut stack,
                &mut on_stack,
            )];
            while let Some(top) = calls.last_mut() {
                if let Some(producer) = top.producers.get(top.next).copied() {
                    top.next += 1;
                    let reader = top.deriv as usize;
                    let producer_index = producer as usize;
                    if order[producer_index].0 == UNVISITED {
                        let call = visit(
                            producer,
                            &mut order,
                            &mut next_index,
                            &mut stack,
                            &mut on_stack,
                        );
                        calls.push(call);
                    } else if on_stack[producer_index] {
                        order[reader].1 = order[reader].1.min(order[producer_index].0);
                    } else if downstream[component_of[producer_index]] {
                        reads_selected[reader] = true;
                    }
                    continue;
                }
                let finished = calls.pop().expect("a call");
                let (index, low) = order[finished.deriv as usize];
                if let Some(parent) = calls.last() {
                    let parent_low = &mut order[parent.deriv as usize].1;
                    *parent_low = (*parent_low).min(low);
                }
                if index != low {
                    continue;
                }
                let mut members = Vec::new();
                loop {
                    let member = stack.pop().expect("on the stack");
                    on_stack[member as usize] = false;
                    members.push(member);
                    if member == finished.deriv {
                        break;
                    }
                }
                let component_index = downstream.len();
                for member in &members {
                    component_of[*member as usize] = component_index;
                }
                let upstream_selected = members
                    .iter()
                    .any(|member| reads_selected[*member as usize]);
                let mut component = members
                    .iter()
                    .map(|member| self.derivations[*member as usize].id)
                    .collect::<Vec<_>>();
                component.sort_unstable();
                let selected = component.len() > 1 && selects(&component);
                downstream.push(upstream_selected || selected);
                if selected && !upstream_selected {
                    loops.push(component);
                }
                if (upstream_selected || selected)
                    && let Some(parent) = calls.last()
                {
                    reads_selected[parent.deriv as usize] = true;
                }
            }
        }
        loops.sort_unstable();
        loops
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
            .into_iter()
            .flat_map(|slot| self.table.producers(slot))
            .map(|deriv| self.derivations[deriv as usize].id)
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
                let reader = self.derivations[deriv as usize].id;
                if reader.kind != DerivationKind::Diagnostics
                    && reader.file_id != owner
                    && reader.file_id != FileId::VIRTUAL
                {
                    dependents.entry(owner).or_default().insert(reader.file_id);
                }
            }
        }
        dependents
    }

    fn record(&self, id: DerivationId) -> Option<(DerivIdx, &Record)> {
        let deriv = *self.deriv_slots.get(&id)?;
        Some((deriv, self.derivations[deriv as usize].record.as_ref()?))
    }

    /// The fingerprint `deriv`'s recorded read of `slot` saw.
    fn seen(&self, slot: KeyIdx, deriv: DerivIdx) -> Fingerprint {
        self.table.keys[slot as usize].readers.seen_by(deriv)
    }

    /// The slot `key` is interned to.
    pub(super) fn find_slot(&self, key: &HashedKey) -> Option<KeyIdx> {
        self.table.find(key)
    }

    /// The key table, for checking memoised values against it.
    pub(super) fn keys(&self) -> &KeyTable {
        &self.table
    }

    pub(super) fn key(&self, slot: KeyIdx) -> &HashedKey {
        self.table.key(slot)
    }

    /// What a group member that ended at stamp `ended` saw of each of
    /// `slots`, the key table's windows closed. A key a later member moved
    /// since, other than one `reset` before the group ran, is
    /// [`super::UNSEEN`]; any other key it saw as it stands, which recording
    /// takes. A key that came back as it was before its window reads as
    /// unwritten, so only a moved one is unseen.
    pub fn seen_by_member(
        &self,
        slots: Vec<KeyIdx>,
        ended: u64,
        reset: &FxHashSet<HashedKey>,
    ) -> Vec<(KeyIdx, Option<Fingerprint>)> {
        slots
            .into_iter()
            .map(|slot| {
                let moved = self.table.keys[slot as usize].written > ended
                    && !reset.contains(self.key(slot));
                (slot, moved.then_some(super::UNSEEN))
            })
            .collect()
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
            || entry.producer != NO_PRODUCER
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
        self.table.remove_producer(key, deriv);
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
        for key in record.reads.iter() {
            self.table.keys[*key as usize].readers.remove(deriv);
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
        assert!(table.log(slot, 1, 1, 1).is_some());
        assert_eq!(table.intern_read(key(9), 0), slot);
        assert!(table.log(slot, 2, 2, 2).is_some());
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

    fn groups(readers: &Readers) -> Vec<(Fingerprint, Vec<DerivIdx>)> {
        readers
            .groups()
            .map(|(seen, derivs)| (seen, derivs.to_vec()))
            .collect()
    }

    #[test]
    fn a_reader_that_sees_the_value_the_others_saw_rejoins_their_group() {
        let mut readers = Readers::default();
        readers.insert(1, 5);
        readers.insert(2, 5);
        readers.insert(3, 6);

        readers.insert(3, 5);

        assert_eq!(groups(&readers), vec![(5, vec![1, 2, 3])]);
    }

    #[test]
    fn relabelling_one_group_as_another_merges_them() {
        let mut readers = Readers::default();
        readers.insert(1, 5);
        readers.insert(2, 6);
        readers.insert(3, 6);

        readers.relabel(6, 5);

        assert_eq!(groups(&readers), vec![(5, vec![1, 2, 3])]);
    }

    #[test]
    fn removing_the_only_reader_of_a_value_leaves_the_others_group() {
        let mut readers = Readers::default();
        readers.insert(1, 5);
        readers.insert(2, 5);
        readers.insert(3, 6);

        readers.remove(3);

        assert_eq!(
            (groups(&readers), readers.seen_by(2)),
            (vec![(5, vec![1, 2])], 5)
        );
    }

    /// Deleting text moves a key after the deletion onto the place of one
    /// inside it: a derivation that read both reads one key, at one value.
    #[test]
    fn a_reader_of_two_keys_an_edit_merges_leaves_no_reader_behind() {
        use crate::{LuaDeclId, PositionMap};
        let decl = |position: u32| {
            HashedKey::new(FactKey::Decl(LuaDeclId::new(
                FileId::new(1),
                TextSize::new(position),
            )))
        };
        let mut graph = ReaderGraph::default();
        graph.replace(
            id(2, 0),
            vec![(decl(20), Some(5)), (decl(30), Some(6))],
            vec![],
            |_| 0,
        );
        let old_text = format!("{}{}{}", "a".repeat(20), "b".repeat(10), "c".repeat(10));
        let new_text = format!("{}{}", "a".repeat(20), "c".repeat(10));
        graph.apply_edits(&EditMaps::new([(
            FileId::new(1),
            PositionMap::new(&old_text, &new_text),
        )]));

        graph.remove_file(FileId::new(2));

        assert_eq!(graph.stale_readers(&decl(20), |_| false).count(), 0);
    }

    #[test]
    fn a_key_keeps_its_other_producers_when_its_first_stops() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![], vec![key(9)], |_| 0);
        graph.replace(id(2, 0), vec![], vec![key(9)], |_| 0);
        graph.replace(id(3, 0), vec![], vec![key(9)], |_| 0);

        graph.replace(id(1, 0), vec![], vec![], |_| 0);

        let mut producers = graph.producers(&key(9)).collect::<Vec<_>>();
        producers.sort();
        assert_eq!(producers, vec![id(2, 0), id(3, 0)]);
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
