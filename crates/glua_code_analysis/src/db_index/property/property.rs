use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use glua_parser::{LuaVersionCondition, VisibilityKind};

use crate::{
    LuaType, LuaTypeDeclId,
    db_index::{
        property::decl_feature::{DeclFeatureFlag, PropertyDeclFeature},
        signature::LuaDocDefaultValue,
    },
};

#[derive(Clone, PartialEq, Eq)]
pub struct LuaCommonProperty {
    pub visibility: VisibilityKind,
    /// Whether a tag stated `visibility`, rather than it being the default.
    visibility_stated: bool,
    pub decl_features: DeclFeatureFlag,

    extras: Option<Box<PropertyExtras>>,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct PropertyExtras {
    description: Option<Box<String>>,
    default_value: Option<LuaDocDefaultValue>,
    source: Option<Box<String>>,
    deprecated: Option<Box<LuaDeprecated>>,
    version_conds: Option<Box<Vec<LuaVersionCondition>>>,
    tag_content: Option<Box<LuaTagContent>>,
    export: Option<LuaExport>,
    attribute_uses: Option<Arc<Vec<LuaAttributeUse>>>,
}

impl fmt::Debug for LuaCommonProperty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LuaCommonProperty")
            .field("visibility", &self.visibility)
            .field("visibility_stated", &self.visibility_stated)
            .field("description", &self.description())
            .field("default_value", &self.default_value())
            .field("source", &self.source())
            .field("deprecated", &self.deprecated())
            .field("version_conds", &self.version_conds())
            .field("tag_content", &self.tag_content())
            .field("export", &self.export())
            .field("decl_features", &self.decl_features)
            .field("attribute_uses", &self.attribute_uses())
            .finish()
    }
}

impl Hash for LuaCommonProperty {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.visibility.hash(state);
        self.visibility_stated.hash(state);
        self.description().hash(state);
        self.default_value().hash(state);
        self.source().hash(state);
        self.deprecated().hash(state);
        self.version_conds().hash(state);
        self.tag_content().hash(state);
        self.export().hash(state);
        self.decl_features.hash(state);
        self.attribute_uses().hash(state);
    }
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
            visibility_stated: false,
            decl_features: DeclFeatureFlag::new(),
            extras: None,
        }
    }

    fn extras_mut(&mut self) -> &mut PropertyExtras {
        self.extras.get_or_insert_default()
    }

    pub fn description(&self) -> Option<&String> {
        self.extras.as_deref()?.description.as_deref()
    }

    pub fn version_conds(&self) -> Option<&Vec<LuaVersionCondition>> {
        self.extras.as_deref()?.version_conds.as_deref()
    }

    pub fn export(&self) -> Option<&LuaExport> {
        self.extras.as_deref()?.export.as_ref()
    }

    pub fn tag_content(&self) -> Option<&LuaTagContent> {
        self.extras.as_deref()?.tag_content.as_deref()
    }

    pub fn deprecated(&self) -> Option<&LuaDeprecated> {
        self.extras.as_deref()?.deprecated.as_deref()
    }

    pub fn source(&self) -> Option<&String> {
        self.extras.as_deref()?.source.as_deref()
    }

    pub fn default_value(&self) -> Option<&LuaDocDefaultValue> {
        self.extras.as_deref()?.default_value.as_ref()
    }

    pub fn set_visibility(&mut self, visibility: VisibilityKind) {
        self.visibility = visibility;
        self.visibility_stated = true;
    }

    pub fn add_extra_description(&mut self, description: String) {
        self.extras_mut().description = Some(Box::new(description));
    }

    pub fn add_extra_source(&mut self, source: String) {
        self.extras_mut().source = Some(Box::new(source));
    }

    pub fn add_extra_default_value(&mut self, default_value: LuaDocDefaultValue) {
        self.extras_mut().default_value = Some(default_value);
    }

    pub fn add_extra_deprecated(&mut self, message: Option<String>) {
        self.extras_mut().deprecated = match message {
            Some(msg) => Some(Box::new(LuaDeprecated::DeprecatedWithMessage(msg))),
            None => Some(Box::new(LuaDeprecated::Deprecated)),
        };
    }

    pub fn add_extra_version_cond(&mut self, conds: Vec<LuaVersionCondition>) {
        self.extras_mut().version_conds = Some(Box::new(conds));
    }

    pub fn add_extra_tag(&mut self, tag: String, content: String) {
        self.extras_mut()
            .tag_content
            .get_or_insert_with(|| Box::new(LuaTagContent::new()))
            .add_tag(tag, content);
    }

    pub fn add_extra_export(&mut self, export: LuaExport) {
        self.extras_mut().export = Some(export);
    }

    pub fn add_decl_feature(&mut self, feature: PropertyDeclFeature) {
        self.decl_features.add_feature(feature);
    }

    pub fn add_attribute_use(&mut self, attribute_use: LuaAttributeUse) {
        Arc::make_mut(
            self.extras_mut()
                .attribute_uses
                .get_or_insert_with(|| Arc::new(Vec::new())),
        )
        .push(attribute_use);
    }

    pub fn attribute_uses(&self) -> Option<&Arc<Vec<LuaAttributeUse>>> {
        self.extras.as_deref()?.attribute_uses.as_ref()
    }

    /// Folds a later contributor's documentation into an earlier one's. Scalar fields keep the
    /// first value supplied; the additive fields union in contributor order.
    pub fn merge_from(&mut self, other: &LuaCommonProperty) {
        if !self.visibility_stated {
            self.visibility = other.visibility;
            self.visibility_stated = other.visibility_stated;
        }
        self.decl_features = self.decl_features.union(other.decl_features);

        let Some(other_extras) = other.extras.as_deref() else {
            return;
        };
        let mut new_extras = PropertyExtras::default();
        let extras = self.extras.as_deref_mut().unwrap_or(&mut new_extras);
        // A bare `---@class Foo` records an empty description, which must not mask the docs
        // another file wrote for the same class.
        if extras
            .description
            .as_deref()
            .is_none_or(|text| text.is_empty())
            && other_extras
                .description
                .as_deref()
                .is_some_and(|text| !text.is_empty())
        {
            extras.description.clone_from(&other_extras.description);
        }
        if extras.default_value.is_none() {
            extras.default_value.clone_from(&other_extras.default_value);
        }
        if extras.source.is_none() {
            extras.source.clone_from(&other_extras.source);
        }
        if extras.deprecated.is_none() {
            extras.deprecated.clone_from(&other_extras.deprecated);
        }
        if extras.version_conds.is_none() {
            extras.version_conds.clone_from(&other_extras.version_conds);
        }
        if extras.export.is_none() {
            extras.export.clone_from(&other_extras.export);
        }

        if let Some(tags) = &other_extras.tag_content {
            for (tag, content) in tags.get_all_tags() {
                let merged = extras
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

        if let Some(uses) = &other_extras.attribute_uses {
            for attribute_use in uses.iter() {
                let merged = Arc::make_mut(
                    extras
                        .attribute_uses
                        .get_or_insert_with(|| Arc::new(Vec::new())),
                );
                if !merged.contains(attribute_use) {
                    merged.push(attribute_use.clone());
                }
            }
        }
        if new_extras != PropertyExtras::default() {
            self.extras = Some(Box::new(new_extras));
        }
    }

    pub fn find_attribute_use(&self, id: &str) -> Option<&LuaAttributeUse> {
        self.attribute_uses().and_then(|attribute_uses| {
            attribute_uses
                .iter()
                .find(|attribute_use| attribute_use.id.get_name() == id)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaDeprecated {
    Deprecated,
    DeprecatedWithMessage(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaExportScope {
    Default, // Default declaration; handled differently depending on the config file.
    Global,
    Namespace,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
