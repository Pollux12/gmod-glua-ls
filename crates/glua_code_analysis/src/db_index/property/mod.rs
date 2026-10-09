mod decl_feature;
#[allow(clippy::module_inception)]
mod property;

use crate::db_index::cow_shards::{CowCell, CowShards};
use std::collections::BTreeMap;
use std::sync::Arc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

pub use decl_feature::{DeclFeatureFlag, PropertyDeclFeature};
use glua_parser::{LuaAstNode, LuaDocTagField, LuaDocType, LuaVersionCondition, VisibilityKind};
pub use property::LuaCommonProperty;
pub use property::{LuaDeprecated, LuaExport, LuaExportScope, LuaPropertyId};
use rowan::TextRange;
use smol_str::SmolStr;

use crate::LuaDocDefaultValue;
use crate::db_index::derivation::{self, FactKey, Fingerprint};
pub use crate::db_index::property::property::LuaAttributeUse;
use crate::{DbIndex, FileId, LuaDeclId, LuaMember, LuaSignatureId};

use super::{LuaSemanticDeclId, traits::LuaIndex};

/// An inferred string default from a self-coalescing `x = x or "literal"` pattern.
#[derive(Debug, Clone, Hash, PartialEq)]
pub struct LuaInferredStringDefault {
    /// The string value (e.g. "DScrollPanel").
    pub value: SmolStr,
    /// The source range of the assignment statement that produced this default.
    pub source_range: TextRange,
}

/// Merge order of one file's contribution to a shared property: annotation and library
/// workspaces before the main workspace, then lowest normalized path. File ids are not stable
/// between a cold build and an incremental session, so they are only the final tie-break.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PropertyContributorKey {
    workspace_rank: u8,
    normalized_path: Arc<str>,
    file_id: u32,
}

/// One file's contribution to a property, normalized so the sole-contributor
/// fast path and the multi-contributor map read as the same record shape.
/// Borrowed throughout: the payload is the stored contribution (for a sole
/// contributor the merged value *is* the contribution).
#[derive(Debug)]
pub struct LuaPropertyContribution<'a> {
    /// The documented symbol the contribution is filed under. Owner aliases
    /// sharing one property each get their own record.
    pub owner_id: &'a LuaSemanticDeclId,
    /// The file that wrote the contribution.
    pub file_id: FileId,
    /// Merge precedence inputs: workspace rank, then normalized path.
    pub workspace_rank: u8,
    pub normalized_path: &'a str,
    /// The contribution payload.
    pub property: &'a LuaCommonProperty,
}

#[derive(Debug, Clone)]
pub struct LuaPropertyIndex {
    /// Merged view, read by `get_property`. For the common single-contributor property this is
    /// the contribution itself and nothing else is stored.
    properties: CowCell<HashMap<LuaPropertyId, LuaCommonProperty>>,
    property_owners_map: CowCell<HashMap<LuaSemanticDeclId, LuaPropertyId>>,
    /// `property_owners_map` inverted: the owners sharing each property.
    owners_by_property: CowCell<HashMap<LuaPropertyId, Vec<LuaSemanticDeclId>>>,
    signature_owner_by_property: CowCell<HashMap<LuaPropertyId, LuaSignatureId>>,

    /// `TypeDecl` properties are keyed by class name alone, so several files can document one
    /// class. Only those properties get per-file contributions; everything else stays sole.
    sole_contributor: CowCell<HashMap<LuaPropertyId, FileId>>,
    contributions:
        CowCell<HashMap<LuaPropertyId, BTreeMap<PropertyContributorKey, LuaCommonProperty>>>,
    file_merge_order: CowCell<HashMap<FileId, (u8, Arc<str>)>>,

    id_count: u32,
    in_filed_owner: CowCell<HashMap<FileId, HashSet<LuaSemanticDeclId>>>,

    /// Inferred string defaults from `x = x or "literal"` patterns.
    /// Keyed by `LuaDeclId`; one decl can have multiple candidates (e.g.
    /// multiple self-coalescing assignments in different scopes of the same
    /// function).  File ownership is tracked for cleanup on reindex.
    inferred_string_defaults: CowShards<LuaDeclId, Vec<LuaInferredStringDefault>>,
    inferred_string_defaults_file_owners: CowShards<FileId, HashSet<LuaDeclId>>,
}

impl Default for LuaPropertyIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaPropertyIndex {
    pub fn new() -> Self {
        Self {
            id_count: 0,
            in_filed_owner: Default::default(),
            properties: Default::default(),
            property_owners_map: Default::default(),
            owners_by_property: Default::default(),
            signature_owner_by_property: Default::default(),
            sole_contributor: Default::default(),
            contributions: Default::default(),
            file_merge_order: Default::default(),
            inferred_string_defaults: Default::default(),
            inferred_string_defaults_file_owners: Default::default(),
        }
    }

    fn get_or_create_property(
        &mut self,
        owner_id: LuaSemanticDeclId,
    ) -> Option<(&mut LuaCommonProperty, LuaPropertyId)> {
        if let Some(property_id) = self.property_owners_map.get(&owner_id) {
            self.properties
                .get_mut(property_id)
                .map(|prop| (prop, *property_id))
        } else {
            let id = LuaPropertyId::new(self.id_count);
            self.id_count += 1;
            self.map_owner(owner_id.clone(), id);
            if let LuaSemanticDeclId::Signature(signature_id) = owner_id {
                self.signature_owner_by_property.insert(id, signature_id);
            }
            self.properties.insert(id, LuaCommonProperty::new());
            self.properties.get_mut(&id).map(|prop| (prop, id))
        }
    }

    /// Records the merge order of a file's documentation contributions. Called once per file
    /// before its doc tags are analyzed; a file that never registers sorts last.
    pub fn set_file_merge_order(&mut self, file_id: FileId, workspace_rank: u8, path: Arc<str>) {
        self.file_merge_order
            .insert(file_id, (workspace_rank, path));
    }

    /// Orders the documentation contributions of each of `order_keys`' files
    /// by its new order key, republishing every merged view they are part of.
    pub(crate) fn rekey_files(&mut self, order_keys: &[(FileId, Arc<str>)]) {
        for (file_id, order_key) in order_keys {
            let Some((workspace_rank, held)) = self.file_merge_order.get(file_id).cloned() else {
                continue;
            };
            if held == *order_key {
                continue;
            }
            let before = self.contributor_key(*file_id);
            self.file_merge_order
                .insert(*file_id, (workspace_rank, order_key.clone()));
            let after = self.contributor_key(*file_id);
            let owners = self
                .in_filed_owner
                .get(file_id)
                .map(|owners| owners.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for owner_id in owners {
                let Some(property_id) = self.property_owners_map.get(&owner_id).copied() else {
                    continue;
                };
                let Some(contributions) = self.contributions.get_mut(&property_id) else {
                    continue;
                };
                let Some(contribution) = contributions.remove(&before) else {
                    continue;
                };
                contributions.insert(after.clone(), contribution);
                self.rebuild_merged(property_id);
                self.note_property_write(property_id);
            }
        }
    }

    fn contributor_key(&self, file_id: FileId) -> PropertyContributorKey {
        let (workspace_rank, normalized_path) = self
            .file_merge_order
            .get(&file_id)
            .cloned()
            .unwrap_or((u8::MAX, Arc::from("")));
        PropertyContributorKey {
            workspace_rank,
            normalized_path,
            file_id: file_id.id,
        }
    }

    /// Applies one file's write to that file's own contribution, then republishes the merged view.
    fn contribute(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        write: impl FnOnce(&mut LuaCommonProperty),
    ) -> Option<()> {
        let (_, property_id) = self.get_or_create_property(owner_id.clone())?;
        self.note_property_write(property_id);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        if !self.contributions.contains_key(&property_id) {
            match self.sole_contributor.get(&property_id).copied() {
                None => {
                    self.sole_contributor.insert(property_id, file_id);
                    write(self.properties.get_mut(&property_id)?);
                    return Some(());
                }
                Some(sole) if sole == file_id => {
                    write(self.properties.get_mut(&property_id)?);
                    return Some(());
                }
                Some(sole) => {
                    // Second writer: the merged value so far is the first file's contribution.
                    let sole_value = self
                        .properties
                        .get(&property_id)
                        .cloned()
                        .unwrap_or_default();
                    let sole_key = self.contributor_key(sole);
                    self.contributions
                        .entry(property_id)
                        .or_default()
                        .insert(sole_key, sole_value);
                    self.sole_contributor.remove(&property_id);
                }
            }
        }

        let key = self.contributor_key(file_id);
        write(
            self.contributions
                .get_mut(&property_id)?
                .entry(key)
                .or_default(),
        );
        self.rebuild_merged(property_id);

        Some(())
    }

    fn rebuild_merged(&mut self, property_id: LuaPropertyId) {
        let Some(contributions) = self.contributions.get(&property_id) else {
            return;
        };
        let mut merged = LuaCommonProperty::new();
        for contribution in contributions.values() {
            merged.merge_from(contribution);
        }
        self.properties.insert(property_id, merged);
    }

    pub fn add_owner_map(
        &mut self,
        source_owner_id: LuaSemanticDeclId,
        same_property_owner_id: LuaSemanticDeclId,
        file_id: FileId,
    ) -> Option<()> {
        let (_, property_id) = self.get_or_create_property(source_owner_id.clone())?;
        derivation::note_write(|| FactKey::Property(same_property_owner_id.clone()));
        self.note_property_write(property_id);
        // Owner aliases are always within one file, so this file owns the property outright
        // unless a doc contribution has already promoted it.
        self.sole_contributor.entry(property_id).or_insert(file_id);
        self.map_owner(same_property_owner_id.clone(), property_id);
        if let LuaSemanticDeclId::Signature(signature_id) = &source_owner_id {
            self.signature_owner_by_property
                .insert(property_id, *signature_id);
        }
        if let LuaSemanticDeclId::Signature(signature_id) = &same_property_owner_id {
            self.signature_owner_by_property
                .insert(property_id, *signature_id);
        }

        let file_owners = self.in_filed_owner.entry(file_id).or_default();
        file_owners.insert(source_owner_id);
        // Also track the alias so it is removed from property_owners_map when the
        // file is invalidated/reparsed.  Without this, the alias key accumulates as
        // a dead entry pointing to a deleted LuaPropertyId across LS session reparsing.
        file_owners.insert(same_property_owner_id);

        Some(())
    }

    pub fn add_description(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        description: String,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_description(description)
        })
    }

    pub fn add_visibility(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        visibility: VisibilityKind,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.visibility = visibility;
        })
    }

    pub fn add_source(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        source: String,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_source(source)
        })
    }

    pub fn add_default_value(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        default_value: LuaDocDefaultValue,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_default_value(default_value)
        })
    }

    pub fn add_deprecated(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        message: Option<String>,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_deprecated(message)
        })
    }

    pub fn add_version(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        version_conds: Vec<LuaVersionCondition>,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_version_cond(version_conds)
        })
    }

    pub fn add_see(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        mut see_content: String,
        see_description: Option<String>,
    ) -> Option<()> {
        if let Some(see_description) = see_description {
            see_content += " ";
            see_content += &see_description;
        }

        self.contribute(file_id, owner_id, |property| {
            property.add_extra_tag("see".into(), see_content)
        })
    }

    pub fn add_other(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        tag_name: String,
        other_content: String,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_tag(tag_name, other_content)
        })
    }

    pub fn add_export(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        export: property::LuaExport,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_extra_export(export)
        })
    }

    pub fn add_decl_feature(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        feature: PropertyDeclFeature,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_decl_feature(feature)
        })
    }

    pub fn add_attribute_use(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        attribute_use: LuaAttributeUse,
    ) -> Option<()> {
        self.contribute(file_id, owner_id, |property| {
            property.add_attribute_use(attribute_use)
        })
    }

    /// Every documented symbol this file declares, with its property.
    pub fn properties_in_file(
        &self,
        file_id: FileId,
    ) -> Vec<(&LuaSemanticDeclId, &LuaCommonProperty)> {
        let Some(owners) = self.in_filed_owner.get(&file_id) else {
            return Vec::new();
        };
        owners
            .iter()
            .filter_map(|owner| Some((owner, self.get_property(owner)?)))
            .collect()
    }

    pub fn get_property(&self, owner_id: &LuaSemanticDeclId) -> Option<&LuaCommonProperty> {
        derivation::note_read(|| FactKey::Property(owner_id.clone()));
        self.property_owners_map
            .get(owner_id)
            .and_then(|id| self.properties.get(id))
    }

    pub fn get_signature_owner(
        &self,
        owner_id: &LuaSemanticDeclId,
    ) -> Option<crate::LuaSignatureId> {
        derivation::note_read(|| FactKey::Property(owner_id.clone()));
        let property_id = self.property_owners_map.get(owner_id)?;
        self.signature_owner_by_property.get(property_id).copied()
    }

    pub fn iter_owner_properties(
        &self,
    ) -> impl Iterator<Item = (&LuaSemanticDeclId, &LuaCommonProperty)> {
        self.property_owners_map
            .iter()
            .filter_map(|(owner_id, property_id)| {
                self.properties
                    .get(property_id)
                    .map(|property| (owner_id, property))
            })
    }

    /// Every source-owned contribution, normalized across the sole-contributor
    /// fast path and the multi-contributor map into the same record shape.
    /// Internal property ids and which storage slot a contribution landed in
    /// are deliberately not exposed — only owner, contributing file, merge
    /// precedence and payload are semantic.
    pub fn iter_source_contributions(&self) -> impl Iterator<Item = LuaPropertyContribution<'_>> {
        self.property_owners_map
            .iter()
            .flat_map(move |(owner_id, property_id)| {
                let sole = self
                    .sole_contributor
                    .get(property_id)
                    .into_iter()
                    .map(move |file_id| {
                        let (workspace_rank, normalized_path) = self
                            .file_merge_order
                            .get(file_id)
                            .map(|(rank, path)| (*rank, path.as_ref()))
                            .unwrap_or((u8::MAX, ""));
                        LuaPropertyContribution {
                            owner_id,
                            file_id: *file_id,
                            workspace_rank,
                            normalized_path,
                            property: self
                                .properties
                                .get(property_id)
                                .expect("sole contributor without merged value"),
                        }
                    });
                let multi =
                    self.contributions
                        .get(property_id)
                        .into_iter()
                        .flat_map(move |by_key| {
                            by_key
                                .iter()
                                .map(move |(key, property)| LuaPropertyContribution {
                                    owner_id,
                                    file_id: FileId { id: key.file_id },
                                    workspace_rank: key.workspace_rank,
                                    normalized_path: key.normalized_path.as_ref(),
                                    property,
                                })
                        });
                sole.chain(multi)
            })
    }

    /// Register an inferred string default from `x = x or "literal"`.
    pub fn add_inferred_string_default(
        &mut self,
        file_id: FileId,
        decl_id: LuaDeclId,
        value: SmolStr,
        source_range: TextRange,
    ) {
        let entry = LuaInferredStringDefault {
            value,
            source_range,
        };
        derivation::note_write(|| FactKey::DeclDefault(decl_id));
        self.inferred_string_defaults
            .entry(decl_id)
            .or_default()
            .push(entry);
        self.inferred_string_defaults_file_owners
            .entry(file_id)
            .or_default()
            .insert(decl_id);
    }

    /// Drops `decl_id`'s inferred string defaults before the derivations that
    /// inferred them run again.
    pub(crate) fn reset_inferred_string_defaults(&mut self, decl_id: &LuaDeclId) {
        if self.inferred_string_defaults.remove(decl_id).is_some() {
            derivation::note_write(|| FactKey::DeclDefault(*decl_id));
        }
    }

    /// Get all inferred string default candidates for a declaration.
    pub fn get_inferred_string_defaults(
        &self,
        decl_id: &LuaDeclId,
    ) -> Option<&[LuaInferredStringDefault]> {
        derivation::note_read(|| FactKey::DeclDefault(*decl_id));
        self.inferred_string_defaults
            .get(decl_id)
            .map(|v| v.as_slice())
    }

    /// Notes a write to every owner mapped to `property_id`: aliases share one
    /// property, so a write through any of them changes what each one reads.
    fn note_property_write(&self, property_id: LuaPropertyId) {
        if !derivation::recording() {
            return;
        }
        for owner_id in self
            .owners_by_property
            .get(&property_id)
            .into_iter()
            .flatten()
        {
            derivation::note_write(|| FactKey::Property(owner_id.clone()));
        }
    }

    fn map_owner(&mut self, owner_id: LuaSemanticDeclId, property_id: LuaPropertyId) {
        if let Some(previous) = self
            .property_owners_map
            .insert(owner_id.clone(), property_id)
        {
            if previous == property_id {
                return;
            }
            self.forget_owner(previous, &owner_id);
        }
        self.owners_by_property
            .entry(property_id)
            .or_default()
            .push(owner_id);
    }

    fn unmap_owner(&mut self, owner_id: &LuaSemanticDeclId) {
        if let Some(property_id) = self.property_owners_map.remove(owner_id) {
            self.forget_owner(property_id, owner_id);
        }
    }

    fn forget_owner(&mut self, property_id: LuaPropertyId, owner_id: &LuaSemanticDeclId) {
        if let Some(owners) = self.owners_by_property.get_mut(&property_id) {
            owners.retain(|owner| owner != owner_id);
            if owners.is_empty() {
                self.owners_by_property.remove(&property_id);
            }
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Property(owner_id) => {
                let Some(property_id) = self.property_owners_map.get(owner_id) else {
                    return derivation::ABSENT;
                };
                derivation::fingerprint_of(&(
                    self.properties.get(property_id),
                    self.signature_owner_by_property.get(property_id),
                ))
            }
            FactKey::DeclDefault(decl_id) => {
                derivation::fingerprint_opt(self.inferred_string_defaults.get(decl_id))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a property fact");
                derivation::ABSENT
            }
        }
    }
}

impl LuaPropertyIndex {
    /// Drops one file's contribution to a property. The property and the owner mapping that
    /// reaches it only go when no other file still documents it.
    fn drop_contribution(&mut self, file_id: FileId, owner_id: &LuaSemanticDeclId) {
        let Some(property_id) = self.property_owners_map.get(owner_id).copied() else {
            return;
        };
        self.note_property_write(property_id);

        if let Some(contributions) = self.contributions.get_mut(&property_id) {
            contributions.retain(|key, _| key.file_id != file_id.id);
            match contributions.len() {
                0 => {
                    self.contributions.remove(&property_id);
                }
                1 => {
                    let (key, value) = self
                        .contributions
                        .remove(&property_id)
                        .and_then(|mut map| map.pop_first())
                        .expect("length checked above");
                    self.sole_contributor
                        .insert(property_id, FileId::new(key.file_id));
                    self.properties.insert(property_id, value);
                    return;
                }
                _ => {
                    self.rebuild_merged(property_id);
                    return;
                }
            }
        } else if self
            .sole_contributor
            .get(&property_id)
            .is_some_and(|sole| *sole != file_id)
        {
            // Another file created and owns this property; nothing of ours to drop.
            return;
        }

        self.unmap_owner(owner_id);
        self.properties.remove(&property_id);
        self.signature_owner_by_property.remove(&property_id);
        self.sole_contributor.remove(&property_id);
    }
}

impl LuaIndex for LuaPropertyIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(property_owner_ids) = self.in_filed_owner.remove(&file_id) {
            for property_owner_id in property_owner_ids {
                self.drop_contribution(file_id, &property_owner_id);
            }
        }
        self.file_merge_order.remove(&file_id);
        // Clean up inferred string defaults owned by this file.
        if let Some(decl_ids) = self.inferred_string_defaults_file_owners.remove(&file_id) {
            for decl_id in decl_ids {
                self.inferred_string_defaults.remove(&decl_id);
                derivation::note_write(|| FactKey::DeclDefault(decl_id));
            }
        }
    }

    fn clear(&mut self) {
        for decl_id in self.inferred_string_defaults.keys() {
            derivation::note_write(|| FactKey::DeclDefault(*decl_id));
        }
        for owner_id in self.property_owners_map.keys() {
            derivation::note_write(|| FactKey::Property(owner_id.clone()));
        }
        self.properties.clear();
        self.property_owners_map.clear();
        self.owners_by_property.clear();
        self.signature_owner_by_property.clear();
        self.sole_contributor.clear();
        self.contributions.clear();
        self.file_merge_order.clear();
        self.in_filed_owner.clear();
        self.inferred_string_defaults.clear();
        self.inferred_string_defaults_file_owners.clear();
        self.id_count = 0;
    }
}

/// Tries to extract the position of a function type from a @field definition
pub fn try_extract_signature_id_from_field(
    db: &DbIndex,
    member: &LuaMember,
) -> Option<LuaSignatureId> {
    // Check whether it is a field definition
    if !member.is_field() {
        return None;
    }

    let field_node = db
        .get_vfs()
        .syntax_node_around(&member.get_file_id(), member.get_syntax_id())?;

    // Try converting to LuaDocTagField
    let field_tag = LuaDocTagField::cast(field_node)?;

    // Get the type definition
    let type_node = field_tag.get_type()?;

    match &type_node {
        LuaDocType::Func(doc_func) => Some(LuaSignatureId::from_doc_func(
            member.get_file_id(),
            doc_func,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rowan::TextSize;

    use super::*;

    /// A file's owner set holds both an alias and its source. Removing the file has to unmap
    /// both: a leftover mapping to a deleted property makes every later write to that owner
    /// disappear, because `get_or_create_property` finds the id and no value behind it.
    #[test]
    fn removing_a_file_unmaps_every_alias_of_its_property() {
        let file_id = FileId::new(1);
        let source = LuaSemanticDeclId::LuaDecl(LuaDeclId::new(file_id, TextSize::new(0)));
        let alias = LuaSemanticDeclId::LuaDecl(LuaDeclId::new(file_id, TextSize::new(10)));

        let mut index = LuaPropertyIndex::new();
        index.add_owner_map(source.clone(), alias.clone(), file_id);
        index.add_description(file_id, source.clone(), "first".to_string());

        index.remove(file_id);
        assert!(index.get_property(&source).is_none());
        assert!(index.get_property(&alias).is_none());

        // Whichever owner the removal visited last is the one a stale mapping would strand,
        // so both have to accept a write again.
        index.add_description(file_id, source.clone(), "second".to_string());
        index.add_description(file_id, alias.clone(), "third".to_string());
        assert_eq!(
            index.get_property(&source).and_then(|p| p.description()),
            Some(&"second".to_string())
        );
        assert_eq!(
            index.get_property(&alias).and_then(|p| p.description()),
            Some(&"third".to_string())
        );
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaPropertyIndex {
    properties,
    property_owners_map,
    owners_by_property,
    signature_owner_by_property,
    sole_contributor,
    contributions,
    file_merge_order,
    id_count,
    in_filed_owner,
    inferred_string_defaults,
    inferred_string_defaults_file_owners,
});
