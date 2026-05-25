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
    /// owner → assignment locations for writes through non-literal keys.
    wildcard_definitions: HashMap<DynamicFieldOwner, Vec<InFiled<TextRange>>>,
    /// file → wildcard assignments contributed by this file.
    wildcard_file_contributions: HashMap<FileId, Vec<(DynamicFieldOwner, TextRange)>>,
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
        let is_new_definition = !field_definitions.contains(&definition);
        if is_new_definition {
            field_definitions.push(definition);
        }

        if is_new_definition {
            self.file_contributions
                .entry(file_id)
                .or_default()
                .push((owner, field_name, range));
        }
    }

    pub fn add_wildcard_definition(
        &mut self,
        owner: DynamicFieldOwner,
        file_id: FileId,
        range: TextRange,
    ) {
        let definitions = self.wildcard_definitions.entry(owner.clone()).or_default();
        let definition = InFiled::new(file_id, range);
        let is_new_definition = !definitions.contains(&definition);
        if is_new_definition {
            definitions.push(definition);
            self.wildcard_file_contributions
                .entry(file_id)
                .or_default()
                .push((owner, range));
        }
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

    pub fn get_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> Vec<InFiled<TextRange>> {
        self.wildcard_definitions
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_all_wildcard_definitions(&self) -> Vec<InFiled<TextRange>> {
        let mut definitions = self
            .wildcard_definitions
            .values()
            .flat_map(|definitions| definitions.iter().cloned())
            .collect::<Vec<_>>();
        definitions.sort_by_key(|definition| (definition.file_id, definition.value.start()));
        definitions.dedup();
        definitions
    }

    fn rebuild_derived_state(&mut self) {
        self.owner_fields.clear();
        self.file_contributions.clear();
        self.wildcard_file_contributions.clear();

        for (owner, fields) in &self.field_definitions {
            for (field_name, definitions) in fields {
                for definition in definitions {
                    self.owner_fields
                        .entry(owner.clone())
                        .or_default()
                        .entry(field_name.clone())
                        .or_default()
                        .insert(definition.file_id);
                    self.file_contributions
                        .entry(definition.file_id)
                        .or_default()
                        .push((owner.clone(), field_name.clone(), definition.value));
                }
            }
        }

        for (owner, definitions) in &self.wildcard_definitions {
            for definition in definitions {
                self.wildcard_file_contributions
                    .entry(definition.file_id)
                    .or_default()
                    .push((owner.clone(), definition.value));
            }
        }
    }
}

impl LuaIndex for DynamicFieldIndex {
    fn remove(&mut self, file_id: FileId) {
        self.field_definitions.retain(|_, fields| {
            fields.retain(|_, definitions| {
                definitions.retain(|definition| definition.file_id != file_id);
                !definitions.is_empty()
            });
            !fields.is_empty()
        });

        self.wildcard_definitions.retain(|_, definitions| {
            definitions.retain(|definition| definition.file_id != file_id);
            !definitions.is_empty()
        });

        self.rebuild_derived_state();
    }

    fn clear(&mut self) {
        self.owner_fields.clear();
        self.field_definitions.clear();
        self.file_contributions.clear();
        self.wildcard_definitions.clear();
        self.wildcard_file_contributions.clear();
    }
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};
    use smol_str::SmolStr;

    use super::*;
    use crate::LuaTypeDeclId;

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    #[test]
    fn remove_prunes_orphaned_field_definitions_without_contribution_entries() {
        let file_to_remove = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index
            .field_definitions
            .entry(owner.clone())
            .or_default()
            .entry(field.clone())
            .or_default()
            .extend([
                InFiled::new(file_to_remove, range(1, 2)),
                InFiled::new(remaining_file, range(3, 4)),
            ]);
        index
            .wildcard_definitions
            .entry(owner.clone())
            .or_default()
            .extend([
                InFiled::new(file_to_remove, range(5, 6)),
                InFiled::new(remaining_file, range(7, 8)),
            ]);

        index.remove(file_to_remove);

        assert_eq!(index.get_field_definitions(&owner, &field).len(), 1);
        assert_eq!(
            index.get_field_definitions(&owner, &field)[0].file_id,
            remaining_file
        );
        assert_eq!(index.get_wildcard_definitions(&owner).len(), 1);
        assert_eq!(
            index.get_wildcard_definitions(&owner)[0].file_id,
            remaining_file
        );
        assert_eq!(
            index.get_fields_in_file(&owner, file_to_remove),
            Vec::<&SmolStr>::new()
        );
        assert_eq!(
            index.get_fields_in_file(&owner, remaining_file),
            vec![&field]
        );
        assert!(!index.file_contributions.contains_key(&file_to_remove));
        assert!(
            !index
                .wildcard_file_contributions
                .contains_key(&file_to_remove)
        );
        assert!(index.file_contributions.contains_key(&remaining_file));
        assert!(
            index
                .wildcard_file_contributions
                .contains_key(&remaining_file)
        );
    }
}
