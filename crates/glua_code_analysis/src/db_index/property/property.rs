use std::sync::Arc;

use glua_parser::{LuaVersionCondition, VisibilityKind};

use crate::{
    LuaType, LuaTypeDeclId,
    db_index::{
        property::decl_feature::{DeclFeatureFlag, PropertyDeclFeature},
        signature::LuaDocDefaultValue,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaCommonProperty {
    pub visibility: VisibilityKind,
    pub description: Option<Box<String>>,
    pub default_value: Option<LuaDocDefaultValue>,
    pub source: Option<Box<String>>,
    pub deprecated: Option<Box<LuaDeprecated>>,
    pub version_conds: Option<Box<Vec<LuaVersionCondition>>>,
    pub tag_content: Option<Box<LuaTagContent>>,
    pub export: Option<LuaExport>,
    pub decl_features: DeclFeatureFlag,
    pub attribute_uses: Option<Arc<Vec<LuaAttributeUse>>>,
}

impl Default for LuaCommonProperty {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaCommonProperty {
    pub fn new() -> Self {
        Self {
            visibility: VisibilityKind::Public,
            description: None,
            default_value: None,
            source: None,
            deprecated: None,
            version_conds: None,
            tag_content: None,
            export: None,
            decl_features: DeclFeatureFlag::new(),
            attribute_uses: None,
        }
    }

    pub fn description(&self) -> Option<&String> {
        self.description.as_deref()
    }

    pub fn version_conds(&self) -> Option<&Vec<LuaVersionCondition>> {
        self.version_conds.as_deref()
    }

    pub fn export(&self) -> Option<&LuaExport> {
        self.export.as_ref()
    }

    pub fn tag_content(&self) -> Option<&LuaTagContent> {
        self.tag_content.as_deref()
    }

    pub fn deprecated(&self) -> Option<&LuaDeprecated> {
        self.deprecated.as_deref()
    }

    pub fn source(&self) -> Option<&String> {
        self.source.as_deref()
    }

    pub fn default_value(&self) -> Option<&LuaDocDefaultValue> {
        self.default_value.as_ref()
    }

    pub fn add_extra_description(&mut self, description: String) {
        self.description = Some(Box::new(description));
    }

    pub fn add_extra_source(&mut self, source: String) {
        self.source = Some(Box::new(source));
    }

    pub fn add_extra_default_value(&mut self, default_value: LuaDocDefaultValue) {
        self.default_value = Some(default_value);
    }

    pub fn add_extra_deprecated(&mut self, message: Option<String>) {
        self.deprecated = match message {
            Some(msg) => Some(Box::new(LuaDeprecated::DeprecatedWithMessage(msg))),
            None => Some(Box::new(LuaDeprecated::Deprecated)),
        };
    }

    pub fn add_extra_version_cond(&mut self, conds: Vec<LuaVersionCondition>) {
        self.version_conds = Some(Box::new(conds));
    }

    pub fn add_extra_tag(&mut self, tag: String, content: String) {
        self.tag_content
            .get_or_insert_with(|| Box::new(LuaTagContent::new()))
            .add_tag(tag, content);
    }

    pub fn add_extra_export(&mut self, export: LuaExport) {
        self.export = Some(export);
    }

    pub fn add_decl_feature(&mut self, feature: PropertyDeclFeature) {
        self.decl_features.add_feature(feature);
    }

    pub fn add_attribute_use(&mut self, attribute_use: LuaAttributeUse) {
        Arc::make_mut(
            self.attribute_uses
                .get_or_insert_with(|| Arc::new(Vec::new())),
        )
        .push(attribute_use);
    }

    pub fn attribute_uses(&self) -> Option<&Arc<Vec<LuaAttributeUse>>> {
        self.attribute_uses.as_ref()
    }

    /// Folds a later contributor's documentation into an earlier one's. Scalar fields keep the
    /// first value supplied; the additive fields union in contributor order.
    pub fn merge_from(&mut self, other: &LuaCommonProperty) {
        if self.visibility == VisibilityKind::Public {
            self.visibility = other.visibility;
        }
        // A bare `---@class Foo` records an empty description, which must not mask the docs
        // another file wrote for the same class.
        if self
            .description
            .as_deref()
            .is_none_or(|text| text.is_empty())
            && other
                .description
                .as_deref()
                .is_some_and(|text| !text.is_empty())
        {
            self.description.clone_from(&other.description);
        }
        if self.default_value.is_none() {
            self.default_value.clone_from(&other.default_value);
        }
        if self.source.is_none() {
            self.source.clone_from(&other.source);
        }
        if self.deprecated.is_none() {
            self.deprecated.clone_from(&other.deprecated);
        }
        if self.version_conds.is_none() {
            self.version_conds.clone_from(&other.version_conds);
        }
        if self.export.is_none() {
            self.export.clone_from(&other.export);
        }
        self.decl_features = self.decl_features.union(other.decl_features);

        if let Some(tags) = other.tag_content() {
            for (tag, content) in tags.get_all_tags() {
                let merged = self
                    .tag_content
                    .get_or_insert_with(|| Box::new(LuaTagContent::new()));
                if !merged
                    .tags
                    .iter()
                    .any(|existing| existing == &(tag.clone(), content.clone()))
                {
                    merged.add_tag(tag.clone(), content.clone());
                }
            }
        }

        if let Some(uses) = other.attribute_uses() {
            for attribute_use in uses.iter() {
                let merged = Arc::make_mut(
                    self.attribute_uses
                        .get_or_insert_with(|| Arc::new(Vec::new())),
                );
                if !merged.contains(attribute_use) {
                    merged.push(attribute_use.clone());
                }
            }
        }
    }

    pub fn find_attribute_use(&self, id: &str) -> Option<&LuaAttributeUse> {
        self.attribute_uses.as_ref().and_then(|attribute_uses| {
            attribute_uses
                .iter()
                .find(|attribute_use| attribute_use.id.get_name() == id)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LuaDeprecated {
    Deprecated,
    DeprecatedWithMessage(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LuaExportScope {
    Default, // 默认声明, 会根据配置文件作不同的处理.
    Global,
    Namespace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaTagContent {
    pub tags: Vec<(String, String)>,
}

impl Default for LuaTagContent {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaTagContent {
    pub fn new() -> Self {
        Self { tags: Vec::new() }
    }

    pub fn add_tag(&mut self, tag: String, content: String) {
        self.tags.push((tag, content));
    }

    pub fn get_all_tags(&self) -> &[(String, String)] {
        &self.tags
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaExport {
    pub scope: LuaExportScope,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Copy)]
pub struct LuaPropertyId {
    id: u32,
}

impl LuaPropertyId {
    pub fn new(id: u32) -> Self {
        Self { id }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct LuaAttributeUse {
    pub id: LuaTypeDeclId,
    pub args: Vec<(String, Option<LuaType>)>,
}

impl LuaAttributeUse {
    pub fn new(id: LuaTypeDeclId, args: Vec<(String, Option<LuaType>)>) -> Self {
        Self { id, args }
    }

    pub fn get_param_by_name(&self, name: &str) -> Option<&LuaType> {
        self.args
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, typ)| typ.as_ref())
    }
}
