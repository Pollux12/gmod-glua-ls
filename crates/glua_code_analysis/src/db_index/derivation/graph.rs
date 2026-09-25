use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

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
    /// The evaluation that last recorded a read of the key; see
    /// [`KeyTable::note_read`].
    stamp: u32,
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
        }
    }
}

const NO_GENERATION: u32 = u32::MAX;

/// A key's readers, grouped by the fingerprint each saw. Nearly every key has
/// one group, so checking its readers against a new value is one comparison
/// per group rather than one per reader.
#[derive(Debug, Clone, Default)]
struct Readers(Vec<(Fingerprint, FxHashSet<DerivIdx>)>);

impl Readers {
    fn insert(&mut self, deriv: DerivIdx, seen: Fingerprint) {
        match self.0.iter_mut().find(|(group, _)| *group == seen) {
            Some((_, derivs)) => {
                derivs.insert(deriv);
            }
            None => self.0.push((seen, FxHashSet::from_iter([deriv]))),
        }
    }

    fn remove(&mut self, deriv: DerivIdx, seen: Fingerprint) {
        if let Some(at) = self.0.iter().position(|(group, _)| *group == seen) {
            self.0[at].1.remove(&deriv);
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
    key_slots: FxHashMap<HashedKey, KeyIdx>,
    keys: Vec<KeySlot>,
    free_keys: Vec<KeyIdx>,
}

impl KeyTable {
    fn intern(&mut self, key: HashedKey) -> KeyIdx {
        if let Some(slot) = self.key_slots.get(&key) {
            return *slot;
        }
        let slot = match self.free_keys.pop() {
            Some(slot) => slot,
            None => {
                self.keys.push(KeySlot::default());
                KeyIdx::try_from(self.keys.len() - 1).expect("fewer than 2^32 fact keys")
            }
        };
        let entry = &mut self.keys[slot as usize];
        entry.key = Some(key.clone());
        entry.stamp = 0;
        entry
            .current_generation
            .store(NO_GENERATION, Ordering::Relaxed);
        self.key_slots.insert(key, slot);
        slot
    }

    /// Whether evaluation `stamp` recorded a read of `key`.
    pub(super) fn stamped(&self, key: &HashedKey, stamp: u32) -> bool {
        self.key_slots
            .get(key)
            .is_some_and(|slot| self.keys[*slot as usize].stamp == stamp)
    }

    /// Interns `key` and returns its slot if evaluation `stamp` has not
    /// recorded it yet.
    pub(super) fn note_read(&mut self, key: HashedKey, stamp: u32) -> Option<KeyIdx> {
        let slot = self.intern(key);
        let entry = &mut self.keys[slot as usize];
        (entry.stamp != stamp).then(|| {
            entry.stamp = stamp;
            slot
        })
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
        match self.table.key_slots.get(key) {
            Some(slot) => self.slot_fingerprint(*slot, fingerprint),
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

    /// Forgets `key`'s cached fingerprint: its value may have moved.
    pub fn forget_fingerprint(&self, key: &HashedKey) {
        if let Some(slot) = self.table.key_slots.get(key) {
            self.table.keys[*slot as usize]
                .current_generation
                .store(NO_GENERATION, Ordering::Relaxed);
        }
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
                self.drop_producer(key, deriv, id.file_id);
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
            .key_slots
            .get(key)
            .map(|slot| &self.table.keys[*slot as usize].readers.0)
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
            if *seen != fingerprint {
                moved = true;
                let readers = &mut self.table.keys[*key as usize].readers;
                readers.remove(deriv, *seen);
                readers.insert(deriv, fingerprint);
                *seen = fingerprint;
            }
        }
        moved
    }

    /// Renames every recorded read of a key `edits` moved to the key's new
    /// position, keeping the fingerprint the reader saw. Returns the readers
    /// of keys the edits destroyed.
    pub fn apply_edits(&mut self, edits: &EditMaps) -> Vec<DerivationId> {
        let files: FxHashSet<FileId> = edits.files().collect();
        let mut renames = Vec::new();
        let mut lost = Vec::new();
        for (key, slot) in &self.table.key_slots {
            let readers = &self.table.keys[*slot as usize].readers;
            if readers.is_empty()
                || !key
                    .positional_file()
                    .is_some_and(|file_id| files.contains(&file_id))
            {
                continue;
            }
            match edits.map_key(key) {
                Some(moved) if moved == **key => {}
                Some(moved) => renames.push((*slot, HashedKey::new(moved))),
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
            .key_slots
            .get(key)
            .map(|slot| &self.table.keys[*slot as usize].producers)
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
        for slot in &self.table.keys {
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
        self.table.intern(key)
    }

    /// Interns a read made while no key table was lent.
    pub fn intern_read(&mut self, key: HashedKey) -> KeyIdx {
        self.table.intern(key)
    }

    /// Frees `slot` once no derivation reads or produces its key.
    fn release_key(&mut self, slot: KeyIdx) {
        let entry = &mut self.table.keys[slot as usize];
        if !entry.readers.is_empty() || !entry.producers.is_empty() {
            return;
        }
        if let Some(key) = entry.key.take() {
            self.table.key_slots.remove(&key);
            self.table.free_keys.push(slot);
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
        self.release_key(key);
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
        let slot = table.note_read(key(9), 1).expect("first read");
        assert_eq!(table.note_read(key(9), 2), Some(slot));
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
