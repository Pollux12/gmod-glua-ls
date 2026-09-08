use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use rowan::TextRange;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{
    DbIndex, FileId, GlobalId, InFiled, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaTypeDeclId,
};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum DynamicFieldOwner {
    Type(LuaTypeDeclId),
    Table(InFiled<TextRange>),
    /// A global path whose table several files bootstrap. Keyed by the path
    /// rather than by one of its literals, so `X.k[v] = w` written against any
    /// of them lands in the bucket every reader of `X.k` consults.
    GlobalPath(GlobalId),
}

/// The one owner a dynamic field written on `owner` belongs to.
///
/// Mirrors [`crate::LuaMemberIndex::canonical_owner`]: a table literal that
/// initialises a global path is not an owner of its own, so a field registered
/// on it belongs to the path (or, once the path's declaration carries a
/// `---@class`, to that class). Literals that initialise nothing keep their
/// range, exactly as before.
pub fn canonical_dynamic_field_owner(db: &DbIndex, owner: DynamicFieldOwner) -> DynamicFieldOwner {
    let DynamicFieldOwner::Table(range) = &owner else {
        return owner;
    };
    match db
        .get_member_index()
        .canonical_owner(LuaMemberOwner::Element(range.clone()))
    {
        LuaMemberOwner::GlobalPath(path) => DynamicFieldOwner::GlobalPath(path),
        LuaMemberOwner::Type(type_id) => DynamicFieldOwner::Type(type_id),
        _ => owner,
    }
}

/// The canonical dynamic-field owner a member owner names.
pub fn dynamic_field_owner_of(db: &DbIndex, owner: &LuaMemberOwner) -> Option<DynamicFieldOwner> {
    match db.get_member_index().canonical_owner(owner.clone()) {
        LuaMemberOwner::Type(type_id) => Some(DynamicFieldOwner::Type(type_id)),
        LuaMemberOwner::GlobalPath(path) => Some(DynamicFieldOwner::GlobalPath(path)),
        LuaMemberOwner::Element(range) => Some(DynamicFieldOwner::Table(range)),
        LuaMemberOwner::LocalUnresolve => None,
    }
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
        DynamicFieldOwner::GlobalPath(path) => LuaMemberOwner::GlobalPath(path.clone()),
    };
    let member_owner = db.get_member_index().canonical_owner(member_owner);
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
    /// file → finite-named members contributed by this file, so removal visits
    /// only the owners the removed files touched instead of retaining over
    /// every member in the workspace. Keyed by the member's own file, which is
    /// the file whose analysis recorded it.
    finite_named_file_contributions: HashMap<FileId, Vec<(DynamicFieldOwner, LuaMemberId)>>,
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
    /// Count of dynamic-field definition changes, keyed by owner and field
    /// name (`None` is the wildcard bucket). Field assignments land from the
    /// dynamic-field passes and from member resolution during inference, so
    /// without this counter every such write looks like unversioned state to
    /// the fixpoint filter.
    dynamic_writes: u64,
    dynamic_write_versions: HashMap<(DynamicFieldOwner, Option<SmolStr>), u64>,
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

    /// Returns whether the definition site was new to the index.
    pub fn add_field(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) -> bool {
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
        self.add_field_inner(owner, field_name, file_id, range)
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
    ) -> bool {
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
            self.note_dynamic_write(owner.clone(), Some(field_name.clone()));
            self.file_contributions
                .entry(file_id)
                .or_default()
                .push((owner, field_name, range));
        }
        is_new_definition
    }

    /// Returns whether the definition site was new to the index.
    pub fn add_wildcard_definition(
        &mut self,
        owner: DynamicFieldOwner,
        file_id: FileId,
        range: TextRange,
    ) -> bool {
        let definitions = self.wildcard_definitions.entry(owner.clone()).or_default();
        let definition = InFiled::new(file_id, range);
        let is_new_definition = !definitions.contains(&definition);
        if is_new_definition {
            definitions.push(definition);
            self.note_dynamic_write(owner.clone(), None);
            self.wildcard_file_contributions
                .entry(file_id)
                .or_default()
                .push((owner, range));
        }
        is_new_definition
    }

    /// Compare it across an operation to tell whether the operation changed
    /// any dynamic-field definition.
    pub fn dynamic_writes(&self) -> u64 {
        self.dynamic_writes
    }

    /// Records a dynamic-field definition change. Callers bump only when the
    /// write added a novel fact (`is_new` at the add sites, a drained
    /// definition at removal); re-adding an identical definition must not
    /// bump, or the fixpoint's staleness filter degrades to a full requeue.
    pub(crate) fn note_dynamic_write(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: Option<SmolStr>,
    ) {
        self.dynamic_writes += 1;
        self.dynamic_write_versions
            .insert((owner, field_name), self.dynamic_writes);
    }

    pub(crate) fn dynamic_write_version(&self, key: &(DynamicFieldOwner, Option<SmolStr>)) -> u64 {
        self.dynamic_write_versions.get(key).copied().unwrap_or(0)
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

    /// Takes back an unattributed record once a settled retry has attributed
    /// the write to a real owner.
    /// Returns whether a record was actually removed.
    pub fn remove_unattributed_field(&mut self, field_name: &str, file_id: FileId) -> bool {
        let mut removed = false;
        if let Some(files) = self.unattributed_fields.get_mut(field_name) {
            removed = files.remove(&file_id);
            if removed && files.is_empty() {
                self.unattributed_fields.remove(field_name);
            }
        }
        if let Some(names) = self.unattributed_file_contributions.get_mut(&file_id) {
            names.retain(|name| name != field_name);
            if names.is_empty() {
                self.unattributed_file_contributions.remove(&file_id);
            }
        }
        removed
    }

    /// Re-keys the table-literal owners in the edited file and drops the
    /// groups whose literal the edit destroyed.
    ///
    /// Returns the files that contributed a field to a destroyed group: a
    /// dynamic field is filed under the literal's range and a write from
    /// another file is not re-collected when this one is re-indexed, so
    /// nothing else rebuilds it.
    pub fn remap_file_table_owners(&mut self, remap: &crate::FileRemap) -> HashSet<FileId> {
        let owners: HashSet<DynamicFieldOwner> = self
            .owner_fields
            .keys()
            .chain(self.field_definitions.keys())
            .chain(self.direct_field_definitions.keys())
            .chain(self.finite_named_members.keys())
            .chain(self.wildcard_definitions.keys())
            .filter(|owner| {
                matches!(owner, DynamicFieldOwner::Table(range) if range.file_id == remap.file_id)
            })
            .cloned()
            .collect();

        let mut moved = rustc_hash::FxHashMap::default();
        let mut lost: HashSet<DynamicFieldOwner> = HashSet::default();
        for owner in owners {
            let DynamicFieldOwner::Table(range) = &owner else {
                continue;
            };
            match remap.table_range(range) {
                crate::Remap::Moved(new) => {
                    if &new != range {
                        moved.insert(range.clone(), new);
                    }
                }
                crate::Remap::Unrelated => {}
                crate::Remap::Lost => {
                    lost.insert(owner);
                }
            }
        }

        self.remap_table_ranges(&moved);

        let mut dirty = HashSet::default();
        if !lost.is_empty() {
            // Ticked in a stable owner order: the counter values must not
            // depend on hash iteration. Comparison-only, but deterministic
            // anyway.
            let mut lost = lost.into_iter().collect::<Vec<_>>();
            lost.sort_by_cached_key(|owner| format!("{owner:?}"));
            for owner in &lost {
                // Dropping the group is a novel fact for its readers.
                let field_names = self
                    .owner_fields
                    .get(owner)
                    .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let has_wildcards = self.wildcard_definitions.contains_key(owner);
                for field_name in field_names {
                    self.note_dynamic_write(owner.clone(), Some(field_name));
                }
                if has_wildcards {
                    self.note_dynamic_write(owner.clone(), None);
                }
                self.owner_fields.remove(owner);
                self.field_definitions.remove(owner);
                self.direct_field_definitions.remove(owner);
                self.finite_named_members.remove(owner);
                self.wildcard_definitions.remove(owner);
            }
            for (file_id, entries) in &mut self.file_contributions {
                let before = entries.len();
                entries.retain(|(owner, _, _)| !lost.contains(owner));
                if entries.len() != before {
                    dirty.insert(*file_id);
                }
            }
            for (file_id, entries) in &mut self.wildcard_file_contributions {
                let before = entries.len();
                entries.retain(|(owner, _)| !lost.contains(owner));
                if entries.len() != before {
                    dirty.insert(*file_id);
                }
            }
            for entries in self.finite_named_file_contributions.values_mut() {
                entries.retain(|(owner, _)| !lost.contains(owner));
            }
            self.finite_named_file_contributions
                .retain(|_, entries| !entries.is_empty());
        }
        dirty.remove(&remap.file_id);
        dirty
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
                DynamicFieldOwner::Type(_) | DynamicFieldOwner::GlobalPath(_) => None,
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
        for entries in self.finite_named_file_contributions.values_mut() {
            for (owner, _) in entries.iter_mut() {
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
                DynamicFieldOwner::Type(_) | DynamicFieldOwner::GlobalPath(_) => None,
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
        if self
            .finite_named_members
            .entry(owner.clone())
            .or_default()
            .insert(member_id)
        {
            self.finite_named_file_contributions
                .entry(member_id.file_id)
                .or_default()
                .push((owner, member_id));
        }
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
        if let Some(contributions) = self
            .finite_named_file_contributions
            .get_mut(&member_id.file_id)
        {
            contributions.retain(|(o, m)| o != owner || m != &member_id);
            if contributions.is_empty() {
                self.finite_named_file_contributions
                    .remove(&member_id.file_id);
            }
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
        crate::db_index::read_set::record_dynamic_field(owner, Some(field_name));
        self.field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn get_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> Vec<InFiled<TextRange>> {
        crate::db_index::read_set::record_dynamic_field(owner, None);
        self.wildcard_definitions
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }

    pub fn has_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> bool {
        crate::db_index::read_set::record_dynamic_field(owner, None);
        self.wildcard_definitions
            .get(owner)
            .is_some_and(|definitions| !definitions.is_empty())
    }

    /// Every owner's assigning-file records, for whole-index snapshots.
    pub fn iter_all_owner_fields(
        &self,
    ) -> impl Iterator<Item = (&DynamicFieldOwner, &HashMap<SmolStr, HashSet<FileId>>)> {
        self.owner_fields.iter()
    }

    /// Every owner's field-definition sites, for whole-index snapshots.
    pub fn iter_all_field_definitions(
        &self,
    ) -> impl Iterator<
        Item = (
            &DynamicFieldOwner,
            &HashMap<SmolStr, Vec<InFiled<TextRange>>>,
        ),
    > {
        self.field_definitions.iter()
    }

    /// Every unattributed-field record, for whole-index snapshots.
    pub fn iter_all_unattributed_fields(
        &self,
    ) -> impl Iterator<Item = (&SmolStr, &HashSet<FileId>)> {
        self.unattributed_fields.iter()
    }

    /// Every finite-named-member record, for whole-index snapshots.
    pub fn iter_all_finite_named_members(
        &self,
    ) -> impl Iterator<Item = (&DynamicFieldOwner, &HashSet<LuaMemberId>)> {
        self.finite_named_members.iter()
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
}

/// Drops one definition entry filed under `(owner, field_name)`, pruning the
/// emptied levels so no owner or field outlives its last definition.
fn remove_field_definition(
    definitions: &mut HashMap<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
    owner: &DynamicFieldOwner,
    field_name: &str,
    definition: &InFiled<TextRange>,
) {
    let Some(fields) = definitions.get_mut(owner) else {
        return;
    };
    let Some(entries) = fields.get_mut(field_name) else {
        return;
    };
    entries.retain(|existing| existing != definition);
    if entries.is_empty() {
        fields.remove(field_name);
    }
    if fields.is_empty() {
        definitions.remove(owner);
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
                HashMap::<(DynamicFieldOwner, SmolStr, TextRange), usize>::default(),
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
                HashMap::<(DynamicFieldOwner, TextRange), usize>::default(),
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
                        HashMap::<InFiled<TextRange>, usize>::default(),
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
        // Every primary map is pruned through the per-file contribution
        // indexes: only the owners the removed files contributed to are
        // visited, never the whole index. Each contribution corresponds to
        // exactly one definition entry, so draining a file's contributions
        // removes precisely its definitions; pruning is idempotent, and the
        // surviving entries keep their canonical order.
        for &file_id in file_ids {
            if let Some(contributions) = self.file_contributions.remove(&file_id) {
                for (owner, field_name, range) in contributions {
                    let definition = InFiled::new(file_id, range);
                    remove_field_definition(
                        &mut self.field_definitions,
                        &owner,
                        &field_name,
                        &definition,
                    );
                    remove_field_definition(
                        &mut self.direct_field_definitions,
                        &owner,
                        &field_name,
                        &definition,
                    );
                    // Draining a definition is a novel fact for the slot's
                    // readers, mirroring the signature index's removal
                    // tombstones.
                    self.note_dynamic_write(owner.clone(), Some(field_name.clone()));
                    if let Some(fields) = self.owner_fields.get_mut(&owner) {
                        if let Some(files) = fields.get_mut(field_name.as_str()) {
                            files.remove(&file_id);
                            if files.is_empty() {
                                fields.remove(field_name.as_str());
                            }
                        }
                        if fields.is_empty() {
                            self.owner_fields.remove(&owner);
                        }
                    }
                }
            }

            if let Some(contributions) = self.wildcard_file_contributions.remove(&file_id) {
                for (owner, range) in contributions {
                    let definition = InFiled::new(file_id, range);
                    if let Some(definitions) = self.wildcard_definitions.get_mut(&owner) {
                        definitions.retain(|existing| existing != &definition);
                        if definitions.is_empty() {
                            self.wildcard_definitions.remove(&owner);
                        }
                    }
                    self.note_dynamic_write(owner, None);
                }
            }

            if let Some(contributions) = self.finite_named_file_contributions.remove(&file_id) {
                for (owner, member_id) in contributions {
                    if let Some(members) = self.finite_named_members.get_mut(&owner) {
                        members.remove(&member_id);
                        if members.is_empty() {
                            self.finite_named_members.remove(&owner);
                        }
                    }
                }
            }

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
    }

    fn clear(&mut self) {
        self.owner_fields.clear();
        self.field_definitions.clear();
        self.direct_field_definitions.clear();
        self.finite_named_members.clear();
        self.file_contributions.clear();
        self.finite_named_file_contributions.clear();
        self.wildcard_definitions.clear();
        self.wildcard_file_contributions.clear();
        self.unattributed_fields.clear();
        self.unattributed_file_contributions.clear();
        self.dynamic_write_versions.clear();
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

    /// Removal is driven by the per-file contribution indexes, so state is
    /// built through the same `add_*` entry points production uses.
    #[test]
    fn remove_prunes_definitions_leaving_no_orphaned_entries() {
        let file_to_remove = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), file_to_remove, range(1, 2));
        index.add_field(owner.clone(), field.clone(), remaining_file, range(3, 4));
        index.add_wildcard_definition(owner.clone(), file_to_remove, range(5, 6));
        index.add_wildcard_definition(owner.clone(), remaining_file, range(7, 8));

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

    /// Only a novel definition may bump the dynamic epoch: re-adding an
    /// identical definition reports not-new and leaves the version alone, or
    /// the fixpoint filter degrades to a full requeue.
    #[test]
    fn readding_an_identical_definition_bumps_no_dynamic_epoch() {
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynVersionTest"));
        let field = SmolStr::new("value");
        let file_id = FileId::new(1);

        let mut index = DynamicFieldIndex::new();
        assert!(index.add_field(owner.clone(), field.clone(), file_id, range(1, 2)));
        let writes_after_insert = index.dynamic_writes();
        assert!(writes_after_insert > 0);

        assert!(!index.add_field(owner.clone(), field.clone(), file_id, range(1, 2)));
        assert_eq!(index.dynamic_writes(), writes_after_insert);

        let key = (owner.clone(), Some(field.clone()));
        assert_eq!(index.dynamic_write_version(&key), writes_after_insert);
    }

    /// The wildcard bucket versions apart from the named slots: a wildcard
    /// write must not requeue a reader of an unrelated named field.
    #[test]
    fn wildcard_and_named_slots_version_independently() {
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynVersionTest"));
        let file_id = FileId::new(1);

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), SmolStr::new("named"), file_id, range(1, 2));
        let named_version =
            index.dynamic_write_version(&(owner.clone(), Some(SmolStr::new("named"))));
        assert_eq!(
            index.dynamic_write_version(&(owner.clone(), None)),
            0,
            "a named write must not tick the wildcard bucket"
        );

        index.add_wildcard_definition(owner.clone(), file_id, range(3, 4));
        assert_eq!(
            index.dynamic_write_version(&(owner.clone(), Some(SmolStr::new("named")))),
            named_version,
            "a wildcard write must not tick an unrelated named slot"
        );
        assert!(
            index.dynamic_write_version(&(owner.clone(), None)) > 0,
            "the wildcard write must tick its own bucket"
        );
    }

    /// Draining a definition is a novel fact for the slot's readers.
    #[test]
    fn removing_a_definition_bumps_its_slot_version() {
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynVersionTest"));
        let field = SmolStr::new("value");
        let removed_file = FileId::new(1);
        let remaining_file = FileId::new(2);

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), removed_file, range(1, 2));
        index.add_field(owner.clone(), field.clone(), remaining_file, range(3, 4));
        index.add_wildcard_definition(owner.clone(), removed_file, range(5, 6));
        let key = (owner.clone(), Some(field.clone()));
        let wildcard_key: (DynamicFieldOwner, Option<SmolStr>) = (owner.clone(), None);
        let version_before = index.dynamic_write_version(&key);
        let wildcard_before = index.dynamic_write_version(&wildcard_key);

        index.remove(removed_file);

        assert!(
            index.dynamic_write_version(&key) > version_before,
            "draining a definition must invalidate the slot's readers"
        );
        assert!(
            index.dynamic_write_version(&wildcard_key) > wildcard_before,
            "draining a wildcard must invalidate the bucket's readers"
        );
    }

    /// Contributions-driven removal leaves exactly the state a fresh index
    /// built from the surviving files' inputs holds, finite-named members
    /// included.
    #[test]
    fn remove_matches_fresh_state_including_finite_members() {
        use glua_parser::LuaSyntaxKind;

        fn member(file_id: FileId, start: u32, end: u32) -> LuaMemberId {
            LuaMemberId::new(
                LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(start, end)),
                file_id,
            )
        }

        let removed_file = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");
        let removed_member = member(removed_file, 1, 2);
        let remaining_member = member(remaining_file, 3, 4);

        fn build(
            removed_file: FileId,
            remaining_file: FileId,
            removed_member: LuaMemberId,
            remaining_member: LuaMemberId,
            owner: &DynamicFieldOwner,
            field: &SmolStr,
            files: &[FileId],
        ) -> DynamicFieldIndex {
            let mut index = DynamicFieldIndex::new();
            if files.contains(&removed_file) {
                index.add_field(owner.clone(), field.clone(), removed_file, range(1, 2));
                index.add_wildcard_definition(owner.clone(), removed_file, range(5, 6));
                index.add_finite_named_member(owner.clone(), removed_member);
                index.add_unattributed_field(SmolStr::new("loose"), removed_file);
            }
            if files.contains(&remaining_file) {
                index.add_field(owner.clone(), field.clone(), remaining_file, range(3, 4));
                index.add_wildcard_definition(owner.clone(), remaining_file, range(7, 8));
                index.add_finite_named_member(owner.clone(), remaining_member);
                index.add_unattributed_field(SmolStr::new("loose"), remaining_file);
            }
            index
        }

        let mut index = build(
            removed_file,
            remaining_file,
            removed_member,
            remaining_member,
            &owner,
            &field,
            &[removed_file, remaining_file],
        );
        index.remove(removed_file);
        let fresh = build(
            removed_file,
            remaining_file,
            removed_member,
            remaining_member,
            &owner,
            &field,
            &[remaining_file],
        );

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
        assert_eq!(index.finite_named_members, fresh.finite_named_members);
        assert_eq!(
            index.finite_named_file_contributions,
            fresh.finite_named_file_contributions
        );
        assert_eq!(index.unattributed_fields, fresh.unattributed_fields);
        assert_eq!(
            index.unattributed_file_contributions,
            fresh.unattributed_file_contributions
        );
    }
}
