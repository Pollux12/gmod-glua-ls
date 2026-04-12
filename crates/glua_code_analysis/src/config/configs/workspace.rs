use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcWorkspace {
    /// Ignore directories.
    #[serde(default)]
    pub ignore_dir: Vec<String>,
    /// Default directories/globs to ignore. Accepts strings (legacy, replaces built-ins) or
    /// objects with `id`, optional `label`, `glob`, and `disabled`. Objects start from the
    /// built-in list and apply overrides: `disabled: true` removes a built-in by id; a new id
    /// adds a custom default; an existing id with a `glob` changes the built-in's pattern.
    /// Set `useDefaultIgnores` to `false` to suppress all resolved defaults at runtime.
    #[serde(default = "ignore_dir_defaults")]
    pub ignore_dir_defaults: Vec<IgnoreDirDefaultEntry>,
    /// Whether to apply default ignore directories. Set to false to disable
    /// default exclusions like E2 files and test directories.
    #[serde(default = "use_default_ignores_default")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub use_default_ignores: bool,
    /// Whether multi-root workspaces should remain isolated.
    ///
    /// When false, workspace configs are merged into one global baseline
    /// (first workspace config wins scalar conflicts, arrays are unioned), while
    /// local workspace configs can still override file-scoped behavior.
    #[serde(default = "enable_isolation_default")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub enable_isolation: bool,
    /// Ignore globs. eg: ["**/*.lua"]
    #[serde(default)]
    pub ignore_globs: Vec<String>,
    #[serde(default)]
    /// Library paths. Can be a string path or an object with path and ignore rules.
    /// eg: ["/usr/local/share/lua/5.1"] or [{"path": "/usr/local/share/lua/5.1", "ignoreDir": ["test"], "ignoreGlobs": ["**/*.spec.lua"]}]
    pub library: Vec<EmmyLibraryItem>,
    #[serde(default)]
    /// Package directories. Treat the parent directory as a `library`, but only add files from the specified directory.
    /// eg: `/usr/local/share/lua/5.1/module`
    pub package_dirs: Vec<String>,
    #[serde(default)]
    /// Workspace roots. eg: ["src", "test"]
    pub workspace_roots: Vec<String>,
    /// Encoding. eg: "utf-8"
    #[serde(default = "encoding_default")]
    pub encoding: String,
    /// Module map. key is regex, value is new module regex
    /// eg: {
    ///     "^(.*)$": "module_$1"
    ///     "^lib(.*)$": "script$1"
    /// }
    #[serde(default)]
    pub module_map: Vec<EmmyrcWorkspaceModuleMap>,
    /// Delay between changing a file and full project reindex, in milliseconds.
    #[serde(default = "reindex_duration_default")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub reindex_duration: u64,
    /// Enable full project reindex after changing a file.
    #[serde(default = "enable_reindex_default")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub enable_reindex: bool,
}

impl Default for EmmyrcWorkspace {
    fn default() -> Self {
        Self {
            ignore_dir: Vec::new(),
            ignore_dir_defaults: ignore_dir_defaults(),
            use_default_ignores: true,
            enable_isolation: enable_isolation_default(),
            ignore_globs: Vec::new(),
            library: Vec::new(),
            package_dirs: Vec::new(),
            workspace_roots: Vec::new(),
            encoding: encoding_default(),
            module_map: Vec::new(),
            reindex_duration: 5000,
            enable_reindex: false,
        }
    }
}

impl EmmyrcWorkspace {
    /// Resolve `ignore_dir_defaults` entries to a flat list of glob patterns.
    ///
    /// - If the list contains **only** legacy string entries, they replace the built-ins
    ///   (original behaviour preserved for backward compatibility).
    /// - If the list contains **any** object entry, we start from the built-in defaults and
    ///   apply each entry in order:
    ///   - Object with a known `id` + `disabled: true` → removes that built-in.
    ///   - Object with a known `id` + optional `glob` → replaces that built-in's glob pattern.
    ///   - Object with an unknown `id` and a `glob` → appends a new entry.
    ///   - String entries mixed in alongside objects are appended as-is.
    pub fn resolve_ignore_dir_defaults(&self) -> Vec<String> {
        let has_object_entry = self
            .ignore_dir_defaults
            .iter()
            .any(|e| matches!(e, IgnoreDirDefaultEntry::Definition(_)));

        if !has_object_entry {
            // Legacy path: pure string list replaces built-ins entirely.
            return self
                .ignore_dir_defaults
                .iter()
                .filter_map(|e| match e {
                    IgnoreDirDefaultEntry::LegacyGlob(s) => {
                        let t = s.trim();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t.to_string())
                        }
                    }
                    IgnoreDirDefaultEntry::Definition(_) => None,
                })
                .collect();
        }

        // Object path: start from built-ins, apply overrides.
        let mut entries: Vec<(String, String)> = builtin_ignore_dir_defaults()
            .into_iter()
            .map(|d| (d.id.to_string(), d.glob.to_string()))
            .collect();

        for entry in &self.ignore_dir_defaults {
            match entry {
                IgnoreDirDefaultEntry::LegacyGlob(s) => {
                    let t = s.trim();
                    if !t.is_empty() && !entries.iter().any(|(_, g)| g == t) {
                        entries.push((t.to_string(), t.to_string()));
                    }
                }
                IgnoreDirDefaultEntry::Definition(def) => {
                    let id = def.id.trim();
                    if id.is_empty() {
                        continue;
                    }
                    if def.disabled.unwrap_or(false) {
                        entries.retain(|(eid, _)| eid != id);
                        continue;
                    }
                    if let Some(pos) = entries.iter().position(|(eid, _)| eid == id) {
                        if let Some(new_glob) = def.glob.as_deref().map(str::trim) {
                            if !new_glob.is_empty() {
                                entries[pos].1 = new_glob.to_string();
                            }
                        }
                    } else if let Some(new_glob) = def.glob.as_deref().map(str::trim) {
                        if !new_glob.is_empty() {
                            entries.push((id.to_string(), new_glob.to_string()));
                        }
                    }
                }
            }
        }

        entries.into_iter().map(|(_, g)| g).collect()
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
pub struct EmmyrcWorkspaceModuleMap {
    pub pattern: String,
    pub replace: String,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Hash, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyLibraryItem {
    /// Simple library path string
    Path(String),
    /// Library configuration with path and ignore rules
    Config(EmmyLibraryConfig),
}

impl EmmyLibraryItem {
    pub fn get_path(&self) -> &String {
        match self {
            EmmyLibraryItem::Path(p) => p,
            EmmyLibraryItem::Config(c) => &c.path,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Hash, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmmyLibraryConfig {
    /// Library path
    pub path: String,
    /// Ignore directories within this library
    #[serde(default)]
    pub ignore_dir: Vec<String>,
    /// Ignore globs within this library. eg: ["**/*.lua"]
    #[serde(default)]
    pub ignore_globs: Vec<String>,
}

/// An entry in `workspace.ignoreDirDefaults`. Accepts either a legacy glob string (backward
/// compatible) or an object that can override, disable, or add to the built-in default list.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Hash, PartialEq, Eq)]
#[serde(untagged)]
pub enum IgnoreDirDefaultEntry {
    /// Legacy string glob — when the whole list is pure strings, replaces built-ins entirely.
    LegacyGlob(String),
    /// Object entry for granular override of built-in defaults.
    Definition(Box<IgnoreDirDefaultDefinition>),
}

/// Object form for `workspace.ignoreDirDefaults` entries.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Hash, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IgnoreDirDefaultDefinition {
    /// Stable identifier. Built-in ids: `"wire-expression2"`, `"wire-expression-files"`,
    /// `"tests"`, `"test"`. Custom ids can be any non-empty string.
    pub id: String,
    /// Optional human-readable label (informational only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The glob pattern to apply. Omit when only using `disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Set to `true` to remove this built-in default for the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

/// A resolved built-in default entry (id + glob).
struct BuiltinIgnoreDirDefault {
    id: &'static str,
    glob: &'static str,
}

fn builtin_ignore_dir_defaults() -> Vec<BuiltinIgnoreDirDefault> {
    vec![
        BuiltinIgnoreDirDefault {
            id: "wire-expression2",
            glob: "**/gmod_wire_expression2/**",
        },
        BuiltinIgnoreDirDefault {
            id: "wire-expression-files",
            glob: "**/wire_expression*.lua",
        },
        BuiltinIgnoreDirDefault {
            id: "tests",
            glob: "**/tests/**",
        },
        BuiltinIgnoreDirDefault {
            id: "test",
            glob: "**/test/**",
        },
    ]
}

fn encoding_default() -> String {
    "utf-8".to_string()
}

fn reindex_duration_default() -> u64 {
    5000
}

fn enable_reindex_default() -> bool {
    false
}

fn ignore_dir_defaults() -> Vec<IgnoreDirDefaultEntry> {
    builtin_ignore_dir_defaults()
        .into_iter()
        .map(|d| {
            IgnoreDirDefaultEntry::Definition(Box::new(IgnoreDirDefaultDefinition {
                id: d.id.to_string(),
                label: None,
                glob: Some(d.glob.to_string()),
                disabled: None,
            }))
        })
        .collect()
}

fn use_default_ignores_default() -> bool {
    true
}

fn enable_isolation_default() -> bool {
    false
}
