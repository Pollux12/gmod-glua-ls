use rustc_hash::{FxHashMap, FxHashSet};

use super::{DerivationId, EditMaps, FactKey, Fingerprint};
use crate::FileId;

/// What one derivation's last evaluation read and wrote.
#[derive(Debug, Clone, Default)]
struct Record {
    reads: Box<[FactKey]>,
    outputs: Box<[FactKey]>,
}

/// The reader graph: for every derivation the facts its last evaluation read
/// (with the fingerprint it saw) and the facts it produced, plus the reverse
/// edges from a fact to its readers and producers.
#[derive(Debug, Clone, Default)]
pub struct ReaderGraph {
    records: FxHashMap<DerivationId, Record>,
    readers: FxHashMap<FactKey, FxHashMap<DerivationId, Fingerprint>>,
    producers: FxHashMap<FactKey, FxHashSet<DerivationId>>,
    by_file: FxHashMap<FileId, FxHashSet<DerivationId>>,
    /// Derivations that produced a key owned by another file, by that file.
    /// Removing the file's facts removes their outputs too, so they must run
    /// again when it is re-analysed.
    foreign_producers: FxHashMap<FileId, FxHashSet<DerivationId>>,
}

impl ReaderGraph {
    /// Replaces `id`'s record with the reads and outputs of its latest
    /// evaluation. Returns the outputs the previous evaluation produced and
    /// this one did not.
    pub fn replace(
        &mut self,
        id: DerivationId,
        reads: Vec<(FactKey, Fingerprint)>,
        outputs: Vec<FactKey>,
    ) -> Vec<FactKey> {
        let previous = self.detach(id);
        let kept: FxHashSet<&FactKey> = outputs.iter().collect();
        let dropped = previous
            .outputs
            .into_vec()
            .into_iter()
            .filter(|key| !kept.contains(key))
            .collect();
        for (key, fingerprint) in &reads {
            self.readers
                .entry(key.clone())
                .or_default()
                .insert(id, *fingerprint);
        }
        for key in &outputs {
            self.producers.entry(key.clone()).or_default().insert(id);
            if let Some(owner) = key.owning_file()
                && owner != id.file_id
            {
                self.foreign_producers.entry(owner).or_default().insert(id);
            }
        }
        self.by_file.entry(id.file_id).or_default().insert(id);
        self.records.insert(
            id,
            Record {
                reads: reads.into_iter().map(|(key, _)| key).collect(),
                outputs: outputs.into_boxed_slice(),
            },
        );
        dropped
    }

    /// Forgets every derivation of `file_id`, returning each with the facts
    /// it produced.
    pub fn remove_file(&mut self, file_id: FileId) -> Vec<(DerivationId, Box<[FactKey]>)> {
        let Some(ids) = self.by_file.remove(&file_id) else {
            return Vec::new();
        };
        let mut removed: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let outputs = self.detach(id).outputs;
                (id, outputs)
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
            .copied()
            .collect();
        ids.sort();
        ids
    }

    /// The derivations whose recorded read of `key` no longer holds:
    /// `holds(seen)` says whether the value a reader saw is still current.
    pub fn stale_readers<'a>(
        &'a self,
        key: &FactKey,
        holds: impl Fn(Fingerprint) -> bool + 'a,
    ) -> impl Iterator<Item = DerivationId> + 'a {
        self.readers
            .get(key)
            .into_iter()
            .flatten()
            .filter(move |(_, seen)| !holds(**seen))
            .map(|(id, _)| *id)
    }

    /// Whether any read `id` last made no longer holds.
    pub fn is_stale(
        &self,
        id: DerivationId,
        holds: impl Fn(&FactKey, Fingerprint) -> bool,
    ) -> bool {
        let Some(record) = self.records.get(&id) else {
            return true;
        };
        record.reads.iter().any(|key| {
            self.readers
                .get(key)
                .and_then(|readers| readers.get(&id))
                .is_none_or(|seen| !holds(key, *seen))
        })
    }

    /// The keys `id` last read.
    pub fn reads_of(&self, id: DerivationId) -> &[FactKey] {
        self.records.get(&id).map_or(&[], |record| &record.reads)
    }

    /// Replaces the fingerprints `id` recorded for its reads, in
    /// [`Self::reads_of`] order.
    /// Replaces `id`'s recorded fingerprints, in read order. Returns whether
    /// any differed.
    pub fn set_fingerprints(&mut self, id: DerivationId, fingerprints: Vec<Fingerprint>) -> bool {
        let Some(record) = self.records.get(&id) else {
            return false;
        };
        let mut moved = false;
        for (key, fingerprint) in record.reads.iter().zip(fingerprints) {
            if let Some(seen) = self
                .readers
                .get_mut(key)
                .and_then(|readers| readers.get_mut(&id))
            {
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
        let mut renames = FxHashMap::default();
        let mut lost = Vec::new();
        for key in self.readers.keys() {
            if !key
                .positional_file()
                .is_some_and(|file_id| files.contains(&file_id))
            {
                continue;
            }
            match edits.map_key(key) {
                Some(moved) if moved == *key => {}
                Some(moved) => {
                    renames.insert(key.clone(), moved);
                }
                None => lost.extend(self.readers[key].keys().copied()),
            }
        }
        let moved = renames
            .keys()
            .filter_map(|key| Some((key.clone(), self.readers.remove(key)?)))
            .collect::<Vec<_>>();
        let mut affected = FxHashSet::default();
        for (key, readers) in moved {
            let new_key = renames[&key].clone();
            for (id, seen) in readers {
                affected.insert(id);
                self.readers
                    .entry(new_key.clone())
                    .or_default()
                    .insert(id, seen);
            }
        }
        for id in affected {
            if let Some(record) = self.records.get_mut(&id) {
                for read in record.reads.iter_mut() {
                    if let Some(moved) = renames.get(read) {
                        *read = moved.clone();
                    }
                }
            }
        }
        lost.sort();
        lost.dedup();
        lost
    }

    /// Every key some derivation read.
    #[cfg(test)]
    pub fn read_keys(&self) -> impl Iterator<Item = &FactKey> {
        self.readers.keys()
    }

    /// Every derivation with a record.
    #[cfg(test)]
    pub fn derivations(&self) -> impl Iterator<Item = DerivationId> + '_ {
        self.records.keys().copied()
    }

    pub fn contains(&self, id: DerivationId) -> bool {
        self.records.contains_key(&id)
    }

    pub fn outputs_of(&self, id: DerivationId) -> &[FactKey] {
        self.records.get(&id).map_or(&[], |record| &record.outputs)
    }

    pub fn producers(&self, key: &FactKey) -> impl Iterator<Item = DerivationId> + '_ {
        self.producers.get(key).into_iter().flatten().copied()
    }

    pub fn derivations_of_file(&self, file_id: FileId) -> impl Iterator<Item = DerivationId> + '_ {
        self.by_file.get(&file_id).into_iter().flatten().copied()
    }

    /// For every file, the other files whose derivations read a fact it
    /// owns.
    pub fn dependent_files(&self) -> FxHashMap<FileId, FxHashSet<FileId>> {
        let mut dependents: FxHashMap<FileId, FxHashSet<FileId>> = FxHashMap::default();
        for (key, readers) in &self.readers {
            let Some(owner) = key.owning_file() else {
                continue;
            };
            for reader in readers.keys() {
                if reader.file_id != owner && reader.file_id != FileId::VIRTUAL {
                    dependents.entry(owner).or_default().insert(reader.file_id);
                }
            }
        }
        dependents
    }

    fn detach(&mut self, id: DerivationId) -> Record {
        let Some(record) = self.records.remove(&id) else {
            return Record::default();
        };
        for key in record.reads.iter() {
            if let Some(readers) = self.readers.get_mut(key) {
                readers.remove(&id);
                if readers.is_empty() {
                    self.readers.remove(key);
                }
            }
        }
        for key in record.outputs.iter() {
            if let Some(producers) = self.producers.get_mut(key) {
                producers.remove(&id);
                if producers.is_empty() {
                    self.producers.remove(key);
                }
            }
            if let Some(owner) = key.owning_file()
                && owner != id.file_id
                && let Some(ids) = self.foreign_producers.get_mut(&owner)
            {
                ids.remove(&id);
                if ids.is_empty() {
                    self.foreign_producers.remove(&owner);
                }
            }
        }
        if let Some(ids) = self.by_file.get_mut(&id.file_id) {
            ids.remove(&id);
            if ids.is_empty() {
                self.by_file.remove(&id.file_id);
            }
        }
        record
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

    fn key(file: u32) -> FactKey {
        FactKey::DeclTree(FileId::new(file))
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
}
