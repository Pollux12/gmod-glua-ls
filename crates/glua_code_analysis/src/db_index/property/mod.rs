mod decl_feature;
#[allow(clippy::module_inception)]
mod property;

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
pub use crate::db_index::property::property::LuaAttributeUse;
use crate::{DbIndex, FileId, LuaDeclId, LuaMember, LuaSignatureId};

use super::{LuaSemanticDeclId, traits::LuaIndex};

/// An inferred string default from a self-coalescing `x = x or "literal"` pattern.
#[derive(Debug, Clone)]
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

#[derive(Debug)]
pub struct LuaPropertyIndex {
    /// Merged view, read by `get_property`. For the common single-contributor property this is
    /// the contribution itself and nothing else is stored.
    properties: HashMap<LuaPropertyId, LuaCommonProperty>,
    property_owners_map: HashMap<LuaSemanticDeclId, LuaPropertyId>,
    signature_owner_by_property: HashMap<LuaPropertyId, LuaSignatureId>,

    /// `TypeDecl` properties are keyed by class name alone, so several files can document one
    /// class. Only those properties get per-file contributions; everything else stays sole.
    sole_contributor: HashMap<LuaPropertyId, FileId>,
    contributions: HashMap<LuaPropertyId, BTreeMap<PropertyContributorKey, LuaCommonProperty>>,
    file_merge_order: HashMap<FileId, (u8, Arc<str>)>,

    id_count: u32,
    in_filed_owner: HashMap<FileId, HashSet<LuaSemanticDeclId>>,

    /// Inferred string defaults from `x = x or "literal"` patterns.
    /// Keyed by `LuaDeclId`; one decl can have multiple candidates (e.g.
    /// multiple self-coalescing assignments in different scopes of the same
    /// function).  File ownership is tracked for cleanup on reindex.
    inferred_string_defaults: HashMap<LuaDeclId, Vec<LuaInferredStringDefault>>,
    inferred_string_defaults_file_owners: HashMap<FileId, HashSet<LuaDeclId>>,
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
            in_filed_owner: HashMap::default(),
            properties: HashMap::default(),
            property_owners_map: HashMap::default(),
            signature_owner_by_property: HashMap::default(),
            sole_contributor: HashMap::default(),
            contributions: HashMap::default(),
            file_merge_order: HashMap::default(),
            inferred_string_defaults: HashMap::default(),
            inferred_string_defaults_file_owners: HashMap::default(),
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
            self.property_owners_map.insert(owner_id.clone(), id);
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
        // Owner aliases are always within one file, so this file owns the property outright
        // unless a doc contribution has already promoted it.
        self.sole_contributor.entry(property_id).or_insert(file_id);
        self.property_owners_map
            .insert(same_property_owner_id.clone(), property_id);
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
        self.property_owners_map
            .get(owner_id)
            .and_then(|id| self.properties.get(id))
    }

    pub fn get_signature_owner(
        &self,
        owner_id: &LuaSemanticDeclId,
    ) -> Option<crate::LuaSignatureId> {
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
        self.inferred_string_defaults
            .entry(decl_id)
            .or_default()
            .push(entry);
        self.inferred_string_defaults_file_owners
            .entry(file_id)
            .or_default()
            .insert(decl_id);
    }

    /// Get all inferred string default candidates for a declaration.
    pub fn get_inferred_string_defaults(
        &self,
        decl_id: &LuaDeclId,
    ) -> Option<&[LuaInferredStringDefault]> {
        self.inferred_string_defaults
            .get(decl_id)
            .map(|v| v.as_slice())
    }
}

impl LuaPropertyIndex {
    /// Drops one file's contribution to a property. The property and the owner mapping that
    /// reaches it only go when no other file still documents it.
    fn drop_contribution(&mut self, file_id: FileId, owner_id: &LuaSemanticDeclId) {
        let Some(property_id) = self.property_owners_map.get(owner_id).copied() else {
            return;
        };

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

        self.property_owners_map.remove(owner_id);
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
            }
        }
    }

    fn clear(&mut self) {
        self.properties.clear();
        self.property_owners_map.clear();
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

/// 尝试从 @field 定义中提取函数类型的位置信息
pub fn try_extract_signature_id_from_field(
    db: &DbIndex,
    member: &LuaMember,
) -> Option<LuaSignatureId> {
    // 检查是否是 field 定义
    if !member.is_field() {
        return None;
    }

    let root = db
        .get_vfs()
        .get_syntax_tree(&member.get_file_id())?
        .get_red_root();
    let field_node = member.get_syntax_id().to_node_from_root(&root)?;

    // 尝试转换为 LuaDocTagField
    let field_tag = LuaDocTagField::cast(field_node)?;

    // 获取类型定义
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
