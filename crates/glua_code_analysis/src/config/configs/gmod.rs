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
        EmmyrcGmodScriptedClassScopeEntry::Definition(default_scripted_class_definition(
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
        )),
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

    pub fn detect_class_for_path(
        &self,
        file_path: &Path,
    ) -> Option<ResolvedGmodScriptedClassMatch> {
        if !matches_scope_patterns(
            file_path,
            &self.include_patterns(),
            &self.exclude_patterns(),
        ) {
            return None;
        }

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
        for definition in definitions {
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

        let (definition, best_end_idx, _) = best_match?;
        let class_idx = best_end_idx + 1;
        if class_idx >= lower_segments.len() {
            return None;
        }

        let class_name = if class_idx == original_segments.len() - 1 {
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

        let class_name = match definition.class_name_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}{class_name}"),
            _ => class_name,
        };

        Some(ResolvedGmodScriptedClassMatch {
            definition,
            class_name,
        })
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
            eq(&["weapons/gmod_tool/**".to_string()])
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
}
