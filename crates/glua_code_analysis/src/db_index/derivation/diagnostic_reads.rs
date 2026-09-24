use rustc_hash::{FxHashMap, FxHashSet};

use super::{EditMaps, FactKey, Fingerprint};
use crate::FileId;

/// What each file's last diagnostics read, so an analysis run knows which
/// files to diagnose again: those whose recorded reads moved.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticReads {
    files: FxHashMap<FileId, FxHashMap<FactKey, Fingerprint>>,
    readers: FxHashMap<FactKey, FxHashSet<FileId>>,
    /// The reads of the workspace-wide data every file's diagnostics share.
    shared: FxHashMap<FactKey, Fingerprint>,
}

impl DiagnosticReads {
    /// Replaces `file_id`'s reads with those of its latest diagnostics.
    pub fn record(&mut self, file_id: FileId, reads: Vec<(FactKey, Fingerprint)>) {
        self.forget(file_id);
        for (key, _) in &reads {
            self.readers.entry(key.clone()).or_default().insert(file_id);
        }
        self.files.insert(file_id, reads.into_iter().collect());
    }

    /// Replaces the shared data's reads.
    pub fn record_shared(&mut self, reads: Vec<(FactKey, Fingerprint)>) {
        self.shared = reads.into_iter().collect();
    }

    pub fn forget(&mut self, file_id: FileId) {
        let Some(reads) = self.files.remove(&file_id) else {
            return;
        };
        for key in reads.keys() {
            if let Some(readers) = self.readers.get_mut(key) {
                readers.remove(&file_id);
                if readers.is_empty() {
                    self.readers.remove(key);
                }
            }
        }
    }

    /// Renames the recorded reads of keys `edits` moved, keeping the
    /// fingerprints; a read of a key the edits destroyed can no longer hold.
    pub fn apply_edits(&mut self, edits: &EditMaps) {
        let files: FxHashSet<FileId> = edits.files().collect();
        let moves_key = |key: &FactKey| {
            key.positional_file()
                .is_some_and(|file_id| files.contains(&file_id))
        };
        let renamed = |key: FactKey| {
            edits
                .map_key(&key)
                .unwrap_or(FactKey::FileText(FileId::VIRTUAL))
        };
        for reads in self.files.values_mut() {
            if reads.keys().any(moves_key) {
                *reads = std::mem::take(reads)
                    .into_iter()
                    .map(|(key, seen)| {
                        if moves_key(&key) {
                            (renamed(key), seen)
                        } else {
                            (key, seen)
                        }
                    })
                    .collect();
            }
        }
        if self.shared.keys().any(moves_key) {
            self.shared = std::mem::take(&mut self.shared)
                .into_iter()
                .map(|(key, seen)| {
                    if moves_key(&key) {
                        (renamed(key), seen)
                    } else {
                        (key, seen)
                    }
                })
                .collect();
        }
        self.readers.clear();
        for (file_id, reads) in &self.files {
            for key in reads.keys() {
                self.readers
                    .entry(key.clone())
                    .or_default()
                    .insert(*file_id);
            }
        }
    }

    /// The files whose recorded read of one of `touched` no longer holds
    /// (`key_holds(key)(seen)`); every recorded file when the shared data's
    /// reads moved.
    pub fn stale_files<'a, F: Fn(Fingerprint) -> bool>(
        &self,
        touched: impl IntoIterator<Item = &'a FactKey>,
        key_holds: impl Fn(&FactKey) -> F,
    ) -> Vec<FileId> {
        let mut stale = FxHashSet::default();
        let mut shared_stale = false;
        for key in touched {
            let shared = self.shared.get(key);
            let readers = self.readers.get(key);
            if shared.is_none() && readers.is_none() {
                continue;
            }
            let holds = key_holds(key);
            if shared.is_some_and(|seen| !holds(*seen)) {
                shared_stale = true;
            }
            for file_id in readers.into_iter().flatten() {
                if self.files[file_id]
                    .get(key)
                    .is_none_or(|seen| !holds(*seen))
                {
                    stale.insert(*file_id);
                }
            }
        }
        if shared_stale {
            stale.extend(self.files.keys().copied());
        }
        let mut stale = stale.into_iter().collect::<Vec<_>>();
        stale.sort_unstable();
        stale
    }
}
