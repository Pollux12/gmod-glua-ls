use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::Arc;

use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use crate::db_index::op_log;
use crate::db_index::read_set::{KeyedEpochs, WriteKey};
use crate::{FileId, InFiled};

use super::LuaIndex;

/// `None` when the edit destroyed the literal this range named.
fn map_range(remap: &crate::FileRemap, range: &InFiled<TextRange>) -> Option<InFiled<TextRange>> {
    match remap.table_range(range) {
        crate::Remap::Moved(new) => Some(new),
        crate::Remap::Unrelated => Some(range.clone()),
        crate::Remap::Lost => None,
    }
}

#[derive(Debug, Clone)]
pub struct LuaMetatableIndex {
    /// Table literal -> the bindings written for it, ordered by writer. Both the key and the
    /// metatable range can live in a file other than the one that wrote `setmetatable`, so a
    /// binding is owned by its writer and only the writer's removal drops it.
    metatables: HashMap<InFiled<TextRange>, Vec<MetatableBinding>>,
    by_writer: HashMap<FileId, Vec<InFiled<TextRange>>>,
    factory_bindings: HashMap<FileId, Vec<SetmetatableFactoryBinding>>,
    /// Write epochs for `factory_bindings`/`by_writer` —
    /// `FileScopedKind::MetatableFactory`.
    pub(crate) file_write_epochs: KeyedEpochs<FileId>,
    /// Write epochs for `metatables`, stamped per table site so a binding
    /// change under one literal stales only readers of that site.
    pub(crate) site_epochs: KeyedEpochs<InFiled<TextRange>>,
}

#[derive(Debug, Clone)]
pub struct MetatableBinding {
    pub metatable: InFiled<TextRange>,
    pub writer_file_id: FileId,
    /// Normalized path of the writing file. File ids are not stable between a cold build and an
    /// incremental session, so the winner among several writers is ordered by path.
    writer_sort_key: Arc<str>,
}

impl MetatableBinding {
    fn order_key(&self) -> (&str, u32) {
        (&self.writer_sort_key, self.writer_file_id.id)
    }
}

#[derive(Debug, Clone)]
pub struct SetmetatableFactoryBinding {
    pub file_id: FileId,
    pub table_range: InFiled<TextRange>,
    pub metatable_range: InFiled<TextRange>,
    pub local_name: SmolStr,
    pub call_position: TextSize,
    pub function_scope: TextRange,
}

impl Default for LuaMetatableIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaMetatableIndex {
    pub fn new() -> Self {
        Self {
            metatables: HashMap::default(),
            by_writer: HashMap::default(),
            factory_bindings: HashMap::default(),
            file_write_epochs: KeyedEpochs::default(),
            site_epochs: KeyedEpochs::default(),
        }
    }

    pub fn add(
        &mut self,
        table: InFiled<TextRange>,
        metatable: InFiled<TextRange>,
        writer_file_id: FileId,
        writer_sort_key: Arc<str>,
    ) {
        if op_log::recording() {
            let table2 = table.clone();
            let metatable2 = metatable.clone();
            let sort_key2 = writer_sort_key.clone();
            op_log::push(
                op_log::Domain::Metatable,
                Box::new([
                    WriteKey::Metatable(table.clone()),
                    WriteKey::FileScoped(writer_file_id),
                ]),
                move |db| {
                    db.get_metatable_index_mut()
                        .add(table2, metatable2, writer_file_id, sort_key2)
                },
            );
        }
        let bindings = self.metatables.entry(table.clone()).or_default();
        if let Some(existing) = bindings
            .iter_mut()
            .find(|binding| binding.writer_file_id == writer_file_id)
        {
            if existing.metatable != metatable {
                existing.metatable = metatable;
                self.site_epochs.note(table);
            }
            return;
        }

        let binding = MetatableBinding {
            metatable,
            writer_file_id,
            writer_sort_key,
        };
        let position = bindings.partition_point(|other| other.order_key() < binding.order_key());
        bindings.insert(position, binding);
        self.by_writer
            .entry(writer_file_id)
            .or_default()
            .push(table.clone());
        self.site_epochs.note(table);
        self.file_write_epochs.note(writer_file_id);
    }

    /// Number of recorded `setmetatable` bindings, so a test can show its
    /// fixture actually produced one.
    #[cfg(test)]
    pub fn metatable_count(&self) -> usize {
        self.metatables.values().map(Vec::len).sum()
    }

    pub fn get(&self, table: &InFiled<TextRange>) -> Option<&InFiled<TextRange>> {
        crate::db_index::read_set::record_metatable(&self.site_epochs, table);
        self.metatables
            .get(table)
            .and_then(|bindings| bindings.first())
            .map(|binding| &binding.metatable)
    }

    /// Every recorded binding, paired with the table literal it is keyed on.
    pub fn iter_bindings(
        &self,
    ) -> impl Iterator<Item = (&InFiled<TextRange>, &MetatableBinding)> + '_ {
        op_log::note_untracked_read(op_log::UntrackedDomain::Metatable);
        self.metatables
            .iter()
            .flat_map(|(table, bindings)| bindings.iter().map(move |binding| (table, binding)))
    }

    pub fn add_factory_binding(&mut self, binding: SetmetatableFactoryBinding) {
        if op_log::recording() {
            let binding2 = binding.clone();
            op_log::push_one(
                op_log::Domain::Metatable,
                WriteKey::FileScoped(binding.file_id),
                move |db| db.get_metatable_index_mut().add_factory_binding(binding2),
            );
        }
        let file_id = binding.file_id;
        self.factory_bindings
            .entry(file_id)
            .or_default()
            .push(binding);
        self.file_write_epochs.note(file_id);
    }

    /// Rewrites every `setmetatable` binding that names a table literal in the
    /// edited file.
    ///
    /// Both halves of a binding are table literals and either can live in a
    /// file other than the one that wrote the call, so both sides are mapped.
    /// Returns the files whose binding named a literal the edit destroyed.
    pub fn remap_file_ranges(&mut self, remap: &crate::FileRemap) -> HashSet<FileId> {
        let mut dirty = HashSet::default();
        let mut rebuilt: HashMap<InFiled<TextRange>, Vec<MetatableBinding>> =
            HashMap::with_capacity_and_hasher(self.metatables.len(), Default::default());
        for (table, bindings) in std::mem::take(&mut self.metatables) {
            self.site_epochs.note(table.clone());
            let new_table = map_range(remap, &table);
            for binding in bindings {
                match (new_table.clone(), map_range(remap, &binding.metatable)) {
                    (Some(new_table), Some(new_metatable)) => {
                        rebuilt
                            .entry(new_table)
                            .or_default()
                            .push(MetatableBinding {
                                metatable: new_metatable,
                                ..binding
                            });
                    }
                    // Only the writer can record this binding again.
                    _ => {
                        dirty.insert(binding.writer_file_id);
                    }
                }
            }
        }

        self.by_writer.clear();
        for (table, bindings) in &mut rebuilt {
            bindings.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
            for binding in bindings.iter() {
                self.by_writer
                    .entry(binding.writer_file_id)
                    .or_default()
                    .push(table.clone());
            }
        }
        self.metatables = rebuilt;

        for (file_id, bindings) in &mut self.factory_bindings {
            let before = bindings.len();
            bindings.retain_mut(|binding| {
                let (Some(table_range), Some(metatable_range)) = (
                    map_range(remap, &binding.table_range),
                    map_range(remap, &binding.metatable_range),
                ) else {
                    return false;
                };
                binding.table_range = table_range;
                binding.metatable_range = metatable_range;
                true
            });
            if bindings.len() != before {
                dirty.insert(*file_id);
            }
        }
        self.factory_bindings
            .retain(|_, bindings| !bindings.is_empty());
        dirty.remove(&remap.file_id);
        // Every surviving site re-keyed or lost a binding: stamp old and new
        // keys so readers of either observe the move.
        for site in self.metatables.keys() {
            self.site_epochs.note(site.clone());
        }
        for file_id in &dirty {
            self.file_write_epochs.note(*file_id);
        }
        dirty
    }

    pub fn factory_bindings_for_file(
        &self,
        file_id: FileId,
    ) -> Option<&[SetmetatableFactoryBinding]> {
        crate::db_index::read_set::record_file_scoped(
            &self.file_write_epochs,
            &file_id,
            crate::db_index::read_set::FileScopedKind::MetatableFactory,
        );
        self.factory_bindings.get(&file_id).map(Vec::as_slice)
    }

    fn remove_writer(&mut self, file_id: FileId) {
        for table in self.by_writer.remove(&file_id).unwrap_or_default() {
            let Some(bindings) = self.metatables.get_mut(&table) else {
                continue;
            };
            bindings.retain(|binding| binding.writer_file_id != file_id);
            if bindings.is_empty() {
                self.metatables.remove(&table);
            }
            self.site_epochs.note(table);
        }
        if self.factory_bindings.remove(&file_id).is_some() {
            self.file_write_epochs.note(file_id);
        }
    }
}

impl LuaIndex for LuaMetatableIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_writer(file_id);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for file_id in file_ids {
            self.remove_writer(*file_id);
        }
    }

    fn clear(&mut self) {
        for file_id in self.by_writer.keys().chain(self.factory_bindings.keys()) {
            self.file_write_epochs.note(*file_id);
        }
        for site in self.metatables.keys() {
            self.site_epochs.note(site.clone());
        }
        self.metatables.clear();
        self.by_writer.clear();
        self.factory_bindings.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rowan::{TextRange, TextSize};

    use super::{LuaIndex, LuaMetatableIndex};
    use crate::{FileId, InFiled};

    fn key(path: &str) -> Arc<str> {
        Arc::from(path)
    }

    #[test]
    fn batch_removal_drops_only_bindings_the_removed_file_wrote() {
        let removed = FileId::new(1);
        let other_removed = FileId::new(2);
        let surviving = FileId::new(3);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        // Both table literals live in `removed`; the writers differ.
        let written_by_removed = InFiled::new(removed, range);
        let written_by_surviving = InFiled::new(removed, TextRange::new(range.end(), range.end()));
        let mut index = LuaMetatableIndex::new();
        index.add(
            written_by_removed.clone(),
            InFiled::new(surviving, range),
            removed,
            key("a.lua"),
        );
        index.add(
            written_by_surviving.clone(),
            InFiled::new(removed, range),
            surviving,
            key("c.lua"),
        );

        index.remove_files(&[other_removed, removed, other_removed]);

        assert!(index.get(&written_by_removed).is_none());
        assert_eq!(
            index.get(&written_by_surviving),
            Some(&InFiled::new(removed, range))
        );
    }

    #[test]
    fn reindexing_the_table_file_keeps_a_foreign_writers_binding() {
        let table_file = FileId::new(1);
        let writer = FileId::new(2);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let table = InFiled::new(table_file, range);
        let mut index = LuaMetatableIndex::new();
        index.add(
            table.clone(),
            InFiled::new(writer, range),
            writer,
            key("writer.lua"),
        );

        index.remove(table_file);

        assert_eq!(index.get(&table), Some(&InFiled::new(writer, range)));

        index.remove(writer);

        assert!(index.get(&table).is_none());
    }

    #[test]
    fn several_writers_of_one_table_resolve_by_lowest_path() {
        let table_file = FileId::new(1);
        let late_writer = FileId::new(2);
        let early_writer = FileId::new(3);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let table = InFiled::new(table_file, range);
        let mut index = LuaMetatableIndex::new();
        index.add(
            table.clone(),
            InFiled::new(late_writer, range),
            late_writer,
            key("b.lua"),
        );
        index.add(
            table.clone(),
            InFiled::new(early_writer, range),
            early_writer,
            key("a.lua"),
        );

        assert_eq!(index.get(&table), Some(&InFiled::new(early_writer, range)));

        index.remove(early_writer);

        assert_eq!(index.get(&table), Some(&InFiled::new(late_writer, range)));
    }
}
