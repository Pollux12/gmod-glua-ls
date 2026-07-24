use crate::FileId;

pub trait LuaIndex {
    fn remove(&mut self, file_id: FileId);

    /// Remove every file in one logical operation.
    ///
    /// Indexes with workspace-derived state may override this to defer global
    /// rebuilds until all file-owned inputs have been erased.
    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            self.remove(file_id);
        }
    }

    fn clear(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingIndex {
        removed: Vec<FileId>,
    }

    impl LuaIndex for RecordingIndex {
        fn remove(&mut self, file_id: FileId) {
            self.removed.push(file_id);
        }

        fn clear(&mut self) {
            self.removed.clear();
        }
    }

    #[test]
    fn default_batch_removal_visits_every_file_in_order() {
        let file_ids = [FileId::new(3), FileId::new(1), FileId::new(2)];
        let mut index = RecordingIndex::default();

        index.remove_files(&file_ids);

        assert_eq!(index.removed, file_ids);
    }
}
