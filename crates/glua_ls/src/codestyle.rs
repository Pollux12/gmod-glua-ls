use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use glua_code_analysis::{
    Emmyrc, EmmyrcFormatConfigPrecedence, EmmyrcFormatPreset, EmmyrcFormatStyleOverrides,
    WorkspaceFolder, WorkspaceImport, update_code_style,
};
use serde_json::Value;
use walkdir::{DirEntry, WalkDir};

const VCS_DIRS: [&str; 3] = [".git", ".hg", ".svn"];

pub fn apply_workspace_code_style(
    workspace_folders: &[WorkspaceFolder],
    emmyrc: &Emmyrc,
) -> Option<()> {
    let editorconfig_files = collect_workspace_editorconfigs(workspace_folders);

    match emmyrc.format.config_precedence {
        EmmyrcFormatConfigPrecedence::PreferEditorconfig => {
            let generated_applied = apply_generated_code_style(workspace_folders, emmyrc);
            apply_editorconfig_files(&editorconfig_files);
            if generated_applied || !editorconfig_files.is_empty() {
                Some(())
            } else {
                None
            }
        }
        EmmyrcFormatConfigPrecedence::PreferGluarc => {
            apply_editorconfig_files(&editorconfig_files);
            let generated_applied = apply_generated_code_style(workspace_folders, emmyrc);
            if generated_applied || !editorconfig_files.is_empty() {
                Some(())
            } else {
                None
            }
        }
    }
}

pub fn apply_editorconfig_file(path: &Path) -> Option<()> {
    let parent_dir = path
        .parent()
        .map(normalize_path)
        .unwrap_or_else(|| String::from("."));
    let file_normalized = normalize_path(path);
    update_code_style(&parent_dir, &file_normalized);
    Some(())
}

fn apply_editorconfig_files(editorconfig_files: &[PathBuf]) {
    for file in editorconfig_files {
        let _ = apply_editorconfig_file(file);
    }
}

pub fn should_apply_editorconfig_updates(emmyrc: &Emmyrc) -> bool {
    !(matches!(
        emmyrc.format.config_precedence,
        EmmyrcFormatConfigPrecedence::PreferGluarc
    ) && has_generated_style(emmyrc))
}

fn has_generated_style(emmyrc: &Emmyrc) -> bool {
    !build_style_map(emmyrc).is_empty()
}

fn apply_generated_code_style(workspace_folders: &[WorkspaceFolder], emmyrc: &Emmyrc) -> bool {
    let style_map = build_style_map(emmyrc);
    if style_map.is_empty() {
        return false;
    }

    let content = render_editorconfig(&style_map);
    let mut applied_any = false;
    for root in collect_workspace_style_roots(workspace_folders) {
        let Some(path) = write_generated_editorconfig(&root, &content) else {
            continue;
        };
        let root_normalized = normalize_path(&root);
        let file_normalized = normalize_path(&path);
        update_code_style(&root_normalized, &file_normalized);
        applied_any = true;
    }

    applied_any
}

fn build_style_map(emmyrc: &Emmyrc) -> BTreeMap<String, String> {
    let mut map = match emmyrc.format.preset {
        EmmyrcFormatPreset::Default | EmmyrcFormatPreset::Custom => BTreeMap::new(),
        EmmyrcFormatPreset::Cfc => cfc_preset_map(),
    };

    if let Some(style_overrides) = &emmyrc.format.style_overrides {
        extend_style_map_with_overrides(&mut map, style_overrides);
    }

    map
}

fn extend_style_map_with_overrides(
    map: &mut BTreeMap<String, String>,
    style_overrides: &EmmyrcFormatStyleOverrides,
) {
    let Ok(Value::Object(entries)) = serde_json::to_value(style_overrides) else {
        return;
    };

    for (key, value) in entries {
        if let Some(value) = json_value_to_editorconfig_string(&value) {
            map.insert(key, value);
        }
    }
}

fn json_value_to_editorconfig_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(if *value {
            String::from("true")
        } else {
            String::from("false")
        }),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn cfc_preset_map() -> BTreeMap<String, String> {
    BTreeMap::from([
        (String::from("indent_style"), String::from("space")),
        (String::from("indent_size"), String::from("4")),
        (String::from("tab_width"), String::from("4")),
        (String::from("max_line_length"), String::from("110")),
        (
            String::from("space_inside_function_call_parentheses"),
            String::from("true"),
        ),
        (
            String::from("space_inside_function_param_list_parentheses"),
            String::from("true"),
        ),
        (
            String::from("space_around_table_field_list"),
            String::from("true"),
        ),
        (
            String::from("space_inside_square_brackets"),
            String::from("false"),
        ),
        (
            String::from("space_after_comment_dash"),
            String::from("true"),
        ),
        (
            String::from("end_statement_with_semicolon"),
            String::from("replace_with_newline"),
        ),
    ])
}

fn render_editorconfig(style_map: &BTreeMap<String, String>) -> String {
    let mut lines = vec![
        String::from("root = true"),
        String::new(),
        String::from("[*.lua]"),
    ];
    for (key, value) in style_map {
        lines.push(format!("{key} = {value}"));
    }
    lines.join("\n")
}

fn write_generated_editorconfig(workspace_root: &Path, content: &str) -> Option<PathBuf> {
    let mut hasher = DefaultHasher::new();
    workspace_root.hash(&mut hasher);
    let hash = hasher.finish();

    let cache_dir = std::env::temp_dir()
        .join("gluals")
        .join("formatter")
        .join("generated");
    if fs::create_dir_all(&cache_dir).is_err() {
        return None;
    }

    let pid = std::process::id();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();

    // Use create_new to avoid overwriting attacker-controlled paths in shared temp dirs.
    for attempt in 0..8 {
        let path = cache_dir.join(format!(
            "{hash:016x}-{pid}-{timestamp}-{attempt}.editorconfig"
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if file.write_all(content.as_bytes()).is_err() {
            let _ = fs::remove_file(&path);
            return None;
        }
        return Some(path);
    }

    None
}

fn collect_workspace_editorconfigs(workspace_folders: &[WorkspaceFolder]) -> Vec<PathBuf> {
    let mut editorconfig_files = Vec::new();
    for workspace in workspace_folders {
        match &workspace.import {
            WorkspaceImport::All => collect_editorconfigs(&workspace.root, &mut editorconfig_files),
            WorkspaceImport::SubPaths(subs) => {
                for sub in subs {
                    collect_editorconfigs(&workspace.root.join(sub), &mut editorconfig_files);
                }
            }
        }
    }

    editorconfig_files
}

fn collect_workspace_style_roots(workspace_folders: &[WorkspaceFolder]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for workspace in workspace_folders {
        match &workspace.import {
            WorkspaceImport::All => roots.push(workspace.root.clone()),
            WorkspaceImport::SubPaths(subs) => {
                for sub in subs {
                    roots.push(workspace.root.join(sub));
                }
            }
        }
    }
    roots
}

fn collect_editorconfigs(root: &PathBuf, results: &mut Vec<PathBuf>) {
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_vcs_dir(e, &VCS_DIRS))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file());
    for entry in walker {
        let path = entry.path();
        if path.ends_with(".editorconfig") {
            results.push(path.to_path_buf());
        }
    }
}

fn is_vcs_dir(entry: &DirEntry, vcs_dirs: &[&str]) -> bool {
    if entry.file_type().is_dir() {
        let name = entry.file_name().to_string_lossy();
        vcs_dirs.iter().any(|&vcs| vcs == name)
    } else {
        false
    }
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().to_string().replace("\\", "/")
}

#[cfg(test)]
mod tests {
    use glua_code_analysis::{
        Emmyrc, EmmyrcFormatConfigPrecedence, EmmyrcFormatPreset, EmmyrcFormatStyleOverrides,
    };

    use crate::codestyle::{build_style_map, should_apply_editorconfig_updates};

    #[test]
    fn cfc_preset_contains_parenthesis_spacing_rules() {
        let mut emmyrc = Emmyrc::default();
        emmyrc.format.preset = EmmyrcFormatPreset::Cfc;

        let style_map = build_style_map(&emmyrc);
        assert_eq!(style_map.get("tab_width"), Some(&String::from("4")));
        assert_eq!(
            style_map.get("space_inside_function_call_parentheses"),
            Some(&String::from("true"))
        );
        assert_eq!(
            style_map.get("space_inside_function_param_list_parentheses"),
            Some(&String::from("true"))
        );
    }

    #[test]
    fn cfc_preset_allows_non_conflicting_overrides() {
        let mut emmyrc = Emmyrc::default();
        emmyrc.format.preset = EmmyrcFormatPreset::Cfc;
        emmyrc.format.style_overrides = Some(EmmyrcFormatStyleOverrides {
            space_after_comment_dash: Some(false),
            ..Default::default()
        });

        let style_map = build_style_map(&emmyrc);
        assert_eq!(
            style_map.get("space_inside_function_call_parentheses"),
            Some(&String::from("true"))
        );
        assert_eq!(
            style_map.get("space_after_comment_dash"),
            Some(&String::from("false"))
        );
    }

    #[test]
    fn style_overrides_take_priority_over_preset() {
        let mut emmyrc = Emmyrc::default();
        emmyrc.format.preset = EmmyrcFormatPreset::Cfc;
        emmyrc.format.style_overrides = Some(EmmyrcFormatStyleOverrides {
            space_inside_function_call_parentheses: Some(false),
            ..Default::default()
        });

        let style_map = build_style_map(&emmyrc);
        assert_eq!(
            style_map.get("space_inside_function_call_parentheses"),
            Some(&String::from("false"))
        );
    }

    #[test]
    fn prefer_gluarc_only_blocks_editorconfig_when_generated_style_exists() {
        let mut emmyrc = Emmyrc::default();
        emmyrc.format.config_precedence = EmmyrcFormatConfigPrecedence::PreferGluarc;
        assert!(should_apply_editorconfig_updates(&emmyrc));

        emmyrc.format.preset = EmmyrcFormatPreset::Cfc;
        assert!(!should_apply_editorconfig_updates(&emmyrc));
    }
}
