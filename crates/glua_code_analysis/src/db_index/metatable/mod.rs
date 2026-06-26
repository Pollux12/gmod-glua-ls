use std::collections::HashMap;

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

    fn clear(&mut self) {
        self.metatables.clear();
        self.factory_bindings.clear();
    }
}
