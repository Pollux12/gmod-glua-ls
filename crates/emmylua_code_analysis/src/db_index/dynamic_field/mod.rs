use std::collections::{HashMap, HashSet};

use rowan::TextRange;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{FileId, InFiled, LuaTypeDeclId};

/// Index tracking dynamically-assigned fields on typed variables.
///
/// When `gmod.inferDynamicFields` is enabled, field assignments like
/// `player.customField = value` are recorded here so that both
/// `InjectField` and `UndefinedField` diagnostics can be suppressed.
#[derive(Debug, Default)]
pub struct DynamicFieldIndex {
    /// type → (field_name → set of files that assign this field)
    type_fields: HashMap<LuaTypeDeclId, HashMap<SmolStr, HashSet<FileId>>>,
    /// type → (field_name → assignment locations)
    field_definitions: HashMap<LuaTypeDeclId, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
    /// file → list of (type, field_name) pairs contributed by this file
    file_contributions: HashMap<FileId, Vec<(LuaTypeDeclId, SmolStr, TextRange)>>,
}

impl DynamicFieldIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_field(
        &mut self,
        type_id: LuaTypeDeclId,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) {
        self.type_fields
            .entry(type_id.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default()
            .insert(file_id);

        let field_definitions = self
            .field_definitions
            .entry(type_id.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default();
        let definition = InFiled::new(file_id, range);
        if !field_definitions.contains(&definition) {
            field_definitions.push(definition);
        }

        self.file_contributions
            .entry(file_id)
            .or_default()
            .push((type_id, field_name, range));
    }

    pub fn has_field(&self, type_id: &LuaTypeDeclId, field_name: &str) -> bool {
        self.type_fields
            .get(type_id)
            .is_some_and(|fields| fields.contains_key(field_name))
    }

    pub fn get_fields(
        &self,
        type_id: &LuaTypeDeclId,
    ) -> Option<&HashMap<SmolStr, HashSet<FileId>>> {
        self.type_fields.get(type_id)
    }

    pub fn get_fields_in_file(&self, type_id: &LuaTypeDeclId, file_id: FileId) -> Vec<&SmolStr> {
        self.type_fields
            .get(type_id)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|(name, files)| files.contains(&file_id).then_some(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_field_definitions(
        &self,
        type_id: &LuaTypeDeclId,
        field_name: &str,
    ) -> Vec<InFiled<TextRange>> {
        self.field_definitions
            .get(type_id)
            .and_then(|fields| fields.get(field_name))
            .cloned()
            .unwrap_or_default()
    }
}

impl LuaIndex for DynamicFieldIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(contributions) = self.file_contributions.remove(&file_id) {
            for (type_id, field_name, range) in contributions {
                if let Some(fields) = self.type_fields.get_mut(&type_id) {
                    if let Some(files) = fields.get_mut(&field_name) {
                        files.remove(&file_id);
                        if files.is_empty() {
                            fields.remove(&field_name);
                        }
                    }
                    if fields.is_empty() {
                        self.type_fields.remove(&type_id);
                    }
                }

                if let Some(field_map) = self.field_definitions.get_mut(&type_id) {
                    if let Some(definitions) = field_map.get_mut(&field_name) {
                        definitions.retain(|def| !(def.file_id == file_id && def.value == range));
                        if definitions.is_empty() {
                            field_map.remove(&field_name);
                        }
                    }
                    if field_map.is_empty() {
                        self.field_definitions.remove(&type_id);
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        self.type_fields.clear();
        self.field_definitions.clear();
        self.file_contributions.clear();
    }
}
