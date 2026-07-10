use std::collections::{HashMap, HashSet};

use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use crate::{FileId, InFiled};

use super::LuaIndex;

#[derive(Debug)]
pub struct LuaMetatableIndex {
    pub metatables: HashMap<InFiled<TextRange>, InFiled<TextRange>>,
    factory_bindings: HashMap<FileId, Vec<SetmetatableFactoryBinding>>,
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
            metatables: HashMap::new(),
            factory_bindings: HashMap::new(),
        }
    }

    pub fn add(&mut self, table: InFiled<TextRange>, metatable: InFiled<TextRange>) {
        self.metatables.insert(table, metatable);
    }

    pub fn get(&self, table: &InFiled<TextRange>) -> Option<&InFiled<TextRange>> {
        self.metatables.get(table)
    }

    pub fn add_factory_binding(&mut self, binding: SetmetatableFactoryBinding) {
        self.factory_bindings
            .entry(binding.file_id)
            .or_default()
            .push(binding);
    }

    pub fn factory_bindings_for_file(
        &self,
        file_id: FileId,
    ) -> Option<&[SetmetatableFactoryBinding]> {
        self.factory_bindings.get(&file_id).map(Vec::as_slice)
    }
}

impl LuaIndex for LuaMetatableIndex {
    fn remove(&mut self, file_id: FileId) {
        self.metatables.retain(|key, _| key.file_id != file_id);
        self.factory_bindings.remove(&file_id);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let removed_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        self.metatables
            .retain(|table, _| !removed_file_ids.contains(&table.file_id));
        self.factory_bindings
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
    }

    fn clear(&mut self) {
        self.metatables.clear();
        self.factory_bindings.clear();
    }
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};

    use super::{LuaIndex, LuaMetatableIndex};
    use crate::{FileId, InFiled};

    #[test]
    fn batch_removal_preserves_metatables_from_surviving_files() {
        let removed = FileId::new(1);
        let other_removed = FileId::new(2);
        let surviving = FileId::new(3);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let removed_table = InFiled::new(removed, range);
        let surviving_table = InFiled::new(surviving, range);
        let mut index = LuaMetatableIndex::new();
        index.add(removed_table.clone(), InFiled::new(surviving, range));
        index.add(surviving_table.clone(), InFiled::new(removed, range));

        index.remove_files(&[other_removed, removed, other_removed]);

        assert!(index.get(&removed_table).is_none());
        assert_eq!(
            index.get(&surviving_table),
            Some(&InFiled::new(removed, range))
        );
    }
}
