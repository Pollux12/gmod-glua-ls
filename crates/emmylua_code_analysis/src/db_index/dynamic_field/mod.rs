use std::collections::{HashMap, HashSet};

use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{FileId, LuaTypeDeclId};

/// Index tracking dynamically-assigned fields on typed variables.
///
/// When `gmod.inferDynamicFields` is enabled, field assignments like
/// `player.customField = value` are recorded here so that both
/// `InjectField` and `UndefinedField` diagnostics can be suppressed.
#[derive(Debug, Default)]
pub struct DynamicFieldIndex {
    /// type → (field_name → set of files that assign this field)
    type_fields: HashMap<LuaTypeDeclId, HashMap<SmolStr, HashSet<FileId>>>,
    /// file → list of (type, field_name) pairs contributed by this file
    file_contributions: HashMap<FileId, Vec<(LuaTypeDeclId, SmolStr)>>,
}

impl DynamicFieldIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_field(&mut self, type_id: LuaTypeDeclId, field_name: SmolStr, file_id: FileId) {
        self.type_fields
            .entry(type_id.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default()
            .insert(file_id);

        self.file_contributions
            .entry(file_id)
            .or_default()
            .push((type_id, field_name));
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
}

impl LuaIndex for DynamicFieldIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(contributions) = self.file_contributions.remove(&file_id) {
            for (type_id, field_name) in contributions {
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
            }
        }
    }

    fn clear(&mut self) {
        self.type_fields.clear();
        self.file_contributions.clear();
    }
}
