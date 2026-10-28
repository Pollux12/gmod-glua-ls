use crate::db_index::cow_shards::CowShards;
use rustc_hash::FxHashSet as HashSet;
use std::sync::Arc;

use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use crate::db_index::derivation::{self, FactKey, Fingerprint};
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
    metatables: CowShards<InFiled<TextRange>, Vec<MetatableBinding>>,
    by_writer: CowShards<FileId, Vec<InFiled<TextRange>>>,
    factory_bindings: CowShards<FileId, Vec<SetmetatableFactoryBinding>>,
    /// File -> the table literals of other files one of whose bindings'
    /// metatables lies in it. A table literal's own file finds it in its shard.
    sites_by_metatable_file: CowShards<FileId, HashSet<InFiled<TextRange>>>,
    /// File -> the writers whose factory bindings name a range in it.
    factory_writers_by_file: CowShards<FileId, HashSet<FileId>>,
}

/// What a table's metatable is known as.
#[derive(Debug, Clone, PartialEq, Hash)]
pub enum BoundMetatable {
    /// The table literal backing the metatable.
    Table(InFiled<TextRange>),
    /// The expression of a metatable no table literal backs.
    Opaque(InFiled<TextRange>),
}

impl BoundMetatable {
    fn range(&self) -> &InFiled<TextRange> {
        match self {
            BoundMetatable::Table(range) | BoundMetatable::Opaque(range) => range,
        }
    }

    fn table(&self) -> Option<&InFiled<TextRange>> {
        match self {
            BoundMetatable::Table(range) => Some(range),
            BoundMetatable::Opaque(_) => None,
        }
    }

    fn with_range(&self, range: InFiled<TextRange>) -> Self {
        match self {
            BoundMetatable::Table(_) => BoundMetatable::Table(range),
            BoundMetatable::Opaque(_) => BoundMetatable::Opaque(range),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetatableBinding {
    pub metatable: BoundMetatable,
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

#[derive(Debug, Clone, Hash, PartialEq)]
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
            metatables: Default::default(),
            by_writer: Default::default(),
            factory_bindings: Default::default(),
            sites_by_metatable_file: Default::default(),
            factory_writers_by_file: Default::default(),
        }
    }

    pub fn add(
        &mut self,
        table: InFiled<TextRange>,
        metatable: InFiled<TextRange>,
        writer_file_id: FileId,
        writer_sort_key: Arc<str>,
    ) {
        self.bind(
            table,
            BoundMetatable::Table(metatable),
            writer_file_id,
            writer_sort_key,
        );
    }

    /// Records that `table` was given the metatable written as `expr`, which
    /// no table literal backs.
    pub fn add_opaque(
        &mut self,
        table: InFiled<TextRange>,
        expr: InFiled<TextRange>,
        writer_file_id: FileId,
        writer_sort_key: Arc<str>,
    ) {
        self.bind(
            table,
            BoundMetatable::Opaque(expr),
            writer_file_id,
            writer_sort_key,
        );
    }

    fn bind(
        &mut self,
        table: InFiled<TextRange>,
        metatable: BoundMetatable,
        writer_file_id: FileId,
        writer_sort_key: Arc<str>,
    ) {
        derivation::note_write(|| FactKey::Metatable(table.clone()));
        let linked = self.site_files(&table);
        let bindings = self.metatables.entry(table.clone()).or_default();
        if let Some(existing) = bindings
            .iter_mut()
            .find(|binding| binding.writer_file_id == writer_file_id)
        {
            if existing.metatable != metatable {
                existing.metatable = metatable;
                self.relink_site(&table, linked);
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
        self.relink_site(&table, linked);
        self.by_writer
            .entry(writer_file_id)
            .or_default()
            .push(table.clone());
    }

    /// The files other than its own that `table`'s bindings' metatables lie in.
    fn site_files(&self, table: &InFiled<TextRange>) -> HashSet<FileId> {
        self.metatables
            .get(table)
            .into_iter()
            .flatten()
            .map(|binding| binding.metatable.range().file_id)
            .filter(|file_id| *file_id != table.file_id)
            .collect()
    }

    /// Re-files `table` in `sites_by_metatable_file` under the files it names
    /// now, given the files it named before its bindings changed.
    fn relink_site(&mut self, table: &InFiled<TextRange>, linked: HashSet<FileId>) {
        let files = self.site_files(table);
        for file_id in linked.difference(&files) {
            if let Some(sites) = self.sites_by_metatable_file.get_mut(file_id) {
                sites.remove(table);
                if sites.is_empty() {
                    self.sites_by_metatable_file.remove(file_id);
                }
            }
        }
        for file_id in files.difference(&linked) {
            self.sites_by_metatable_file
                .entry(*file_id)
                .or_default()
                .insert(table.clone());
        }
    }

    /// Re-files `writer` in `factory_writers_by_file` under the files its
    /// factory bindings name now, given those `previous` named.
    fn relink_factory_writer(&mut self, writer: FileId, previous: &[SetmetatableFactoryBinding]) {
        let linked = factory_files(previous);
        let files = factory_files(
            self.factory_bindings
                .get(&writer)
                .map_or(&[][..], Vec::as_slice),
        );
        for file_id in linked.difference(&files) {
            if let Some(writers) = self.factory_writers_by_file.get_mut(file_id) {
                writers.remove(&writer);
                if writers.is_empty() {
                    self.factory_writers_by_file.remove(file_id);
                }
            }
        }
        for file_id in files.difference(&linked) {
            self.factory_writers_by_file
                .entry(*file_id)
                .or_default()
                .insert(writer);
        }
    }

    /// Number of recorded `setmetatable` bindings, so a test can show its
    /// fixture actually produced one.
    #[cfg(test)]
    pub fn metatable_count(&self) -> usize {
        self.metatables.values().map(Vec::len).sum()
    }

    pub fn get(&self, table: &InFiled<TextRange>) -> Option<&InFiled<TextRange>> {
        derivation::note_read(|| FactKey::Metatable(table.clone()));
        self.metatables.get(table).and_then(|bindings| {
            bindings
                .iter()
                .find_map(|binding| binding.metatable.table())
        })
    }

    /// The expressions of the metatables `table` was given that no table
    /// literal backs.
    pub fn opaque_metatable_exprs(&self, table: &InFiled<TextRange>) -> Vec<InFiled<TextRange>> {
        derivation::note_read(|| FactKey::Metatable(table.clone()));
        self.metatables
            .get(table)
            .into_iter()
            .flatten()
            .filter_map(|binding| match &binding.metatable {
                BoundMetatable::Opaque(expr) => Some(expr.clone()),
                BoundMetatable::Table(_) => None,
            })
            .collect()
    }

    /// Every recorded binding, paired with the table literal it is keyed on.
    pub fn iter_bindings(
        &self,
    ) -> impl Iterator<Item = (&InFiled<TextRange>, &MetatableBinding)> + '_ {
        self.metatables
            .iter()
            .flat_map(|(table, bindings)| bindings.iter().map(move |binding| (table, binding)))
    }

    pub fn add_factory_binding(&mut self, binding: SetmetatableFactoryBinding) {
        let file_id = binding.file_id;
        derivation::note_write(|| FactKey::MetatableFactory(file_id));
        for named in [binding.table_range.file_id, binding.metatable_range.file_id] {
            if !self
                .factory_writers_by_file
                .get(&named)
                .is_some_and(|writers| writers.contains(&file_id))
            {
                self.factory_writers_by_file
                    .entry(named)
                    .or_default()
                    .insert(file_id);
            }
        }
        self.factory_bindings
            .entry(file_id)
            .or_default()
            .push(binding);
    }

    /// Rewrites every `setmetatable` binding that names a table literal in the
    /// edited file.
    ///
    /// Both halves of a binding are table literals and either can live in a
    /// file other than the one that wrote the call, so both sides are mapped.
    /// Returns the bindings that named a literal the edit destroyed, each with
    /// the file that wrote it.
    pub fn remap_file_ranges(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        let mut lost = Vec::new();
        let mut sites = self
            .metatables
            .file_entries(remap.file_id)
            .map(|(table, _)| table)
            .chain(
                self.sites_by_metatable_file
                    .get(&remap.file_id)
                    .into_iter()
                    .flatten(),
            )
            .cloned()
            .collect::<Vec<_>>();
        sites.sort_unstable_by_key(site_sort_key);
        sites.dedup();
        // Every site is detached before any is re-filed: one literal's new
        // range can be another's old one.
        let mut rebuilt: Vec<(InFiled<TextRange>, Vec<MetatableBinding>)> = Vec::new();
        for table in sites {
            let linked = self.site_files(&table);
            let Some(bindings) = self.metatables.remove(&table) else {
                continue;
            };
            derivation::note_write(|| FactKey::Metatable(table.clone()));
            self.relink_site(&table, linked);
            let new_table = map_range(remap, &table);
            let mut kept = Vec::new();
            for binding in bindings {
                if let Some(tables) = self.by_writer.get_mut(&binding.writer_file_id) {
                    tables.retain(|existing| existing != &table);
                }
                let new_metatable = map_range(remap, binding.metatable.range());
                match (new_table.clone(), new_metatable) {
                    (Some(_), Some(new_metatable)) => kept.push(MetatableBinding {
                        metatable: binding.metatable.with_range(new_metatable),
                        ..binding
                    }),
                    // Only the writer can record this binding again.
                    _ => lost.push(crate::LostFact::new(
                        binding.writer_file_id,
                        FactKey::Metatable(table.clone()),
                    )),
                }
            }
            if let Some(new_table) = new_table
                && !kept.is_empty()
            {
                rebuilt.push((new_table, kept));
            }
        }
        for (table, kept) in rebuilt {
            derivation::note_write(|| FactKey::Metatable(table.clone()));
            let linked = self.site_files(&table);
            for binding in &kept {
                self.by_writer
                    .entry(binding.writer_file_id)
                    .or_default()
                    .push(table.clone());
            }
            let bindings = self.metatables.entry(table.clone()).or_default();
            bindings.extend(kept);
            bindings.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
            self.relink_site(&table, linked);
        }

        let mut writers = self
            .factory_writers_by_file
            .get(&remap.file_id)
            .map(|writers| writers.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        writers.sort_unstable();
        for writer in writers {
            let Some(bindings) = self.factory_bindings.get_mut(&writer) else {
                continue;
            };
            derivation::note_write(|| FactKey::MetatableFactory(writer));
            let previous = bindings.clone();
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
            if bindings.len() != previous.len() {
                lost.push(crate::LostFact::new(
                    writer,
                    FactKey::MetatableFactory(writer),
                ));
            }
            if bindings.is_empty() {
                self.factory_bindings.remove(&writer);
            }
            self.relink_factory_writer(writer, &previous);
        }
        lost
    }

    pub fn factory_bindings_for_file(
        &self,
        file_id: FileId,
    ) -> Option<&[SetmetatableFactoryBinding]> {
        derivation::note_read(|| FactKey::MetatableFactory(file_id));
        self.factory_bindings.get(&file_id).map(Vec::as_slice)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Metatable(table) => derivation::fingerprint_opt(
                self.metatables
                    .get(table)
                    .map(|bindings| {
                        bindings
                            .iter()
                            .map(|binding| &binding.metatable)
                            .collect::<Vec<_>>()
                    })
                    .as_ref(),
            ),
            FactKey::MetatableFactory(file_id) => {
                derivation::fingerprint_opt(self.factory_bindings.get(file_id))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a metatable fact");
                derivation::ABSENT
            }
        }
    }

    /// Drops every binding of `table` before the derivations that bound it
    /// run again.
    pub(crate) fn reset_table(&mut self, table: &InFiled<TextRange>) {
        let linked = self.site_files(table);
        let Some(bindings) = self.metatables.remove(table) else {
            return;
        };
        self.relink_site(table, linked);
        derivation::note_write(|| FactKey::Metatable(table.clone()));
        for binding in bindings {
            if let Some(tables) = self.by_writer.get_mut(&binding.writer_file_id) {
                tables.retain(|existing| existing != table);
            }
        }
    }

    /// Drops `file_id`'s factory bindings before the derivation that found
    /// them runs again.
    pub(crate) fn reset_factory_bindings(&mut self, file_id: FileId) {
        if let Some(previous) = self.factory_bindings.remove(&file_id) {
            derivation::note_write(|| FactKey::MetatableFactory(file_id));
            self.relink_factory_writer(file_id, &previous);
        }
    }

    fn remove_writer(&mut self, file_id: FileId) {
        for table in self.by_writer.remove(&file_id).unwrap_or_default() {
            derivation::note_write(|| FactKey::Metatable(table.clone()));
            let linked = self.site_files(&table);
            let Some(bindings) = self.metatables.get_mut(&table) else {
                continue;
            };
            bindings.retain(|binding| binding.writer_file_id != file_id);
            if bindings.is_empty() {
                self.metatables.remove(&table);
            }
            self.relink_site(&table, linked);
        }
        self.reset_factory_bindings(file_id);
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
        for file_id in self.factory_bindings.keys() {
            derivation::note_write(|| FactKey::MetatableFactory(*file_id));
        }
        for site in self.metatables.keys() {
            derivation::note_write(|| FactKey::Metatable(site.clone()));
        }
        self.metatables.clear();
        self.by_writer.clear();
        self.factory_bindings.clear();
        self.sites_by_metatable_file.clear();
        self.factory_writers_by_file.clear();
    }
}

fn site_sort_key(site: &InFiled<TextRange>) -> (FileId, TextSize, TextSize) {
    (site.file_id, site.value.start(), site.value.end())
}

/// The files `bindings`' table and metatable ranges lie in.
fn factory_files(bindings: &[SetmetatableFactoryBinding]) -> HashSet<FileId> {
    bindings
        .iter()
        .flat_map(|binding| [binding.table_range.file_id, binding.metatable_range.file_id])
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rowan::{TextRange, TextSize};
    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{LuaIndex, LuaMetatableIndex, SetmetatableFactoryBinding, factory_files};
    use crate::db_index::derivation::FactKey;
    use crate::{FileId, FileRemap, InFiled, LostFact, PositionMap};

    fn key(path: &str) -> Arc<str> {
        Arc::from(path)
    }

    /// The reverse indexes name exactly the sites and writers a scan of the
    /// bindings finds.
    fn assert_reverse_indexes_match_a_scan(index: &LuaMetatableIndex) {
        let mut sites: FxHashMap<FileId, FxHashSet<InFiled<TextRange>>> = FxHashMap::default();
        for (table, binding) in index.iter_bindings() {
            let metatable = binding.metatable.range();
            if metatable.file_id != table.file_id {
                sites
                    .entry(metatable.file_id)
                    .or_default()
                    .insert(table.clone());
            }
        }
        let indexed: FxHashMap<_, _> = index
            .sites_by_metatable_file
            .iter()
            .map(|(file_id, sites)| (*file_id, sites.clone()))
            .collect();
        assert_eq!(indexed, sites);
        let mut writers: FxHashMap<FileId, FxHashSet<FileId>> = FxHashMap::default();
        for (writer, bindings) in index.factory_bindings.iter() {
            for file_id in factory_files(bindings) {
                writers.entry(file_id).or_default().insert(*writer);
            }
        }
        let indexed: FxHashMap<_, _> = index
            .factory_writers_by_file
            .iter()
            .map(|(file_id, writers)| (*file_id, writers.clone()))
            .collect();
        assert_eq!(indexed, writers);
    }

    #[test]
    fn an_opaque_metatable_binding_belongs_to_its_writer_and_survives_remaps() {
        let (table_file, writer) = (FileId::new(1), FileId::new(2));
        let range = |start: u32| TextRange::new(TextSize::new(start), TextSize::new(start + 4));
        let table = InFiled::new(table_file, range(10));
        let mut index = LuaMetatableIndex::new();
        let expr = InFiled::new(writer, range(40));
        index.add_opaque(table.clone(), expr.clone(), writer, key("w.lua"));
        assert_eq!(index.opaque_metatable_exprs(&table), [expr]);
        assert_eq!(index.get(&table), None);

        let shifted = index.remap_file_ranges(&FileRemap::unvalidated(
            table_file,
            PositionMap::new(
                "abc", "
abc",
            ),
        ));
        assert!(shifted.is_empty());
        let shifted_table = InFiled::new(table_file, range(11));
        assert!(index.opaque_metatable_exprs(&table).is_empty());
        assert_eq!(index.opaque_metatable_exprs(&shifted_table).len(), 1);

        index.remove(writer);
        assert!(index.opaque_metatable_exprs(&shifted_table).is_empty());
    }

    #[test]
    fn reverse_indexes_follow_adds_resets_remaps_and_removals() {
        let (table_file, metatable_file, writer) = (FileId::new(1), FileId::new(2), FileId::new(3));
        let range = |start: u32| TextRange::new(TextSize::new(start), TextSize::new(start + 4));
        let moved = InFiled::new(table_file, range(10));
        let reset = InFiled::new(table_file, range(30));
        let mut index = LuaMetatableIndex::new();
        index.add(
            moved.clone(),
            InFiled::new(metatable_file, range(0)),
            writer,
            key("w.lua"),
        );
        index.add(
            reset.clone(),
            InFiled::new(metatable_file, range(20)),
            writer,
            key("w.lua"),
        );
        index.add(
            moved.clone(),
            InFiled::new(table_file, range(50)),
            table_file,
            key("t.lua"),
        );
        index.add_factory_binding(SetmetatableFactoryBinding {
            file_id: writer,
            table_range: InFiled::new(writer, range(0)),
            metatable_range: InFiled::new(metatable_file, range(40)),
            local_name: "object".into(),
            call_position: TextSize::new(0),
            function_scope: range(0),
        });
        assert_reverse_indexes_match_a_scan(&index);

        index.reset_table(&reset);
        assert_reverse_indexes_match_a_scan(&index);

        let shifted = index.remap_file_ranges(&FileRemap::unvalidated(
            metatable_file,
            PositionMap::new("abc", "\nabc"),
        ));
        assert!(shifted.is_empty());
        assert_reverse_indexes_match_a_scan(&index);

        let deleted = index.remap_file_ranges(&FileRemap::unvalidated(
            metatable_file,
            PositionMap::whole_file(100, 0),
        ));
        assert_eq!(
            deleted,
            [
                LostFact::new(writer, FactKey::Metatable(moved.clone())),
                LostFact::new(writer, FactKey::MetatableFactory(writer)),
            ]
        );
        assert_eq!(
            index.get(&moved),
            Some(&InFiled::new(table_file, range(50)))
        );
        assert_reverse_indexes_match_a_scan(&index);

        index.remove(table_file);
        assert!(index.sites_by_metatable_file.is_empty());
        assert!(index.factory_writers_by_file.is_empty());
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

crate::db_index::cow_shards::task_fork_fields!(LuaMetatableIndex {
    metatables,
    by_writer,
    factory_bindings,
    sites_by_metatable_file => crate::db_index::cow_shards::merge_set,
    factory_writers_by_file => crate::db_index::cow_shards::merge_set,
});
