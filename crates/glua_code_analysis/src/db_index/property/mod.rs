mod decl_feature;
#[allow(clippy::module_inception)]
mod property;

use std::collections::{HashMap, HashSet};

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

#[derive(Debug)]
pub struct LuaPropertyIndex {
    properties: HashMap<LuaPropertyId, LuaCommonProperty>,
    property_owners_map: HashMap<LuaSemanticDeclId, LuaPropertyId>,
    signature_owner_by_property: HashMap<LuaPropertyId, LuaSignatureId>,

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
            in_filed_owner: HashMap::new(),
            properties: HashMap::new(),
            property_owners_map: HashMap::new(),
            signature_owner_by_property: HashMap::new(),
            inferred_string_defaults: HashMap::new(),
            inferred_string_defaults_file_owners: HashMap::new(),
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

    pub fn add_owner_map(
        &mut self,
        source_owner_id: LuaSemanticDeclId,
        same_property_owner_id: LuaSemanticDeclId,
        file_id: FileId,
    ) -> Option<()> {
        let (_, property_id) = self.get_or_create_property(source_owner_id.clone())?;
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
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_description(description);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_visibility(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        visibility: VisibilityKind,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.visibility = visibility;

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_source(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        source: String,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_source(source);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_default_value(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        default_value: LuaDocDefaultValue,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_default_value(default_value);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_deprecated(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        message: Option<String>,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_deprecated(message);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_version(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        version_conds: Vec<LuaVersionCondition>,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_version_cond(version_conds);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_see(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        mut see_content: String,
        see_description: Option<String>,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;

        if let Some(see_description) = see_description {
            see_content += " ";
            see_content += &see_description;
        }

        property.add_extra_tag("see".into(), see_content);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_other(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        tag_name: String,
        other_content: String,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_tag(tag_name, other_content);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_export(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        export: property::LuaExport,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_extra_export(export);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_decl_feature(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        feature: PropertyDeclFeature,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_decl_feature(feature);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);

        Some(())
    }

    pub fn add_attribute_use(
        &mut self,
        file_id: FileId,
        owner_id: LuaSemanticDeclId,
        attribute_use: LuaAttributeUse,
    ) -> Option<()> {
        let (property, _) = self.get_or_create_property(owner_id.clone())?;
        property.add_attribute_use(attribute_use);

        self.in_filed_owner
            .entry(file_id)
            .or_default()
            .insert(owner_id);
        Some(())
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

impl LuaIndex for LuaPropertyIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(property_owner_ids) = self.in_filed_owner.remove(&file_id) {
            for property_owner_id in property_owner_ids {
                if let Some(property_id) = self.property_owners_map.remove(&property_owner_id) {
                    self.properties.remove(&property_id);
                    self.signature_owner_by_property.remove(&property_id);
                }
            }
        }
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
