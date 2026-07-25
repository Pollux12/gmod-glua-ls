use std::collections::HashMap;

use rowan::TextRange;

use crate::{FileId, LuaType};

#[derive(Debug, Clone)]
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

#[derive(Debug, Default)]
pub struct NumericRangePopulationIndex {
    by_file: HashMap<FileId, Vec<TableNumericRangePopulation>>,
    by_global: HashMap<String, Vec<TableNumericRangePopulation>>,
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
        self.by_global
            .get(table_global)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn has_global(&self, table_global: &str) -> bool {
        self.by_global.contains_key(table_global)
    }

    pub fn remove(&mut self, file_id: FileId) {
        if let Some(populations) = self.by_file.remove(&file_id) {
            for population in populations {
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
        self.by_file.clear();
        self.by_global.clear();
    }
}
