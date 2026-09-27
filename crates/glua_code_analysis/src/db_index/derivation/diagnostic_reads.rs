use hashbrown::HashTable;
use rustc_hash::{FxHashMap, FxHashSet};

use super::paged::Paged;
use super::{EditMaps, FactKey, Fingerprint, HashedKey};
use crate::FileId;

/// An interned read key: its index into [`DiagnosticReads::readers`].
type ReadKey = u32;

/// What each file's last diagnostics read, so an analysis run knows which
/// files to diagnose again: those whose recorded reads moved.
///
/// Each distinct key is stored once; a file's reads are its keys' indices
/// with the fingerprints seen, sorted by index. A key nothing reads any more
/// is released and its slot reused.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticReads {
    /// Each read key, found by its hash: the key itself is kept only in
    /// `key_values`.
    keys: HashTable<ReadKey>,
    key_values: Paged<HashedKey>,
    /// The files that read each key, sorted.
    readers: Paged<Vec<FileId>>,
    files: FxHashMap<FileId, Box<[(ReadKey, Fingerprint)]>>,
    /// The keys read out of each base key ([`FactKey::base`]).
    derived: FxHashMap<ReadKey, Vec<ReadKey>>,
    /// The keys naming positions in each file, which an edit of it renames.
    positional: FxHashMap<FileId, FxHashSet<ReadKey>>,
    /// Released slots.
    free: Vec<ReadKey>,
}

impl DiagnosticReads {
    /// Counts and estimated heap sizes, for the profile. Excludes what the
    /// keys themselves point to.
    pub fn census(&self) -> String {
        use crate::profile::{hash_table_bytes, megabytes};
        use std::mem::size_of;
        let read_entry = size_of::<(ReadKey, Fingerprint)>();
        let reads: usize = self.files.values().map(|reads| reads.len()).sum();
        let file_bytes = hash_table_bytes(
            self.files.capacity(),
            size_of::<FileId>() + size_of::<Box<[(ReadKey, Fingerprint)]>>(),
        ) + reads * read_entry;
        let key_bytes = hash_table_bytes(self.keys.capacity(), size_of::<ReadKey>())
            + self.key_values.capacity() * size_of::<HashedKey>()
            + self
                .positional
                .values()
                .map(|keys| hash_table_bytes(keys.capacity(), size_of::<ReadKey>()))
                .sum::<usize>()
            + self.free.capacity() * size_of::<ReadKey>();
        let reader_edges: usize = self.readers.iter().map(Vec::len).sum();
        let reader_bytes = self.readers.capacity() * size_of::<Vec<FileId>>()
            + self
                .readers
                .iter()
                .map(|files| files.capacity() * size_of::<FileId>())
                .sum::<usize>();
        format!(
            "files {}, reads {reads}, distinct keys {}, reader edges {reader_edges}; MB: per-file reads {}, keys {}, readers {}",
            self.files.len(),
            self.key_values.len() - self.free.len(),
            megabytes(file_bytes),
            megabytes(key_bytes),
            megabytes(reader_bytes)
        )
    }

    /// Replaces `file_id`'s reads with those of its latest diagnostics.
    pub fn record(&mut self, file_id: FileId, reads: Vec<(FactKey, Fingerprint)>) {
        let reads = self.intern_all(reads);
        for (key, _) in reads.iter() {
            let readers = &mut self.readers[*key as usize];
            if let Err(at) = readers.binary_search(&file_id) {
                readers.insert(at, file_id);
            }
        }
        let Some(previous) = self.files.insert(file_id, reads) else {
            return;
        };
        for (key, _) in previous.iter() {
            if seen_in(&self.files[&file_id], *key).is_none() {
                self.drop_reader(*key, file_id);
            }
        }
    }

    /// Every key some file's diagnostics read.
    #[cfg(test)]
    pub fn read_keys(&self) -> impl Iterator<Item = &FactKey> {
        self.key_values
            .iter()
            .zip(self.readers.iter())
            .filter_map(|(key, readers)| (!readers.is_empty()).then_some(key.key()))
    }

    pub fn forget(&mut self, file_id: FileId) {
        let Some(reads) = self.files.remove(&file_id) else {
            return;
        };
        for (key, _) in reads.iter() {
            self.drop_reader(*key, file_id);
        }
    }

    /// Renames the recorded reads of keys `edits` moved, keeping the
    /// fingerprints; a read of a key the edits destroyed can no longer hold.
    pub fn apply_edits(&mut self, edits: &EditMaps) {
        let mut named = edits
            .files()
            .filter_map(|file_id| self.positional.get(&file_id))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        named.sort_unstable();
        // Every moved key is taken out before any is filed again, so a key
        // moving onto the position another one left is not taken for it.
        let mut moved = Vec::new();
        for index in named {
            let key = self.key_values[index as usize].key();
            let renamed = edits
                .map_key(key)
                .unwrap_or(FactKey::FileText(FileId::VIRTUAL));
            if renamed != *key {
                self.unfile(index);
                moved.push((index, renamed));
            }
        }
        for (index, renamed) in moved {
            match self.find(&renamed) {
                Some(existing) => self.merge(index, existing),
                None => self.refile(index, renamed),
            }
        }
    }

    /// Removes `file_id` from `key`'s readers, releasing the key once nothing
    /// reads it or out of it.
    fn drop_reader(&mut self, key: ReadKey, file_id: FileId) {
        let readers = &mut self.readers[key as usize];
        if let Ok(at) = readers.binary_search(&file_id) {
            readers.remove(at);
        }
        self.release_if_unread(key);
    }

    fn release_if_unread(&mut self, key: ReadKey) {
        if !self.readers[key as usize].is_empty()
            || self
                .derived
                .get(&key)
                .is_some_and(|derived| !derived.is_empty())
        {
            return;
        }
        self.derived.remove(&key);
        let base = self.key_values[key as usize].key().base();
        self.unfile(key);
        self.free.push(key);
        if let Some(base) = base.and_then(|base| self.find(&base)) {
            self.leave_base(key, base);
        }
    }

    /// Removes `key` from its base's derived keys, releasing the base once
    /// nothing reads it or out of it.
    fn leave_base(&mut self, key: ReadKey, base: ReadKey) {
        if let Some(derived) = self.derived.get_mut(&base) {
            derived.retain(|derived| *derived != key);
        }
        self.release_if_unread(base);
    }

    /// Takes `index`'s key out of the lookups; its readers stay.
    fn unfile(&mut self, index: ReadKey) {
        let key = &self.key_values[index as usize];
        if let Ok(entry) = self.keys.find_entry(key.hash, |filed| *filed == index) {
            entry.remove();
        }
        if let Some(file_id) = key.key().positional_file()
            && let Some(keys) = self.positional.get_mut(&file_id)
        {
            keys.remove(&index);
            if keys.is_empty() {
                self.positional.remove(&file_id);
            }
        }
    }

    /// Files `index`'s readers under `key`.
    fn refile(&mut self, index: ReadKey, key: FactKey) {
        let previous_base = self.key_values[index as usize].key().base();
        let base = key.base();
        self.key_values[index as usize] = HashedKey::new(key);
        self.file(index);
        if previous_base != base {
            if let Some(previous) = previous_base.and_then(|base| self.find(&base)) {
                self.leave_base(index, previous);
            }
            if let Some(base) = base {
                let base = self.intern(base);
                self.derived.entry(base).or_default().push(index);
            }
        }
    }

    /// Moves `from`'s readers to `into`, whose key `from`'s now names, and
    /// releases `from`. A file that read both keeps what it saw of `into`.
    fn merge(&mut self, from: ReadKey, into: ReadKey) {
        for file_id in std::mem::take(&mut self.readers[from as usize]) {
            if let Some(reads) = self.files.get_mut(&file_id) {
                let mut merged = reads.to_vec();
                let at = merged
                    .binary_search_by_key(&from, |(read, _)| *read)
                    .expect("a reader reads its key");
                let (_, seen) = merged.remove(at);
                if let Err(at) = merged.binary_search_by_key(&into, |(read, _)| *read) {
                    merged.insert(at, (into, seen));
                }
                *reads = merged.into_boxed_slice();
            }
            let readers = &mut self.readers[into as usize];
            if let Err(at) = readers.binary_search(&file_id) {
                readers.insert(at, file_id);
            }
        }
        if let Some(derived) = self.derived.remove(&from) {
            self.derived.entry(into).or_default().extend(derived);
        }
        let base = self.key_values[from as usize].key().base();
        self.free.push(from);
        if let Some(base) = base.and_then(|base| self.find(&base)) {
            self.leave_base(from, base);
        }
    }

    /// Files the key in `index` under its hash and the file it names.
    fn file(&mut self, index: ReadKey) {
        let key_values = &self.key_values;
        let key = &key_values[index as usize];
        self.keys
            .insert_unique(key.hash, index, |filed| key_values[*filed as usize].hash);
        if let Some(file_id) = key.key().positional_file() {
            self.positional.entry(file_id).or_default().insert(index);
        }
    }

    /// The files whose recorded read of one of `touched` no longer holds: it
    /// saw neither the key's `current` fingerprint nor, when edits moved the
    /// key, its fingerprint `before_edits`. A read that held only through the
    /// edits is recorded again at the current fingerprint, so the next edit of
    /// the same file compares against it.
    pub fn stale_files<'a>(
        &mut self,
        touched: impl IntoIterator<Item = &'a FactKey>,
        current: impl Fn(&FactKey) -> Fingerprint,
        before_edits: impl Fn(&FactKey) -> Option<Fingerprint>,
    ) -> Vec<FileId> {
        let mut stale = FxHashSet::default();
        let mut moved = Vec::new();
        for key in touched {
            let Some(index) = self.find(key) else {
                continue;
            };
            self.check_reads(index, key, &current, &before_edits, &mut stale, &mut moved);
            for derived in self.derived.get(&index).into_iter().flatten() {
                self.check_reads(
                    *derived,
                    self.key_values[*derived as usize].key(),
                    &current,
                    &before_edits,
                    &mut stale,
                    &mut moved,
                );
            }
        }
        for (file_id, index, now) in moved {
            if let Some(reads) = self.files.get_mut(&file_id)
                && let Ok(at) = reads.binary_search_by_key(&index, |(read, _)| *read)
            {
                reads[at].1 = now;
            }
        }
        let mut stale = stale.into_iter().collect::<Vec<_>>();
        stale.sort_unstable();
        stale
    }

    /// Adds to `stale` the files whose read of `key`, interned as `index`, no
    /// longer holds, and to `moved` the reads that held only through the edits.
    fn check_reads(
        &self,
        index: ReadKey,
        key: &FactKey,
        current: &dyn Fn(&FactKey) -> Fingerprint,
        before_edits: &dyn Fn(&FactKey) -> Option<Fingerprint>,
        stale: &mut FxHashSet<FileId>,
        moved: &mut Vec<(FileId, ReadKey, Fingerprint)>,
    ) {
        let readers = &self.readers[index as usize];
        if readers.is_empty() {
            return;
        }
        let now = current(key);
        let before = std::cell::OnceCell::new();
        let held_moved =
            |seen: Fingerprint| *before.get_or_init(|| before_edits(key)) == Some(seen);
        for file_id in readers {
            match seen_in(&self.files[file_id], index) {
                Some(seen) if seen == now => {}
                Some(seen) if held_moved(seen) => moved.push((*file_id, index, now)),
                _ => {
                    if stale.insert(*file_id) && crate::profile::tracing_evaluations() {
                        eprintln!("[trace] diagnostics of {file_id:?} stale by {key:?}");
                    }
                }
            }
        }
    }

    /// The index `key` is interned to.
    fn find(&self, key: &FactKey) -> Option<ReadKey> {
        use std::hash::BuildHasher;
        let hash = rustc_hash::FxBuildHasher.hash_one(key);
        self.keys
            .find(hash, |index| self.key_values[*index as usize].key() == key)
            .copied()
    }

    fn intern(&mut self, key: FactKey) -> ReadKey {
        if let Some(index) = self.find(&key) {
            return index;
        }
        let base = key.base();
        let key = HashedKey::new(key);
        let index = match self.free.pop() {
            Some(index) => {
                self.key_values[index as usize] = key;
                index
            }
            None => {
                self.readers.push(Vec::new());
                ReadKey::try_from(self.key_values.push(key)).expect("fewer than 2^32 read keys")
            }
        };
        self.file(index);
        if let Some(base) = base {
            let base = self.intern(base);
            self.derived.entry(base).or_default().push(index);
        }
        index
    }

    /// `reads` as interned keys, sorted and unique.
    fn intern_all(&mut self, reads: Vec<(FactKey, Fingerprint)>) -> Box<[(ReadKey, Fingerprint)]> {
        let mut reads = reads
            .into_iter()
            .map(|(key, seen)| (self.intern(key), seen))
            .collect::<Vec<_>>();
        reads.sort_unstable_by_key(|(key, _)| *key);
        reads.dedup_by_key(|(key, _)| *key);
        reads.into_boxed_slice()
    }
}

/// The fingerprint `reads` saw for `key`.
fn seen_in(reads: &[(ReadKey, Fingerprint)], key: ReadKey) -> Option<Fingerprint> {
    reads
        .binary_search_by_key(&key, |(read, _)| *read)
        .ok()
        .map(|at| reads[at].1)
}

#[cfg(test)]
mod tests {
    use rowan::TextSize;

    use super::*;
    use crate::{LuaDeclId, PositionMap};

    const SEEN: Fingerprint = 7;

    fn reader() -> FileId {
        FileId::new(1)
    }

    fn other_reader() -> FileId {
        FileId::new(2)
    }

    fn edited() -> FileId {
        FileId::new(3)
    }

    fn decl_in_edited_file(position: u32) -> FactKey {
        FactKey::Decl(LuaDeclId::new(edited(), TextSize::new(position)))
    }

    /// Lengthens the first line of the edited file, moving the declaration at
    /// 18 (`a`) to 20.
    fn lengthen_first_line() -> EditMaps {
        EditMaps::new([(
            edited(),
            PositionMap::new("local x = 1\nlocal a = 1\n", "local x = 100\nlocal a = 1\n"),
        )])
    }

    /// Lengthens the edited file's first line again, moving the declaration
    /// from 20 to 22.
    fn lengthen_first_line_again() -> EditMaps {
        EditMaps::new([(
            edited(),
            PositionMap::new(
                "local x = 100
local a = 1
",
                "local x = 10000
local a = 1
",
            ),
        )])
    }

    fn stale_after_edit(reads: &mut DiagnosticReads, now: Fingerprint) -> Vec<FileId> {
        reads.apply_edits(&lengthen_first_line());
        reads.stale_files([&decl_in_edited_file(20)], |_| now, |_| None)
    }

    #[test]
    fn a_key_one_file_read_marks_it_stale_after_an_edit_moved_the_key() {
        let mut reads = DiagnosticReads::default();
        reads.record(reader(), vec![(decl_in_edited_file(18), SEEN)]);
        reads.record(
            other_reader(),
            vec![(FactKey::FileText(other_reader()), SEEN)],
        );

        assert_eq!(stale_after_edit(&mut reads, SEEN + 1), vec![reader()]);
    }

    #[test]
    fn a_read_the_edit_only_moved_stays_fresh() {
        let mut reads = DiagnosticReads::default();
        reads.record(reader(), vec![(decl_in_edited_file(18), SEEN)]);

        assert_eq!(stale_after_edit(&mut reads, SEEN), Vec::<FileId>::new());
    }

    /// A read that held only because the edit moved its key reads the key as
    /// it stands after that edit, so the next edit compares against that.
    #[test]
    fn a_read_the_edits_only_moved_stays_fresh_through_a_second_edit() {
        let mut reads = DiagnosticReads::default();
        reads.record(reader(), vec![(decl_in_edited_file(18), SEEN)]);
        reads.apply_edits(&lengthen_first_line());
        reads.stale_files([&decl_in_edited_file(20)], |_| SEEN + 1, |_| Some(SEEN));

        reads.apply_edits(&lengthen_first_line_again());
        let stale = reads.stale_files([&decl_in_edited_file(22)], |_| SEEN + 2, |_| Some(SEEN + 1));

        assert_eq!(stale, Vec::<FileId>::new());
    }

    /// The declaration at 18 moves to 20 while the one at 20 moves to 22:
    /// each reader keeps its own key and what it saw.
    #[test]
    fn a_key_moving_onto_a_position_another_key_left_keeps_its_own_reader() {
        let mut reads = DiagnosticReads::default();
        reads.record(reader(), vec![(decl_in_edited_file(18), SEEN)]);
        reads.record(other_reader(), vec![(decl_in_edited_file(20), SEEN + 5)]);
        reads.apply_edits(&lengthen_first_line());

        let moved_first = reads.stale_files([&decl_in_edited_file(20)], |_| SEEN, |_| None);
        let moved_second = reads.stale_files([&decl_in_edited_file(22)], |_| SEEN + 5, |_| None);

        assert_eq!((moved_first, moved_second), (Vec::new(), Vec::new()));
    }

    /// Re-diagnosing files with different reads leaves one slot per key some
    /// file still reads.
    #[test]
    fn a_key_no_file_reads_any_more_is_released() {
        let mut reads = DiagnosticReads::default();
        for position in 0..100 {
            reads.record(reader(), vec![(decl_in_edited_file(position), SEEN)]);
        }
        reads.record(
            other_reader(),
            vec![(FactKey::FileText(other_reader()), SEEN)],
        );

        assert!(
            reads.census().contains("distinct keys 2,"),
            "{}",
            reads.census()
        );
    }

    /// Deleting a line destroys both declarations on it: their reads share
    /// the one key a destroyed read is filed under, and it is released with
    /// its last reader.
    #[test]
    fn keys_an_edit_destroyed_share_a_slot_released_with_their_readers() {
        let mut reads = DiagnosticReads::default();
        reads.record(reader(), vec![(decl_in_edited_file(6), SEEN)]);
        reads.record(other_reader(), vec![(decl_in_edited_file(8), SEEN)]);
        reads.apply_edits(&EditMaps::new([(
            edited(),
            PositionMap::new(
                "local x = 1
local a = 1
",
                "local a = 1
",
            ),
        )]));
        let shared = reads.census();
        reads.forget(reader());
        reads.forget(other_reader());

        assert!(shared.contains("distinct keys 1,"), "{shared}");
        assert!(
            reads.census().contains("distinct keys 0,"),
            "{}",
            reads.census()
        );
    }

    #[test]
    fn a_forgotten_file_is_not_stale_after_an_edit() {
        let mut reads = DiagnosticReads::default();
        reads.record(reader(), vec![(decl_in_edited_file(18), SEEN)]);
        reads.forget(reader());
        reads.record(
            other_reader(),
            vec![(FactKey::FileText(other_reader()), SEEN)],
        );

        assert_eq!(stale_after_edit(&mut reads, SEEN + 1), Vec::<FileId>::new());
    }
}
