use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::db_index::cow_shards::CowShards;
use rowan::TextRange;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{
    DbIndex, FileId, GlobalId, InFiled, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaTypeDeclId,
    db_index::derivation::{self, FactKey, Fingerprint},
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
#[derive(Debug, Default, Clone)]
pub struct DynamicFieldIndex {
    /// owner → (field_name → set of files that assign this field)
    owner_fields: CowShards<DynamicFieldOwner, HashMap<SmolStr, HashSet<FileId>>>,
    /// owner → (field_name → assignment locations)
    field_definitions: CowShards<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
    /// Exact fields collected on their original owner, excluding inherited propagation.
    direct_field_definitions:
        CowShards<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
    /// Expression-key members whose complete string domain was resolved to finite names.
    finite_named_members: CowShards<DynamicFieldOwner, HashSet<LuaMemberId>>,
    /// file → list of (owner, field_name) pairs contributed by this file
    file_contributions: CowShards<FileId, Vec<(DynamicFieldOwner, SmolStr, TextRange)>>,
    /// file → finite-named members contributed by this file, so removal visits
    /// only the owners the removed files touched instead of retaining over
    /// every member in the workspace. Keyed by the member's own file, which is
    /// the file whose analysis recorded it.
    finite_named_file_contributions: CowShards<FileId, Vec<(DynamicFieldOwner, LuaMemberId)>>,
    /// owner → assignment locations for writes through non-literal keys.
    wildcard_definitions: CowShards<DynamicFieldOwner, Vec<InFiled<TextRange>>>,
    /// file → wildcard assignments contributed by this file.
    wildcard_file_contributions: CowShards<FileId, Vec<(DynamicFieldOwner, TextRange)>>,
    /// field_name → write sites (`file`, index-key `range`) whose
    /// `X.field_name = ...` receiver could not be resolved to any owner, so
    /// the write landed nowhere. Names here are known to be assigned
    /// *somewhere* at runtime even though no table in the index can claim
    /// them. Keyed by site, not by file: the settled tail attributes each
    /// site independently, so two same-name sites in one file each own their
    /// record.
    unattributed_fields: CowShards<SmolStr, HashSet<InFiled<TextRange>>>,
    /// file → unattributed (field_name, site range) pairs contributed by this
    /// file.
    unattributed_file_contributions: CowShards<FileId, Vec<(SmolStr, TextRange)>>,
    /// Multiset fingerprint of each owner's named slots, maintained by
    /// [`DynamicFieldIndex::settle_named`].
    owner_named_fingerprints: CowShards<DynamicFieldOwner, Fingerprint>,
}

/// One named slot's contribution to its owner's multiset fingerprint.
fn named_entry(field_name: &str, slot: Fingerprint) -> Fingerprint {
    if slot == derivation::ABSENT {
        0
    } else {
        derivation::fingerprint_of(&(field_name, slot))
    }
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

/// Files `definition` in canonical order unless `into` already holds it,
/// so a file's rebuild restores the list a cold build has. Returns whether
/// it was new.
fn insert_definition(into: &mut Vec<InFiled<TextRange>>, definition: InFiled<TextRange>) -> bool {
    let at = into.partition_point(|existing| {
        definition_sort_key(existing) < definition_sort_key(&definition)
    });
    if into.get(at) == Some(&definition) {
        return false;
    }
    into.insert(at, definition);
    true
}

fn merge_wildcard_definitions(into: &mut Vec<InFiled<TextRange>>, from: Vec<InFiled<TextRange>>) {
    for definition in from {
        insert_definition(into, definition);
    }
}

impl DynamicFieldIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns whether the definition site was new to the index — to the
    /// union of definitions or to this owner's direct set.
    pub fn add_field(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldFile(file_id));
        let old_slot = self.named_slot_fingerprint(&owner, &field_name);
        let definition = InFiled::new(file_id, range);
        let direct_definitions = self
            .direct_field_definitions
            .entry(owner.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default();
        let is_new_direct = insert_definition(direct_definitions, definition);
        let is_new_definition =
            self.add_field_inner(owner.clone(), field_name.clone(), file_id, range);
        self.settle_named(&owner, &field_name, old_slot);
        if is_new_direct && !is_new_definition {
            // The union already held the definition through propagation, so
            // `add_field_inner` filed nothing — but direct membership is a
            // novel fact for the slot's readers all the same.
            self.note_dynamic_write(owner, Some(field_name));
        }
        is_new_direct || is_new_definition
    }

    pub fn add_propagated_field(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldFile(file_id));
        let old_slot = self.named_slot_fingerprint(&owner, &field_name);
        let is_new_definition =
            self.add_field_inner(owner.clone(), field_name.clone(), file_id, range);
        self.settle_named(&owner, &field_name, old_slot);
        is_new_definition
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
        let is_new_definition = insert_definition(field_definitions, InFiled::new(file_id, range));

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
        derivation::note_write(|| FactKey::DynamicFieldFile(file_id));
        let definitions = self.wildcard_definitions.entry(owner.clone()).or_default();
        let definition = InFiled::new(file_id, range);
        let is_new_definition = insert_definition(definitions, definition);
        if is_new_definition {
            derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
            self.wildcard_file_contributions
                .entry(file_id)
                .or_default()
                .push((owner, range));
        }
        is_new_definition
    }

    /// Records a dynamic-field definition change.
    pub(crate) fn note_dynamic_write(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: Option<SmolStr>,
    ) {
        derivation::note_write(|| FactKey::DynamicField(owner, field_name));
    }

    /// Returns whether the unattributed record was new to the index.
    ///
    /// `range` is the site's index-key range; with `file_id` it names the
    /// write site, so two same-name sites in one file hold independent
    /// records.
    pub fn add_unattributed_field(
        &mut self,
        field_name: SmolStr,
        file_id: FileId,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldFile(file_id));
        let is_new = self
            .unattributed_fields
            .entry(field_name.clone())
            .or_default()
            .insert(InFiled::new(file_id, range));
        if is_new {
            derivation::note_write(|| FactKey::UnattributedField(field_name.clone()));
            self.unattributed_file_contributions
                .entry(file_id)
                .or_default()
                .push((field_name, range));
        }
        is_new
    }

    /// Replaces one write site's contribution set with a re-derived one.
    ///
    /// The settled tail answers from a more settled index than the pass that
    /// first collected the site, so the new owner set can differ in either
    /// direction: definitions the retry no longer produces are withdrawn,
    /// ones it now produces are added, and unchanged ones keep their version
    /// — re-filing an identical definition must not stale the readers that
    /// already saw it. Direct membership is reconciled against `direct`
    /// alone, independently of the direct∪propagated union, so a definition
    /// that stays filed but changes tier (a prefix that resolved to a
    /// subtype at collection and to its super at settle, or back) is moved
    /// between the direct and propagated sets rather than left stale.
    ///
    /// `unattributed` is the site's exact current unattributed name set:
    /// names the site filed a record for that are absent from it are
    /// retracted (whether the name attributed this round or vanished from
    /// the site's names entirely), and names present are (re-)filed.
    ///
    /// `range` is the site's index-key range; with `file_id` it names exactly
    /// the definitions this site produced, so sibling sites writing the same
    /// field onto the same owner from different positions are untouched.
    pub fn replace_site_contributions(
        &mut self,
        file_id: FileId,
        range: TextRange,
        direct: &[(DynamicFieldOwner, SmolStr)],
        propagated: &[(DynamicFieldOwner, SmolStr)],
        wildcards: &[DynamicFieldOwner],
        unattributed: &[SmolStr],
    ) -> bool {
        let definition = InFiled::new(file_id, range);
        let mut changed = false;

        let new_named: HashSet<(DynamicFieldOwner, SmolStr)> =
            direct.iter().chain(propagated.iter()).cloned().collect();
        let stale_named: Vec<(DynamicFieldOwner, SmolStr)> = self
            .file_contributions
            .get(&file_id)
            .map(|contributions| {
                contributions
                    .iter()
                    .filter(|(_, _, contribution_range)| *contribution_range == range)
                    .filter(|(owner, field_name, _)| {
                        !new_named.contains(&(owner.clone(), field_name.clone()))
                    })
                    .map(|(owner, field_name, _)| (owner.clone(), field_name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (owner, field_name) in stale_named {
            let old_slot = self.named_slot_fingerprint(&owner, &field_name);
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
            self.note_dynamic_write(owner.clone(), Some(field_name.clone()));
            // The file keeps its slot while another of its definitions on the
            // same owner/field survives; only the file's last definition for
            // the slot frees it.
            let has_remaining = self
                .field_definitions
                .get(&owner)
                .and_then(|fields| fields.get(field_name.as_str()))
                .is_some_and(|defs| defs.iter().any(|def| def.file_id == file_id));
            if !has_remaining && let Some(fields) = self.owner_fields.get_mut(&owner) {
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
            self.settle_named(&owner, &field_name, old_slot);
            if let Some(contributions) = self.file_contributions.get_mut(&file_id) {
                contributions.retain(|(o, n, contribution_range)| {
                    !(o == &owner && n == &field_name && *contribution_range == range)
                });
                if contributions.is_empty() {
                    self.file_contributions.remove(&file_id);
                }
            }
            changed = true;
        }
        // Direct membership reconciles against `direct` alone, not the
        // union: a definition that stays filed but is now propagated (or
        // vice versa) keeps its union slot while its tier changed. The
        // stale-union pass above already drained the site's contributions
        // absent from the new union, so the site's remaining
        // `file_contributions` entries are exactly the union members that
        // could still hold a direct record.
        let new_direct: HashSet<(DynamicFieldOwner, SmolStr)> = direct.iter().cloned().collect();
        let stale_direct: Vec<(DynamicFieldOwner, SmolStr)> = self
            .file_contributions
            .get(&file_id)
            .map(|contributions| {
                contributions
                    .iter()
                    .filter(|(_, _, contribution_range)| *contribution_range == range)
                    .filter(|(owner, field_name, _)| {
                        !new_direct.contains(&(owner.clone(), field_name.clone()))
                            && self
                                .direct_field_definitions
                                .get(owner)
                                .and_then(|fields| fields.get(field_name.as_str()))
                                .is_some_and(|defs| defs.contains(&definition))
                    })
                    .map(|(owner, field_name, _)| (owner.clone(), field_name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (owner, field_name) in stale_direct {
            let old_slot = self.named_slot_fingerprint(&owner, &field_name);
            remove_field_definition(
                &mut self.direct_field_definitions,
                &owner,
                &field_name,
                &definition,
            );
            self.settle_named(&owner, &field_name, old_slot);
            self.note_dynamic_write(owner, Some(field_name));
            changed = true;
        }
        for (owner, field_name) in direct {
            changed |= self.add_field(owner.clone(), field_name.clone(), file_id, range);
        }
        for (owner, field_name) in propagated {
            changed |= self.add_propagated_field(owner.clone(), field_name.clone(), file_id, range);
        }

        let new_wildcards: HashSet<DynamicFieldOwner> = wildcards.iter().cloned().collect();
        let stale_wildcards: Vec<DynamicFieldOwner> = self
            .wildcard_file_contributions
            .get(&file_id)
            .map(|contributions| {
                contributions
                    .iter()
                    .filter(|(_, contribution_range)| *contribution_range == range)
                    .map(|(owner, _)| owner.clone())
                    .filter(|owner| !new_wildcards.contains(owner))
                    .collect()
            })
            .unwrap_or_default();
        for owner in stale_wildcards {
            if let Some(definitions) = self.wildcard_definitions.get_mut(&owner) {
                definitions.retain(|existing| existing != &definition);
                if definitions.is_empty() {
                    self.wildcard_definitions.remove(&owner);
                }
            }
            derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
            if let Some(contributions) = self.wildcard_file_contributions.get_mut(&file_id) {
                contributions.retain(|(o, contribution_range)| {
                    !(o == &owner && *contribution_range == range)
                });
                if contributions.is_empty() {
                    self.wildcard_file_contributions.remove(&file_id);
                }
            }
            changed = true;
        }
        for owner in wildcards {
            changed |= self.add_wildcard_definition(owner.clone(), file_id, range);
        }
        // The site's unattributed records reconcile the same way: names it
        // filed a record for that are absent from the new set are retracted
        // (attributed this round, or no longer among the site's names at
        // all), and names present are (re-)filed. Sibling sites' records
        // under the same name are untouched — the record is keyed by site.
        let new_unattributed: HashSet<&SmolStr> = unattributed.iter().collect();
        let stale_unattributed: Vec<SmolStr> = self
            .unattributed_file_contributions
            .get(&file_id)
            .map(|contributions| {
                contributions
                    .iter()
                    .filter(|(_, site_range)| *site_range == range)
                    .filter(|(name, _)| !new_unattributed.contains(name))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        for field_name in stale_unattributed {
            derivation::note_write(|| FactKey::UnattributedField(field_name.clone()));
            let site = InFiled::new(file_id, range);
            if let Some(sites) = self.unattributed_fields.get_mut(field_name.as_str()) {
                sites.remove(&site);
                if sites.is_empty() {
                    self.unattributed_fields.remove(field_name.as_str());
                }
            }
            if let Some(names) = self.unattributed_file_contributions.get_mut(&file_id) {
                names.retain(|(name, site_range)| name != &field_name || *site_range != range);
                if names.is_empty() {
                    self.unattributed_file_contributions.remove(&file_id);
                }
            }
            changed = true;
        }
        for field_name in unattributed {
            changed |= self.add_unattributed_field(field_name.clone(), file_id, range);
        }
        changed
    }

    /// Replaces the finite-named-member marks one write site produced.
    ///
    /// A dynamic-key write marks `(owner, member_id)` when its key domain
    /// resolved finitely against the owner's type — the same settle-time
    /// partiality the named/wildcard reconcile covers applies here: owners
    /// the site's prefix no longer resolves to lose the mark, ones it now
    /// resolves to gain it. `member_id` is the site's own syntax identity,
    /// so every existing mark under it came from this site.
    pub fn replace_site_finite_members(
        &mut self,
        member_id: LuaMemberId,
        marked: &HashSet<DynamicFieldOwner>,
    ) -> bool {
        let file_id = member_id.file_id;
        let stale: Vec<DynamicFieldOwner> = self
            .finite_named_file_contributions
            .get(&file_id)
            .map(|contributions| {
                contributions
                    .iter()
                    .filter(|(_, contribution_member)| *contribution_member == member_id)
                    .map(|(owner, _)| owner.clone())
                    .filter(|owner| !marked.contains(owner))
                    .collect()
            })
            .unwrap_or_default();
        let mut changed = false;
        for owner in stale {
            self.remove_finite_named_member(&owner, member_id);
            changed = true;
        }
        for owner in marked {
            changed |= self.add_finite_named_member(owner.clone(), member_id);
        }
        changed
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
                self.note_whole_owner_write(owner);
                self.owner_named_fingerprints.remove(owner);
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
                    derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
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

        let remapped_owners: Vec<DynamicFieldOwner> = self
            .owner_fields
            .keys()
            .chain(self.field_definitions.keys())
            .chain(self.direct_field_definitions.keys())
            .chain(self.finite_named_members.keys())
            .chain(self.wildcard_definitions.keys())
            .filter_map(|owner| Some([owner.clone(), remap_owner(owner, map)?]))
            .flatten()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for owner in &remapped_owners {
            self.note_whole_owner_write(owner);
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
        for owner in &remapped_owners {
            self.recompute_owner_named(owner);
        }

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

    pub fn has_unattributed_field(&self, field_name: &str) -> bool {
        derivation::note_read(|| FactKey::UnattributedField(field_name.into()));
        self.unattributed_fields.contains_key(field_name)
    }

    pub fn has_field(&self, owner: &DynamicFieldOwner, field_name: &str) -> bool {
        derivation::note_dynamic_field_read(owner, Some(field_name));
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
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.owner_fields
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .is_some_and(|files| files.contains(&file_id))
    }

    pub fn get_fields(
        &self,
        owner: &DynamicFieldOwner,
    ) -> Option<&HashMap<SmolStr, HashSet<FileId>>> {
        derivation::note_dynamic_field_read(owner, None);
        self.owner_fields.get(owner)
    }

    /// Whether `owner` has a direct definition of `field_name`: read as that
    /// field alone, not the owner's whole field set.
    pub fn has_direct_field(&self, owner: &DynamicFieldOwner, field_name: &str) -> bool {
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.direct_field_definitions
            .get(owner)
            .is_some_and(|fields| fields.contains_key(field_name))
    }

    pub fn get_direct_fields(
        &self,
        owner: &DynamicFieldOwner,
    ) -> Option<&HashMap<SmolStr, Vec<InFiled<TextRange>>>> {
        derivation::note_dynamic_field_read(owner, None);
        self.direct_field_definitions.get(owner)
    }

    /// Returns whether the mark was new to the index.
    pub fn add_finite_named_member(
        &mut self,
        owner: DynamicFieldOwner,
        member_id: LuaMemberId,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldFile(member_id.file_id));
        let is_new = self
            .finite_named_members
            .entry(owner.clone())
            .or_default()
            .insert(member_id);
        if is_new {
            derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
            self.finite_named_file_contributions
                .entry(member_id.file_id)
                .or_default()
                .push((owner, member_id));
        }
        is_new
    }

    pub fn remove_finite_named_member(
        &mut self,
        owner: &DynamicFieldOwner,
        member_id: LuaMemberId,
    ) {
        derivation::note_write(|| FactKey::DynamicFieldFile(member_id.file_id));
        let Some(members) = self.finite_named_members.get_mut(owner) else {
            return;
        };
        derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
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
        derivation::note_read(|| FactKey::DynamicFieldFinite(owner.clone()));
        self.finite_named_members
            .get(owner)
            .is_some_and(|members| members.contains(&member_id))
    }

    pub fn owner_has_finite_named_members(&self, owner: &DynamicFieldOwner) -> bool {
        derivation::note_read(|| FactKey::DynamicFieldFinite(owner.clone()));
        self.finite_named_members
            .get(owner)
            .is_some_and(|members| !members.is_empty())
    }

    pub fn get_fields_in_file(&self, owner: &DynamicFieldOwner, file_id: FileId) -> Vec<&SmolStr> {
        derivation::note_dynamic_field_read(owner, None);
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
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn get_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> Vec<InFiled<TextRange>> {
        derivation::note_read(|| FactKey::DynamicFieldWildcards(owner.clone()));
        self.wildcard_definitions
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }

    pub fn has_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> bool {
        derivation::note_read(|| FactKey::DynamicFieldWildcards(owner.clone()));
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

    /// Every owner's direct field-definition sites, for whole-index
    /// snapshots. Direct membership is a distinct fact from the union
    /// `iter_all_field_definitions` reports — a site whose write moved
    /// between the direct and propagated tiers leaves the union unchanged.
    pub fn iter_all_direct_field_definitions(
        &self,
    ) -> impl Iterator<
        Item = (
            &DynamicFieldOwner,
            &HashMap<SmolStr, Vec<InFiled<TextRange>>>,
        ),
    > {
        self.direct_field_definitions.iter()
    }

    /// Every unattributed-field record, for whole-index snapshots.
    pub fn iter_all_unattributed_fields(
        &self,
    ) -> impl Iterator<Item = (&SmolStr, &HashSet<InFiled<TextRange>>)> {
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

    /// What a read of `DynamicField(owner, Some(field_name))` observes: the
    /// assigning files, the definitions and the direct definitions.
    fn named_slot_fingerprint(&self, owner: &DynamicFieldOwner, field_name: &str) -> Fingerprint {
        let files = self
            .owner_fields
            .get(owner)
            .and_then(|fields| fields.get(field_name));
        let definitions = self
            .field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name));
        let direct = self
            .direct_field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name));
        if files.is_none() && definitions.is_none() && direct.is_none() {
            return derivation::ABSENT;
        }
        let files = files.map(|files| {
            let mut files: Vec<FileId> = files.iter().copied().collect();
            files.sort_unstable();
            files
        });
        derivation::fingerprint_of(&(files, definitions, direct))
    }

    /// Notes a named slot's change since `old_slot` and folds it into the
    /// owner's multiset fingerprint.
    fn settle_named(&mut self, owner: &DynamicFieldOwner, field_name: &str, old_slot: Fingerprint) {
        let new_slot = self.named_slot_fingerprint(owner, field_name);
        if new_slot == old_slot {
            return;
        }
        derivation::note_write(|| FactKey::DynamicField(owner.clone(), Some(field_name.into())));
        derivation::note_write(|| FactKey::DynamicField(owner.clone(), None));
        let digest = self
            .owner_named_fingerprints
            .entry(owner.clone())
            .or_default();
        *digest = digest
            .wrapping_sub(named_entry(field_name, old_slot))
            .wrapping_add(named_entry(field_name, new_slot));
        if *digest == 0 {
            self.owner_named_fingerprints.remove(owner);
        }
    }

    fn owner_field_names(&self, owner: &DynamicFieldOwner) -> HashSet<SmolStr> {
        [&self.field_definitions, &self.direct_field_definitions]
            .into_iter()
            .filter_map(|map| map.get(owner))
            .flat_map(|fields| fields.keys().cloned())
            .chain(
                self.owner_fields
                    .get(owner)
                    .into_iter()
                    .flat_map(|fields| fields.keys().cloned()),
            )
            .collect()
    }

    /// Notes every key of `owner` ahead of a whole-owner rewrite.
    fn note_whole_owner_write(&self, owner: &DynamicFieldOwner) {
        if !derivation::recording() {
            return;
        }
        derivation::note_write(|| FactKey::DynamicField(owner.clone(), None));
        derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
        derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
        for field_name in self.owner_field_names(owner) {
            derivation::note_write(|| FactKey::DynamicField(owner.clone(), Some(field_name)));
        }
    }

    /// `owner`'s named slots summed afresh, which is what the incremental
    /// `owner_named_fingerprints` sum holds.
    fn owner_named_digest(&self, owner: &DynamicFieldOwner) -> Fingerprint {
        self.owner_field_names(owner)
            .iter()
            .map(|field_name| {
                named_entry(field_name, self.named_slot_fingerprint(owner, field_name))
            })
            .fold(0u64, u64::wrapping_add)
    }

    /// Rebuilds `owner`'s multiset fingerprint after a whole-owner rewrite.
    fn recompute_owner_named(&mut self, owner: &DynamicFieldOwner) {
        let digest = self.owner_named_digest(owner);
        if digest == 0 {
            self.owner_named_fingerprints.remove(owner);
        } else {
            self.owner_named_fingerprints.insert(owner.clone(), digest);
        }
    }

    /// `DynamicField(owner, None)` covers the owner-wide reads: wildcard
    /// definitions, finite-named members and every named slot.
    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::DynamicFieldFile(file_id) => {
                let entries = self.file_contributions.get(file_id).map(|entries| {
                    entries
                        .iter()
                        .map(|(owner, field_name, range)| {
                            (owner, field_name, InFiled::new(*file_id, *range))
                        })
                        .collect::<Vec<_>>()
                });
                derivation::fingerprint_opt(entries.as_ref())
            }
            FactKey::DynamicField(owner, Some(field_name)) => {
                self.named_slot_fingerprint(owner, field_name)
            }
            FactKey::DynamicField(owner, None) => {
                let named = if derivation::mapping_back() {
                    Some(self.owner_named_digest(owner)).filter(|digest| *digest != 0)
                } else {
                    self.owner_named_fingerprints.get(owner).copied()
                };
                derivation::fingerprint_opt(named.as_ref())
            }
            FactKey::DynamicFieldWildcards(owner) => {
                derivation::fingerprint_opt(self.wildcard_definitions.get(owner))
            }
            FactKey::DynamicFieldFinite(owner) => {
                self.finite_named_members
                    .get(owner)
                    .map_or(derivation::ABSENT, |members| {
                        derivation::fingerprint_of(
                            &members
                                .iter()
                                .map(derivation::fingerprint_of)
                                .fold(0u64, u64::wrapping_add),
                        )
                    })
            }
            FactKey::UnattributedField(field_name) => {
                if self.unattributed_fields.contains_key(field_name) {
                    derivation::fingerprint_of(&true)
                } else {
                    derivation::ABSENT
                }
            }
            _ => {
                debug_assert!(false, "{key:?} is not a dynamic-field fact");
                derivation::ABSENT
            }
        }
    }
}

/// Drops one definition entry filed under `(owner, field_name)`, pruning the
/// emptied levels so no owner or field outlives its last definition.
fn remove_field_definition(
    definitions: &mut CowShards<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
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
    contributions: &CowShards<FileId, Vec<(DynamicFieldOwner, SmolStr, TextRange)>>,
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
    contributions: &CowShards<FileId, Vec<(DynamicFieldOwner, TextRange)>>,
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
    definitions: &CowShards<DynamicFieldOwner, HashMap<SmolStr, Vec<InFiled<TextRange>>>>,
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
                    let old_slot = self.named_slot_fingerprint(&owner, &field_name);
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
                    self.settle_named(&owner, &field_name, old_slot);
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
                    derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
                }
            }

            if let Some(contributions) = self.finite_named_file_contributions.remove(&file_id) {
                for (owner, member_id) in contributions {
                    derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
                    if let Some(members) = self.finite_named_members.get_mut(&owner) {
                        members.remove(&member_id);
                        if members.is_empty() {
                            self.finite_named_members.remove(&owner);
                        }
                    }
                }
            }

            if let Some(contributions) = self.unattributed_file_contributions.remove(&file_id) {
                for (field_name, range) in contributions {
                    derivation::note_write(|| FactKey::UnattributedField(field_name.clone()));
                    let site = InFiled::new(file_id, range);
                    if let Some(sites) = self.unattributed_fields.get_mut(&field_name) {
                        sites.remove(&site);
                        if sites.is_empty() {
                            self.unattributed_fields.remove(&field_name);
                        }
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            let owners: HashSet<&DynamicFieldOwner> = self
                .owner_fields
                .keys()
                .chain(self.field_definitions.keys())
                .chain(self.direct_field_definitions.keys())
                .chain(self.finite_named_members.keys())
                .chain(self.wildcard_definitions.keys())
                .collect();
            for owner in owners {
                self.note_whole_owner_write(owner);
            }
            for field_name in self.unattributed_fields.keys() {
                derivation::note_write(|| FactKey::UnattributedField(field_name.clone()));
            }
        }
        self.owner_named_fingerprints.clear();
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
                index.add_unattributed_field(SmolStr::new("loose"), removed_file, range(9, 10));
            }
            if files.contains(&remaining_file) {
                index.add_field(owner.clone(), field.clone(), remaining_file, range(3, 4));
                index.add_wildcard_definition(owner.clone(), remaining_file, range(7, 8));
                index.add_finite_named_member(owner.clone(), remaining_member);
                index.add_unattributed_field(SmolStr::new("loose"), remaining_file, range(11, 12));
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

    /// The settled tail replaces a site's contribution set rather than adding
    /// beside it: definitions the re-derivation no longer produces are
    /// withdrawn, and a sibling site writing the same field from another
    /// position is untouched.
    #[test]
    fn replace_site_contributions_withdraws_only_the_sites_own_definitions() {
        let site_file = FileId::new(1);
        let owner_a = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerA"));
        let owner_b = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerB"));
        let site_range = range(1, 2);
        let sibling_range = range(5, 6);

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner_a.clone(), SmolStr::new("f"), site_file, site_range);
        index.add_field(owner_a.clone(), SmolStr::new("f"), site_file, sibling_range);
        index.add_field(owner_a.clone(), SmolStr::new("g"), site_file, site_range);

        let changed = index.replace_site_contributions(
            site_file,
            site_range,
            &[(owner_b.clone(), SmolStr::new("f"))],
            &[],
            &[],
            &[],
        );

        assert!(changed);
        // The site moved `f` to `owner_b` and no longer produces `g`; the
        // sibling site's `f` definition on `owner_a` survives.
        assert_eq!(
            index.field_definitions(&owner_a, "f"),
            [InFiled::new(site_file, sibling_range)]
        );
        assert_eq!(index.field_definitions(&owner_b, "f").len(), 1);
        assert!(!index.has_field(&owner_a, "g"));
        // `owner_a.f` still counts this file through the sibling site's
        // surviving definition; only `g` released the file's slot.
        assert_eq!(
            index.get_fields_in_file(&owner_a, site_file),
            vec![&SmolStr::new("f")]
        );
        assert_eq!(
            index.file_contributions[&site_file]
                .iter()
                .filter(|(_, _, r)| *r == site_range)
                .count(),
            1
        );
        // A second identical replacement is a no-op: re-filing what the site
        // already produced must not churn.
        assert!(!index.replace_site_contributions(
            site_file,
            site_range,
            &[(owner_b.clone(), SmolStr::new("f"))],
            &[],
            &[],
            &[],
        ));
    }

    /// Propagated contributions filed beside a site's direct ones reconcile
    /// the same way: a stale propagated owner is withdrawn while a kept
    /// direct definition stands.
    #[test]
    fn replace_site_contributions_withdraws_stale_propagated_definitions() {
        let site_file = FileId::new(1);
        let owner_direct = DynamicFieldOwner::Type(LuaTypeDeclId::global("Direct"));
        let owner_super = DynamicFieldOwner::Type(LuaTypeDeclId::global("Super"));
        let site_range = range(1, 2);

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            owner_direct.clone(),
            SmolStr::new("f"),
            site_file,
            site_range,
        );
        index.add_propagated_field(
            owner_super.clone(),
            SmolStr::new("f"),
            site_file,
            site_range,
        );

        // The re-derived site keeps the direct definition but produces no
        // propagated one this round.
        let changed = index.replace_site_contributions(
            site_file,
            site_range,
            &[(owner_direct.clone(), SmolStr::new("f"))],
            &[],
            &[],
            &[],
        );

        assert!(changed);
        assert_eq!(index.field_definitions(&owner_direct, "f").len(), 1);
        assert!(!index.has_field(&owner_super, "f"));
        // The stale propagated entry is out of the file's contributions.
        assert!(
            index.file_contributions[&site_file]
                .iter()
                .all(|(owner, _, _)| *owner != owner_super)
        );
    }

    /// Wildcard definitions reconcile under the same site identity: owners
    /// the re-derivation no longer covers lose the site's wildcard.
    #[test]
    fn replace_site_contributions_withdraws_stale_wildcards() {
        let site_file = FileId::new(1);
        let owner_a = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerA"));
        let owner_b = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerB"));
        let site_range = range(1, 2);

        let mut index = DynamicFieldIndex::new();
        index.add_wildcard_definition(owner_a.clone(), site_file, site_range);

        let changed = index.replace_site_contributions(
            site_file,
            site_range,
            &[],
            &[],
            std::slice::from_ref(&owner_b),
            &[],
        );

        assert!(changed);
        assert!(index.get_wildcard_definitions(&owner_a).is_empty());
        assert_eq!(index.get_wildcard_definitions(&owner_b).len(), 1);
        assert!(
            index.wildcard_file_contributions[&site_file]
                .iter()
                .all(|(owner, _)| *owner != owner_a)
        );
    }

    /// Finite-named marks reconcile against the marks the site files this
    /// round: every mark under the member id is that site's own, so stale
    /// ones withdraw and new ones land.
    #[test]
    fn replace_site_finite_members_reconciles_the_sites_marks() {
        use glua_parser::LuaSyntaxKind;

        let site_file = FileId::new(1);
        let member_id = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(1, 2)),
            site_file,
        );
        let other_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(9, 10)),
            site_file,
        );
        let owner_a = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerA"));
        let owner_b = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerB"));

        let mut index = DynamicFieldIndex::new();
        index.add_finite_named_member(owner_a.clone(), member_id);
        index.add_finite_named_member(owner_a.clone(), other_member);

        let marked: HashSet<DynamicFieldOwner> = [owner_b.clone()].into_iter().collect();
        let changed = index.replace_site_finite_members(member_id, &marked);

        assert!(changed);
        assert!(!index.member_has_finite_named_definition(&owner_a, member_id));
        assert!(index.member_has_finite_named_definition(&owner_b, member_id));
        // The other member's mark on the same owner is untouched.
        assert!(index.member_has_finite_named_definition(&owner_a, other_member));
        assert!(!index.replace_site_finite_members(member_id, &marked));
    }

    /// Unattributed records are owned by the write site, not by
    /// `(field_name, file)`: two same-name sites in one file hold independent
    /// records, so a settled retry attributing one site withdraws only that
    /// site's record while the sibling's survives — and removing the file
    /// drains both.
    #[test]
    fn unattributed_records_are_site_owned() {
        let file_id = FileId::new(1);
        let site_range = range(1, 2);
        let sibling_range = range(5, 6);

        let mut index = DynamicFieldIndex::new();
        index.add_unattributed_field(SmolStr::new("shared"), file_id, site_range);
        index.add_unattributed_field(SmolStr::new("shared"), file_id, sibling_range);
        assert!(index.has_unattributed_field("shared"));

        // Attributing one site must not take the sibling's record with it.
        assert!(index.replace_site_contributions(file_id, site_range, &[], &[], &[], &[],));
        assert!(index.has_unattributed_field("shared"));
        assert_eq!(
            index
                .unattributed_file_contributions
                .get(&file_id)
                .expect("expected the sibling's contribution to survive"),
            &vec![(SmolStr::new("shared"), sibling_range)]
        );

        // Removing the file drains the surviving site's record.
        index.remove(file_id);
        assert!(!index.has_unattributed_field("shared"));
        assert!(!index.unattributed_file_contributions.contains_key(&file_id));
    }

    /// Direct membership is a separate fact from the union of definitions:
    /// a site whose write moves between the direct and propagated tiers
    /// keeps its union slot, so reconciling only the union would leave the
    /// direct set stale in both directions.
    #[test]
    fn replace_site_contributions_reconciles_direct_membership_independently() {
        let site_file = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerA"));
        let field = SmolStr::new("f");
        let site_range = range(1, 2);
        let sibling_range = range(5, 6);
        let site_def = InFiled::new(site_file, site_range);
        let sibling_def = InFiled::new(site_file, sibling_range);

        let mut index = DynamicFieldIndex::new();
        index.add_field(owner.clone(), field.clone(), site_file, site_range);
        index.add_field(owner.clone(), field.clone(), site_file, sibling_range);

        // direct → propagated: the union is unchanged, but the site's
        // definition leaves the direct set. The sibling's direct record
        // survives — it is not this site's to withdraw.
        assert!(index.replace_site_contributions(
            site_file,
            site_range,
            &[],
            &[(owner.clone(), field.clone())],
            &[],
            &[],
        ));
        assert_eq!(
            index.field_definitions(&owner, &field),
            [site_def.clone(), sibling_def.clone()],
            "the union keeps both definitions"
        );
        assert_eq!(
            index
                .get_direct_fields(&owner)
                .and_then(|fields| fields.get(field.as_str()))
                .map(Vec::as_slice),
            Some(&[sibling_def.clone()][..]),
            "the site's definition left the direct set; the sibling's stands"
        );

        // propagated → direct: the union is still unchanged, so only the
        // direct-set fix reports the move.
        assert!(index.replace_site_contributions(
            site_file,
            site_range,
            &[(owner.clone(), field.clone())],
            &[],
            &[],
            &[],
        ));
        let mut direct = index
            .get_direct_fields(&owner)
            .and_then(|fields| fields.get(field.as_str()))
            .cloned()
            .unwrap_or_default();
        direct.sort_by_key(definition_sort_key);
        assert_eq!(direct, [site_def.clone(), sibling_def.clone()]);

        // An identical replacement is a no-op.
        assert!(!index.replace_site_contributions(
            site_file,
            site_range,
            &[(owner.clone(), field.clone())],
            &[],
            &[],
            &[],
        ));
    }

    /// The site's unattributed set is exact: a name the site filed a record
    /// for that is absent from the re-derived set is retracted — whether it
    /// attributed this round or vanished from the site's names entirely —
    /// while a sibling site's record under the same name survives.
    #[test]
    fn replace_site_contributions_retracts_absent_unattributed_names() {
        let site_file = FileId::new(1);
        let site_range = range(1, 2);
        let sibling_range = range(5, 6);

        let mut index = DynamicFieldIndex::new();
        index.add_unattributed_field(SmolStr::new("kept"), site_file, site_range);
        index.add_unattributed_field(SmolStr::new("gone"), site_file, site_range);
        index.add_unattributed_field(SmolStr::new("gone"), site_file, sibling_range);

        // The re-derived site still fails to attribute `kept`; `gone` is no
        // longer among its names at all.
        assert!(index.replace_site_contributions(
            site_file,
            site_range,
            &[],
            &[],
            &[],
            &[SmolStr::new("kept")],
        ));
        assert!(index.has_unattributed_field("kept"));
        assert!(
            index.has_unattributed_field("gone"),
            "the sibling site's record survives the site's own retraction"
        );
        assert_eq!(
            index
                .unattributed_file_contributions
                .get(&site_file)
                .expect("expected the site's surviving contribution"),
            &vec![
                (SmolStr::new("kept"), site_range),
                (SmolStr::new("gone"), sibling_range),
            ]
        );

        // An identical replacement is a no-op.
        assert!(!index.replace_site_contributions(
            site_file,
            site_range,
            &[],
            &[],
            &[],
            &[SmolStr::new("kept")],
        ));
    }
}
