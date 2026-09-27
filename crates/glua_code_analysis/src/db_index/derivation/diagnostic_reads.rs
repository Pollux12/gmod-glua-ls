use rustc_hash::{FxHashMap, FxHashSet};

use super::{EditMaps, FactKey, Fingerprint};
use crate::FileId;

/// An interned read key: its index into [`DiagnosticReads::readers`].
type ReadKey = u32;

/// What each file's last diagnostics read, so an analysis run knows which
/// files to diagnose again: those whose recorded reads moved.
///
/// Each distinct key is stored once; a file's reads are its keys' indices
/// with the fingerprints seen, sorted by index.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticReads {
    keys: FxHashMap<FactKey, ReadKey>,
    /// The files that read each key, sorted.
    readers: Vec<Vec<FileId>>,
    files: FxHashMap<FileId, Box<[(ReadKey, Fingerprint)]>>,
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
        let key_bytes = hash_table_bytes(
            self.keys.capacity(),
            size_of::<FactKey>() + size_of::<ReadKey>(),
        );
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
            self.keys.len(),
            megabytes(file_bytes),
            megabytes(key_bytes),
            megabytes(reader_bytes)
        )
    }

    /// Replaces `file_id`'s reads with those of its latest diagnostics.
    pub fn record(&mut self, file_id: FileId, reads: Vec<(FactKey, Fingerprint)>) {
        self.forget(file_id);
        let reads = self.intern_all(reads);
        for (key, _) in reads.iter() {
            let readers = &mut self.readers[*key as usize];
            if let Err(at) = readers.binary_search(&file_id) {
                readers.insert(at, file_id);
            }
        }
        self.files.insert(file_id, reads);
    }

    /// Every key some file's diagnostics read.
    #[cfg(test)]
    pub fn read_keys(&self) -> impl Iterator<Item = &FactKey> {
        self.keys
            .iter()
            .filter_map(|(key, index)| (!self.readers[*index as usize].is_empty()).then_some(key))
    }

    pub fn forget(&mut self, file_id: FileId) {
        let Some(reads) = self.files.remove(&file_id) else {
            return;
        };
        for (key, _) in reads.iter() {
            let readers = &mut self.readers[*key as usize];
            if let Ok(at) = readers.binary_search(&file_id) {
                readers.remove(at);
            }
        }
    }

    /// Renames the recorded reads of keys `edits` moved, keeping the
    /// fingerprints; a read of a key the edits destroyed can no longer hold.
    pub fn apply_edits(&mut self, edits: &EditMaps) {
        let edited: FxHashSet<FileId> = edits.files().collect();
        let moves = |key: &FactKey| {
            key.positional_file()
                .is_some_and(|file_id| edited.contains(&file_id))
        };
        if !self.keys.keys().any(moves) {
            return;
        }
        let mut renamed = Self::default();
        let mut index_of = vec![None; self.readers.len()];
        for (key, index) in std::mem::take(&mut self.keys) {
            if self.readers[index as usize].is_empty() {
                continue;
            }
            let key = if moves(&key) {
                edits
                    .map_key(&key)
                    .unwrap_or(FactKey::FileText(FileId::VIRTUAL))
            } else {
                key
            };
            index_of[index as usize] = Some(renamed.intern(key));
        }
        let reindex = |reads: &[(ReadKey, Fingerprint)]| {
            let mut reads = reads
                .iter()
                .filter_map(|(key, seen)| Some((index_of[*key as usize]?, *seen)))
                .collect::<Vec<_>>();
            reads.sort_unstable_by_key(|(key, _)| *key);
            reads.dedup_by_key(|(key, _)| *key);
            reads.into_boxed_slice()
        };
        for (file_id, reads) in &self.files {
            let reads = reindex(reads);
            for (key, _) in reads.iter() {
                renamed.readers[*key as usize].push(*file_id);
            }
            renamed.files.insert(*file_id, reads);
        }
        for readers in &mut renamed.readers {
            readers.sort_unstable();
        }
        *self = renamed;
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
            let Some(&index) = self.keys.get(key) else {
                continue;
            };
            let readers = &self.readers[index as usize];
            if readers.is_empty() {
                continue;
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
                        stale.insert(*file_id);
                    }
                }
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

    fn intern(&mut self, key: FactKey) -> ReadKey {
        if let Some(&index) = self.keys.get(&key) {
            return index;
        }
        let index = self.readers.len() as ReadKey;
        self.readers.push(Vec::new());
        self.keys.insert(key, index);
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
