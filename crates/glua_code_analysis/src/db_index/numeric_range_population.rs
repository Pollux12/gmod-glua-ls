use std::sync::Arc;

use crate::db_index::cow_shards::CowShards;
use rowan::{TextRange, TextSize};

use smol_str::SmolStr;

use crate::{
    FileId, LuaType,
    db_index::derivation::{self, FactKey, Fingerprint},
};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TableNumericRangePopulation {
    pub table_global: String,
    pub start: i64,
    pub end: i64,
    pub value_type: LuaType,
    pub write_roots: Vec<String>,
    pub alias_roots: Vec<String>,
    pub file_id: FileId,
    /// The file's place in the stable cross-file order, which orders a
    /// global's populations whatever order files were analysed in.
    pub file_order: Arc<str>,
    pub call_range: TextRange,
}

impl TableNumericRangePopulation {
    fn order(&self) -> (Arc<str>, TextSize, i64) {
        (self.file_order.clone(), self.call_range.start(), self.start)
    }
}

#[derive(Debug, Default, Clone)]
pub struct NumericRangePopulationIndex {
    by_file: CowShards<FileId, Vec<TableNumericRangePopulation>>,
    /// Each global's populations, in [`TableNumericRangePopulation::order`].
    by_global: CowShards<String, Vec<TableNumericRangePopulation>>,
}

impl NumericRangePopulationIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_file_populations(
        &mut self,
        file_id: FileId,
        populations: Vec<TableNumericRangePopulation>,
    ) {
        self.remove(file_id);
        for population in &populations {
            derivation::note_write(|| {
                FactKey::NumericPopulation(SmolStr::new(&population.table_global))
            });
            let entries = self
                .by_global
                .entry(population.table_global.clone())
                .or_default();
            let at = entries.partition_point(|entry| entry.order() <= population.order());
            entries.insert(at, population.clone());
        }
        if !populations.is_empty() {
            self.by_file.insert(file_id, populations);
        }
    }

    pub fn get_for_global(&self, table_global: &str) -> &[TableNumericRangePopulation] {
        derivation::note_read(|| FactKey::NumericPopulation(SmolStr::new(table_global)));
        self.by_global
            .get(table_global)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn has_global(&self, table_global: &str) -> bool {
        derivation::note_read(|| FactKey::NumericPopulation(SmolStr::new(table_global)));
        self.by_global.contains_key(table_global)
    }

    pub fn populations_by_file(
        &self,
    ) -> impl Iterator<Item = (&FileId, &Vec<TableNumericRangePopulation>)> {
        self.by_file.iter().inspect(|(_, populations)| {
            for population in populations.iter() {
                derivation::note_read(|| {
                    FactKey::NumericPopulation(SmolStr::new(&population.table_global))
                });
            }
        })
    }

    pub fn populations_by_global(
        &self,
    ) -> impl Iterator<Item = (&String, &Vec<TableNumericRangePopulation>)> {
        self.by_global.iter().inspect(|(table_global, _)| {
            derivation::note_read(|| FactKey::NumericPopulation(SmolStr::new(table_global)));
        })
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::NumericPopulation(table_global) => {
                derivation::fingerprint_opt(self.by_global.get(table_global.as_str()))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a numeric-population fact");
                derivation::ABSENT
            }
        }
    }

    pub fn remove(&mut self, file_id: FileId) {
        if let Some(populations) = self.by_file.remove(&file_id) {
            for population in populations {
                derivation::note_write(|| {
                    FactKey::NumericPopulation(SmolStr::new(&population.table_global))
                });
                if let Some(entries) = self.by_global.get_mut(&population.table_global) {
                    entries.retain(|entry| entry.file_id != file_id);
                    if entries.is_empty() {
                        self.by_global.remove(&population.table_global);
                    }
                }
            }
        }
    }

    pub fn clear(&mut self) {
        if derivation::recording() {
            for table_global in self.by_global.keys() {
                derivation::note_write(|| FactKey::NumericPopulation(SmolStr::new(table_global)));
            }
        }
        self.by_file.clear();
        self.by_global.clear();
    }
}

crate::db_index::cow_shards::task_fork_fields!(NumericRangePopulationIndex {
    by_file,
    by_global => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_sorted_by(
            current,
            frozen,
            fork,
            TableNumericRangePopulation::order,
        )
    },
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_index::cow_shards::TaskFork;

    fn population(file: u32, file_order: &str) -> TableNumericRangePopulation {
        TableNumericRangePopulation {
            table_global: "Ranks".to_string(),
            start: 1,
            end: 3,
            value_type: LuaType::Integer,
            write_roots: Vec::new(),
            alias_roots: Vec::new(),
            file_id: FileId::new(file),
            file_order: Arc::from(file_order),
            call_range: TextRange::default(),
        }
    }

    fn populating_files(index: &NumericRangePopulationIndex) -> Vec<FileId> {
        index
            .get_for_global("Ranks")
            .iter()
            .map(|population| population.file_id)
            .collect()
    }

    #[test]
    fn two_tasks_populating_one_global_both_land() {
        let frozen = NumericRangePopulationIndex::new();
        let mut first = frozen.task_fork();
        first.set_file_populations(FileId::new(1), vec![population(1, "b")]);
        let mut second = frozen.task_fork();
        second.set_file_populations(FileId::new(2), vec![population(2, "a")]);
        let mut index = frozen.clone();
        let mut conflicts = vec![Vec::new(), Vec::new()];

        index.install_tasks_reporting(&frozen, vec![(0, first), (1, second)], &mut conflicts);

        assert_eq!(
            populating_files(&index),
            vec![FileId::new(2), FileId::new(1)]
        );
    }

    #[test]
    fn a_globals_populations_follow_file_order_whatever_order_files_were_set_in() {
        let mut index = NumericRangePopulationIndex::new();
        index.set_file_populations(FileId::new(1), vec![population(1, "a")]);
        index.set_file_populations(FileId::new(2), vec![population(2, "b")]);

        index.set_file_populations(FileId::new(1), vec![population(1, "a")]);

        assert_eq!(
            populating_files(&index),
            vec![FileId::new(1), FileId::new(2)]
        );
    }
}
