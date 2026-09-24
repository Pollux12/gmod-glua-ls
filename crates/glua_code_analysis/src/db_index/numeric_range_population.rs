use rustc_hash::FxHashMap;

use rowan::TextRange;

use smol_str::SmolStr;

use crate::{
    FileId, LuaType,
    db_index::derivation::{self, FactKey, Fingerprint},
};

#[derive(Debug, Clone, Hash)]
pub struct TableNumericRangePopulation {
    pub table_global: String,
    pub start: i64,
    pub end: i64,
    pub value_type: LuaType,
    pub write_roots: Vec<String>,
    pub alias_roots: Vec<String>,
    pub file_id: FileId,
    pub call_range: TextRange,
}

#[derive(Debug, Default, Clone)]
pub struct NumericRangePopulationIndex {
    by_file: FxHashMap<FileId, Vec<TableNumericRangePopulation>>,
    by_global: FxHashMap<String, Vec<TableNumericRangePopulation>>,
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
            self.by_global
                .entry(population.table_global.clone())
                .or_default()
                .push(population.clone());
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
