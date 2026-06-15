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

    fn erase_file_from_derived(&mut self, file_id: FileId) -> (bool, bool) {
        let had_field_contributions =
            if let Some(contributions) = self.file_contributions.remove(&file_id) {
                for (owner, field_name, _) in contributions {
                    let mut remove_owner = false;

                    if let Some(fields) = self.owner_fields.get_mut(&owner) {
                        let mut remove_field = false;

                        if let Some(files) = fields.get_mut(&field_name) {
                            files.remove(&file_id);
                            remove_field = files.is_empty();
                        }

                        if remove_field {
                            fields.remove(&field_name);
                        }

                        remove_owner = fields.is_empty();
                    }

                    if remove_owner {
                        self.owner_fields.remove(&owner);
                    }
                }
                true
            } else {
                false
            };

        let had_wildcard_contributions =
            self.wildcard_file_contributions.remove(&file_id).is_some();

        (had_field_contributions, had_wildcard_contributions)
    }
}

#[cfg(test)]
fn normalize_file_contributions(
    contributions: &HashMap<FileId, Vec<(DynamicFieldOwner, SmolStr, TextRange)>>,
) -> HashMap<FileId, HashMap<(DynamicFieldOwner, SmolStr, TextRange), usize>> {
    contributions
        .iter()
        .map(|(file_id, entries)| {
            let entry_counts = entries.iter().cloned().fold(
                HashMap::<(DynamicFieldOwner, SmolStr, TextRange), usize>::new(),
                |mut counts, entry| {
                    *counts.entry(entry).or_default() += 1;
                    counts
                },
            );
            (*file_id, entry_counts)
        })
        .collect()
}

#[cfg(test)]
fn normalize_wildcard_file_contributions(
    contributions: &HashMap<FileId, Vec<(DynamicFieldOwner, TextRange)>>,
) -> HashMap<FileId, HashMap<(DynamicFieldOwner, TextRange), usize>> {
    contributions
        .iter()
        .map(|(file_id, entries)| {
            let entry_counts = entries.iter().cloned().fold(
                HashMap::<(DynamicFieldOwner, TextRange), usize>::new(),
                |mut counts, entry| {
                    *counts.entry(entry).or_default() += 1;
                    counts
                },
            );
            (*file_id, entry_counts)
        })
        .collect()
}

#[cfg(test)]
fn normalize_field_definitions(
    definitions: &HashMap<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
) -> HashMap<DynamicFieldOwner, HashMap<SmolStr, HashMap<InFiled<TextRange>, usize>>> {
    definitions
        .iter()
        .map(|(owner, fields)| {
            let normalized_fields = fields
                .iter()
                .map(|(field_name, definitions)| {
                    let definition_counts = definitions.iter().cloned().fold(
                        HashMap::<InFiled<TextRange>, usize>::new(),
                        |mut counts, definition| {
                            *counts.entry(definition).or_default() += 1;
                            counts
                        },
                    );
                    (field_name.clone(), definition_counts)
                })
                .collect();
            (owner.clone(), normalized_fields)
        })
        .collect()
}

impl LuaIndex for DynamicFieldIndex {
    fn remove(&mut self, file_id: FileId) {
        let mut removed_field_definitions = false;
        self.field_definitions.retain(|_, fields| {
            fields.retain(|_, definitions| {
                let definition_count_before = definitions.len();
                definitions.retain(|definition| definition.file_id != file_id);
                removed_field_definitions |= definitions.len() != definition_count_before;
                !definitions.is_empty()
            });
            !fields.is_empty()
        });

        let mut removed_wildcard_definitions = false;
        self.wildcard_definitions.retain(|_, definitions| {
            let definition_count_before = definitions.len();
            definitions.retain(|definition| definition.file_id != file_id);
            removed_wildcard_definitions |= definitions.len() != definition_count_before;
            !definitions.is_empty()
        });

        // `file_contributions` is an internal removal index only; no downstream consumer
        // observes its Vec order, and `rebuild_derived_state` may repopulate it through
        // HashMap iteration.
        let (had_field_contributions, had_wildcard_contributions) =
            self.erase_file_from_derived(file_id);

        if (removed_field_definitions && !had_field_contributions)
            || (removed_wildcard_definitions && !had_wildcard_contributions)
        {
            self.rebuild_derived_state();
        }
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
    use crate::{InFiled, LuaTypeDeclId};

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

    #[test]
    fn remove_keeps_other_file_field_then_prunes_last_file() {
        let file_a = FileId::new(1);
        let file_b = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), file_a, range(1, 2));
        index.add_field(owner.clone(), field.clone(), file_b, range(3, 4));

        index.remove(file_a);

        assert!(index.has_field(&owner, &field));
        assert_eq!(
            index.get_fields_in_file(&owner, file_a),
            Vec::<&SmolStr>::new()
        );
        assert_eq!(index.get_fields_in_file(&owner, file_b), vec![&field]);

        index.remove(file_b);

        assert!(!index.has_field(&owner, &field));
        assert!(index.get_fields(&owner).is_none());
        assert!(index.get_field_definitions(&owner, &field).is_empty());
    }

    #[test]
    fn remove_tolerates_same_file_multiple_ranges_for_same_field() {
        let file_id = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), file_id, range(1, 2));
        index.add_field(owner.clone(), field.clone(), file_id, range(3, 4));

        assert_eq!(
            index
                .file_contributions
                .get(&file_id)
                .expect("expected file contributions")
                .len(),
            2
        );

        index.remove(file_id);

        assert!(!index.has_field(&owner, &field));
        assert!(index.get_fields(&owner).is_none());
        assert!(!index.file_contributions.contains_key(&file_id));
    }

    #[test]
    fn remove_prunes_wildcard_contributions_for_type_and_table_owners() {
        let file_to_remove = FileId::new(1);
        let remaining_file = FileId::new(2);
        let type_owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("TypeOwner"));
        let table_owner = DynamicFieldOwner::Table(InFiled::new(file_to_remove, range(20, 30)));

        let mut index = DynamicFieldIndex::new();
        index.add_wildcard_definition(type_owner.clone(), file_to_remove, range(1, 2));
        index.add_wildcard_definition(type_owner.clone(), remaining_file, range(3, 4));
        index.add_wildcard_definition(table_owner.clone(), file_to_remove, range(5, 6));

        index.remove(file_to_remove);

        assert_eq!(
            index.get_wildcard_definitions(&type_owner),
            vec![InFiled::new(remaining_file, range(3, 4))]
        );
        assert!(index.get_wildcard_definitions(&table_owner).is_empty());
        assert!(
            !index
                .wildcard_file_contributions
                .contains_key(&file_to_remove)
        );
        assert!(
            index
                .wildcard_file_contributions
                .contains_key(&remaining_file)
        );
    }

    #[test]
    fn remove_missing_file_is_no_op() {
        let existing_file = FileId::new(1);
        let missing_file = FileId::new(99);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), existing_file, range(1, 2));
        index.add_wildcard_definition(owner.clone(), existing_file, range(3, 4));

        let expected_owner_fields = index.owner_fields.clone();
        let expected_field_definitions = index.field_definitions.clone();
        let expected_file_contributions = normalize_file_contributions(&index.file_contributions);
        let expected_wildcard_definitions = index.wildcard_definitions.clone();
        let expected_wildcard_file_contributions =
            normalize_wildcard_file_contributions(&index.wildcard_file_contributions);

        index.remove(missing_file);

        assert_eq!(index.owner_fields, expected_owner_fields);
        assert_eq!(index.field_definitions, expected_field_definitions);
        assert_eq!(
            normalize_file_contributions(&index.file_contributions),
            expected_file_contributions
        );
        assert_eq!(index.wildcard_definitions, expected_wildcard_definitions);
        assert_eq!(
            normalize_wildcard_file_contributions(&index.wildcard_file_contributions),
            expected_wildcard_file_contributions
        );
    }

    #[test]
    fn remove_then_readd_matches_fresh_state() {
        let removed_file = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let table_owner = DynamicFieldOwner::Table(InFiled::new(removed_file, range(30, 40)));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), removed_file, range(1, 2));
        index.add_field(owner.clone(), field.clone(), remaining_file, range(3, 4));
        index.add_wildcard_definition(owner.clone(), removed_file, range(5, 6));
        index.add_wildcard_definition(table_owner.clone(), removed_file, range(7, 8));

        index.remove(removed_file);
        index.add_field(owner.clone(), field.clone(), removed_file, range(1, 2));
        index.add_wildcard_definition(owner.clone(), removed_file, range(5, 6));
        index.add_wildcard_definition(table_owner.clone(), removed_file, range(7, 8));

        let mut fresh = DynamicFieldIndex::new();
        fresh.add_field(owner.clone(), field.clone(), removed_file, range(1, 2));
        fresh.add_field(owner.clone(), field.clone(), remaining_file, range(3, 4));
        fresh.add_wildcard_definition(owner.clone(), removed_file, range(5, 6));
        fresh.add_wildcard_definition(table_owner.clone(), removed_file, range(7, 8));

        assert_eq!(index.owner_fields, fresh.owner_fields);
        assert_eq!(
            normalize_field_definitions(&index.field_definitions),
            normalize_field_definitions(&fresh.field_definitions)
        );
        assert_eq!(
            normalize_file_contributions(&index.file_contributions),
            normalize_file_contributions(&fresh.file_contributions)
        );
        assert_eq!(index.wildcard_definitions, fresh.wildcard_definitions);
        assert_eq!(
            normalize_wildcard_file_contributions(&index.wildcard_file_contributions),
            normalize_wildcard_file_contributions(&fresh.wildcard_file_contributions)
        );
    }
}
