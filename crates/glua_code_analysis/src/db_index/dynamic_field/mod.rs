use std::collections::{HashMap, HashSet};

use rowan::TextRange;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{DbIndex, FileId, InFiled, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaTypeDeclId};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum DynamicFieldOwner {
    Type(LuaTypeDeclId),
    Table(InFiled<TextRange>),
}

/// True when a wildcard (computed-key) assignment is the *only* thing known
/// about `owner`: no named dynamic fields and no statically-known named
/// members.
pub fn is_pure_wildcard_registry(db: &DbIndex, owner: &DynamicFieldOwner) -> bool {
    let index = db.get_dynamic_field_index();
    if !index.has_wildcard_definitions(owner) {
        return false;
    }
    if index
        .get_fields(owner)
        .is_some_and(|fields| !fields.is_empty())
    {
        return false;
    }

    let member_owner = match owner {
        DynamicFieldOwner::Type(id) => LuaMemberOwner::Type(id.clone()),
        DynamicFieldOwner::Table(range) => LuaMemberOwner::Element(range.clone()),
    };
    db.get_member_index()
        .get_members(&member_owner)
        .is_none_or(|members| {
            !members.iter().any(|member| {
                matches!(
                    member.get_key(),
                    LuaMemberKey::Name(_) | LuaMemberKey::Integer(_)
                )
            })
        })
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
    /// Exact fields collected on their original owner, excluding inherited propagation.
    direct_field_definitions: HashMap<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
    /// Expression-key members whose complete string domain was resolved to finite names.
    finite_named_members: HashMap<DynamicFieldOwner, HashSet<LuaMemberId>>,
    /// file → list of (owner, field_name) pairs contributed by this file
    file_contributions: HashMap<FileId, Vec<(DynamicFieldOwner, SmolStr, TextRange)>>,
    /// owner → assignment locations for writes through non-literal keys.
    wildcard_definitions: HashMap<DynamicFieldOwner, Vec<InFiled<TextRange>>>,
    /// file → wildcard assignments contributed by this file.
    wildcard_file_contributions: HashMap<FileId, Vec<(DynamicFieldOwner, TextRange)>>,
    /// field_name → files containing an `X.field_name = ...` write whose
    /// receiver could not be resolved to any owner, so the write landed
    /// nowhere. Names here are known to be assigned *somewhere* at runtime even
    /// though no table in the index can claim them.
    unattributed_fields: HashMap<SmolStr, HashSet<FileId>>,
    /// file → unattributed field names contributed by this file.
    unattributed_file_contributions: HashMap<FileId, Vec<SmolStr>>,
    sealed: bool,
}

fn definition_sort_key(definition: &InFiled<TextRange>) -> (u32, u32, u32) {
    (
        definition.file_id.id,
        definition.value.start().into(),
        definition.value.end().into(),
    )
}

/// Merges one owner's named definitions into another's, per field name, so a
/// name both hold keeps the definitions of each.
fn merge_field_definitions(
    into: &mut HashMap<SmolStr, Vec<InFiled<TextRange>>>,
    from: HashMap<SmolStr, Vec<InFiled<TextRange>>>,
) {
    for (field_name, definitions) in from {
        // The canonical order and the no-duplicates rule `add_field_inner`
        // maintains on insert have to survive the merge.
        let slot = into.entry(field_name).or_default();
        slot.extend(definitions);
        slot.sort_unstable_by_key(definition_sort_key);
        slot.dedup();
    }
}

/// Appends the wildcard definitions the target does not already hold.
///
/// Order is left alone: `add_wildcard_definition` files these in walk order
/// rather than a canonical one, so sorting here would make a re-indexed
/// workspace disagree with a cold build.
fn merge_wildcard_definitions(into: &mut Vec<InFiled<TextRange>>, from: Vec<InFiled<TextRange>>) {
    for definition in from {
        if !into.contains(&definition) {
            into.push(definition);
        }
    }
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
        let definition = InFiled::new(file_id, range);
        let direct_definitions = self
            .direct_field_definitions
            .entry(owner.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default();
        if !direct_definitions.contains(&definition) {
            direct_definitions.push(definition);
        }
        self.add_field_inner(owner, field_name, file_id, range);
    }

    pub fn add_propagated_field(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) {
        self.add_field_inner(owner, field_name, file_id, range);
    }

    fn add_field_inner(
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
        // Kept in canonical order: `field_definitions` feeds a union of
        // overloads, so insertion order would make the elected arm depend on the
        // batch walk order rather than on the workspace.
        let insert_at = field_definitions.partition_point(|existing| {
            definition_sort_key(existing) < definition_sort_key(&definition)
        });
        let is_new_definition = field_definitions.get(insert_at) != Some(&definition);
        if is_new_definition {
            field_definitions.insert(insert_at, definition);
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

    pub fn add_unattributed_field(&mut self, field_name: SmolStr, file_id: FileId) {
        if self
            .unattributed_fields
            .entry(field_name.clone())
            .or_default()
            .insert(file_id)
        {
            self.unattributed_file_contributions
                .entry(file_id)
                .or_default()
                .push(field_name);
        }
    }

    /// Re-keys owners whose table literal shifted offset.
    ///
    /// A dynamic field is filed under the literal's range, and a write from
    /// another file is not re-collected when this one is re-indexed, so a
    /// stale key makes the field unreachable from the type the literal has.
    pub fn remap_table_ranges(
        &mut self,
        map: &rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
    ) {
        fn remap_owner(
            owner: &DynamicFieldOwner,
            map: &rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
        ) -> Option<DynamicFieldOwner> {
            match owner {
                DynamicFieldOwner::Table(range) => map
                    .get(range)
                    .map(|new| DynamicFieldOwner::Table(new.clone())),
                DynamicFieldOwner::Type(_) => None,
            }
        }

        // The moved group is merged into whatever the target key already
        // holds rather than replacing it. Only literals with a stable anchor
        // are in `map`, so an unanchored literal's cross-file entry can already
        // sit on the range an anchored one moves onto; `extend` on the nested
        // maps would drop that entry instead of merging it.
        macro_rules! remap_owner_keyed {
            ($field:expr, |$slot:ident, $group:ident| $merge:block) => {{
                let moved: Vec<(DynamicFieldOwner, DynamicFieldOwner)> = $field
                    .keys()
                    .filter_map(|owner| Some((owner.clone(), remap_owner(owner, map)?)))
                    .collect();
                // Detached before any is re-filed: one literal's new range can
                // be another's old one.
                let detached: Vec<_> = moved
                    .into_iter()
                    .filter_map(|(old, new)| Some((new, $field.remove(&old)?)))
                    .collect();
                for (new, group) in detached {
                    let $slot = $field.entry(new).or_default();
                    let $group = group;
                    $merge
                }
            }};
        }

        remap_owner_keyed!(self.owner_fields, |slot, group| {
            for (field_name, files) in group {
                slot.entry(field_name).or_default().extend(files);
            }
        });
        remap_owner_keyed!(self.field_definitions, |slot, group| {
            merge_field_definitions(slot, group)
        });
        remap_owner_keyed!(self.direct_field_definitions, |slot, group| {
            merge_field_definitions(slot, group)
        });
        remap_owner_keyed!(self.finite_named_members, |slot, group| {
            slot.extend(group)
        });
        remap_owner_keyed!(self.wildcard_definitions, |slot, group| {
            merge_wildcard_definitions(slot, group)
        });

        for entries in self.file_contributions.values_mut() {
            for (owner, _, _) in entries.iter_mut() {
                if let Some(new) = remap_owner(owner, map) {
                    *owner = new;
                }
            }
        }
        for entries in self.wildcard_file_contributions.values_mut() {
            for (owner, _) in entries.iter_mut() {
                if let Some(new) = remap_owner(owner, map) {
                    *owner = new;
                }
            }
        }
    }

    /// Every table-literal range this index is keyed by.
    #[cfg(test)]
    pub(crate) fn table_ranges(&self) -> Vec<InFiled<TextRange>> {
        fn owner_range(owner: &DynamicFieldOwner) -> Option<InFiled<TextRange>> {
            match owner {
                DynamicFieldOwner::Table(range) => Some(range.clone()),
                DynamicFieldOwner::Type(_) => None,
            }
        }
        self.owner_fields
            .keys()
            .chain(self.field_definitions.keys())
            .chain(self.direct_field_definitions.keys())
            .chain(self.finite_named_members.keys())
            .chain(self.wildcard_definitions.keys())
            .filter_map(owner_range)
            .chain(
                self.file_contributions
                    .values()
                    .flatten()
                    .filter_map(|(owner, _, _)| owner_range(owner)),
            )
            .chain(
                self.wildcard_file_contributions
                    .values()
                    .flatten()
                    .filter_map(|(owner, _)| owner_range(owner)),
            )
            .collect()
    }

    /// Whether the index has finished being built for the current analysis
    /// round. A read taken before that answers from however far the batch walk
    /// happened to get, so an absent field is not yet known to be absent.
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    pub fn set_sealed(&mut self, sealed: bool) {
        self.sealed = sealed;
    }

    pub fn has_unattributed_field(&self, field_name: &str) -> bool {
        self.unattributed_fields.contains_key(field_name)
    }

    pub fn has_field(&self, owner: &DynamicFieldOwner, field_name: &str) -> bool {
        self.owner_fields
            .get(owner)
            .is_some_and(|fields| fields.contains_key(field_name))
    }

    pub fn has_field_in_file(
        &self,
        owner: &DynamicFieldOwner,
        field_name: &str,
        file_id: FileId,
    ) -> bool {
        self.owner_fields
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .is_some_and(|files| files.contains(&file_id))
    }

    pub fn get_fields(
        &self,
        owner: &DynamicFieldOwner,
    ) -> Option<&HashMap<SmolStr, HashSet<FileId>>> {
        self.owner_fields.get(owner)
    }

    pub fn get_direct_fields(
        &self,
        owner: &DynamicFieldOwner,
    ) -> Option<&HashMap<SmolStr, Vec<InFiled<TextRange>>>> {
        self.direct_field_definitions.get(owner)
    }

    pub fn add_finite_named_member(&mut self, owner: DynamicFieldOwner, member_id: LuaMemberId) {
        self.finite_named_members
            .entry(owner)
            .or_default()
            .insert(member_id);
    }

    pub fn remove_finite_named_member(
        &mut self,
        owner: &DynamicFieldOwner,
        member_id: LuaMemberId,
    ) {
        let Some(members) = self.finite_named_members.get_mut(owner) else {
            return;
        };
        members.remove(&member_id);
        if members.is_empty() {
            self.finite_named_members.remove(owner);
        }
    }

    pub fn member_has_finite_named_definition(
        &self,
        owner: &DynamicFieldOwner,
        member_id: LuaMemberId,
    ) -> bool {
        self.finite_named_members
            .get(owner)
            .is_some_and(|members| members.contains(&member_id))
    }

    pub fn owner_has_finite_named_members(&self, owner: &DynamicFieldOwner) -> bool {
        self.finite_named_members
            .get(owner)
            .is_some_and(|members| !members.is_empty())
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

    /// Every recorded definition of one field, in canonical order.
    pub fn field_definitions(
        &self,
        owner: &DynamicFieldOwner,
        field_name: &str,
    ) -> &[InFiled<TextRange>] {
        self.field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn get_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> Vec<InFiled<TextRange>> {
        self.wildcard_definitions
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }

    pub fn has_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> bool {
        self.wildcard_definitions
            .get(owner)
            .is_some_and(|definitions| !definitions.is_empty())
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
        self.remove_files(&[file_id]);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let removed: HashSet<FileId> = file_ids.iter().copied().collect();
        // The definition maps are swept once for the whole batch: retaining
        // per file made removal cost O(files × index) on high fan-in edits.
        let mut files_with_removed_fields: HashSet<FileId> = HashSet::new();
        self.field_definitions.retain(|_, fields| {
            fields.retain(|_, definitions| {
                definitions.retain(|definition| {
                    if removed.contains(&definition.file_id) {
                        files_with_removed_fields.insert(definition.file_id);
                        false
                    } else {
                        true
                    }
                });
                !definitions.is_empty()
            });
            !fields.is_empty()
        });
        self.direct_field_definitions.retain(|_, fields| {
            fields.retain(|_, definitions| {
                definitions.retain(|definition| !removed.contains(&definition.file_id));
                !definitions.is_empty()
            });
            !fields.is_empty()
        });
        self.finite_named_members.retain(|_, members| {
            members.retain(|member_id| !removed.contains(&member_id.file_id));
            !members.is_empty()
        });

        let mut files_with_removed_wildcards: HashSet<FileId> = HashSet::new();
        self.wildcard_definitions.retain(|_, definitions| {
            definitions.retain(|definition| {
                if removed.contains(&definition.file_id) {
                    files_with_removed_wildcards.insert(definition.file_id);
                    false
                } else {
                    true
                }
            });
            !definitions.is_empty()
        });

        // `file_contributions` is an internal removal index only; no downstream consumer
        // observes its Vec order, and `rebuild_derived_state` may repopulate it through
        // HashMap iteration.
        for &file_id in file_ids {
            if let Some(field_names) = self.unattributed_file_contributions.remove(&file_id) {
                for field_name in field_names {
                    if let Some(files) = self.unattributed_fields.get_mut(&field_name) {
                        files.remove(&file_id);
                        if files.is_empty() {
                            self.unattributed_fields.remove(&field_name);
                        }
                    }
                }
            }
        }

        let mut need_rebuild = false;
        for &file_id in file_ids {
            let (had_field_contributions, had_wildcard_contributions) =
                self.erase_file_from_derived(file_id);
            need_rebuild |= (files_with_removed_fields.contains(&file_id)
                && !had_field_contributions)
                || (files_with_removed_wildcards.contains(&file_id) && !had_wildcard_contributions);
        }
        // Rebuilding is a pure function of the primary maps, so one rebuild at
        // batch end is equivalent to the per-file rebuilds it replaces.
        if need_rebuild {
            self.rebuild_derived_state();
        }
    }

    fn clear(&mut self) {
        self.owner_fields.clear();
        self.field_definitions.clear();
        self.direct_field_definitions.clear();
        self.finite_named_members.clear();
        self.file_contributions.clear();
        self.wildcard_definitions.clear();
        self.wildcard_file_contributions.clear();
        self.unattributed_fields.clear();
        self.unattributed_file_contributions.clear();
        self.sealed = false;
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};
    use smol_str::SmolStr;

    use super::*;
    use crate::{InFiled, LuaTypeDeclId};

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    fn shift(
        file_id: FileId,
        from: TextRange,
        to: TextRange,
    ) -> rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>> {
        let mut map = rustc_hash::FxHashMap::default();
        map.insert(InFiled::new(file_id, from), InFiled::new(file_id, to));
        map
    }

    /// Every owner-keyed store has to move together. `owner_fields` backs
    /// `has_field`, so leaving it behind strands the field on a range no type
    /// resolves to any more.
    #[test]
    fn remapping_a_literal_moves_the_field_lookup_with_it() {
        let edited = FileId::new(1);
        let contributor = FileId::new(2);
        let old = DynamicFieldOwner::Table(InFiled::new(edited, range(0, 10)));
        let new = DynamicFieldOwner::Table(InFiled::new(edited, range(20, 30)));

        let mut index = DynamicFieldIndex::new();
        index.add_field(old.clone(), SmolStr::new("f"), contributor, range(1, 2));
        index.remap_table_ranges(&shift(edited, range(0, 10), range(20, 30)));

        assert!(index.has_field(&new, "f"));
        assert!(!index.has_field(&old, "f"));
        assert_eq!(index.field_definitions(&new, "f").len(), 1);
    }

    /// Only anchored literals are remapped, so an unanchored one's cross-file
    /// entry can already sit on the range an anchored one moves onto. Merging
    /// has to keep both, per field name.
    #[test]
    fn remapping_onto_an_occupied_range_keeps_both_owners_definitions() {
        let edited = FileId::new(1);
        let contributor = FileId::new(2);
        let moved_from = DynamicFieldOwner::Table(InFiled::new(edited, range(0, 10)));
        let occupied = DynamicFieldOwner::Table(InFiled::new(edited, range(20, 30)));

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            moved_from.clone(),
            SmolStr::new("shared"),
            contributor,
            range(1, 2),
        );
        index.add_field(
            occupied.clone(),
            SmolStr::new("shared"),
            contributor,
            range(3, 4),
        );
        index.remap_table_ranges(&shift(edited, range(0, 10), range(20, 30)));

        assert_eq!(
            index.field_definitions(&occupied, "shared"),
            [
                InFiled::new(contributor, range(1, 2)),
                InFiled::new(contributor, range(3, 4)),
            ]
        );
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

        assert_eq!(index.field_definitions(&owner, &field).len(), 1);
        assert_eq!(
            index.field_definitions(&owner, &field)[0].file_id,
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
        assert!(index.field_definitions(&owner, &field).is_empty());
    }

    #[test]
    fn field_definitions_are_canonically_ordered_regardless_of_insertion_order() {
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");
        let inserts = [
            (FileId::new(2), range(5, 6)),
            (FileId::new(1), range(9, 10)),
            (FileId::new(1), range(3, 4)),
        ];

        let mut forward = DynamicFieldIndex::new();
        for (file_id, range) in inserts {
            forward.add_field(owner.clone(), field.clone(), file_id, range);
        }
        let mut reverse = DynamicFieldIndex::new();
        for (file_id, range) in inserts.into_iter().rev() {
            reverse.add_field(owner.clone(), field.clone(), file_id, range);
        }

        assert_eq!(
            forward.field_definitions(&owner, &field),
            vec![
                InFiled::new(FileId::new(1), range(3, 4)),
                InFiled::new(FileId::new(1), range(9, 10)),
                InFiled::new(FileId::new(2), range(5, 6)),
            ]
        );
        assert_eq!(
            forward.field_definitions(&owner, &field),
            reverse.field_definitions(&owner, &field)
        );
    }

    #[test]
    fn keyed_file_lookup_only_matches_contributing_files() {
        let file_a = FileId::new(1);
        let file_b = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");
        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), file_a, range(1, 2));

        assert!(index.has_field(&owner, &field));
        assert!(index.has_field_in_file(&owner, &field, file_a));
        assert!(!index.has_field_in_file(&owner, &field, file_b));
        assert!(!index.has_field_in_file(&owner, "missing", file_a));
    }

    #[test]
    fn finite_named_member_lookups_track_lifecycle() {
        let file_id = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let mut index = DynamicFieldIndex::new();
        let direct_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(0, 2)),
            file_id,
        );
        let wildcard_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(2, 4)),
            file_id,
        );
        let containing_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(0, 4)),
            file_id,
        );
        index.add_finite_named_member(owner.clone(), direct_member);

        assert!(index.member_has_finite_named_definition(&owner, direct_member));
        assert!(!index.member_has_finite_named_definition(&owner, wildcard_member));
        assert!(!index.member_has_finite_named_definition(&owner, containing_member));

        index.remove_finite_named_member(&owner, direct_member);
        assert!(!index.member_has_finite_named_definition(&owner, direct_member));
        index.add_finite_named_member(owner.clone(), direct_member);

        index.remove(file_id);
        assert!(!index.member_has_finite_named_definition(&owner, direct_member));

        index.add_finite_named_member(owner.clone(), direct_member);
        index.clear();
        assert!(!index.member_has_finite_named_definition(&owner, direct_member));
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
