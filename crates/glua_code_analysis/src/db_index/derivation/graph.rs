use rustc_hash::{FxHashMap, FxHashSet};

use super::{DerivationId, EditMaps, FactKey, Fingerprint, HashedKey};
use crate::FileId;

/// A fact key's slot in [`ReaderGraph::keys`].
type KeyIdx = u32;
/// A derivation's slot in [`ReaderGraph::derivations`].
type DerivIdx = u32;

/// What one derivation's last evaluation read and wrote, as sorted key slots.
#[derive(Debug, Clone, Default)]
struct Record {
    reads: Box<[KeyIdx]>,
    outputs: Box<[KeyIdx]>,
}

/// A fact key and the derivations that read it (with the fingerprint each
/// saw) or produced it. `key` is `None` while the slot is free.
#[derive(Debug, Clone, Default)]
struct KeySlot {
    key: Option<HashedKey>,
    readers: FxHashMap<DerivIdx, Fingerprint>,
    producers: Vec<DerivIdx>,
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
#[derive(Debug, Clone, Default)]
pub struct ReaderGraph {
    key_slots: FxHashMap<HashedKey, KeyIdx>,
    keys: Vec<KeySlot>,
    free_keys: Vec<KeyIdx>,
    deriv_slots: FxHashMap<DerivationId, DerivIdx>,
    derivations: Vec<DerivSlot>,
    free_derivations: Vec<DerivIdx>,
    by_file: FxHashMap<FileId, FxHashSet<DerivIdx>>,
    /// Derivations that produced a key owned by another file, by that file.
    /// Removing the file's facts removes their outputs too, so they must run
    /// again when it is re-analysed.
    foreign_producers: FxHashMap<FileId, FxHashSet<DerivIdx>>,
}

impl ReaderGraph {
    /// Replaces `id`'s record with the reads and outputs of its latest
    /// evaluation. Returns the outputs the previous evaluation produced and
    /// this one did not.
    pub fn replace(
        &mut self,
        id: DerivationId,
        reads: Vec<(HashedKey, Fingerprint)>,
        outputs: Vec<HashedKey>,
    ) -> Vec<HashedKey> {
        let deriv = self.intern_derivation(id);
        let mut reads = reads
            .into_iter()
            .map(|(key, fingerprint)| (self.intern_key(key), fingerprint))
            .collect::<Vec<_>>();
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
        merge_sorted(
            &old.reads,
            reads.iter().map(|(key, _)| *key),
            |key| unread.push(key),
            |_| {},
        );
        for (key, fingerprint) in &reads {
            self.keys[*key as usize].readers.insert(deriv, *fingerprint);
        }
        for key in &unread {
            self.keys[*key as usize].readers.remove(&deriv);
        }

        let mut dropped = Vec::new();
        let mut added = Vec::new();
        merge_sorted(
            &old.outputs,
            outputs.iter().copied(),
            |key| dropped.push(key),
            |key| added.push(key),
        );
        for key in added {
            self.keys[key as usize].producers.push(deriv);
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
        for key in unread {
            self.release_key(key);
        }

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
        self.key_slots
            .get(key)
            .map(|slot| &self.keys[*slot as usize].readers)
            .into_iter()
            .flatten()
            .filter(move |(_, seen)| !holds(**seen))
            .map(|(deriv, _)| self.derivations[*deriv as usize].id)
    }

    /// Whether any read `id` last made no longer holds.
    pub fn is_stale(
        &self,
        id: DerivationId,
        holds: impl Fn(&HashedKey, Fingerprint) -> bool,
    ) -> bool {
        let Some((deriv, record)) = self.record(id) else {
            return true;
        };
        record.reads.iter().any(|key| {
            self.keys[*key as usize]
                .readers
                .get(&deriv)
                .is_none_or(|seen| !holds(self.key(*key), *seen))
        })
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
            if let Some(seen) = self.keys[*key as usize].readers.get_mut(&deriv) {
                moved |= *seen != fingerprint;
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
        for (key, slot) in &self.key_slots {
            let readers = &self.keys[*slot as usize].readers;
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
                        .keys()
                        .map(|deriv| self.derivations[*deriv as usize].id),
                ),
            }
        }
        // Every moved key gives up its readers before any takes new ones:
        // positions shift in chains, where one key's new place is another's
        // old one.
        let moved = renames
            .into_iter()
            .map(|(slot, new_key)| {
                let readers = std::mem::take(&mut self.keys[slot as usize].readers);
                (slot, new_key, readers)
            })
            .collect::<Vec<_>>();
        let mut rewrites: FxHashMap<DerivIdx, FxHashMap<KeyIdx, KeyIdx>> = FxHashMap::default();
        let mut vacated = Vec::with_capacity(moved.len());
        for (slot, new_key, readers) in moved {
            let target = self.intern_key(new_key);
            for (deriv, seen) in readers {
                self.keys[target as usize].readers.insert(deriv, seen);
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
        self.keys
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
        self.key_slots
            .get(key)
            .map(|slot| &self.keys[*slot as usize].producers)
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
        for slot in &self.keys {
            let Some(owner) = slot.key.as_deref().and_then(FactKey::owning_file) else {
                continue;
            };
            for deriv in slot.readers.keys() {
                let reader = self.derivations[*deriv as usize].id.file_id;
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
        self.keys[slot as usize]
            .key
            .as_ref()
            .expect("a referenced key slot is live")
    }

    fn intern_key(&mut self, key: HashedKey) -> KeyIdx {
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
        self.keys[slot as usize].key = Some(key.clone());
        self.key_slots.insert(key, slot);
        slot
    }

    /// Frees `slot` once no derivation reads or produces its key.
    fn release_key(&mut self, slot: KeyIdx) {
        let entry = &mut self.keys[slot as usize];
        if !entry.readers.is_empty() || !entry.producers.is_empty() {
            return;
        }
        if let Some(key) = entry.key.take() {
            self.key_slots.remove(&key);
            self.free_keys.push(slot);
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
        let producers = &mut self.keys[key as usize].producers;
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
        for key in record.reads.iter() {
            self.keys[*key as usize].readers.remove(&deriv);
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
    old: &[KeyIdx],
    new: impl Iterator<Item = KeyIdx>,
    mut only_old: impl FnMut(KeyIdx),
    mut only_new: impl FnMut(KeyIdx),
) {
    let mut old = old.iter().copied().peekable();
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
        graph.replace(id(1, 0), vec![(key(9), 5)], vec![key(1)]);
        graph.replace(id(2, 0), vec![(key(9), 6)], vec![key(2)]);

        let stale: Vec<_> = graph.stale_readers(&key(9), |seen| seen == 6).collect();

        assert_eq!(stale, vec![id(1, 0)]);
    }

    #[test]
    fn replace_drops_old_edges_and_reports_outputs_no_longer_produced() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), 5)], vec![key(1), key(2)]);

        let dropped = graph.replace(id(1, 0), vec![(key(8), 1)], vec![key(2)]);

        assert_eq!(dropped, vec![key(1)]);
        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 0).count(), 0);
        assert_eq!(graph.producers(&key(1)).count(), 0);
        assert_eq!(graph.producers(&key(2)).collect::<Vec<_>>(), vec![id(1, 0)]);
    }

    #[test]
    fn a_read_kept_across_evaluations_holds_the_latest_fingerprint() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), 5), (key(8), 1)], vec![]);

        graph.replace(id(1, 0), vec![(key(9), 7)], vec![]);

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
        graph.replace(id(1, 0), vec![(key(9), 5)], vec![key(1)]);
        graph.replace(id(1, 0), vec![], vec![]);

        assert_eq!(graph.read_keys().count(), 0);
        assert_eq!(graph.producers(&key(1)).count(), 0);

        graph.replace(id(1, 0), vec![(key(7), 1)], vec![key(6)]);

        assert_eq!(graph.keys.len(), 2);
    }

    #[test]
    fn remove_file_returns_outputs_and_forgets_its_readers() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), 5)], vec![key(1)]);
        graph.replace(id(2, 0), vec![(key(1), 5)], vec![key(2)]);

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
        graph.replace(id(2, 0), vec![], vec![key(1), key(2)]);

        assert_eq!(graph.foreign_producers(FileId::new(1)), vec![id(2, 0)]);
        assert!(graph.foreign_producers(FileId::new(2)).is_empty());

        graph.replace(id(2, 0), vec![], vec![key(2)]);

        assert!(graph.foreign_producers(FileId::new(1)).is_empty());
    }

    #[test]
    fn a_key_read_before_and_produced_now_stays_live() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), 5)], vec![]);

        graph.replace(id(1, 0), vec![], vec![key(9)]);

        assert_eq!(graph.producers(&key(9)).collect::<Vec<_>>(), vec![id(1, 0)]);
        assert_eq!(
            graph.outputs_of(id(1, 0)).collect::<Vec<_>>(),
            vec![&key(9)]
        );
    }
}
