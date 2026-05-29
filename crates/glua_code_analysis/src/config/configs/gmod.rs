use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;

use schemars::JsonSchema;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use wax::Pattern;

const FILE_PARAM_DEFAULTS: &[(&str, &str)] = &[
    ("ply", "Player"),
    ("player", "Player"),
    ("ent", "Entity"),
    ("entity", "Entity"),
    ("veh", "Entity"),
    ("vehicle", "Entity"),
    ("wep", "Weapon"),
    ("weapon", "Weapon"),
    ("pnl", "Panel"),
    ("panel", "Panel"),
    ("npc", "NPC"),
    ("trace", "TraceResult"),
    ("tr", "TraceResult"),
    ("ang", "Angle"),
    ("angle", "Angle"),
    ("vec", "Vector"),
    ("pos", "Vector"),
    ("color", "Color"),
    ("col", "Color"),
    ("phys", "PhysObj"),
    ("dmginfo", "CTakeDamageInfo"),
    ("attacker", "Entity"),
    ("inflictor", "Entity"),
    ("victim", "Entity"),
    ("cmd", "CUserCmd"),
    ("func", "function"),
    ("mat", "IMaterial"),
];

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmod {
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub default_realm: EmmyrcGmodRealm,
    #[serde(default)]
    pub scripted_class_scopes: EmmyrcGmodScriptedClassScopes,
    #[serde(default)]
    pub hook_mappings: EmmyrcGmodHookMappings,
    /// Ordered plugin ids persisted by editor integrations.
    /// The language server remains plugin-agnostic and consumes resolved config only.
    #[serde(default)]
    #[schemars(extend("x-gluals-editor" = "pluginList"))]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub network: EmmyrcGmodNetwork,
    #[serde(default)]
    pub vgui: EmmyrcGmodVgui,
    #[serde(default)]
    pub outline: EmmyrcGmodOutline,
    /// Parameter-name to type-name fallbacks for otherwise unresolved params.
    #[serde(
        default = "file_param_defaults_default",
        deserialize_with = "deserialize_file_param_defaults"
    )]
    #[schemars(extend(
        "x-gluals-editor" = "mappingTable",
        "x-gluals-key-label" = "Parameter",
        "x-gluals-value-label" = "Type"
    ))]
    pub file_param_defaults: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_realm_from_filename: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_realm_from_calls: Option<bool>,
    #[serde(default = "infer_dynamic_fields_default")]
    pub infer_dynamic_fields: bool,
    #[serde(default = "dynamic_fields_global_default")]
    pub dynamic_fields_global: bool,
    /// Override path to GMod annotations directory. Set to empty to use VSCode downloaded annotations.
    /// When set to empty string or not provided, uses VSCode extension's auto-downloaded annotations (if enabled).
    /// Set to explicit path to override, or use `autoLoadAnnotations: false` in .gluarc to disable entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations_path: Option<String>,
    /// Disable auto-loading of annotations (from VSCode or default path).
    /// This takes precedence over extension settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_load_annotations: Option<bool>,
    /// Path to custom GLua scaffolding templates folder.
    /// Built-in templates are used as fallback when a custom one is not found.
    /// Accepts an absolute path or a path relative to the workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_path: Option<String>,
    /// Automatically add base gamemodes as libraries when a gamemode
    /// derives from another (via the `"base"` field in the gamemode `.txt` file).
    /// Set to `false` to disable this detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_detect_gamemode_base: Option<bool>,
    /// Configures additional hook-owner globals beyond the built-in
    /// `GM` / `GAMEMODE` / `SANDBOX` set.
    #[serde(default)]
    pub scripted_owners: EmmyrcGmodScriptedOwners,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassScopes {
    #[serde(default = "scripted_scope_include_default")]
    #[schemars(extend("x-gluals-editor" = "scriptedClassTable"))]
    pub include: Vec<EmmyrcGmodScriptedClassScopeEntry>,
    #[serde(default, rename = "exclude", skip_serializing)]
    #[schemars(skip)]
    pub legacy_exclude: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(untagged)]
pub enum EmmyrcGmodScriptedClassScopeEntry {
    LegacyGlob(String),
    Definition(Box<EmmyrcGmodScriptedClassDefinition>),
}

#[derive(Serialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassDefinition {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_global: Option<String>,
    /// When set, every file matched by this scope resolves to this class name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_class_name: Option<String>,
    /// When true, the scope's class global is a workspace-global singleton.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_global_singleton: Option<bool>,
    /// When true, strips sh_/sv_/cl_ from single-file class names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_file_prefix: Option<bool>,
    /// When true, editor outline/class explorer views should hide this scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_from_outline: Option<bool>,
    /// Additional global names exposed for the same class global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// Class globals this scope inherits from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub super_types: Option<Vec<String>>,
    /// Whether this class global should be treated as a hook owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_owner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaffold: Option<EmmyrcGmodScriptedClassScaffold>,
    /// Optional prefix prepended to class names derived from the folder segment.
    /// For example, gamemodes use `"gamemode_"` so a folder `sandbox` produces the
    /// class name `gamemode_sandbox`, matching the runtime convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassScaffold {
    #[serde(default)]
    pub files: Vec<EmmyrcGmodScriptedClassScaffoldFile>,
}

#[derive(Serialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassScaffoldFile {
    pub path: String,
    pub template: String,
}

#[derive(Serialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedGmodScriptedClassDefinition {
    pub id: String,
    pub label: String,
    pub path: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub class_global: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_class_name: Option<String>,
    #[serde(default)]
    pub is_global_singleton: bool,
    #[serde(default)]
    pub strip_file_prefix: bool,
    #[serde(default)]
    pub hide_from_outline: bool,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub super_types: Vec<String>,
    #[serde(default)]
    pub hook_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub root_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaffold: Option<EmmyrcGmodScriptedClassScaffold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGmodScriptedClassMatch {
    pub definition: ResolvedGmodScriptedClassDefinition,
    pub class_name: String,
}

impl<'de> Deserialize<'de> for EmmyrcGmodScriptedClassDefinition {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "id",
            "label",
            "path",
            "include",
            "exclude",
            "classGlobal",
            "fixedClassName",
            "isGlobalSingleton",
            "stripFilePrefix",
            "hideFromOutline",
            "aliases",
            "superTypes",
            "hookOwner",
            "parentId",
            "icon",
            "rootDir",
            "scaffold",
            "classNamePrefix",
            "disabled",
        ];

        struct DefinitionVisitor;

        impl<'de> serde::de::Visitor<'de> for DefinitionVisitor {
            type Value = EmmyrcGmodScriptedClassDefinition;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct EmmyrcGmodScriptedClassDefinition")
            }

            fn visit_map<MapType>(self, mut map: MapType) -> Result<Self::Value, MapType::Error>
            where
                MapType: serde::de::MapAccess<'de>,
            {
                let mut id = None;
                let mut label = None;
                let mut path = None;
                let mut include = None;
                let mut exclude = None;
                let mut class_global = None;
                let mut fixed_class_name = None;
                let mut is_global_singleton = None;
                let mut strip_file_prefix = None;
                let mut hide_from_outline = None;
                let mut aliases = None;
                let mut super_types = None;
                let mut hook_owner = None;
                let mut parent_id = None;
                let mut icon = None;
                let mut root_dir = None;
                let mut scaffold = None;
                let mut class_name_prefix = None;
                let mut disabled = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => read_unique_field(&mut id, &mut map, "id")?,
                        "label" => read_unique_field(&mut label, &mut map, "label")?,
                        "path" => read_unique_field(&mut path, &mut map, "path")?,
                        "include" => read_unique_field(&mut include, &mut map, "include")?,
                        "exclude" => read_unique_field(&mut exclude, &mut map, "exclude")?,
                        "classGlobal" => {
                            read_unique_field(&mut class_global, &mut map, "classGlobal")?
                        }
                        "fixedClassName" => {
                            read_unique_field(&mut fixed_class_name, &mut map, "fixedClassName")?
                        }
                        "isGlobalSingleton" => read_unique_field(
                            &mut is_global_singleton,
                            &mut map,
                            "isGlobalSingleton",
                        )?,
                        "stripFilePrefix" => {
                            read_unique_field(&mut strip_file_prefix, &mut map, "stripFilePrefix")?
                        }
                        "hideFromOutline" => {
                            read_unique_field(&mut hide_from_outline, &mut map, "hideFromOutline")?
                        }
                        "aliases" => read_unique_field(&mut aliases, &mut map, "aliases")?,
                        "superTypes" => {
                            read_unique_field(&mut super_types, &mut map, "superTypes")?
                        }
                        "hookOwner" => read_unique_field(&mut hook_owner, &mut map, "hookOwner")?,
                        "parentId" => read_unique_field(&mut parent_id, &mut map, "parentId")?,
                        "icon" => read_unique_field(&mut icon, &mut map, "icon")?,
                        "rootDir" => read_unique_field(&mut root_dir, &mut map, "rootDir")?,
                        "scaffold" => read_unique_field(&mut scaffold, &mut map, "scaffold")?,
                        "classNamePrefix" => {
                            read_unique_field(&mut class_name_prefix, &mut map, "classNamePrefix")?
                        }
                        "disabled" => read_unique_field(&mut disabled, &mut map, "disabled")?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(EmmyrcGmodScriptedClassDefinition {
                    id: required_field::<_, MapType::Error>(id, "id")?,
                    label: label.unwrap_or_default(),
                    path: path.unwrap_or_default(),
                    include: include.unwrap_or_default(),
                    exclude: exclude.unwrap_or_default(),
                    class_global: class_global.unwrap_or_default(),
                    fixed_class_name: fixed_class_name.unwrap_or_default(),
                    is_global_singleton: is_global_singleton.unwrap_or_default(),
                    strip_file_prefix: strip_file_prefix.unwrap_or_default(),
                    hide_from_outline: hide_from_outline.unwrap_or_default(),
                    aliases: aliases.unwrap_or_default(),
                    super_types: super_types.unwrap_or_default(),
                    hook_owner: hook_owner.unwrap_or_default(),
                    parent_id: parent_id.unwrap_or_default(),
                    icon: icon.unwrap_or_default(),
                    root_dir: root_dir.unwrap_or_default(),
                    scaffold: scaffold.unwrap_or_default(),
                    class_name_prefix: class_name_prefix.unwrap_or_default(),
                    disabled: disabled.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_struct(
            "EmmyrcGmodScriptedClassDefinition",
            FIELDS,
            DefinitionVisitor,
        )
    }
}

impl<'de> Deserialize<'de> for EmmyrcGmodScriptedClassScaffoldFile {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        const FIELDS: &[&str] = &["path", "template"];

        struct ScaffoldFileVisitor;

        impl<'de> serde::de::Visitor<'de> for ScaffoldFileVisitor {
            type Value = EmmyrcGmodScriptedClassScaffoldFile;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct EmmyrcGmodScriptedClassScaffoldFile")
            }

            fn visit_map<MapType>(self, mut map: MapType) -> Result<Self::Value, MapType::Error>
            where
                MapType: serde::de::MapAccess<'de>,
            {
                let mut path = None;
                let mut template = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" => read_unique_field(&mut path, &mut map, "path")?,
                        "template" => read_unique_field(&mut template, &mut map, "template")?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(EmmyrcGmodScriptedClassScaffoldFile {
                    path: required_field::<_, MapType::Error>(path, "path")?,
                    template: required_field::<_, MapType::Error>(template, "template")?,
                })
            }
        }

        deserializer.deserialize_struct(
            "EmmyrcGmodScriptedClassScaffoldFile",
            FIELDS,
            ScaffoldFileVisitor,
        )
    }
}

impl<'de> Deserialize<'de> for ResolvedGmodScriptedClassDefinition {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "id",
            "label",
            "path",
            "include",
            "exclude",
            "classGlobal",
            "fixedClassName",
            "isGlobalSingleton",
            "stripFilePrefix",
            "hideFromOutline",
            "aliases",
            "superTypes",
            "hookOwner",
            "parentId",
            "icon",
            "rootDir",
            "scaffold",
            "classNamePrefix",
        ];

        struct ResolvedDefinitionVisitor;

        impl<'de> serde::de::Visitor<'de> for ResolvedDefinitionVisitor {
            type Value = ResolvedGmodScriptedClassDefinition;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct ResolvedGmodScriptedClassDefinition")
            }

            fn visit_map<MapType>(self, mut map: MapType) -> Result<Self::Value, MapType::Error>
            where
                MapType: serde::de::MapAccess<'de>,
            {
                let mut id = None;
                let mut label = None;
                let mut path = None;
                let mut include = None;
                let mut exclude = None;
                let mut class_global = None;
                let mut fixed_class_name = None;
                let mut is_global_singleton = None;
                let mut strip_file_prefix = None;
                let mut hide_from_outline = None;
                let mut aliases = None;
                let mut super_types = None;
                let mut hook_owner = None;
                let mut parent_id = None;
                let mut icon = None;
                let mut root_dir = None;
                let mut scaffold = None;
                let mut class_name_prefix = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => read_unique_field(&mut id, &mut map, "id")?,
                        "label" => read_unique_field(&mut label, &mut map, "label")?,
                        "path" => read_unique_field(&mut path, &mut map, "path")?,
                        "include" => read_unique_field(&mut include, &mut map, "include")?,
                        "exclude" => read_unique_field(&mut exclude, &mut map, "exclude")?,
                        "classGlobal" => {
                            read_unique_field(&mut class_global, &mut map, "classGlobal")?
                        }
                        "fixedClassName" => {
                            read_unique_field(&mut fixed_class_name, &mut map, "fixedClassName")?
                        }
                        "isGlobalSingleton" => read_unique_field(
                            &mut is_global_singleton,
                            &mut map,
                            "isGlobalSingleton",
                        )?,
                        "stripFilePrefix" => {
                            read_unique_field(&mut strip_file_prefix, &mut map, "stripFilePrefix")?
                        }
                        "hideFromOutline" => {
                            read_unique_field(&mut hide_from_outline, &mut map, "hideFromOutline")?
                        }
                        "aliases" => read_unique_field(&mut aliases, &mut map, "aliases")?,
                        "superTypes" => {
                            read_unique_field(&mut super_types, &mut map, "superTypes")?
                        }
                        "hookOwner" => read_unique_field(&mut hook_owner, &mut map, "hookOwner")?,
                        "parentId" => read_unique_field(&mut parent_id, &mut map, "parentId")?,
                        "icon" => read_unique_field(&mut icon, &mut map, "icon")?,
                        "rootDir" => read_unique_field(&mut root_dir, &mut map, "rootDir")?,
                        "scaffold" => read_unique_field(&mut scaffold, &mut map, "scaffold")?,
                        "classNamePrefix" => {
                            read_unique_field(&mut class_name_prefix, &mut map, "classNamePrefix")?
                        }
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(ResolvedGmodScriptedClassDefinition {
                    id: required_field::<_, MapType::Error>(id, "id")?,
                    label: required_field::<_, MapType::Error>(label, "label")?,
                    path: required_field::<_, MapType::Error>(path, "path")?,
                    include: required_field::<_, MapType::Error>(include, "include")?,
                    exclude: required_field::<_, MapType::Error>(exclude, "exclude")?,
                    class_global: required_field::<_, MapType::Error>(class_global, "classGlobal")?,
                    fixed_class_name: fixed_class_name.unwrap_or_default(),
                    is_global_singleton: is_global_singleton.unwrap_or(false),
                    strip_file_prefix: strip_file_prefix.unwrap_or(false),
                    hide_from_outline: hide_from_outline.unwrap_or(false),
                    aliases: aliases.unwrap_or_default(),
                    super_types: super_types.unwrap_or_default(),
                    hook_owner: hook_owner.unwrap_or(false),
                    parent_id: parent_id.unwrap_or_default(),
                    icon: icon.unwrap_or_default(),
                    root_dir: required_field::<_, MapType::Error>(root_dir, "rootDir")?,
                    scaffold: scaffold.unwrap_or_default(),
                    class_name_prefix: class_name_prefix.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_struct(
            "ResolvedGmodScriptedClassDefinition",
            FIELDS,
            ResolvedDefinitionVisitor,
        )
    }
}

fn read_unique_field<'de, MapType, FieldType>(
    field: &mut Option<FieldType>,
    map: &mut MapType,
    name: &'static str,
) -> Result<(), MapType::Error>
where
    MapType: serde::de::MapAccess<'de>,
    FieldType: Deserialize<'de>,
{
    if field.is_some() {
        return Err(<MapType::Error as serde::de::Error>::duplicate_field(name));
    }

    *field = Some(map.next_value()?);
    Ok(())
}

fn required_field<FieldType, ErrorType>(
    field: Option<FieldType>,
    name: &'static str,
) -> Result<FieldType, ErrorType>
where
    ErrorType: serde::de::Error,
{
    field.ok_or_else(|| ErrorType::missing_field(name))
}

fn enabled_default() -> bool {
    true
}

fn infer_dynamic_fields_default() -> bool {
    true
}

fn dynamic_fields_global_default() -> bool {
    true
}

fn file_param_defaults_default() -> HashMap<String, String> {
    FILE_PARAM_DEFAULTS
        .iter()
        .map(|(name, type_name)| ((*name).to_string(), (*type_name).to_string()))
        .collect()
}

fn deserialize_file_param_defaults<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let overrides = HashMap::<String, String>::deserialize(deserializer)?;
    let mut merged_defaults = file_param_defaults_default();

    for (param_name, type_name) in overrides {
        let trimmed_name = param_name.trim();
        if trimmed_name.is_empty() {
            continue;
        }

        let trimmed_type_name = type_name.trim();

        if trimmed_type_name.is_empty() {
            merged_defaults.remove(trimmed_name);
            continue;
        }

        merged_defaults.insert(trimmed_name.to_string(), trimmed_type_name.to_string());
    }

    Ok(merged_defaults)
}

impl Default for EmmyrcGmod {
    fn default() -> Self {
        Self {
            enabled: enabled_default(),
            default_realm: EmmyrcGmodRealm::default(),
            scripted_class_scopes: EmmyrcGmodScriptedClassScopes::default(),
            hook_mappings: EmmyrcGmodHookMappings::default(),
            plugins: Vec::new(),
            network: EmmyrcGmodNetwork::default(),
            vgui: EmmyrcGmodVgui::default(),
            outline: EmmyrcGmodOutline::default(),
            file_param_defaults: file_param_defaults_default(),
            detect_realm_from_filename: None,
            detect_realm_from_calls: None,
            infer_dynamic_fields: infer_dynamic_fields_default(),
            dynamic_fields_global: dynamic_fields_global_default(),
            annotations_path: None,
            auto_load_annotations: None,
            template_path: None,
            auto_detect_gamemode_base: None,
            scripted_owners: EmmyrcGmodScriptedOwners::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodVgui {
    #[serde(default = "vgui_code_lens_default")]
    pub code_lens_enabled: bool,
    #[serde(default = "vgui_inlay_hint_default")]
    pub inlay_hint_enabled: bool,
}

fn vgui_code_lens_default() -> bool {
    true
}

fn vgui_inlay_hint_default() -> bool {
    false
}

impl Default for EmmyrcGmodVgui {
    fn default() -> Self {
        Self {
            code_lens_enabled: vgui_code_lens_default(),
            inlay_hint_enabled: vgui_inlay_hint_default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcGmodRealm {
    Client,
    Server,
    #[default]
    Shared,
    Menu,
}

fn scripted_scope_include_default() -> Vec<EmmyrcGmodScriptedClassScopeEntry> {
    vec![
        EmmyrcGmodScriptedClassScopeEntry::Definition(default_scripted_class_definition(
            "entities",
            "Entities",
            &["entities"],
            &["entities/**"],
            &[],
            "ENT",
            None,
            Some("folder-library"),
            Some("lua/entities"),
            Some(EmmyrcGmodScriptedClassScaffold {
                files: vec![
                    EmmyrcGmodScriptedClassScaffoldFile {
                        path: "{{name}}/shared.lua".to_string(),
                        template: "ent_shared.lua".to_string(),
                    },
                    EmmyrcGmodScriptedClassScaffoldFile {
                        path: "{{name}}/init.lua".to_string(),
                        template: "ent_init.lua".to_string(),
                    },
                    EmmyrcGmodScriptedClassScaffoldFile {
                        path: "{{name}}/cl_init.lua".to_string(),
                        template: "ent_cl_init.lua".to_string(),
                    },
                ],
            }),
        )),
        EmmyrcGmodScriptedClassScopeEntry::Definition(default_scripted_class_definition(
            "weapons",
            "SWEPs",
            &["weapons"],
            &["weapons/**"],
            &["weapons/gmod_tool/stools/**"],
            "SWEP",
            None,
            Some("folder-library"),
            Some("lua/weapons"),
            Some(EmmyrcGmodScriptedClassScaffold {
                files: vec![EmmyrcGmodScriptedClassScaffoldFile {
                    path: "{{name}}/shared.lua".to_string(),
                    template: "swep_shared.lua".to_string(),
                }],
            }),
        )),
        EmmyrcGmodScriptedClassScopeEntry::Definition(default_scripted_class_definition(
            "effects",
            "Effects",
            &["effects"],
            &["effects/**"],
            &[],
            "EFFECT",
            None,
            Some("folder-library"),
            Some("lua/effects"),
            Some(EmmyrcGmodScriptedClassScaffold {
                files: vec![EmmyrcGmodScriptedClassScaffoldFile {
                    path: "{{name}}.lua".to_string(),
                    template: "effect.lua".to_string(),
                }],
            }),
        )),
        EmmyrcGmodScriptedClassScopeEntry::Definition(default_scripted_class_definition(
            "stools",
            "STools",
            &["weapons", "gmod_tool", "stools"],
            &["weapons/gmod_tool/stools/**", "weapons/gmod_tool/*.lua"],
            &[],
            "TOOL",
            Some("weapons"),
            Some("tools"),
            Some("lua/weapons/gmod_tool/stools"),
            Some(EmmyrcGmodScriptedClassScaffold {
                files: vec![EmmyrcGmodScriptedClassScaffoldFile {
                    path: "{{name}}.lua".to_string(),
                    template: "tool.lua".to_string(),
                }],
            }),
        )),
        EmmyrcGmodScriptedClassScopeEntry::Definition({
            let mut definition = default_scripted_class_definition(
                "plugins",
                "Plugins",
                &["plugins"],
                &["plugins/**"],
                &[],
                "PLUGIN",
                None,
                Some("extensions"),
                Some("plugins"),
                None,
            );
            definition.super_types = Some(vec![
                "GM".to_string(),
                "GAMEMODE".to_string(),
                "SANDBOX".to_string(),
            ]);
            definition.hook_owner = Some(true);
            definition
        }),
        EmmyrcGmodScriptedClassScopeEntry::Definition({
            let mut definition = default_scripted_class_definition(
                "gamemodes",
                "Gamemodes",
                &["gamemodes"],
                &["gamemodes/*/gamemode/**"],
                &[],
                "GM",
                None,
                Some("folder-library"),
                Some("gamemodes"),
                None,
            );
            // Runtime convention: gamemode tables live at _G["gamemode_<folder>"],
            // and DEFINE_BASECLASS("gamemode_sandbox") references that name. Prefix
            // the class name so it matches the runtime identifier.
            definition.class_name_prefix = Some("gamemode_".to_string());
            definition
        }),
    ]
}

fn default_scripted_class_definition(
    id: &str,
    label: &str,
    path: &[&str],
    include: &[&str],
    exclude: &[&str],
    class_global: &str,
    parent_id: Option<&str>,
    icon: Option<&str>,
    root_dir: Option<&str>,
    scaffold: Option<EmmyrcGmodScriptedClassScaffold>,
) -> Box<EmmyrcGmodScriptedClassDefinition> {
    Box::new(EmmyrcGmodScriptedClassDefinition {
        id: id.to_string(),
        label: Some(label.to_string()),
        path: Some(path.iter().map(|segment| (*segment).to_string()).collect()),
        include: Some(
            include
                .iter()
                .map(|pattern| (*pattern).to_string())
                .collect(),
        ),
        exclude: if exclude.is_empty() {
            None
        } else {
            Some(
                exclude
                    .iter()
                    .map(|pattern| (*pattern).to_string())
                    .collect(),
            )
        },
        class_global: Some(class_global.to_string()),
        parent_id: parent_id.map(str::to_string),
        icon: icon.map(str::to_string),
        root_dir: root_dir.map(str::to_string),
        scaffold,
        class_name_prefix: None,
        fixed_class_name: None,
        is_global_singleton: None,
        strip_file_prefix: None,
        hide_from_outline: None,
        aliases: None,
        super_types: None,
        hook_owner: None,
        disabled: None,
    })
}

fn default_scripted_class_definitions() -> Vec<ResolvedGmodScriptedClassDefinition> {
    scripted_scope_include_default()
        .into_iter()
        .filter_map(|entry| match entry {
            EmmyrcGmodScriptedClassScopeEntry::Definition(definition) => {
                resolve_scripted_class_definition(&definition, &[])
            }
            EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(_) => None,
        })
        .collect()
}

fn resolve_scripted_class_definition(
    definition: &EmmyrcGmodScriptedClassDefinition,
    legacy_exclude: &[String],
) -> Option<ResolvedGmodScriptedClassDefinition> {
    if definition.disabled.unwrap_or(false) {
        return None;
    }

    let label = definition.label.as_deref()?.trim();
    let class_global = definition.class_global.as_deref()?.trim();
    let path = definition.path.as_ref()?.clone();
    let include = definition.include.as_ref()?.clone();
    if label.is_empty()
        || class_global.is_empty()
        || path.is_empty()
        || include.is_empty()
        || definition.id.trim().is_empty()
    {
        return None;
    }

    let mut exclude = definition.exclude.clone().unwrap_or_default();
    exclude.extend(
        legacy_exclude
            .iter()
            .map(|pattern| pattern.trim())
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_string),
    );

    let root_dir = definition.root_dir.clone().unwrap_or_else(|| {
        if path
            .first()
            .is_some_and(|segment| segment.eq_ignore_ascii_case("plugins"))
        {
            path.join("/")
        } else {
            format!("lua/{}", path.join("/"))
        }
    });

    Some(ResolvedGmodScriptedClassDefinition {
        id: definition.id.trim().to_string(),
        label: label.to_string(),
        path: path
            .into_iter()
            .map(|segment| segment.trim().to_string())
            .filter(|segment| !segment.is_empty())
            .collect(),
        include: include
            .into_iter()
            .map(|pattern| pattern.trim().to_string())
            .filter(|pattern| !pattern.is_empty())
            .collect(),
        exclude,
        class_global: class_global.to_string(),
        fixed_class_name: definition
            .fixed_class_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        is_global_singleton: definition.is_global_singleton.unwrap_or(false),
        strip_file_prefix: definition.strip_file_prefix.unwrap_or(false),
        hide_from_outline: definition.hide_from_outline.unwrap_or(false),
        aliases: normalize_name_list(definition.aliases.as_deref()),
        super_types: normalize_name_list(definition.super_types.as_deref()),
        hook_owner: definition.hook_owner.unwrap_or(false),
        parent_id: definition
            .parent_id
            .as_deref()
            .map(str::trim)
            .filter(|parent_id| !parent_id.is_empty())
            .map(str::to_string),
        icon: definition
            .icon
            .as_deref()
            .map(str::trim)
            .filter(|icon| !icon.is_empty())
            .map(str::to_string),
        root_dir,
        scaffold: definition
            .scaffold
            .clone()
            .filter(|scaffold| !scaffold.files.is_empty()),
        class_name_prefix: definition
            .class_name_prefix
            .as_deref()
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_string),
    })
}

fn merge_scripted_class_definitions(
    entries: &[EmmyrcGmodScriptedClassScopeEntry],
    legacy_exclude: &[String],
) -> Vec<ResolvedGmodScriptedClassDefinition> {
    let legacy_include = legacy_include_patterns(entries);
    let has_definition_entries = entries
        .iter()
        .any(|entry| matches!(entry, EmmyrcGmodScriptedClassScopeEntry::Definition(_)));
    let mut resolved = default_scripted_class_definitions();
    if !legacy_exclude.is_empty() {
        let normalized_legacy_exclude = legacy_exclude
            .iter()
            .map(|pattern| pattern.trim())
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        for definition in &mut resolved {
            definition
                .exclude
                .extend(normalized_legacy_exclude.iter().cloned());
        }
    }
    let mut index_by_id = resolved
        .iter()
        .enumerate()
        .map(|(idx, definition)| (definition.id.clone(), idx))
        .collect::<HashMap<_, _>>();
    let mut synthetic_legacy_id = 0usize;

    for entry in entries {
        match entry {
            EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(glob) => {
                let trimmed = glob.trim();
                if trimmed.is_empty() {
                    continue;
                }

                synthetic_legacy_id += 1;
                let definition = EmmyrcGmodScriptedClassDefinition {
                    id: format!("legacy-{synthetic_legacy_id}"),
                    label: Some(trimmed.to_string()),
                    path: None,
                    include: Some(vec![trimmed.to_string()]),
                    exclude: None,
                    class_global: None,
                    fixed_class_name: None,
                    is_global_singleton: None,
                    strip_file_prefix: None,
                    hide_from_outline: None,
                    aliases: None,
                    super_types: None,
                    hook_owner: None,
                    parent_id: None,
                    icon: None,
                    root_dir: None,
                    scaffold: None,
                    class_name_prefix: None,
                    disabled: None,
                };

                if let Some(definition) =
                    resolve_scripted_class_definition(&definition, legacy_exclude)
                {
                    index_by_id.insert(definition.id.clone(), resolved.len());
                    resolved.push(definition);
                }
            }
            EmmyrcGmodScriptedClassScopeEntry::Definition(definition) => {
                let id = definition.id.trim();
                if id.is_empty() {
                    continue;
                }

                if let Some(existing_idx) = index_by_id.get(id).copied() {
                    if definition.disabled.unwrap_or(false) {
                        resolved.remove(existing_idx);
                        index_by_id = resolved
                            .iter()
                            .enumerate()
                            .map(|(idx, definition)| (definition.id.clone(), idx))
                            .collect();
                        continue;
                    }

                    let merged = merge_scripted_class_definition_override(
                        &resolved[existing_idx],
                        definition,
                        legacy_exclude,
                    );
                    resolved[existing_idx] = merged;
                    continue;
                }

                if let Some(definition) =
                    resolve_scripted_class_definition(definition, legacy_exclude)
                {
                    index_by_id.insert(definition.id.clone(), resolved.len());
                    resolved.push(definition);
                }
            }
        }
    }

    let mut seen = HashSet::new();
    resolved.retain(|definition| seen.insert(definition.id.clone()));

    if !legacy_include.is_empty() && !has_definition_entries {
        resolved
            .retain(|definition| definition_matches_legacy_include(definition, &legacy_include));
    }

    resolved
}

fn legacy_include_patterns(entries: &[EmmyrcGmodScriptedClassScopeEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(glob) => Some(glob.trim()),
            EmmyrcGmodScriptedClassScopeEntry::Definition(_) => None,
        })
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_name_list(items: Option<&[String]>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .unwrap_or(&[])
        .iter()
        .filter_map(|item| {
            let trimmed = item.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .filter(|item| seen.insert(item.to_ascii_lowercase()))
        .collect()
}

fn strip_realm_file_prefix(name: &str) -> &str {
    if name.len() > 3 {
        let prefix = &name[..3];
        if matches!(prefix, "sh_" | "sv_" | "cl_") {
            return &name[3..];
        }
    }
    name
}

fn definition_matches_legacy_include(
    definition: &ResolvedGmodScriptedClassDefinition,
    legacy_include: &[String],
) -> bool {
    if legacy_include.is_empty() {
        return true;
    }

    let include_glob = match wax::any(
        legacy_include
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    ) {
        Ok(glob) => glob,
        Err(err) => {
            log::warn!("Invalid legacy gmod.scriptedClassScopes.include pattern: {err}");
            return true;
        }
    };

    scripted_class_definition_candidates(definition)
        .iter()
        .any(|candidate| include_glob.is_match(Path::new(candidate)))
}

fn scripted_class_definition_candidates(
    definition: &ResolvedGmodScriptedClassDefinition,
) -> Vec<String> {
    let mut candidates = Vec::new();
    let definition_path = definition.path.join("/");

    push_scope_candidate(&mut candidates, &definition_path);
    for include in &definition.include {
        push_scope_candidate(&mut candidates, include);
        if let Some(prefix) = include.strip_suffix("/**") {
            push_scope_candidate(&mut candidates, prefix);
            push_scope_candidate(&mut candidates, &format!("{prefix}/example.lua"));
        }
    }

    if !definition_path.is_empty() {
        push_scope_candidate(&mut candidates, &format!("{definition_path}/example.lua"));
        push_scope_candidate(&mut candidates, &format!("lua/{definition_path}"));
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("{definition_path}/example/shared.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example/shared.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("{definition_path}/example/init.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example/init.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("{definition_path}/example/cl_init.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example/cl_init.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("{definition_path}/example/sh_plugin.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example/sh_plugin.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("{definition_path}/example/sv_plugin.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example/sv_plugin.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("{definition_path}/example/cl_plugin.lua"),
        );
        push_scope_candidate(
            &mut candidates,
            &format!("lua/{definition_path}/example/cl_plugin.lua"),
        );
    }

    candidates
}

fn push_scope_candidate(candidates: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || candidates.iter().any(|existing| existing == trimmed) {
        return;
    }

    candidates.push(trimmed.to_string());
}

fn build_scope_candidate_paths(file_path: &Path) -> Vec<String> {
    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let mut candidate_paths = Vec::new();
    push_scope_path_candidates(&mut candidate_paths, &normalized_path);

    let normalized_lower = normalized_path.to_ascii_lowercase();
    if let Some(lua_idx) = normalized_lower.find("/lua/") {
        let lua_relative_path = normalized_path[lua_idx + 1..].to_string();
        push_scope_path_candidates(&mut candidate_paths, &lua_relative_path);
        if let Some(stripped) = lua_relative_path.strip_prefix("lua/") {
            push_scope_path_candidates(&mut candidate_paths, stripped);
        }
    }

    if let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) {
        push_scope_candidate(&mut candidate_paths, file_name);
    }

    candidate_paths
}

fn push_scope_path_candidates(candidate_paths: &mut Vec<String>, path: &str) {
    push_scope_candidate(candidate_paths, path);

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    for idx in 0..segments.len() {
        push_scope_candidate(candidate_paths, &segments[idx..].join("/"));
    }
}

fn matches_scope_patterns(
    file_path: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> bool {
    if include_patterns.is_empty() && exclude_patterns.is_empty() {
        return true;
    }

    let candidate_paths = build_scope_candidate_paths(file_path);

    if !include_patterns.is_empty() {
        let include_set = match wax::any(
            include_patterns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ) {
            Ok(glob) => glob,
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.include pattern: {err}");
                return true;
            }
        };
        if !candidate_paths
            .iter()
            .any(|path| include_set.is_match(Path::new(path)))
        {
            return false;
        }
    }

    if !exclude_patterns.is_empty() {
        let exclude_set = match wax::any(
            exclude_patterns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ) {
            Ok(glob) => glob,
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.exclude pattern: {err}");
                return false;
            }
        };
        if candidate_paths
            .iter()
            .any(|path| exclude_set.is_match(Path::new(path)))
        {
            return false;
        }
    }

    true
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedOwnerEntry {
    pub id: String,
    pub global: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_owner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_owners: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGmodScriptedOwnerDefinition {
    pub id: String,
    pub global: String,
    pub aliases: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub hook_owner: bool,
    pub fallback_owners: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedOwners {
    #[serde(default)]
    pub include: Vec<EmmyrcGmodScriptedOwnerEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
struct PatternSpecificity {
    literal_segs: usize,
    inverse_wildcard_segs: usize,
    literal_chars: usize,
    pattern_len: usize,
}

impl PatternSpecificity {
    fn of(pattern: &str) -> Self {
        let mut literal_segs = 0usize;
        let mut wildcard_segs = 0usize;
        let mut literal_chars = 0usize;
        for segment in pattern.split('/').filter(|segment| !segment.is_empty()) {
            if segment.contains('*') || segment.contains('?') {
                wildcard_segs += 1;
            } else {
                literal_segs += 1;
                literal_chars += segment.len();
            }
        }

        Self {
            literal_segs,
            inverse_wildcard_segs: usize::MAX - wildcard_segs,
            literal_chars,
            pattern_len: pattern.len(),
        }
    }
}

fn resolve_scripted_owner_entry(
    entry: &EmmyrcGmodScriptedOwnerEntry,
) -> Option<ResolvedGmodScriptedOwnerDefinition> {
    if entry.disabled.unwrap_or(false) {
        return None;
    }

    let id = entry.id.trim();
    let global = entry.global.trim();
    if id.is_empty() || global.is_empty() {
        return None;
    }

    let include = normalize_name_list(Some(&entry.include));
    if include.is_empty() {
        return None;
    }

    let exclude = normalize_name_list(entry.exclude.as_deref());
    let aliases = normalize_name_list(entry.aliases.as_deref())
        .into_iter()
        .filter(|alias| alias != global)
        .collect::<Vec<_>>();
    let fallback_owners = normalize_name_list(entry.fallback_owners.as_deref())
        .into_iter()
        .filter(|owner| owner != global && !aliases.iter().any(|alias| alias == owner))
        .collect::<Vec<_>>();

    Some(ResolvedGmodScriptedOwnerDefinition {
        id: id.to_string(),
        global: global.to_string(),
        aliases,
        include,
        exclude,
        hook_owner: entry.hook_owner.unwrap_or(false),
        fallback_owners,
    })
}

fn builtin_hook_owner_fallbacks(owner_name: &str) -> Vec<String> {
    if owner_name.eq_ignore_ascii_case("GM") || owner_name.eq_ignore_ascii_case("GAMEMODE") {
        vec!["SANDBOX".to_string()]
    } else if owner_name.eq_ignore_ascii_case("SANDBOX") {
        vec!["GM".to_string(), "GAMEMODE".to_string()]
    } else {
        Vec::new()
    }
}

fn builtin_hook_owner_candidates(owner_name: &str) -> Vec<String> {
    if owner_name.eq_ignore_ascii_case("GM") || owner_name.eq_ignore_ascii_case("GAMEMODE") {
        vec![
            "GM".to_string(),
            "GAMEMODE".to_string(),
            "SANDBOX".to_string(),
        ]
    } else if owner_name.eq_ignore_ascii_case("SANDBOX") {
        vec![
            "SANDBOX".to_string(),
            "GM".to_string(),
            "GAMEMODE".to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn merge_owner_names_dedup(primary: Vec<String>, secondary: Vec<String>) -> Vec<String> {
    let mut merged = primary;
    let mut seen = merged
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for name in secondary {
        if seen.insert(name.to_ascii_lowercase()) {
            merged.push(name);
        }
    }
    merged
}

impl EmmyrcGmodScriptedOwners {
    pub fn resolved_owners(&self) -> Vec<ResolvedGmodScriptedOwnerDefinition> {
        let mut result = Vec::new();
        let mut seen_ids = HashSet::new();
        for entry in &self.include {
            let id = entry.id.trim();
            if id.is_empty() {
                continue;
            }
            let id_lower = id.to_ascii_lowercase();
            if seen_ids.contains(&id_lower) {
                log::warn!("gmod.scriptedOwners: duplicate id '{id}' - first-valid entry wins");
                continue;
            }
            if let Some(definition) = resolve_scripted_owner_entry(entry) {
                seen_ids.insert(id_lower);
                result.push(definition);
            }
        }
        result
    }

    pub fn detect_owner_for_path(
        &self,
        file_path: &Path,
    ) -> Option<ResolvedGmodScriptedOwnerDefinition> {
        self.detect_owners_for_path_all(file_path)
            .into_iter()
            .next()
    }

    pub fn detect_owners_for_path_all(
        &self,
        file_path: &Path,
    ) -> Vec<ResolvedGmodScriptedOwnerDefinition> {
        let candidate_paths = build_scope_candidate_paths(file_path);
        let mut matches = Vec::new();

        for (idx, definition) in self.resolved_owners().into_iter().enumerate() {
            if !definition.exclude.is_empty()
                && definition.exclude.iter().any(|pattern| {
                    wax::Glob::new(pattern)
                        .map(|glob| {
                            candidate_paths
                                .iter()
                                .any(|path| glob.is_match(Path::new(path)))
                        })
                        .unwrap_or(false)
                })
            {
                continue;
            }

            let best_score = definition
                .include
                .iter()
                .filter_map(|pattern| {
                    let glob = wax::Glob::new(pattern).ok()?;
                    candidate_paths
                        .iter()
                        .any(|path| glob.is_match(Path::new(path)))
                        .then(|| PatternSpecificity::of(pattern))
                })
                .max();

            if let Some(score) = best_score {
                matches.push((score, idx, definition));
            }
        }

        matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        matches
            .into_iter()
            .map(|(_, _, definition)| definition)
            .collect()
    }

    pub fn hook_owner_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        for definition in self.resolved_owners() {
            if !definition.hook_owner {
                continue;
            }
            if seen.insert(definition.global.clone()) {
                names.push(definition.global);
            }
            for alias in definition.aliases {
                if seen.insert(alias.clone()) {
                    names.push(alias);
                }
            }
        }
        names
    }

    pub fn hook_owner_fallbacks_configured(&self, owner_name: &str) -> Option<Vec<String>> {
        self.resolved_owners()
            .into_iter()
            .find(|definition| {
                definition.global.eq_ignore_ascii_case(owner_name)
                    || definition
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(owner_name))
            })
            .map(|definition| {
                merge_owner_names_dedup(
                    definition.fallback_owners,
                    builtin_hook_owner_fallbacks(owner_name),
                )
            })
    }

    pub fn hook_owner_candidates_configured(&self, owner_name: &str) -> Option<Vec<String>> {
        self.resolved_owners()
            .into_iter()
            .find(|definition| {
                definition.global.eq_ignore_ascii_case(owner_name)
                    || definition
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(owner_name))
            })
            .map(|definition| {
                let mut names = vec![definition.global];
                names.extend(definition.aliases);
                names.extend(definition.fallback_owners);
                merge_owner_names_dedup(names, builtin_hook_owner_candidates(owner_name))
            })
    }

    pub fn is_configured_owner_name(&self, name: &str) -> bool {
        self.resolved_owners().into_iter().any(|definition| {
            definition.global.eq_ignore_ascii_case(name)
                || definition
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
        })
    }
}

fn merge_scripted_class_definition_override(
    base: &ResolvedGmodScriptedClassDefinition,
    override_definition: &EmmyrcGmodScriptedClassDefinition,
    legacy_exclude: &[String],
) -> ResolvedGmodScriptedClassDefinition {
    let mut exclude = override_definition
        .exclude
        .clone()
        .unwrap_or_else(|| base.exclude.clone());
    if !legacy_exclude.is_empty() {
        exclude.extend(
            legacy_exclude
                .iter()
                .map(|pattern| pattern.trim())
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_string),
        );
    }

    ResolvedGmodScriptedClassDefinition {
        id: base.id.clone(),
        label: override_definition
            .label
            .clone()
            .unwrap_or_else(|| base.label.clone()),
        path: override_definition
            .path
            .clone()
            .unwrap_or_else(|| base.path.clone()),
        include: override_definition
            .include
            .clone()
            .unwrap_or_else(|| base.include.clone()),
        exclude,
        class_global: override_definition
            .class_global
            .clone()
            .unwrap_or_else(|| base.class_global.clone()),
        fixed_class_name: if override_definition.fixed_class_name.is_some() {
            override_definition
                .fixed_class_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        } else {
            base.fixed_class_name.clone()
        },
        is_global_singleton: override_definition
            .is_global_singleton
            .unwrap_or(base.is_global_singleton),
        strip_file_prefix: override_definition
            .strip_file_prefix
            .unwrap_or(base.strip_file_prefix),
        hide_from_outline: override_definition
            .hide_from_outline
            .unwrap_or(base.hide_from_outline),
        aliases: if override_definition.aliases.is_some() {
            normalize_name_list(override_definition.aliases.as_deref())
        } else {
            base.aliases.clone()
        },
        super_types: if override_definition.super_types.is_some() {
            normalize_name_list(override_definition.super_types.as_deref())
        } else {
            base.super_types.clone()
        },
        hook_owner: override_definition.hook_owner.unwrap_or(base.hook_owner),
        parent_id: if override_definition.parent_id.is_some() {
            override_definition
                .parent_id
                .as_deref()
                .map(str::trim)
                .filter(|parent_id| !parent_id.is_empty())
                .map(str::to_string)
        } else {
            base.parent_id.clone()
        },
        icon: if override_definition.icon.is_some() {
            override_definition
                .icon
                .as_deref()
                .map(str::trim)
                .filter(|icon| !icon.is_empty())
                .map(str::to_string)
        } else {
            base.icon.clone()
        },
        root_dir: override_definition
            .root_dir
            .clone()
            .unwrap_or_else(|| base.root_dir.clone()),
        scaffold: override_definition
            .scaffold
            .clone()
            .or_else(|| base.scaffold.clone()),
        class_name_prefix: if override_definition.class_name_prefix.is_some() {
            override_definition
                .class_name_prefix
                .as_deref()
                .map(str::trim)
                .filter(|prefix| !prefix.is_empty())
                .map(str::to_string)
        } else {
            base.class_name_prefix.clone()
        },
    }
}

impl Default for EmmyrcGmodScriptedClassScopes {
    fn default() -> Self {
        Self {
            include: scripted_scope_include_default(),
            legacy_exclude: Vec::new(),
        }
    }
}

impl EmmyrcGmodScriptedClassScopes {
    pub fn resolved_definitions(&self) -> Vec<ResolvedGmodScriptedClassDefinition> {
        merge_scripted_class_definitions(&self.include, &self.legacy_exclude)
    }

    pub fn include_patterns(&self) -> Vec<String> {
        let legacy_include = legacy_include_patterns(&self.include);
        let has_definition_entries = self
            .include
            .iter()
            .any(|entry| matches!(entry, EmmyrcGmodScriptedClassScopeEntry::Definition(_)));
        if !legacy_include.is_empty() && !has_definition_entries {
            return legacy_include;
        }

        let mut patterns = self
            .resolved_definitions()
            .into_iter()
            .flat_map(|definition| definition.include)
            .collect::<Vec<_>>();
        for pattern in legacy_include {
            if !patterns.iter().any(|existing| existing == &pattern) {
                patterns.push(pattern);
            }
        }

        patterns
    }

    pub fn exclude_patterns(&self) -> Vec<String> {
        self.resolved_definitions()
            .into_iter()
            .flat_map(|definition| definition.exclude)
            .collect()
    }

    /// Returns true if the file matches at least one definition's include
    /// patterns without being excluded by that *same* definition's exclude
    /// patterns.  Unlike the old global exclude check, this prevents a
    /// sibling definition's exclude (e.g. SWEP's `weapons/gmod_tool/stools/**`)
    /// from blocking a file that legitimately belongs to another definition
    /// (e.g. STOOL's `weapons/gmod_tool/stools/**`).
    pub fn is_file_in_scope(&self, file_path: &Path) -> bool {
        let definitions = self.resolved_definitions();
        if definitions.is_empty() {
            return true;
        }

        definitions.iter().any(|definition| {
            matches_scope_patterns(file_path, &definition.include, &definition.exclude)
        })
    }

    pub fn detect_class_for_path(
        &self,
        file_path: &Path,
    ) -> Option<ResolvedGmodScriptedClassMatch> {
        let normalized_path = file_path.to_string_lossy().replace('\\', "/");
        let original_segments = normalized_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let lower_segments = normalized_path
            .to_ascii_lowercase()
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if lower_segments.is_empty() {
            return None;
        }

        let definitions = self.resolved_definitions();
        let mut best_match: Option<(ResolvedGmodScriptedClassDefinition, usize, usize)> = None;
        for definition in &definitions {
            // Check THIS definition's include/exclude patterns — do not merge
            // excludes from other definitions, as they are definition-scoped.
            // E.g. SWEP's "weapons/gmod_tool/stools/**" exclude must not prevent
            // STOOL files from matching the STOOL definition.
            if !matches_scope_patterns(file_path, &definition.include, &definition.exclude) {
                continue;
            }

            let rule_len = definition.path.len();
            if rule_len == 0 || lower_segments.len() < rule_len {
                continue;
            }

            for start_idx in (0..=lower_segments.len() - rule_len).rev() {
                let mut matched = true;
                for (offset, rule_segment) in definition.path.iter().enumerate() {
                    if lower_segments[start_idx + offset] != rule_segment.to_ascii_lowercase() {
                        matched = false;
                        break;
                    }
                }

                if !matched {
                    continue;
                }

                let end_idx = start_idx + rule_len - 1;
                let replace_best = match best_match {
                    None => true,
                    Some((_, best_end_idx, best_rule_len)) => {
                        end_idx > best_end_idx
                            || (end_idx == best_end_idx && rule_len > best_rule_len)
                    }
                };
                if replace_best {
                    best_match = Some((definition.clone(), end_idx, rule_len));
                }

                break;
            }
        }

        if let Some((definition, best_end_idx, _)) = best_match {
            if let Some(class_name) =
                derive_scripted_class_name(&definition, &original_segments, best_end_idx + 1)
            {
                return Some(ResolvedGmodScriptedClassMatch {
                    definition,
                    class_name,
                });
            }
        }

        definitions.into_iter().find_map(|definition| {
            let fixed_name = definition.fixed_class_name.clone()?;
            matches_scope_patterns(file_path, &definition.include, &definition.exclude).then_some(
                ResolvedGmodScriptedClassMatch {
                    definition,
                    class_name: fixed_name,
                },
            )
        })
    }

    pub fn detect_all_class_globals_for_path(&self, file_path: &Path) -> Vec<(String, bool)> {
        let mut seen = HashSet::new();
        self.resolved_definitions()
            .into_iter()
            .filter(|definition| {
                matches_scope_patterns(file_path, &definition.include, &definition.exclude)
            })
            .filter_map(|definition| {
                seen.insert(definition.class_global.to_ascii_lowercase())
                    .then_some((definition.class_global, definition.is_global_singleton))
            })
            .collect()
    }

    pub fn detect_all_scoped_class_matches_for_path(
        &self,
        file_path: &Path,
    ) -> Vec<ResolvedGmodScriptedClassMatch> {
        let normalized_path = file_path.to_string_lossy().replace('\\', "/");
        let original_segments = normalized_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let lower_segments = normalized_path
            .to_ascii_lowercase()
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut matches = Vec::new();
        let mut seen_globals = HashSet::new();

        for definition in self.resolved_definitions() {
            if !matches_scope_patterns(file_path, &definition.include, &definition.exclude) {
                continue;
            }
            if !seen_globals.insert(definition.class_global.to_ascii_lowercase()) {
                continue;
            }

            let class_name = if let Some(fixed_name) = definition.fixed_class_name.clone() {
                Some(fixed_name)
            } else {
                find_scripted_class_path_end(&definition, &lower_segments).and_then(|end_idx| {
                    derive_scripted_class_name(&definition, &original_segments, end_idx + 1)
                })
            };

            if let Some(class_name) = class_name {
                matches.push(ResolvedGmodScriptedClassMatch {
                    definition,
                    class_name,
                });
            }
        }

        matches
    }

    pub fn aliases_for_global(&self, global_name: &str) -> Vec<String> {
        self.resolved_definitions()
            .into_iter()
            .find(|definition| {
                definition.class_global.eq_ignore_ascii_case(global_name)
                    || definition
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(global_name))
            })
            .map(|definition| definition.aliases)
            .unwrap_or_default()
    }

    pub fn super_types_for_global(&self, global_name: &str) -> Vec<String> {
        self.resolved_definitions()
            .into_iter()
            .find(|definition| {
                definition.class_global.eq_ignore_ascii_case(global_name)
                    || definition
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(global_name))
            })
            .map(|definition| definition.super_types)
            .unwrap_or_default()
    }

    pub fn hook_owner_globals(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut globals = Vec::new();
        for definition in self.resolved_definitions() {
            if !definition.hook_owner {
                continue;
            }
            if seen.insert(definition.class_global.clone()) {
                globals.push(definition.class_global);
            }
            for alias in definition.aliases {
                if seen.insert(alias.clone()) {
                    globals.push(alias);
                }
            }
        }
        globals
    }

    pub fn scan_scripted_class_scope_files<'a, T>(
        &self,
        files: impl IntoIterator<Item = (T, &'a Path)>,
    ) -> (HashSet<T>, HashMap<T, ResolvedGmodScriptedClassMatch>)
    where
        T: Copy + Eq + Hash,
    {
        let definitions = self.resolved_definitions();
        if definitions.is_empty() {
            return (HashSet::new(), HashMap::new());
        }

        let compiled_definitions = compile_scope_definitions(&definitions);
        let mut scope_files = HashSet::new();
        let mut matches = HashMap::new();
        for (file_id, file_path) in files {
            let candidate_paths = build_scope_candidate_paths(file_path);
            if !compiled_definitions.iter().any(|definition| {
                matches_compiled_scope_patterns(
                    &candidate_paths,
                    &definition.include,
                    &definition.exclude,
                )
            }) {
                continue;
            }

            scope_files.insert(file_id);
            if let Some(scope_match) = detect_class_for_path_with_compiled_definitions(
                file_path,
                &candidate_paths,
                &compiled_definitions,
            ) {
                matches.insert(file_id, scope_match);
            }
        }

        (scope_files, matches)
    }
}

struct CompiledScopeDefinition<'a> {
    definition: &'a ResolvedGmodScriptedClassDefinition,
    include: ScopePatternSet<'a>,
    exclude: ScopePatternSet<'a>,
}

enum ScopePatternSet<'a> {
    Empty,
    Valid(wax::Any<'a>),
    Invalid,
}

fn find_scripted_class_path_end(
    definition: &ResolvedGmodScriptedClassDefinition,
    lower_segments: &[String],
) -> Option<usize> {
    let rule_len = definition.path.len();
    if rule_len == 0 || lower_segments.len() < rule_len {
        return None;
    }

    for start_idx in (0..=lower_segments.len() - rule_len).rev() {
        let matched = definition
            .path
            .iter()
            .enumerate()
            .all(|(offset, rule_segment)| {
                lower_segments[start_idx + offset] == rule_segment.to_ascii_lowercase()
            });
        if matched {
            return Some(start_idx + rule_len - 1);
        }
    }

    None
}

fn derive_scripted_class_name(
    definition: &ResolvedGmodScriptedClassDefinition,
    original_segments: &[String],
    class_idx: usize,
) -> Option<String> {
    if let Some(fixed_name) = definition.fixed_class_name.clone() {
        return Some(fixed_name);
    }
    if original_segments.is_empty() || class_idx >= original_segments.len() {
        return None;
    }

    let class_name = if definition.strip_file_prefix {
        let raw = original_segments
            .last()?
            .strip_suffix(".lua")
            .unwrap_or(original_segments.last()?.as_str());
        strip_realm_file_prefix(raw).to_string()
    } else if class_idx == original_segments.len() - 1 {
        original_segments[class_idx]
            .strip_suffix(".lua")
            .unwrap_or(original_segments[class_idx].as_str())
            .to_string()
    } else {
        original_segments[class_idx].clone()
    };
    if class_name.is_empty() {
        return None;
    }

    Some(match definition.class_name_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}{class_name}"),
        _ => class_name,
    })
}

fn compile_scope_definitions(
    definitions: &[ResolvedGmodScriptedClassDefinition],
) -> Vec<CompiledScopeDefinition<'_>> {
    definitions
        .iter()
        .map(|definition| CompiledScopeDefinition {
            definition,
            include: compile_scope_patterns(&definition.include, "include"),
            exclude: compile_scope_patterns(&definition.exclude, "exclude"),
        })
        .collect()
}

fn compile_scope_patterns<'a>(patterns: &'a [String], kind: &str) -> ScopePatternSet<'a> {
    if patterns.is_empty() {
        return ScopePatternSet::Empty;
    }

    match wax::any(patterns.iter().map(String::as_str)) {
        Ok(glob) => ScopePatternSet::Valid(glob),
        Err(err) => {
            log::warn!("Invalid gmod.scriptedClassScopes.{kind} pattern: {err}");
            ScopePatternSet::Invalid
        }
    }
}

fn detect_class_for_path_with_compiled_definitions(
    file_path: &Path,
    candidate_paths: &[String],
    definitions: &[CompiledScopeDefinition],
) -> Option<ResolvedGmodScriptedClassMatch> {
    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let original_segments = normalized_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let lower_segments = normalized_path
        .to_ascii_lowercase()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lower_segments.is_empty() {
        return None;
    }

    let mut best_match: Option<(&ResolvedGmodScriptedClassDefinition, usize, usize)> = None;
    for definition in definitions {
        if !matches_compiled_scope_patterns(
            candidate_paths,
            &definition.include,
            &definition.exclude,
        ) {
            continue;
        }

        let rule_len = definition.definition.path.len();
        if rule_len == 0 || lower_segments.len() < rule_len {
            continue;
        }

        for start_idx in (0..=lower_segments.len() - rule_len).rev() {
            let mut matched = true;
            for (offset, rule_segment) in definition.definition.path.iter().enumerate() {
                if lower_segments[start_idx + offset] != rule_segment.to_ascii_lowercase() {
                    matched = false;
                    break;
                }
            }

            if !matched {
                continue;
            }

            let end_idx = start_idx + rule_len - 1;
            let replace_best = match best_match {
                None => true,
                Some((_, best_end_idx, best_rule_len)) => {
                    end_idx > best_end_idx || (end_idx == best_end_idx && rule_len > best_rule_len)
                }
            };
            if replace_best {
                best_match = Some((definition.definition, end_idx, rule_len));
            }

            break;
        }
    }

    if let Some((definition, best_end_idx, _)) = best_match
        && let Some(class_name) =
            derive_scripted_class_name(definition, &original_segments, best_end_idx + 1)
    {
        return Some(ResolvedGmodScriptedClassMatch {
            definition: definition.clone(),
            class_name,
        });
    }

    definitions.iter().find_map(|definition| {
        let fixed_name = definition.definition.fixed_class_name.clone()?;
        matches_compiled_scope_patterns(candidate_paths, &definition.include, &definition.exclude)
            .then(|| ResolvedGmodScriptedClassMatch {
                definition: definition.definition.clone(),
                class_name: fixed_name,
            })
    })
}

fn matches_compiled_scope_patterns(
    candidate_paths: &[String],
    include_patterns: &ScopePatternSet<'_>,
    exclude_patterns: &ScopePatternSet<'_>,
) -> bool {
    match include_patterns {
        ScopePatternSet::Empty => {}
        ScopePatternSet::Valid(include_set) => {
            if !candidate_paths
                .iter()
                .any(|path| include_set.is_match(Path::new(path)))
            {
                return false;
            }
        }
        ScopePatternSet::Invalid => return true,
    }

    match exclude_patterns {
        ScopePatternSet::Empty => {}
        ScopePatternSet::Valid(exclude_set) => {
            if candidate_paths
                .iter()
                .any(|path| exclude_set.is_match(Path::new(path)))
            {
                return false;
            }
        }
        ScopePatternSet::Invalid => return false,
    }

    true
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcGmodOutlineVerbosity {
    /// Show only functions, classes, VGUI panels, hooks, net receivers, timers, concommands.
    Minimal,
    /// Show functions, classes, important tables, hooks, net receivers, timers, concommands, and
    /// non-primitive variables. Hides `if`/`for`/`do` blocks.
    #[default]
    Normal,
    /// Show everything (legacy behavior, includes control-flow blocks and all locals).
    Verbose,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodOutline {
    /// Controls how much detail the document outline shows.
    /// - `minimal`: functions, classes, hooks, net.Receive, timers, concommands only.
    /// - `normal` (default): same as minimal plus non-primitive variables and tables; hides
    ///   control-flow blocks.
    /// - `verbose`: everything including `if`, `for`, `do` blocks and all locals.
    #[serde(default)]
    pub verbosity: EmmyrcGmodOutlineVerbosity,
}

impl Default for EmmyrcGmodOutline {
    fn default() -> Self {
        Self {
            verbosity: EmmyrcGmodOutlineVerbosity::Normal,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodHookMappings {
    #[serde(default)]
    pub method_to_hook: HashMap<String, String>,
    #[serde(default)]
    pub emitter_to_hook: HashMap<String, String>,
    #[serde(default)]
    pub method_prefixes: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodNetwork {
    #[serde(default = "network_enabled_default")]
    pub enabled: bool,
    #[serde(default = "network_code_lens_default")]
    pub code_lens_enabled: bool,
    #[serde(default)]
    pub completion: EmmyrcGmodNetworkCompletion,
}

fn network_enabled_default() -> bool {
    true
}

fn network_code_lens_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetwork {
    fn default() -> Self {
        Self {
            enabled: network_enabled_default(),
            code_lens_enabled: network_code_lens_default(),
            completion: EmmyrcGmodNetworkCompletion::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodNetworkCompletion {
    #[serde(default = "network_smart_read_suggestions_default")]
    pub smart_read_suggestions: bool,
    #[serde(default = "network_mismatch_hints_default")]
    pub mismatch_hints: bool,
}

fn network_smart_read_suggestions_default() -> bool {
    true
}

fn network_mismatch_hints_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetworkCompletion {
    fn default() -> Self {
        Self {
            smart_read_suggestions: network_smart_read_suggestions_default(),
            mismatch_hints: network_mismatch_hints_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    #[gtest]
    fn test_gmod_defaults() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str("{}").or_fail()?;
        let definitions = gmod.scripted_class_scopes.resolved_definitions();
        verify_that!(gmod.enabled, eq(true))?;
        verify_that!(gmod.default_realm, eq(EmmyrcGmodRealm::Shared))?;
        verify_that!(definitions.len(), eq(6usize))?;
        verify_that!(definitions[0].id.as_str(), eq("entities"))?;
        verify_that!(definitions[0].class_global.as_str(), eq("ENT"))?;
        verify_that!(
            definitions[1].exclude.as_slice(),
            eq(&["weapons/gmod_tool/stools/**".to_string()])
        )?;
        verify_that!(definitions[3].parent_id.as_deref(), eq(Some("weapons")))?;
        verify_that!(definitions[4].scaffold.is_none(), eq(true))?;
        verify_that!(definitions[5].id.as_str(), eq("gamemodes"))?;
        verify_that!(definitions[5].class_global.as_str(), eq("GM"))?;
        verify_that!(
            definitions[5].class_name_prefix.as_deref(),
            eq(Some("gamemode_"))
        )?;
        verify_that!(
            gmod.scripted_class_scopes.legacy_exclude.is_empty(),
            eq(true)
        )?;
        verify_that!(gmod.hook_mappings.method_to_hook.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.emitter_to_hook.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.method_prefixes.is_empty(), eq(true))?;
        verify_that!(gmod.network.enabled, eq(true))?;
        verify_that!(gmod.network.code_lens_enabled, eq(true))?;
        verify_that!(gmod.network.completion.smart_read_suggestions, eq(true))?;
        verify_that!(gmod.network.completion.mismatch_hints, eq(true))?;
        verify_that!(gmod.vgui.code_lens_enabled, eq(true))?;
        verify_that!(gmod.vgui.inlay_hint_enabled, eq(false))?;
        verify_that!(
            gmod.file_param_defaults.get("ply"),
            eq(Some(&"Player".to_string()))
        )?;
        verify_that!(
            gmod.file_param_defaults.get("vehicle"),
            eq(Some(&"Entity".to_string()))
        )?;
        verify_that!(gmod.detect_realm_from_filename, eq(None))?;
        verify_that!(gmod.detect_realm_from_calls, eq(None))?;
        verify_that!(gmod.infer_dynamic_fields, eq(true))?;
        verify_that!(gmod.dynamic_fields_global, eq(true))
    }

    #[gtest]
    fn test_scripted_class_definition_deserialize_required_field_edges() -> Result<()> {
        let definition: EmmyrcGmodScriptedClassDefinition = serde_json::from_str(
            r#"{
                "id": "custom",
                "label": null,
                "path": null,
                "classNamePrefix": null,
                "unknown": true
            }"#,
        )
        .or_fail()?;

        verify_that!(definition.id.as_str(), eq("custom"))?;
        verify_that!(definition.label.is_none(), eq(true))?;
        verify_that!(definition.path.is_none(), eq(true))?;
        verify_that!(definition.class_name_prefix.is_none(), eq(true))?;

        let missing_id =
            serde_json::from_str::<EmmyrcGmodScriptedClassDefinition>(r#"{ "label": "Custom" }"#)
                .unwrap_err();
        verify_that!(
            missing_id.to_string(),
            contains_substring("missing field `id`")
        )?;

        let duplicate_id = serde_json::from_str::<EmmyrcGmodScriptedClassDefinition>(
            r#"{ "id": "first", "id": "second" }"#,
        )
        .unwrap_err();
        verify_that!(
            duplicate_id.to_string(),
            contains_substring("duplicate field `id`")
        )
    }

    #[gtest]
    fn test_scripted_class_scaffold_file_deserialize_required_field_edges() -> Result<()> {
        let file: EmmyrcGmodScriptedClassScaffoldFile = serde_json::from_str(
            r#"{
                "path": "{{name}}/shared.lua",
                "template": "ent_shared.lua",
                "unknown": true
            }"#,
        )
        .or_fail()?;

        verify_that!(file.path.as_str(), eq("{{name}}/shared.lua"))?;
        verify_that!(file.template.as_str(), eq("ent_shared.lua"))?;

        let missing_template = serde_json::from_str::<EmmyrcGmodScriptedClassScaffoldFile>(
            r#"{ "path": "{{name}}/shared.lua" }"#,
        )
        .unwrap_err();
        verify_that!(
            missing_template.to_string(),
            contains_substring("missing field `template`")
        )?;

        let duplicate_path = serde_json::from_str::<EmmyrcGmodScriptedClassScaffoldFile>(
            r#"{ "path": "one.lua", "path": "two.lua", "template": "base.lua" }"#,
        )
        .unwrap_err();
        verify_that!(
            duplicate_path.to_string(),
            contains_substring("duplicate field `path`")
        )
    }

    #[gtest]
    fn test_resolved_scripted_class_definition_deserialize_required_field_edges() -> Result<()> {
        let definition: ResolvedGmodScriptedClassDefinition = serde_json::from_str(
            r#"{
                "id": "custom",
                "label": "Custom",
                "path": ["custom"],
                "include": ["custom/**"],
                "exclude": [],
                "classGlobal": "CUSTOM",
                "parentId": null,
                "icon": null,
                "rootDir": "lua/custom",
                "scaffold": null,
                "classNamePrefix": null,
                "unknown": true
            }"#,
        )
        .or_fail()?;

        verify_that!(definition.id.as_str(), eq("custom"))?;
        verify_that!(definition.path.as_slice(), eq(&["custom".to_string()]))?;
        verify_that!(definition.parent_id.is_none(), eq(true))?;
        verify_that!(definition.scaffold.is_none(), eq(true))?;

        let missing_root_dir = serde_json::from_str::<ResolvedGmodScriptedClassDefinition>(
            r#"{
                "id": "custom",
                "label": "Custom",
                "path": ["custom"],
                "include": [],
                "exclude": [],
                "classGlobal": "CUSTOM"
            }"#,
        )
        .unwrap_err();
        verify_that!(
            missing_root_dir.to_string(),
            contains_substring("missing field `rootDir`")
        )?;

        let duplicate_class_global = serde_json::from_str::<ResolvedGmodScriptedClassDefinition>(
            r#"{
                "id": "custom",
                "label": "Custom",
                "path": ["custom"],
                "include": [],
                "exclude": [],
                "classGlobal": "CUSTOM",
                "classGlobal": "OTHER",
                "rootDir": "lua/custom"
            }"#,
        )
        .unwrap_err();
        verify_that!(
            duplicate_class_global.to_string(),
            contains_substring("duplicate field `classGlobal`")
        )
    }

    #[gtest]
    fn test_legacy_include_filters_default_definitions() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": ["plugins/**"]
            }"#,
        )
        .or_fail()?;

        let definitions = scopes.resolved_definitions();
        verify_that!(definitions.len(), eq(1usize))?;
        verify_that!(definitions[0].id.as_str(), eq("plugins"))?;
        verify_that!(
            scopes.include_patterns().as_slice(),
            eq(&["plugins/**".to_string()])
        )
    }

    #[gtest]
    fn test_legacy_include_with_lua_prefix_filters_default_definitions() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": ["lua/entities/**"]
            }"#,
        )
        .or_fail()?;

        let definitions = scopes.resolved_definitions();
        verify_that!(definitions.len(), eq(1usize))?;
        verify_that!(definitions[0].id.as_str(), eq("entities"))?;
        assert_eq!(
            scopes
                .detect_class_for_path(Path::new("lua/entities/TestEntity/shared.lua"))
                .map(|entry| entry.class_name),
            Some("TestEntity".to_string())
        );
        Ok(())
    }

    #[gtest]
    fn test_detect_class_for_path_respects_scope_filters() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": ["entities/**"],
                "exclude": ["entities/tests/**"]
            }"#,
        )
        .or_fail()?;

        let detected = scopes
            .detect_class_for_path(Path::new("lua/entities/test_entity/shared.lua"))
            .map(|entry| entry.class_name);
        assert_eq!(detected, Some("test_entity".to_string()));
        let excluded = scopes.detect_class_for_path(Path::new("lua/entities/tests/shared.lua"));
        assert_eq!(excluded, None);
        Ok(())
    }

    #[gtest]
    fn test_mixed_legacy_and_object_include_keeps_custom_definition() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "myframework-controllers",
                        "label": "Controllers",
                        "classGlobal": "CONTROLLER",
                        "path": ["myframework", "controllers"],
                        "include": ["myframework/controllers/**"],
                        "rootDir": "lua/myframework/controllers"
                    },
                    "legacy/custom_scope/**"
                ]
            }"#,
        )
        .or_fail()?;

        let definitions = scopes.resolved_definitions();
        assert!(
            definitions
                .iter()
                .any(|definition| definition.id == "myframework-controllers")
        );
        assert!(
            scopes
                .include_patterns()
                .iter()
                .any(|pattern| pattern == "legacy/custom_scope/**")
        );
        Ok(())
    }

    #[gtest]
    fn test_detect_class_fixed_class_name_returns_fixed_name() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "helix-schema",
                        "label": "Helix Schema",
                        "classGlobal": "SCHEMA",
                        "fixedClassName": "SCHEMA",
                        "path": ["schema"],
                        "include": ["schema/**", "gamemode/schema.lua"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let result = scopes
            .detect_class_for_path(Path::new("schema/meta/sh_character.lua"))
            .map(|entry| (entry.class_name, entry.definition.class_global));
        assert_eq!(result, Some(("SCHEMA".to_string(), "SCHEMA".to_string())));

        let include_only = scopes
            .detect_class_for_path(Path::new("gamemode/schema.lua"))
            .map(|entry| entry.class_name);
        assert_eq!(include_only, Some("SCHEMA".to_string()));
        Ok(())
    }

    #[gtest]
    fn test_detect_class_strip_file_prefix_for_single_file_classes() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "helix-items",
                        "label": "Helix Items",
                        "classGlobal": "ITEM",
                        "path": ["schema", "items"],
                        "include": ["schema/items/**"],
                        "stripFilePrefix": true
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let result = scopes
            .detect_class_for_path(Path::new("schema/items/books/sh_paper.lua"))
            .map(|entry| entry.class_name);
        assert_eq!(result, Some("paper".to_string()));
        Ok(())
    }

    #[gtest]
    fn test_scripted_class_metadata_normalizes_and_resolves_helpers() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "helix-schema",
                        "label": "Helix Schema",
                        "classGlobal": "SCHEMA",
                        "fixedClassName": "SCHEMA",
                        "path": ["schema"],
                        "include": ["schema/**"],
                        "isGlobalSingleton": true,
                        "hideFromOutline": true,
                        "aliases": [" Schema ", " "],
                        "superTypes": [" GM ", " "],
                        "hookOwner": true
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let definitions = scopes.resolved_definitions();
        let schema = definitions
            .iter()
            .find(|definition| definition.id == "helix-schema")
            .expect("expected helix schema definition");
        verify_that!(schema.fixed_class_name.as_deref(), eq(Some("SCHEMA")))?;
        verify_that!(schema.is_global_singleton, eq(true))?;
        verify_that!(schema.hide_from_outline, eq(true))?;
        verify_that!(schema.aliases.as_slice(), eq(&["Schema".to_string()]))?;
        verify_that!(schema.super_types.as_slice(), eq(&["GM".to_string()]))?;
        verify_that!(schema.hook_owner, eq(true))?;
        verify_that!(
            scopes.aliases_for_global("SCHEMA").as_slice(),
            eq(&["Schema".to_string()])
        )?;
        verify_that!(
            scopes.super_types_for_global("Schema").as_slice(),
            eq(&["GM".to_string()])
        )?;
        let hook_owner_globals = scopes.hook_owner_globals();
        assert!(hook_owner_globals.contains(&"SCHEMA".to_string()));
        assert!(hook_owner_globals.contains(&"Schema".to_string()));
        Ok(())
    }

    #[gtest]
    fn test_detect_all_scoped_class_matches_keeps_overlapping_plugin_scope() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "plugins",
                        "label": "Plugins",
                        "classGlobal": "PLUGIN",
                        "path": ["plugins"],
                        "include": ["plugins/**"]
                    },
                    {
                        "id": "helix-items",
                        "label": "Helix Items",
                        "classGlobal": "ITEM",
                        "path": ["items"],
                        "include": ["plugins/*/items/**"],
                        "stripFilePrefix": true
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let matches = scopes.detect_all_scoped_class_matches_for_path(Path::new(
            "plugins/writing/items/books/sh_paper.lua",
        ));
        let pairs = matches
            .into_iter()
            .map(|entry| (entry.definition.class_global, entry.class_name))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                ("PLUGIN".to_string(), "writing".to_string()),
                ("ITEM".to_string(), "paper".to_string()),
            ]
        );
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_resolve_candidates_and_fallbacks() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "schema",
                        "global": "SCHEMA",
                        "aliases": ["Schema", "schema"],
                        "include": ["schema/**"],
                        "hookOwner": true,
                        "fallbackOwners": ["GM"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        verify_that!(
            owners.hook_owner_names().as_slice(),
            eq(&["SCHEMA".to_string(), "Schema".to_string()])
        )?;
        verify_that!(
            owners
                .hook_owner_candidates_configured("Schema")
                .unwrap()
                .as_slice(),
            eq(&["SCHEMA".to_string(), "Schema".to_string(), "GM".to_string()])
        )?;
        verify_that!(
            owners
                .hook_owner_fallbacks_configured("SCHEMA")
                .unwrap()
                .as_slice(),
            eq(&["GM".to_string()])
        )
    }

    #[gtest]
    fn test_detect_class_for_path_preserves_original_case() -> Result<()> {
        let scopes = EmmyrcGmodScriptedClassScopes::default();
        let detected = scopes
            .detect_class_for_path(Path::new("lua/entities/MyEntity/shared.lua"))
            .map(|entry| entry.class_name);
        assert_eq!(detected, Some("MyEntity".to_string()));
        Ok(())
    }

    #[gtest]
    fn test_gmod_network_camel_case_keys() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str(
            r#"{
                "network": {
                    "enabled": false,
                    "completion": {
                        "smartReadSuggestions": false,
                        "mismatchHints": false
                    }
                },
                "vgui": {
                    "codeLensEnabled": false,
                    "inlayHintEnabled": true
                }
            }"#,
        )
        .or_fail()?;

        verify_that!(gmod.network.enabled, eq(false))?;
        verify_that!(gmod.network.completion.smart_read_suggestions, eq(false))?;
        verify_that!(gmod.network.completion.mismatch_hints, eq(false))?;
        verify_that!(gmod.vgui.code_lens_enabled, eq(false))?;
        verify_that!(gmod.vgui.inlay_hint_enabled, eq(true))
    }

    #[gtest]
    fn test_detect_class_for_path_stool_default_scopes() -> Result<()> {
        let scopes = EmmyrcGmodScriptedClassScopes::default();

        // Standard lua-root path
        let result =
            scopes.detect_class_for_path(Path::new("lua/weapons/gmod_tool/stools/hoverball.lua"));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("TOOL"))?;
        verify_that!(match_.class_name.as_str(), eq("hoverball"))?;

        // Gamemode-nested path (e.g. gamemodes/sandbox/entities/weapons/...)
        let result = scopes.detect_class_for_path(Path::new(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stools/hoverball.lua",
        ));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("TOOL"))?;
        verify_that!(match_.class_name.as_str(), eq("hoverball"))
    }

    #[gtest]
    fn test_stool_not_matched_as_swep() -> Result<()> {
        let scopes = EmmyrcGmodScriptedClassScopes::default();

        // STOOL files must be classified as TOOL, not SWEP
        let result =
            scopes.detect_class_for_path(Path::new("lua/weapons/gmod_tool/stools/rope.lua"));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("TOOL"))?;
        verify_that!(match_.definition.id.as_str(), eq("stools"))?;

        // Regular SWEP files must still be classified as SWEP
        let result =
            scopes.detect_class_for_path(Path::new("lua/weapons/weapon_pistol/shared.lua"));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("SWEP"))?;
        verify_that!(match_.definition.id.as_str(), eq("weapons"))
    }

    #[gtest]
    fn test_is_file_in_scope_stool() -> Result<()> {
        let scopes = EmmyrcGmodScriptedClassScopes::default();

        // STOOL files should be considered in scope
        verify_that!(
            scopes.is_file_in_scope(Path::new("lua/weapons/gmod_tool/stools/hoverball.lua")),
            eq(true)
        )?;

        // SWEP files should also be in scope
        verify_that!(
            scopes.is_file_in_scope(Path::new("lua/weapons/weapon_pistol/shared.lua")),
            eq(true)
        )?;

        // gmod_tool weapon itself should be in scope as SWEP
        verify_that!(
            scopes.is_file_in_scope(Path::new("lua/weapons/gmod_tool/init.lua")),
            eq(true)
        )?;

        // Random files should not be in scope
        verify_that!(
            scopes.is_file_in_scope(Path::new("lua/random/file.lua")),
            eq(false)
        )
    }

    #[gtest]
    fn test_gmod_tool_weapon_detected_as_swep() -> Result<()> {
        let scopes = EmmyrcGmodScriptedClassScopes::default();

        // lua/weapons/gmod_tool/init.lua — the Sword Tool Gun weapon itself
        let result = scopes.detect_class_for_path(Path::new("lua/weapons/gmod_tool/init.lua"));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("SWEP"))?;
        verify_that!(match_.class_name.as_str(), eq("gmod_tool"))?;

        // lua/weapons/gmod_tool/shared.lua — shared SWEP file
        let result = scopes.detect_class_for_path(Path::new("lua/weapons/gmod_tool/shared.lua"));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("SWEP"))?;
        verify_that!(match_.class_name.as_str(), eq("gmod_tool"))?;

        // gamemodes/sandbox/entities/weapons/gmod_tool/init.lua — gamemode-nested SWEP
        let result = scopes.detect_class_for_path(Path::new(
            "gamemodes/sandbox/entities/weapons/gmod_tool/init.lua",
        ));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("SWEP"))?;
        verify_that!(match_.class_name.as_str(), eq("gmod_tool"))?;

        // STOOL files must still be classified as TOOL
        let result =
            scopes.detect_class_for_path(Path::new("lua/weapons/gmod_tool/stools/hoverball.lua"));
        verify_that!(result.is_some(), eq(true))?;
        let match_ = result.unwrap();
        verify_that!(match_.definition.class_global.as_str(), eq("TOOL"))?;
        verify_that!(match_.class_name.as_str(), eq("hoverball"))
    }

    #[gtest]
    fn test_file_param_defaults_merge_workspace_overrides_and_removals() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str(
            r#"{
                "fileParamDefaults": {
                    " vehicle ": "  base_glide  ",
                    " ply ": " "
                }
            }"#,
        )
        .or_fail()?;

        verify_that!(
            gmod.file_param_defaults.get("vehicle"),
            eq(Some(&"base_glide".to_string()))
        )?;
        verify_that!(gmod.file_param_defaults.contains_key("ply"), eq(false))?;
        verify_that!(
            gmod.file_param_defaults.get("ent"),
            eq(Some(&"Entity".to_string()))
        )?;
        verify_that!(
            gmod.file_param_defaults.get("func"),
            eq(Some(&"function".to_string()))
        )
    }
}
