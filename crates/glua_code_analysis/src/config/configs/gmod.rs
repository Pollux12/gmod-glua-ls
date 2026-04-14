use std::collections::{HashMap, HashSet};
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
    /// Path to GMod annotations to load as core library.
    /// When set to empty string or not provided, uses VSCode extension's auto-downloaded annotations (if enabled).
    /// Set to explicit path to override, or use `autoLoadAnnotations: false` in .gluarc to disable entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations_path: Option<String>,
    /// Disable auto-loading of annotations (from VSCode or default path).
    /// This takes precedence over extension settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_load_annotations: Option<bool>,
    /// Path to a folder containing custom GLua scaffolding templates (`.lua` files).
    /// Built-in templates are used as fallback when a custom one is not found.
    /// Accepts an absolute path or a path relative to the workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_path: Option<String>,
    /// Automatically detect and add the base gamemode as a library when a gamemode
    /// derives from another (via the `"base"` field in the gamemode `.txt` file).
    /// Set to `false` to disable this detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_detect_gamemode_base: Option<bool>,
    /// Configures additional hook-owner globals beyond the built-in
    /// `GM` / `GAMEMODE` / `SANDBOX` / `PLUGIN` set.
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

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
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
    /// When set, every file matched by this scope's include patterns resolves to
    /// this fixed class name instead of deriving a name from the next path segment.
    /// Use for singleton / global scripted classes like `SCHEMA` in Helix where
    /// there is only one class instance for the whole directory tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_class_name: Option<String>,
    /// When `true`, the `classGlobal` variable is a workspace-wide singleton that should be
    /// registered as a global (accessible from any file), not a per-file local.
    /// Use for globals like `Schema` in Helix that are set once and persist globally,
    /// similar to how `GM` works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_global_singleton: Option<bool>,
    /// When `true`, the `sh_`, `sv_`, and `cl_` realm prefixes are stripped from
    /// single-file class names when the class name is derived from the filename.
    /// For example, `sh_administrator.lua` becomes `administrator` instead of
    /// `sh_administrator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_file_prefix: Option<bool>,
    /// When `true`, this scope definition is excluded from the outline/class
    /// explorer tree view. Useful for global singletons like `Schema` that
    /// should not appear as a folder in the explorer because there is only
    /// ever one instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_from_outline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaffold: Option<EmmyrcGmodScriptedClassScaffold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassScaffold {
    #[serde(default)]
    pub files: Vec<EmmyrcGmodScriptedClassScaffoldFile>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassScaffoldFile {
    pub path: String,
    pub template: String,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedGmodScriptedClassDefinition {
    pub id: String,
    pub label: String,
    pub path: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub class_global: String,
    /// When set, every file matched by this scope's include patterns uses this
    /// fixed class name rather than one derived from the next path segment.
    /// Used for singleton / global scripted classes like `SCHEMA` in Helix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_class_name: Option<String>,
    /// When `true`, the `classGlobal` variable is workspace-wide: registered as a global
    /// declaration accessible from any file in the workspace.
    #[serde(default)]
    pub is_global_singleton: bool,
    /// When `true`, realm prefixes (`sh_`, `sv_`, `cl_`) are stripped from single-file
    /// class names derived from filenames.
    #[serde(default)]
    pub strip_file_prefix: bool,
    /// When `true`, this scope is hidden from the outline/class explorer tree.
    #[serde(default)]
    pub hide_from_outline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub root_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaffold: Option<EmmyrcGmodScriptedClassScaffold>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGmodScriptedClassMatch {
    pub definition: ResolvedGmodScriptedClassDefinition,
    pub class_name: String,
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
            &["weapons/gmod_tool/**"],
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
        fixed_class_name: None,
        parent_id: parent_id.map(str::to_string),
        icon: icon.map(str::to_string),
        root_dir: root_dir.map(str::to_string),
        scaffold,
        disabled: None,
        is_global_singleton: None,
        strip_file_prefix: None,
        hide_from_outline: None,
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
        format!("lua/{}", path.join("/"))
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
                    parent_id: None,
                    icon: None,
                    root_dir: None,
                    scaffold: None,
                    disabled: None,
                    is_global_singleton: None,
                    strip_file_prefix: None,
                    hide_from_outline: None,
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

/// Strip the Helix/Garry's Mod realm file prefix (`sh_`, `sv_`, `cl_`) from a filename stem.
/// Returns the remainder after the prefix, or the original string if no prefix is present.
fn strip_realm_file_prefix(name: &str) -> &str {
    if name.len() > 3 {
        let prefix = &name[..3];
        if prefix == "sh_" || prefix == "sv_" || prefix == "cl_" {
            return &name[3..];
        }
    }
    name
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

// =========================================================================
// scriptedOwners
// =========================================================================

/// A single entry in `gmod.scriptedOwners.include`.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedOwnerEntry {
    /// Unique identifier for this entry (used for duplicate detection).
    pub id: String,
    /// Primary global name (e.g. `"GM"`).
    pub global: String,
    /// Additional names that resolve to the same owner type (e.g. `["GAMEMODE"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// Glob patterns for files that belong to this owner (required, at least one).
    pub include: Vec<String>,
    /// Glob patterns for files to exclude from this owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// Whether methods on this global are hook entry points used when resolving
    /// `hook.Add` callback parameters.  Defaults to `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_owner: Option<bool>,
    /// Other owner globals to fall back to when looking up hook docs or completing
    /// member access (e.g. `["SANDBOX"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_owners: Option<Vec<String>>,
    /// Set to `true` to suppress this entry without removing it from the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

/// The resolved, normalized form of a `scriptedOwners` entry.
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

/// Configuration for `gmod.scriptedOwners`.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedOwners {
    /// List of scripted-owner entries.  Empty by default; add entries to describe
    /// additional hook-owner globals beyond the built-in
    /// `GM` / `GAMEMODE` / `SANDBOX` / `PLUGIN` set.
    #[serde(default)]
    pub include: Vec<EmmyrcGmodScriptedOwnerEntry>,
}

/// Specificity score for a single glob pattern used by [`EmmyrcGmodScriptedOwners::detect_owner_for_path`].
///
/// Ordering criteria (applied in priority order):
/// 1. `literal_segs`  — more literal (non-wildcard) segments is better (desc).
/// 2. `wildcard_segs` — fewer wildcard segments is better (asc).
/// 3. `literal_chars` — more literal characters is better (desc).
/// 4. `pattern_len`   — longer total pattern is better (desc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PatternSpecificity {
    literal_segs: usize,
    wildcard_segs: usize,
    literal_chars: usize,
    pattern_len: usize,
}

impl PatternSpecificity {
    fn of(pattern: &str) -> Self {
        let mut literal_segs = 0usize;
        let mut wildcard_segs = 0usize;
        let mut literal_chars = 0usize;
        for seg in pattern.split('/').filter(|s| !s.is_empty()) {
            if seg.contains('*') || seg.contains('?') {
                wildcard_segs += 1;
            } else {
                literal_segs += 1;
                literal_chars += seg.len();
            }
        }
        PatternSpecificity {
            literal_segs,
            wildcard_segs,
            literal_chars,
            pattern_len: pattern.len(),
        }
    }
}

impl PartialOrd for PatternSpecificity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PatternSpecificity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.literal_segs
            .cmp(&other.literal_segs)
            // wildcard_segs: asc → invert comparison
            .then(other.wildcard_segs.cmp(&self.wildcard_segs))
            .then(self.literal_chars.cmp(&other.literal_chars))
            .then(self.pattern_len.cmp(&other.pattern_len))
    }
}

fn resolve_scripted_owner_entry(
    entry: &EmmyrcGmodScriptedOwnerEntry,
) -> Option<ResolvedGmodScriptedOwnerDefinition> {
    if entry.disabled.unwrap_or(false) {
        return None;
    }

    let id = entry.id.trim();
    if id.is_empty() {
        return None;
    }

    let global = entry.global.trim().to_string();
    if global.is_empty() {
        return None;
    }

    let include: Vec<String> = entry
        .include
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if include.is_empty() {
        return None;
    }

    let exclude: Vec<String> = entry
        .exclude
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Aliases: trim, dedupe, drop if equal to global (case-insensitive)
    let mut seen_aliases: HashSet<String> = HashSet::new();
    let aliases: Vec<String> = entry
        .aliases
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(&global))
        .filter(|s| seen_aliases.insert(s.to_ascii_lowercase()))
        .collect();

    let hook_owner = entry.hook_owner.unwrap_or(false);

    // Fallback owners: trim, dedupe, drop if == global or any alias (case-insensitive)
    let mut seen_fb: HashSet<String> = HashSet::new();
    let fallback_owners: Vec<String> = entry
        .fallback_owners
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| {
            !s.is_empty()
                && !s.eq_ignore_ascii_case(&global)
                && !aliases.iter().any(|a| a.eq_ignore_ascii_case(s))
        })
        .filter(|s| seen_fb.insert(s.to_ascii_lowercase()))
        .collect();

    Some(ResolvedGmodScriptedOwnerDefinition {
        id: id.to_string(),
        global,
        aliases,
        include,
        exclude,
        hook_owner,
        fallback_owners,
    })
}

/// Built-in fallback owner names used for hover / semantic-decl resolution.
///
/// These mirror the defaults in `build_hover.rs :: gmod_hook_owner_fallbacks` and are
/// used by [`EmmyrcGmodScriptedOwners::hook_owner_fallbacks_configured`] to merge
/// configured fallbacks with the built-in set so that a conservative `scriptedOwners`
/// entry never silently drops essential documentation sources.
fn builtin_hook_owner_fallbacks(owner_name: &str) -> Vec<String> {
    if owner_name.eq_ignore_ascii_case("GM") || owner_name.eq_ignore_ascii_case("GAMEMODE") {
        vec!["SANDBOX".to_string()]
    } else if owner_name.eq_ignore_ascii_case("PLUGIN") || owner_name.eq_ignore_ascii_case("SCHEMA")
    {
        vec![
            "GM".to_string(),
            "GAMEMODE".to_string(),
            "SANDBOX".to_string(),
        ]
    } else if owner_name.eq_ignore_ascii_case("SANDBOX") {
        vec!["GM".to_string(), "GAMEMODE".to_string()]
    } else {
        vec![]
    }
}

/// Built-in completion candidate owner names used for member-access completion.
///
/// These mirror the defaults in `member_provider.rs :: gmod_hook_owner_candidates` and are
/// used by [`EmmyrcGmodScriptedOwners::hook_owner_candidates_configured`] to merge
/// configured candidates with the built-in set so that a conservative `scriptedOwners`
/// entry never silently drops essential completion sources.
fn builtin_hook_owner_candidates(owner_name: &str) -> Vec<String> {
    if owner_name.eq_ignore_ascii_case("GM") || owner_name.eq_ignore_ascii_case("GAMEMODE") {
        vec![
            "GM".to_string(),
            "GAMEMODE".to_string(),
            "SANDBOX".to_string(),
        ]
    } else if owner_name.eq_ignore_ascii_case("PLUGIN") || owner_name.eq_ignore_ascii_case("SCHEMA")
    {
        vec![
            owner_name.to_string(),
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
        vec![]
    }
}

/// Returns a deduplicated list of owner names where items from `primary` come first,
/// followed by items from `secondary` that are not already present (case-insensitive).
/// Original casing from both lists is preserved (first occurrence wins).
fn merge_owner_names_dedup(primary: Vec<String>, secondary: Vec<String>) -> Vec<String> {
    let mut result = primary;
    let mut seen: HashSet<String> = result.iter().map(|s| s.to_ascii_lowercase()).collect();
    for name in secondary {
        if seen.insert(name.to_ascii_lowercase()) {
            result.push(name);
        }
    }
    result
}

impl EmmyrcGmodScriptedOwners {
    /// Returns the normalized list of active scripted-owner definitions,
    /// with disabled entries removed and duplicate ids (first-valid-wins).
    ///
    /// Duplicate detection is based on the first *successfully resolved* entry
    /// for a given id. An invalid earlier entry (e.g. missing `global` or
    /// `include`) does **not** block a later valid one with the same id.
    pub fn resolved_owners(&self) -> Vec<ResolvedGmodScriptedOwnerDefinition> {
        let mut result = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        for entry in &self.include {
            let id = entry.id.trim();
            if id.is_empty() {
                continue;
            }
            let id_lower = id.to_ascii_lowercase();
            // Only block duplicates of ids that were already *successfully* resolved.
            // An invalid prior entry must not prevent a valid later one from being used.
            if seen_ids.contains(&id_lower) {
                log::warn!(
                    "gmod.scriptedOwners: duplicate id '{}' — first-valid entry wins",
                    id
                );
                continue;
            }
            if let Some(def) = resolve_scripted_owner_entry(entry) {
                seen_ids.insert(id_lower);
                result.push(def);
            }
        }

        result
    }

    /// Detects which configured owner (if any) best matches the given file path.
    ///
    /// Uses normalized candidate paths (same as `scriptedClassScopes`) and
    /// deterministic specificity scoring.  When two entries score equally the
    /// one declared first wins (declaration-order asc).
    ///
    /// This is a convenience wrapper around [`detect_owners_for_path_all`] that
    /// returns only the single best match.  Prefer the `_all` variant when
    /// multiple overlapping owners must all be considered (e.g. for
    /// undefined-global suppression).
    pub fn detect_owner_for_path(
        &self,
        file_path: &Path,
    ) -> Option<ResolvedGmodScriptedOwnerDefinition> {
        self.detect_owners_for_path_all(file_path)
            .into_iter()
            .next()
    }

    /// Returns **all** configured owners whose include-patterns match the given
    /// file path, ordered best-match first (highest specificity score, with
    /// ties broken by declaration order — first-declared wins).
    ///
    /// Use this when a file may legitimately be covered by several overlapping
    /// owner globs and all matching globals/aliases should be considered (e.g.
    /// suppressing undefined-global diagnostics).  The first element, when
    /// present, is identical to what [`detect_owner_for_path`] returns.
    pub fn detect_owners_for_path_all(
        &self,
        file_path: &Path,
    ) -> Vec<ResolvedGmodScriptedOwnerDefinition> {
        if self.include.is_empty() {
            return Vec::new();
        }

        let candidate_paths = build_scope_candidate_paths(file_path);
        // Collect (score, declaration_index, definition) for all matching entries.
        let mut matches: Vec<(
            PatternSpecificity,
            usize,
            ResolvedGmodScriptedOwnerDefinition,
        )> = Vec::new();

        for (decl_idx, def) in self.resolved_owners().into_iter().enumerate() {
            // Exclusion check (per-pattern, invalid patterns warned and skipped)
            if !def.exclude.is_empty() {
                let excluded = def
                    .exclude
                    .iter()
                    .any(|pat| match wax::Glob::new(pat.as_str()) {
                        Ok(glob) => candidate_paths.iter().any(|p| glob.is_match(Path::new(p))),
                        Err(e) => {
                            log::warn!(
                                "gmod.scriptedOwners exclude pattern '{}' is invalid: {}",
                                pat,
                                e
                            );
                            false
                        }
                    });
                if excluded {
                    continue;
                }
            }

            // Find best include pattern score for this entry (invalid patterns warned+skipped)
            let mut entry_best: Option<PatternSpecificity> = None;
            for pat in &def.include {
                match wax::Glob::new(pat.as_str()) {
                    Ok(glob) => {
                        if candidate_paths.iter().any(|p| glob.is_match(Path::new(p))) {
                            let score = PatternSpecificity::of(pat);
                            entry_best = Some(match entry_best {
                                None => score,
                                Some(existing) => existing.max(score),
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "gmod.scriptedOwners include pattern '{}' is invalid: {}",
                            pat,
                            e
                        );
                    }
                }
            }

            let Some(entry_score) = entry_best else {
                continue;
            };

            matches.push((entry_score, decl_idx, def));
        }

        // Sort: highest specificity first (descending); ties broken by
        // declaration index ascending so first-declared wins on equal scores.
        matches.sort_by(|a, b| {
            b.0.cmp(&a.0) // score descending
                .then(a.1.cmp(&b.1)) // decl_idx ascending
        });

        matches.into_iter().map(|(_, _, def)| def).collect()
    }

    /// Returns all global names and aliases from entries where `hookOwner` is `true`.
    /// These extend the set of owner types checked when resolving `hook.Add` callbacks.
    pub fn hook_owner_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for def in self.resolved_owners() {
            if !def.hook_owner {
                continue;
            }
            if seen.insert(def.global.to_ascii_lowercase()) {
                names.push(def.global.clone());
            }
            for alias in &def.aliases {
                if seen.insert(alias.to_ascii_lowercase()) {
                    names.push(alias.clone());
                }
            }
        }
        names
    }

    /// Returns the configured `fallbackOwners` for the given owner name, merged with
    /// any built-in defaults, or `None` if no entry claims this name (caller should
    /// use built-in fallback logic).
    ///
    /// Configured fallbacks come first (declaration order); built-in defaults for
    /// well-known owners (GM / GAMEMODE / SANDBOX / PLUGIN) are appended so that a
    /// conservative configured entry that omits `fallbackOwners` still preserves the
    /// expected hover / doc-resolution behaviour.  Deduplication is case-insensitive.
    pub fn hook_owner_fallbacks_configured(&self, owner_name: &str) -> Option<Vec<String>> {
        for def in self.resolved_owners() {
            if def.global.eq_ignore_ascii_case(owner_name)
                || def
                    .aliases
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(owner_name))
            {
                let merged = merge_owner_names_dedup(
                    def.fallback_owners.clone(),
                    builtin_hook_owner_fallbacks(owner_name),
                );
                return Some(merged);
            }
        }
        None
    }

    /// Returns the configured completion candidates (self + aliases + fallbacks) for the given
    /// owner name, merged with any built-in defaults, or `None` if no entry claims this name
    /// (caller should use built-in fallback logic).
    ///
    /// Configured names come first (global → aliases → fallbackOwners); built-in candidates for
    /// well-known owners (GM / GAMEMODE / SANDBOX / PLUGIN) are appended so that a conservative
    /// configured entry that omits aliases / fallbackOwners still surfaces all expected
    /// completions.  Deduplication is case-insensitive.
    pub fn hook_owner_candidates_configured(&self, owner_name: &str) -> Option<Vec<String>> {
        for def in self.resolved_owners() {
            if def.global.eq_ignore_ascii_case(owner_name)
                || def
                    .aliases
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(owner_name))
            {
                let mut candidates: Vec<String> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                seen.insert(def.global.to_ascii_lowercase());
                candidates.push(def.global.clone());
                for alias in &def.aliases {
                    if seen.insert(alias.to_ascii_lowercase()) {
                        candidates.push(alias.clone());
                    }
                }
                for fb in &def.fallback_owners {
                    if seen.insert(fb.to_ascii_lowercase()) {
                        candidates.push(fb.clone());
                    }
                }
                let merged =
                    merge_owner_names_dedup(candidates, builtin_hook_owner_candidates(owner_name));
                return Some(merged);
            }
        }
        None
    }

    /// Returns `true` if the given name is the `global` or any `alias` of any configured entry.
    pub fn is_configured_owner_name(&self, name: &str) -> bool {
        self.resolved_owners().iter().any(|def| {
            def.global.eq_ignore_ascii_case(name)
                || def.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
        })
    }
}

// =========================================================================

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
        is_global_singleton: override_definition
            .is_global_singleton
            .unwrap_or(base.is_global_singleton),
        strip_file_prefix: override_definition
            .strip_file_prefix
            .unwrap_or(base.strip_file_prefix),
        hide_from_outline: override_definition
            .hide_from_outline
            .unwrap_or(base.hide_from_outline),
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

        // ── Pass 1: path-segment matching ─────────────────────────────────────
        // Find the definition whose path pattern best matches a contiguous run
        // of segments in the file's path.  Longer / later matches win.
        let mut best_match: Option<(ResolvedGmodScriptedClassDefinition, usize, usize)> = None;
        for definition in &definitions {
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
            // Fixed-class-name: return immediately with the fixed name (no path derivation).
            if let Some(fixed_name) = definition.fixed_class_name.clone() {
                return Some(ResolvedGmodScriptedClassMatch {
                    definition,
                    class_name: fixed_name,
                });
            }

            // Path-derived name: the class name is derived from the path after the
            // matched scope segment.
            let class_idx = best_end_idx + 1;
            if class_idx >= lower_segments.len() {
                // No segment after the matched path — cannot derive a class name via path.
                // Fall through to the include-pattern fallback below.
            } else {
                // Scopes with `strip_file_prefix` use single-file classes where the
                // name comes from the filename (e.g. items/<category>/sh_paper.lua
                // → "paper"), not from the directory name after the scope path.
                // Scopes without it use directory-per-class (e.g. entities/<name>/
                // → name from the directory).
                let class_name = if definition.strip_file_prefix {
                    // Single-file class: name always comes from the filename (last segment)
                    let last_idx = original_segments.len() - 1;
                    let raw = original_segments[last_idx]
                        .strip_suffix(".lua")
                        .unwrap_or(original_segments[last_idx].as_str());
                    let stripped = strip_realm_file_prefix(raw);
                    stripped.to_string()
                } else if class_idx == original_segments.len() - 1 {
                    // Directory-per-class at the filename level: strip .lua extension
                    let raw = original_segments[class_idx]
                        .strip_suffix(".lua")
                        .unwrap_or(original_segments[class_idx].as_str());
                    raw.to_string()
                } else {
                    original_segments[class_idx].clone()
                };
                if !class_name.is_empty() {
                    return Some(ResolvedGmodScriptedClassMatch {
                        definition,
                        class_name,
                    });
                }
            }
        }

        // ── Pass 2: include-pattern fallback for fixed-class-name definitions ─
        // Handles files that are covered by a scope's include patterns but do
        // not have a segment hierarchy that path-matching can use (e.g.
        // `gamemode/schema.lua` covered by `include: ['gamemode/schema.lua']`
        // with `fixedClassName: 'SCHEMA'`).
        for definition in &definitions {
            let Some(ref fixed_name) = definition.fixed_class_name else {
                continue;
            };
            if matches_scope_patterns(file_path, &definition.include, &definition.exclude) {
                return Some(ResolvedGmodScriptedClassMatch {
                    definition: definition.clone(),
                    class_name: fixed_name.clone(),
                });
            }
        }

        None
    }

    /// Returns the `classGlobal` names of ALL scope definitions whose include
    /// patterns match the given file path. This is used to suppress
    /// undefined-global diagnostics for globals that belong to *any* matching
    /// scope, not just the primary (best-match) one.
    ///
    /// For example, `plugins/writing/items/writing/sh_paper.lua` matches both
    /// the `plugins` scope (classGlobal `PLUGIN`) and the `items` scope
    /// (classGlobal `ITEM`). The primary scope is `items` (later path
    /// segment), but `PLUGIN` should also be suppressed as a valid global.
    pub fn detect_all_class_globals_for_path(&self, file_path: &Path) -> Vec<(String, bool)> {
        let definitions = self.resolved_definitions();
        let mut globals = Vec::new();
        let mut seen = HashSet::new();
        for definition in &definitions {
            if !matches_scope_patterns(file_path, &definition.include, &definition.exclude) {
                continue;
            }
            if seen.insert(definition.class_global.to_ascii_lowercase()) {
                globals.push((
                    definition.class_global.clone(),
                    definition.is_global_singleton,
                ));
            }
        }
        globals
    }

    /// Returns ALL scope matches for a file path, not just the best (primary) one.
    /// Each match includes the derived class_name and the scope definition.
    /// This is used for multi-scope files where multiple classGlobal variables
    /// need proper type declarations (e.g., both PLUGIN and ITEM in a file
    /// inside `plugins/*/items/`).
    pub fn detect_all_scoped_class_matches_for_path(
        &self,
        file_path: &Path,
    ) -> Vec<ResolvedGmodScriptedClassMatch> {
        let definitions = self.resolved_definitions();
        let mut matches = Vec::new();
        let mut seen_globals = HashSet::new();

        for definition in &definitions {
            if !matches_scope_patterns(file_path, &definition.include, &definition.exclude) {
                continue;
            }
            // Skip duplicate classGlobal names (first match wins per global)
            if !seen_globals.insert(definition.class_global.to_ascii_lowercase()) {
                continue;
            }

            // Fixed-class-name: use directly
            if let Some(fixed_name) = &definition.fixed_class_name {
                matches.push(ResolvedGmodScriptedClassMatch {
                    definition: definition.clone(),
                    class_name: fixed_name.clone(),
                });
                continue;
            }

            // Derive class name from path segments (same logic as detect_class_for_path)
            let normalized_path = file_path.to_string_lossy().replace('\\', "/");
            let original_segments: Vec<&str> = normalized_path
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            let lower_segments: Vec<String> = normalized_path
                .to_ascii_lowercase()
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();

            let rule_len = definition.path.len();
            if rule_len == 0 || lower_segments.len() < rule_len {
                continue;
            }

            // Find the LAST occurrence of the scope's path segments in the file path
            let mut best_start: Option<usize> = None;
            for start_idx in (0..=lower_segments.len() - rule_len).rev() {
                let mut matched = true;
                for (offset, rule_segment) in definition.path.iter().enumerate() {
                    if lower_segments[start_idx + offset] != rule_segment.to_ascii_lowercase() {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    best_start = Some(start_idx);
                    break;
                }
            }

            let Some(start_idx) = best_start else {
                continue;
            };
            let end_idx = start_idx + rule_len - 1;

            // Derive class name
            if definition.strip_file_prefix {
                // Single-file class: name comes from the filename (last segment)
                let last_idx = original_segments.len() - 1;
                let raw = original_segments[last_idx]
                    .strip_suffix(".lua")
                    .unwrap_or(original_segments[last_idx]);
                let class_name = strip_realm_file_prefix(raw).to_string();
                if !class_name.is_empty() {
                    matches.push(ResolvedGmodScriptedClassMatch {
                        definition: definition.clone(),
                        class_name,
                    });
                }
            } else {
                let class_idx = end_idx + 1;
                if class_idx < original_segments.len() {
                    let class_name = if class_idx == original_segments.len() - 1 {
                        let raw = original_segments[class_idx]
                            .strip_suffix(".lua")
                            .unwrap_or(original_segments[class_idx]);
                        raw.to_string()
                    } else {
                        original_segments[class_idx].to_string()
                    };
                    if !class_name.is_empty() {
                        matches.push(ResolvedGmodScriptedClassMatch {
                            definition: definition.clone(),
                            class_name,
                        });
                    }
                }
            }
        }

        matches
    }
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
    #[serde(default)]
    pub diagnostics: EmmyrcGmodNetworkDiagnostics,
    #[serde(default)]
    pub completion: EmmyrcGmodNetworkCompletion,
}

fn network_enabled_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetwork {
    fn default() -> Self {
        Self {
            enabled: network_enabled_default(),
            diagnostics: EmmyrcGmodNetworkDiagnostics::default(),
            completion: EmmyrcGmodNetworkCompletion::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodNetworkDiagnostics {
    #[serde(default = "network_type_mismatch_default")]
    pub type_mismatch: bool,
    #[serde(default = "network_order_mismatch_default")]
    pub order_mismatch: bool,
    #[serde(default = "network_missing_counterpart_default")]
    pub missing_counterpart: bool,
}

fn network_type_mismatch_default() -> bool {
    true
}

fn network_order_mismatch_default() -> bool {
    true
}

fn network_missing_counterpart_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetworkDiagnostics {
    fn default() -> Self {
        Self {
            type_mismatch: network_type_mismatch_default(),
            order_mismatch: network_order_mismatch_default(),
            missing_counterpart: network_missing_counterpart_default(),
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
        verify_that!(definitions.len(), eq(4usize))?;
        verify_that!(definitions[0].id.as_str(), eq("entities"))?;
        verify_that!(definitions[0].class_global.as_str(), eq("ENT"))?;
        verify_that!(
            definitions[1].exclude.as_slice(),
            eq(&["weapons/gmod_tool/**".to_string()])
        )?;
        verify_that!(definitions[3].parent_id.as_deref(), eq(Some("weapons")))?;
        // definitions[3] is stools — verify it has a scaffold
        verify_that!(definitions[3].scaffold.is_some(), eq(true))?;
        verify_that!(
            gmod.scripted_class_scopes.legacy_exclude.is_empty(),
            eq(true)
        )?;
        verify_that!(gmod.hook_mappings.method_to_hook.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.emitter_to_hook.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.method_prefixes.is_empty(), eq(true))?;
        verify_that!(gmod.network.enabled, eq(true))?;
        verify_that!(gmod.network.diagnostics.type_mismatch, eq(true))?;
        verify_that!(gmod.network.diagnostics.order_mismatch, eq(true))?;
        verify_that!(gmod.network.diagnostics.missing_counterpart, eq(true))?;
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
    fn test_legacy_include_filters_default_definitions() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": ["entities/**"]
            }"#,
        )
        .or_fail()?;

        let definitions = scopes.resolved_definitions();
        verify_that!(definitions.len(), eq(1usize))?;
        verify_that!(definitions[0].id.as_str(), eq("entities"))?;
        verify_that!(
            scopes.include_patterns().as_slice(),
            eq(&["entities/**".to_string()])
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
    fn test_detect_class_for_path_preserves_original_case() -> Result<()> {
        let scopes = EmmyrcGmodScriptedClassScopes::default();
        let detected = scopes
            .detect_class_for_path(Path::new("lua/entities/MyEntity/shared.lua"))
            .map(|entry| entry.class_name);
        assert_eq!(detected, Some("MyEntity".to_string()));
        Ok(())
    }

    // =========================================================================
    // fixed_class_name tests
    // =========================================================================

    /// A scope with `fixedClassName` always returns the fixed name regardless of
    /// which file is matched, rather than deriving the class name from the path.
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

        // Files inside schema/ use the fixed class name, not a path-derived name.
        let result = scopes
            .detect_class_for_path(Path::new("schema/sh_schema.lua"))
            .map(|m| (m.class_name, m.definition.class_global));
        assert_eq!(result, Some(("SCHEMA".to_string(), "SCHEMA".to_string())));

        // Nested files also use the fixed class name.
        let nested = scopes
            .detect_class_for_path(Path::new("schema/meta/sh_character.lua"))
            .map(|m| m.class_name);
        assert_eq!(nested, Some("SCHEMA".to_string()));

        Ok(())
    }

    /// When `fixedClassName` is set, a file that only matches via the include
    /// pattern (not via path-segment matching) still resolves to the fixed name.
    /// This handles cases like `gamemode/schema.lua` for Helix schemas.
    #[gtest]
    fn test_detect_class_fixed_class_name_fallback_via_include() -> Result<()> {
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

        // gamemode/schema.lua is covered by include but has no 'schema' path segment.
        // It should still resolve to the fixed class name via the include-pattern fallback.
        let result = scopes
            .detect_class_for_path(Path::new("gamemode/schema.lua"))
            .map(|m| m.class_name);
        assert_eq!(result, Some("SCHEMA".to_string()));

        Ok(())
    }

    /// A fixed-class-name scope does NOT override a more-specific path-derived
    /// scope. E.g. schema/factions/foo.lua must resolve via the factions scope,
    /// not via the broader SCHEMA scope.
    #[gtest]
    fn test_detect_class_fixed_class_name_loses_to_more_specific_path() -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "helix-schema",
                        "label": "Helix Schema",
                        "classGlobal": "SCHEMA",
                        "fixedClassName": "SCHEMA",
                        "path": ["schema"],
                        "include": ["schema/**"]
                    },
                    {
                        "id": "helix-factions",
                        "label": "Helix Factions",
                        "classGlobal": "FACTION",
                        "path": ["schema", "factions"],
                        "include": ["schema/factions/**"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        // schema/factions/hl2_resistance.lua belongs to FACTION, not SCHEMA.
        let result = scopes
            .detect_class_for_path(Path::new("schema/factions/hl2_resistance.lua"))
            .map(|m| (m.class_name, m.definition.class_global));
        assert_eq!(
            result,
            Some(("hl2_resistance".to_string(), "FACTION".to_string()))
        );

        // schema/sh_schema.lua is NOT inside factions so SCHEMA wins.
        let schema_file = scopes
            .detect_class_for_path(Path::new("schema/sh_schema.lua"))
            .map(|m| m.definition.class_global);
        assert_eq!(schema_file, Some("SCHEMA".to_string()));

        Ok(())
    }

    #[gtest]
    fn test_detect_class_for_path_skips_more_specific_definition_outside_its_include_domain()
    -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "helix-schema",
                        "label": "Helix Schema",
                        "classGlobal": "SCHEMA",
                        "fixedClassName": "SCHEMA",
                        "path": ["schema"],
                        "include": ["schema/**"]
                    },
                    {
                        "id": "helix-factions",
                        "label": "Helix Factions",
                        "classGlobal": "FACTION",
                        "fixedClassName": "ix_Faction",
                        "path": ["schema", "factions"],
                        "include": ["schema/factions/special/**"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let result = scopes
            .detect_class_for_path(Path::new("schema/factions/regular.lua"))
            .map(|m| (m.class_name, m.definition.class_global));
        assert_eq!(result, Some(("SCHEMA".to_string(), "SCHEMA".to_string())));

        Ok(())
    }

    #[gtest]
    fn test_detect_class_for_path_skips_more_specific_definition_outside_its_exclude_boundary()
    -> Result<()> {
        let scopes: EmmyrcGmodScriptedClassScopes = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "helix-schema",
                        "label": "Helix Schema",
                        "classGlobal": "SCHEMA",
                        "fixedClassName": "SCHEMA",
                        "path": ["schema"],
                        "include": ["schema/**"]
                    },
                    {
                        "id": "helix-factions",
                        "label": "Helix Factions",
                        "classGlobal": "FACTION",
                        "fixedClassName": "ix_Faction",
                        "path": ["schema", "factions"],
                        "include": ["schema/factions/**"],
                        "exclude": ["schema/factions/blocked/**"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let result = scopes
            .detect_class_for_path(Path::new("schema/factions/blocked/ota.lua"))
            .map(|m| (m.class_name, m.definition.class_global));
        assert_eq!(result, Some(("SCHEMA".to_string(), "SCHEMA".to_string())));

        Ok(())
    }

    #[gtest]
    fn test_gmod_network_camel_case_keys() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str(
            r#"{
                "network": {
                    "enabled": false,
                    "diagnostics": {
                        "typeMismatch": false,
                        "orderMismatch": false,
                        "missingCounterpart": false
                    },
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
        verify_that!(gmod.network.diagnostics.type_mismatch, eq(false))?;
        verify_that!(gmod.network.diagnostics.order_mismatch, eq(false))?;
        verify_that!(gmod.network.diagnostics.missing_counterpart, eq(false))?;
        verify_that!(gmod.network.completion.smart_read_suggestions, eq(false))?;
        verify_that!(gmod.network.completion.mismatch_hints, eq(false))?;
        verify_that!(gmod.vgui.code_lens_enabled, eq(false))?;
        verify_that!(gmod.vgui.inlay_hint_enabled, eq(true))
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
        )
    }

    // =========================================================================
    // scriptedOwners tests
    // =========================================================================

    #[gtest]
    fn test_scripted_owners_default_empty() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str("{}").or_fail()?;
        verify_that!(gmod.scripted_owners.include.is_empty(), eq(true))?;
        verify_that!(gmod.scripted_owners.resolved_owners().is_empty(), eq(true))
    }

    #[gtest]
    fn test_scripted_owners_normalization_skips_invalid() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "", "global": "GM", "include": ["gamemodes/**"] },
                    { "id": "a", "global": "", "include": ["gamemodes/**"] },
                    { "id": "b", "global": "GM", "include": [] },
                    { "id": "c", "global": "GM", "include": ["  "] },
                    { "id": "valid", "global": "GM", "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        verify_that!(resolved[0].id.as_str(), eq("valid"))
    }

    #[gtest]
    fn test_scripted_owners_disabled_entry_skipped() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "a", "global": "GM", "include": ["gamemodes/**"], "disabled": true },
                    { "id": "b", "global": "GM2", "include": ["other/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        verify_that!(resolved[0].id.as_str(), eq("b"))
    }

    #[gtest]
    fn test_scripted_owners_duplicate_id_first_valid_wins() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "gm", "global": "FIRST_GM", "include": ["gamemodes/first/**"] },
                    { "id": "gm", "global": "SECOND_GM", "include": ["gamemodes/second/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        verify_that!(resolved[0].global.as_str(), eq("FIRST_GM"))
    }

    /// Regression test for the bug where `seen_ids` was populated *before*
    /// validation, so an invalid first entry with a given id would block a
    /// later valid entry with the same id.
    ///
    /// Expected behaviour: first-*valid*-wins, not first-*seen*-wins.
    #[gtest]
    fn test_scripted_owners_duplicate_id_invalid_first_valid_second_resolved() -> Result<()> {
        // First entry has the id "gm" but an empty `global` — invalid.
        // Second entry has the same id "gm" with a valid `global` — should be kept.
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "gm", "global": "", "include": ["gamemodes/**"] },
                    { "id": "gm", "global": "GM", "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?; // valid second entry must be resolved
        verify_that!(resolved[0].id.as_str(), eq("gm"))?;
        verify_that!(resolved[0].global.as_str(), eq("GM"))
    }

    /// Companion to the regression test above: when the first entry *is* valid,
    /// the second entry with the same id must be dropped (first-valid-wins).
    #[gtest]
    fn test_scripted_owners_duplicate_id_valid_first_blocks_valid_second() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "gm", "global": "FIRST_GM",  "include": ["gamemodes/first/**"] },
                    { "id": "gm", "global": "SECOND_GM", "include": ["gamemodes/second/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        verify_that!(resolved[0].global.as_str(), eq("FIRST_GM")) // first valid entry must win; second must be dropped
    }

    #[gtest]
    fn test_scripted_owners_self_alias_and_fallback_dropped() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["gm", "GAMEMODE", "gm"],
                        "include": ["gamemodes/**"],
                        "fallbackOwners": ["gm", "GM", "SANDBOX"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        // "gm" drops as self-alias (case-insensitive match with "GM")
        // Duplicate "gm" also deduped — only "GAMEMODE" survives
        verify_that!(
            resolved[0].aliases.as_slice(),
            eq(&["GAMEMODE".to_string()])
        )?;
        // "gm"/"GM" drops as self-fallback; "GAMEMODE" drops as alias; only "SANDBOX" survives
        verify_that!(
            resolved[0].fallback_owners.as_slice(),
            eq(&["SANDBOX".to_string()])
        )
    }

    #[gtest]
    fn test_scripted_owners_detect_owner_for_path_basic() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let detected =
            owners.detect_owner_for_path(Path::new("gamemodes/my-gamemode/gamemode.lua"));
        assert!(detected.is_some());
        assert_eq!(detected.unwrap().global, "GM");

        let not_detected = owners.detect_owner_for_path(Path::new("lua/entities/myent/shared.lua"));
        assert!(not_detected.is_none());
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_detect_owner_exclude_overrides_include() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "include": ["gamemodes/**"],
                        "exclude": ["gamemodes/base_sandbox/**"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let included = owners.detect_owner_for_path(Path::new("gamemodes/my-gamemode/shared.lua"));
        assert!(included.is_some());

        let excluded =
            owners.detect_owner_for_path(Path::new("gamemodes/base_sandbox/gamemode/shared.lua"));
        assert!(excluded.is_none());
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_specificity_more_literal_segs_wins() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "broad", "global": "BROAD", "include": ["gamemodes/**"] },
                    { "id": "specific", "global": "SPECIFIC", "include": ["gamemodes/my-gm/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let detected =
            owners.detect_owner_for_path(Path::new("gamemodes/my-gm/gamemode/shared.lua"));
        assert!(detected.is_some());
        assert_eq!(detected.unwrap().global, "SPECIFIC");
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_specificity_declaration_order_tiebreak() -> Result<()> {
        // Both patterns are equally specific — first-declared entry should win.
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "first", "global": "FIRST", "include": ["gamemodes/**"] },
                    { "id": "second", "global": "SECOND", "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let detected = owners.detect_owner_for_path(Path::new("gamemodes/any-gamemode/shared.lua"));
        assert_eq!(detected.map(|d| d.global), Some("FIRST".to_string()));
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_hook_owner_names() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"],
                        "hookOwner": true
                    },
                    {
                        "id": "notahook",
                        "global": "NOTAHOOK",
                        "include": ["other/**"],
                        "hookOwner": false
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let names = owners.hook_owner_names();
        verify_that!(names.as_slice(), contains_each!["GM", "GAMEMODE"])?;
        verify_that!(names.iter().any(|n| n == "NOTAHOOK"), eq(false))
    }

    #[gtest]
    fn test_scripted_owners_fallbacks_configured_match() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"],
                        "fallbackOwners": ["SANDBOX"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        // Should return configured fallbacks when matched by global
        assert_eq!(
            owners.hook_owner_fallbacks_configured("GM"),
            Some(vec!["SANDBOX".to_string()])
        );
        // Should also match by alias
        assert_eq!(
            owners.hook_owner_fallbacks_configured("GAMEMODE"),
            Some(vec!["SANDBOX".to_string()])
        );
        // Unknown name returns None (caller uses built-in fallback)
        assert_eq!(owners.hook_owner_fallbacks_configured("PLUGIN"), None);
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_candidates_configured() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"],
                        "fallbackOwners": ["SANDBOX"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let candidates = owners.hook_owner_candidates_configured("GM");
        assert_eq!(
            candidates,
            Some(vec![
                "GM".to_string(),
                "GAMEMODE".to_string(),
                "SANDBOX".to_string()
            ])
        );
        Ok(())
    }

    #[gtest]
    fn test_scripted_owners_is_configured_owner_name() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        verify_that!(owners.is_configured_owner_name("GM"), eq(true))?;
        verify_that!(owners.is_configured_owner_name("gm"), eq(true))?; // case-insensitive
        verify_that!(owners.is_configured_owner_name("GAMEMODE"), eq(true))?;
        verify_that!(owners.is_configured_owner_name("SANDBOX"), eq(false))
    }

    #[gtest]
    fn test_scripted_owners_trimming() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "  gm  ",
                        "global": "  GM  ",
                        "aliases": ["  GAMEMODE  ", "  "],
                        "include": ["  gamemodes/**  "],
                        "fallbackOwners": ["  SANDBOX  ", "  "]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let resolved = owners.resolved_owners();
        verify_that!(resolved.len(), eq(1usize))?;
        verify_that!(resolved[0].id.as_str(), eq("gm"))?;
        verify_that!(resolved[0].global.as_str(), eq("GM"))?;
        verify_that!(
            resolved[0].aliases.as_slice(),
            eq(&["GAMEMODE".to_string()])
        )?;
        verify_that!(
            resolved[0].include.as_slice(),
            eq(&["gamemodes/**".to_string()])
        )?;
        verify_that!(
            resolved[0].fallback_owners.as_slice(),
            eq(&["SANDBOX".to_string()])
        )
    }

    #[gtest]
    fn test_pattern_specificity_ordering() -> Result<()> {
        // More literal segments = more specific
        let broad = PatternSpecificity::of("gamemodes/**");
        let specific = PatternSpecificity::of("gamemodes/my-gm/**");
        verify_that!(specific > broad, eq(true))?;

        // Same literal segs, fewer wildcards = more specific
        let no_wildcard = PatternSpecificity::of("gamemodes/my-gm/shared.lua");
        verify_that!(no_wildcard > specific, eq(true))?;

        // Tie on segs+wildcards: more literal chars wins
        let shorter = PatternSpecificity::of("a/b/**");
        let longer = PatternSpecificity::of("ab/bc/**");
        verify_that!(longer > shorter, eq(true))
    }

    // =========================================================================
    // detect_owners_for_path_all tests
    // =========================================================================

    #[gtest]
    fn test_detect_owners_for_path_all_returns_empty_when_no_match() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "gm", "global": "GM", "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let matches = owners.detect_owners_for_path_all(Path::new("lua/entities/myent/shared.lua"));
        verify_that!(matches.len(), eq(0usize))
    }

    #[gtest]
    fn test_detect_owners_for_path_all_single_match() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "gm", "global": "GM", "aliases": ["GAMEMODE"], "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let matches = owners.detect_owners_for_path_all(Path::new("gamemodes/my-gm/gamemode.lua"));
        verify_that!(matches.len(), eq(1usize))?;
        verify_that!(matches[0].global.as_str(), eq("GM"))
    }

    #[gtest]
    fn test_detect_owners_for_path_all_overlapping_globs_returns_multiple() -> Result<()> {
        // Two entries whose include globs both cover the same file path.
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "broad", "global": "BROAD", "include": ["gamemodes/**"] },
                    { "id": "specific", "global": "SPECIFIC", "include": ["gamemodes/my-gm/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let matches = owners.detect_owners_for_path_all(Path::new("gamemodes/my-gm/gamemode.lua"));
        // Both entries match — we must get both back.
        verify_that!(matches.len(), eq(2usize))?;
        // Best match (more specific glob) must come first.
        verify_that!(matches[0].global.as_str(), eq("SPECIFIC"))?;
        verify_that!(matches[1].global.as_str(), eq("BROAD"))
    }

    #[gtest]
    fn test_detect_owners_for_path_all_best_match_first_equals_single_detect() -> Result<()> {
        // The first element of `_all` must always equal what `detect_owner_for_path` returns.
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "broad",    "global": "BROAD",    "include": ["gamemodes/**"] },
                    { "id": "specific", "global": "SPECIFIC", "include": ["gamemodes/my-gm/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let path = Path::new("gamemodes/my-gm/gamemode.lua");
        let single = owners.detect_owner_for_path(path);
        let all = owners.detect_owners_for_path_all(path);

        assert!(single.is_some());
        assert!(!all.is_empty());
        verify_that!(all[0].global.as_str(), eq(single.unwrap().global.as_str()))
    }

    #[gtest]
    fn test_detect_owners_for_path_all_declaration_order_tiebreak_on_equal_score() -> Result<()> {
        // Equal specificity globs — first-declared must appear first in the result.
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    { "id": "first",  "global": "FIRST",  "include": ["gamemodes/**"] },
                    { "id": "second", "global": "SECOND", "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        let matches = owners.detect_owners_for_path_all(Path::new("gamemodes/any/shared.lua"));
        verify_that!(matches.len(), eq(2usize))?;
        verify_that!(matches[0].global.as_str(), eq("FIRST"))?;
        verify_that!(matches[1].global.as_str(), eq("SECOND"))
    }

    #[gtest]
    fn test_detect_owners_for_path_all_excluded_entry_not_returned() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "include": ["gamemodes/**"],
                        "exclude": ["gamemodes/base_sandbox/**"]
                    },
                    { "id": "extra", "global": "EXTRA", "include": ["gamemodes/**"] }
                ]
            }"#,
        )
        .or_fail()?;

        // A file under base_sandbox should be excluded from "gm" but not "extra".
        let matches = owners
            .detect_owners_for_path_all(Path::new("gamemodes/base_sandbox/gamemode/shared.lua"));
        verify_that!(matches.len(), eq(1usize))?;
        verify_that!(matches[0].global.as_str(), eq("EXTRA"))
    }

    // =========================================================================
    // Built-in fallback preservation tests
    // =========================================================================

    /// A configured GM entry with hookOwner=true but no fallbackOwners must still
    /// surface the built-in SANDBOX fallback for hover/doc resolution.
    #[gtest]
    fn test_hook_owner_fallbacks_configured_builtin_gm_no_fallbacks_preserves_builtins()
    -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"],
                        "hookOwner": true
                    }
                ]
            }"#,
        )
        .or_fail()?;

        // No fallbackOwners configured — built-in SANDBOX fallback must be preserved
        assert_eq!(
            owners.hook_owner_fallbacks_configured("GM"),
            Some(vec!["SANDBOX".to_string()])
        );
        // Also matches by alias
        assert_eq!(
            owners.hook_owner_fallbacks_configured("GAMEMODE"),
            Some(vec!["SANDBOX".to_string()])
        );
        Ok(())
    }

    /// A configured PLUGIN entry with hookOwner=true but no fallbackOwners must still
    /// surface the built-in GM/GAMEMODE/SANDBOX fallbacks for hover/doc resolution.
    #[gtest]
    fn test_hook_owner_fallbacks_configured_builtin_plugin_no_fallbacks_preserves_builtins()
    -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "plugin",
                        "global": "PLUGIN",
                        "include": ["plugins/**"],
                        "hookOwner": true
                    }
                ]
            }"#,
        )
        .or_fail()?;

        assert_eq!(
            owners.hook_owner_fallbacks_configured("PLUGIN"),
            Some(vec![
                "GM".to_string(),
                "GAMEMODE".to_string(),
                "SANDBOX".to_string()
            ])
        );
        Ok(())
    }

    /// A configured PLUGIN entry with hookOwner=true but no fallbackOwners must still
    /// produce the full built-in completion candidate list for member-access completion.
    #[gtest]
    fn test_hook_owner_candidates_configured_builtin_plugin_no_fallbacks_preserves_builtins()
    -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "plugin",
                        "global": "PLUGIN",
                        "include": ["plugins/**"],
                        "hookOwner": true
                    }
                ]
            }"#,
        )
        .or_fail()?;

        assert_eq!(
            owners.hook_owner_candidates_configured("PLUGIN"),
            Some(vec![
                "PLUGIN".to_string(),
                "GM".to_string(),
                "GAMEMODE".to_string(),
                "SANDBOX".to_string()
            ])
        );
        Ok(())
    }

    /// Extra configured fallbackOwners must come first, with built-in defaults appended.
    #[gtest]
    fn test_hook_owner_fallbacks_configured_extra_fallbacks_merged_with_builtins() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "plugin",
                        "global": "PLUGIN",
                        "include": ["plugins/**"],
                        "hookOwner": true,
                        "fallbackOwners": ["MYBASE"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        // MYBASE (configured) comes first; built-in GM/GAMEMODE/SANDBOX follow
        assert_eq!(
            owners.hook_owner_fallbacks_configured("PLUGIN"),
            Some(vec![
                "MYBASE".to_string(),
                "GM".to_string(),
                "GAMEMODE".to_string(),
                "SANDBOX".to_string()
            ])
        );
        Ok(())
    }

    /// No duplicate owner names in the candidates list even when configured entry
    /// already includes names that are in the built-in candidate set.
    #[gtest]
    fn test_hook_owner_candidates_configured_no_duplicate_owner_names() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "gm",
                        "global": "GM",
                        "aliases": ["GAMEMODE"],
                        "include": ["gamemodes/**"],
                        "fallbackOwners": ["SANDBOX"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        let candidates = owners.hook_owner_candidates_configured("GM").unwrap();
        // Each name must appear exactly once
        let unique: std::collections::HashSet<String> = candidates.iter().cloned().collect();
        assert_eq!(
            candidates.len(),
            unique.len(),
            "duplicate owner names in candidates: {:?}",
            candidates
        );
        assert_eq!(
            candidates,
            vec![
                "GM".to_string(),
                "GAMEMODE".to_string(),
                "SANDBOX".to_string()
            ]
        );
        Ok(())
    }

    /// A custom (non-built-in) owner should only have its own configured fallbacks — no
    /// built-in defaults to merge in.
    #[gtest]
    fn test_hook_owner_fallbacks_configured_custom_owner_no_builtin_merge() -> Result<()> {
        let owners: EmmyrcGmodScriptedOwners = serde_json::from_str(
            r#"{
                "include": [
                    {
                        "id": "mymod",
                        "global": "MYMOD",
                        "include": ["mymod/**"],
                        "hookOwner": true,
                        "fallbackOwners": ["MYBASE"]
                    }
                ]
            }"#,
        )
        .or_fail()?;

        // MYMOD is not a built-in owner: only configured fallbacks, no built-ins merged
        assert_eq!(
            owners.hook_owner_fallbacks_configured("MYMOD"),
            Some(vec!["MYBASE".to_string()])
        );
        Ok(())
    }
}
