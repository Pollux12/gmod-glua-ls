use std::collections::{HashMap, HashSet};

use rowan::TextRange;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{FileId, InFiled, LuaTypeDeclId};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum DynamicFieldOwner {
    Type(LuaTypeDeclId),
    Table(InFiled<TextRange>),
}

/// Index tracking dynamically-assigned fields on typed variables.
///
/// When `gmod.inferDynamicFields` is enabled, field assignments like
/// `player.customField = value` are recorded here so that both
/// `InjectField` and `UndefinedField` diagnostics can be suppressed.
#[derive(Debug, Default)]
pub struct DynamicFieldIndex {
    /// owner → (field_name → set of files that assign this field)
    owner_fields: HashMap<DynamicFieldOwner, HashMap<SmolStr, HashSet<FileId>>>,
    /// owner → (field_name → assignment locations)
    field_definitions: HashMap<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
    /// file → list of (owner, field_name) pairs contributed by this file
    file_contributions: HashMap<FileId, Vec<(DynamicFieldOwner, SmolStr, TextRange)>>,
}

impl DynamicFieldIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_field(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) {
        self.owner_fields
            .entry(owner.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default()
            .insert(file_id);

        let field_definitions = self
            .field_definitions
            .entry(owner.clone())
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
            .push((owner, field_name, range));
    }

    pub fn has_field(&self, owner: &DynamicFieldOwner, field_name: &str) -> bool {
        self.owner_fields
            .get(owner)
            .is_some_and(|fields| fields.contains_key(field_name))
    }

    pub fn get_fields(
        &self,
        owner: &DynamicFieldOwner,
    ) -> Option<&HashMap<SmolStr, HashSet<FileId>>> {
        self.owner_fields.get(owner)
    }

    pub fn get_fields_in_file(&self, owner: &DynamicFieldOwner, file_id: FileId) -> Vec<&SmolStr> {
        self.owner_fields
            .get(owner)
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
        owner: &DynamicFieldOwner,
        field_name: &str,
    ) -> Vec<InFiled<TextRange>> {
        self.field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .cloned()
            .unwrap_or_default()
    }
}

impl LuaIndex for DynamicFieldIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(contributions) = self.file_contributions.remove(&file_id) {
            for (owner, field_name, range) in contributions {
                if let Some(fields) = self.owner_fields.get_mut(&owner) {
                    if let Some(files) = fields.get_mut(&field_name) {
                        files.remove(&file_id);
                        if files.is_empty() {
                            fields.remove(&field_name);
                        }
                    }
                    if fields.is_empty() {
                        self.owner_fields.remove(&owner);
                    }
                }

                if let Some(field_map) = self.field_definitions.get_mut(&owner) {
                    if let Some(definitions) = field_map.get_mut(&field_name) {
                        definitions.retain(|def| !(def.file_id == file_id && def.value == range));
                        if definitions.is_empty() {
                            field_map.remove(&field_name);
                        }
                    }
                    if field_map.is_empty() {
                        self.field_definitions.remove(&owner);
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        self.owner_fields.clear();
        self.field_definitions.clear();
        self.file_contributions.clear();
    }
}
