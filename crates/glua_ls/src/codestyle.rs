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
            let generated_applied = apply_generated_code_style(workspace_folders, emmyrc);
            if !generated_applied {
                apply_editorconfig_files(&editorconfig_files);
            }
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
        (String::from("quote_style"), String::from("double")),
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
        (
            String::from("line_space_after_local_or_assign_statement"),
            String::from("fixed(2)"),
        ),
        (
            String::from("line_space_after_function_statement"),
            String::from("fixed(2)"),
        ),
        (
            String::from("break_multiline_call_expression_list"),
            String::from("true"),
        ),
        (
            String::from("remove_redundant_condition_parentheses"),
            String::from("true"),
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
    use std::{
        collections::{BTreeMap, HashSet},
        fs,
        path::PathBuf,
        sync::{Mutex, OnceLock},
        time::{SystemTime, UNIX_EPOCH},
    };

    use glua_code_analysis::{
        Emmyrc, EmmyrcFormatConfigPrecedence, EmmyrcFormatPreset, EmmyrcFormatStyleOverrides,
        FormattingOptions, WorkspaceFolder, reformat_code, remove_code_style,
    };

    use super::{
        apply_editorconfig_file, apply_workspace_code_style, build_style_map, cfc_preset_map,
        normalize_path, render_editorconfig, should_apply_editorconfig_updates,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CfcGuidelineCoverageKind {
        CfcOutputCase,
        CfcPresetSetting,
        UpstreamFormatterBehavior,
        NonFormatterPolicy,
    }

    #[derive(Debug, Clone, Copy)]
    struct CfcGuidelineCoverage {
        title: &'static str,
        kind: CfcGuidelineCoverageKind,
        rationale: &'static str,
    }

    #[derive(Debug, Clone, Copy)]
    struct CfcFormatCase {
        title: &'static str,
        input: &'static str,
        expected: &'static str,
    }

    const CFC_GUIDELINES: [CfcGuidelineCoverage; 40] = [
        CfcGuidelineCoverage {
            title: "GLuaLint",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Tooling selection is a lint/config concern, not a formatter transform.",
        },
        CfcGuidelineCoverage {
            title: "Spaces around operators",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "Spaces inside parentheses and curly braces if they contain content",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "Spaces after commas",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "Indentation should be done with 4 spaces",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "No spaces inside square brackets",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "Single space after comment operators and before if not at start of line",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "Never have more than 2 newlines",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Formatter-relevant, but not selected by a CFC-specific preset key in this module.",
        },
        CfcGuidelineCoverage {
            title: "Top level blocks should have either 1 or 2 newlines between them",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Formatter-relevant, but not selected by a CFC-specific preset key in this module.",
        },
        CfcGuidelineCoverage {
            title: "Non top level blocks/lines should never have more than 1 newline between them",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Formatter-relevant, but not selected by a CFC-specific preset key in this module.",
        },
        CfcGuidelineCoverage {
            title: "Returns should have one newline before them unless the codeblock is only one line",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Formatter-relevant, but not selected by a CFC-specific preset key in this module.",
        },
        CfcGuidelineCoverage {
            title: "Code should be split into manageable chunks using a single new line",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "This is a readability policy with subjective chunk boundaries, not a safe formatter rewrite.",
        },
        CfcGuidelineCoverage {
            title: "Do not use GMod operators",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Rewriting language operators is a semantic transform, not formatter spacing.",
        },
        CfcGuidelineCoverage {
            title: "Do not use GMod style comments",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Rewriting comment syntax is a syntax transform, not formatter spacing.",
        },
        CfcGuidelineCoverage {
            title: "The use of continue should be avoided if possible",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Control-flow refactors are not formatter work.",
        },
        CfcGuidelineCoverage {
            title: "Local variables and functions should always be written in camelCase",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Identifier renaming is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Constants should be written in SCREAMING_SNAKE",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Identifier renaming is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Global variables should be written in PascalCase",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Identifier renaming is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Methods for objects should be written in PascalCase",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Identifier renaming is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Table keys should only contain a-z A-Z 0-9 and _. and they should not start with 0-9",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Changing table keys changes runtime behavior and is out of formatter scope.",
        },
        CfcGuidelineCoverage {
            title: "Use _ as a variable to throwaway values that will not be used",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Identifier renaming is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Hook naming",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Naming conventions are semantic policy, not formatter behavior.",
        },
        CfcGuidelineCoverage {
            title: "Quotations",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Quote normalization is formatter-relevant, but the CFC preset does not select a quote_style.",
        },
        CfcGuidelineCoverage {
            title: "Do not use redundant parenthesis",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Parenthesis normalization is formatter-relevant, but not selected by a CFC-specific preset key here.",
        },
        CfcGuidelineCoverage {
            title: "Multiline tables",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Table layout is formatter-relevant, but not selected by a CFC-specific preset key here.",
        },
        CfcGuidelineCoverage {
            title: "Multiline function calls",
            kind: CfcGuidelineCoverageKind::UpstreamFormatterBehavior,
            rationale: "Call layout is formatter-relevant, but not selected by a CFC-specific preset key here.",
        },
        CfcGuidelineCoverage {
            title: "Return early from functions",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Control-flow refactors are not formatter work.",
        },
        CfcGuidelineCoverage {
            title: "Magic numbers should be pulled out into meaningful variables",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Extracting variables is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Complex expressions should be written on multiple lines with meaningful variable names",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Naming and expression decomposition are semantic refactors, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Never use semicolons",
            kind: CfcGuidelineCoverageKind::CfcOutputCase,
            rationale: "Guarded by a concrete reformat_code example under the CFC preset.",
        },
        CfcGuidelineCoverage {
            title: "Make use of existing constants where possible",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Replacing literals with constants is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Unnecessarily long conditions should be avoided",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Condition extraction is a semantic refactor, not formatting.",
        },
        CfcGuidelineCoverage {
            title: "Lines should be around 110 characters long at the most",
            kind: CfcGuidelineCoverageKind::CfcPresetSetting,
            rationale: "Guarded by an exact cfc_preset_map assertion for max_line_length = 110.",
        },
        CfcGuidelineCoverage {
            title: "Prefer decimals with leading 0s",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Lexical numeric normalization is not selected by the CFC preset here.",
        },
        CfcGuidelineCoverage {
            title: "Prefer whole numbers without leading 0s",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Lexical numeric normalization is not selected by the CFC preset here.",
        },
        CfcGuidelineCoverage {
            title: "Prefer decimals with no trailing 0s",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Lexical numeric normalization is not selected by the CFC preset here.",
        },
        CfcGuidelineCoverage {
            title: "Do not add useless comments",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Comment usefulness is a readability policy, not a safe formatter rewrite.",
        },
        CfcGuidelineCoverage {
            title: "Avoid unnecessary IsValid/:IsValid() checks",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Control/data-flow rewrites are not formatter work.",
        },
        CfcGuidelineCoverage {
            title: "Use varargs when calling print with multiple values",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "API usage refactors are not formatter work.",
        },
        CfcGuidelineCoverage {
            title: "Avoid using tostring on numbers when concatenating",
            kind: CfcGuidelineCoverageKind::NonFormatterPolicy,
            rationale: "Expression refactors are not formatter work.",
        },
    ];

    const CFC_OUTPUT_CASES: [CfcFormatCase; 10] = [
        CfcFormatCase {
            title: "Spaces around operators",
            input: "local x=a* b+c\n",
            expected: "local x = a * b + c\n",
        },
        CfcFormatCase {
            title: "Spaces inside function parameter list parentheses if they contain content",
            input: "function myFunc(a,b)\nreturn a+b\nend\n",
            expected: "function myFunc( a, b )\n    return a + b\nend\n",
        },
        CfcFormatCase {
            title: "Spaces inside parentheses and curly braces if they contain content",
            input: "function myFunc(a,b)\nreturn {5,{}}\nend\n",
            expected: "function myFunc( a, b )\n    return { 5, {} }\nend\n",
        },
        CfcFormatCase {
            title: "Spaces after commas",
            input: "myFunc(10,{3,5})\n",
            expected: "myFunc( 10, { 3, 5 } )\n",
        },
        CfcFormatCase {
            title: "Indentation should be done with 4 spaces",
            input: "if cond then\nmyFunc()\nend\n",
            expected: "if cond then\n    myFunc()\nend\n",
        },
        CfcFormatCase {
            title: "No spaces inside square brackets",
            input: "local val=tab[ 5 ]+tab[ 5*3 ]\n",
            expected: "local val = tab[5] + tab[5 * 3]\n",
        },
        CfcFormatCase {
            title: "Spaces inside curly braces if they contain content",
            input: "local data={foo=1,bar=2}\n",
            expected: "local data = { foo = 1, bar = 2 }\n",
        },
        CfcFormatCase {
            title: "Single space after comment operators and before if not at start of line",
            input: "--This is a good comment\nlocal a=3--This is also good\n",
            expected: "-- This is a good comment\nlocal a = 3 -- This is also good\n",
        },
        CfcFormatCase {
            title: "Single space after inline comment operators",
            input: "local a=3--comment\n",
            expected: "local a = 3 -- comment\n",
        },
        CfcFormatCase {
            title: "Never use semicolons",
            input: "local a = 3; print( a );\n",
            expected: "local a = 3\nprint( a )\n",
        },
    ];

    const CFC_STRICT_EXPECTATION_CASES: [CfcFormatCase; 9] = [
        CfcFormatCase {
            title: "Spaces inside function call parentheses if they contain content",
            input: "print(1)\n",
            expected: "print( 1 )\n",
        },
        CfcFormatCase {
            title: "Never have more than 2 newlines",
            input: "local config = GM.Config\n\n\nfunction GM:Think()\n-- do thing\nend\n",
            expected: "local config = GM.Config\n\nfunction GM:Think()\n    -- do thing\nend\n",
        },
        CfcFormatCase {
            title: "Top level blocks should have either 1 or 2 newlines between them",
            input: "local config = GM.Config\nfunction GM:Think()\n-- do thing\nend\n",
            expected: "local config = GM.Config\n\nfunction GM:Think()\n    -- do thing\nend\n",
        },
        CfcFormatCase {
            title: "Non top level blocks/lines should never have more than 1 newline between them",
            input: "function test()\nlocal a = 3\n\n\nprint( a )\nend\n",
            expected: "function test()\n    local a = 3\n\n    print( a )\nend\n",
        },
        CfcFormatCase {
            title: "Returns should have one newline before them unless the codeblock is only one line",
            input: "function test()\nlocal a = 3\nreturn a\nend\n",
            expected: "function test()\n    local a = 3\n\n    return a\nend\n",
        },
        CfcFormatCase {
            title: "Quotations",
            input: "myFunc( \"hello \", 'world!' )\n",
            expected: "myFunc( \"hello \", \"world!\" )\n",
        },
        CfcFormatCase {
            title: "Do not use redundant parenthesis",
            input: "if ( x == y ) then\nend\n",
            expected: "if x == y then\nend\n",
        },
        CfcFormatCase {
            title: "Multiline tables",
            input: "tbl = { v1 = c, v2 = d,\nv3 = k, v4 = j }\n",
            expected: "tbl = {\n    v1 = c,\n    v2 = d,\n    v3 = k,\n    v4 = j\n}\n",
        },
        CfcFormatCase {
            title: "Multiline function calls",
            input: "myFunc( \"First arg\",\nsecondArg, { third, arg } )\n",
            expected: "myFunc(\n    \"First arg\",\n    secondArg,\n    { third, arg }\n)\n",
        },
    ];

    fn formatter_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn make_temp_workspace_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();

        std::env::temp_dir()
            .join("gluals")
            .join("cfc-format-tests")
            .join(format!("{}-{timestamp}", std::process::id()))
    }

    fn format_with_style_map(input: &str, style_map: &BTreeMap<String, String>) -> String {
        let _guard = formatter_test_lock()
            .lock()
            .expect("formatter test mutex should not be poisoned");
        let workspace_root = make_temp_workspace_root();
        let workspace_root_normalized = normalize_path(&workspace_root);
        fs::create_dir_all(&workspace_root).expect("should create temp formatter workspace");

        let editorconfig_path = workspace_root.join(".editorconfig");
        fs::write(&editorconfig_path, render_editorconfig(style_map))
            .expect("should write generated formatter editorconfig");

        let lua_path = workspace_root.join("sample.lua");
        fs::write(&lua_path, input).expect("should write lua sample");

        remove_code_style(&workspace_root_normalized);
        let _ = apply_editorconfig_file(&editorconfig_path);

        let formatted = reformat_code(
            input,
            &normalize_path(&lua_path),
            FormattingOptions {
                indent_size: 4,
                use_tabs: false,
                insert_final_newline: true,
                non_standard_symbol: true,
            },
        )
        .replace("\r\n", "\n");

        remove_code_style(&workspace_root_normalized);
        let _ = fs::remove_dir_all(&workspace_root);
        formatted
    }

    fn format_with_cfc_preset(input: &str) -> String {
        format_with_style_map(input, &cfc_preset_map())
    }

    fn guideline_titles_for(kind: CfcGuidelineCoverageKind) -> Vec<&'static str> {
        CFC_GUIDELINES
            .iter()
            .filter(|guideline| guideline.kind == kind)
            .map(|guideline| guideline.title)
            .collect()
    }

    #[test]
    fn cfc_guideline_inventory_is_complete_and_unique() {
        let titles = CFC_GUIDELINES
            .iter()
            .map(|guideline| guideline.title)
            .collect::<Vec<_>>();
        let unique_titles = titles.iter().copied().collect::<HashSet<_>>();

        assert_eq!(
            CFC_GUIDELINES.len(),
            40,
            "update the inventory when the CFC guideline list changes"
        );
        assert_eq!(
            unique_titles.len(),
            CFC_GUIDELINES.len(),
            "each CFC guideline should appear exactly once in the inventory"
        );
        assert!(
            CFC_GUIDELINES
                .iter()
                .all(|guideline| !guideline.rationale.is_empty()),
            "every guideline should explain why it is covered by this suite in a specific way"
        );
    }

    #[test]
    fn cfc_guideline_inventory_tracks_output_and_preset_coverage() {
        let output_titles = guideline_titles_for(CfcGuidelineCoverageKind::CfcOutputCase);
        let preset_titles = guideline_titles_for(CfcGuidelineCoverageKind::CfcPresetSetting);
        let upstream_titles =
            guideline_titles_for(CfcGuidelineCoverageKind::UpstreamFormatterBehavior);
        let output_case_titles = CFC_OUTPUT_CASES
            .iter()
            .map(|case| case.title)
            .collect::<HashSet<_>>();
        let strict_case_titles = CFC_STRICT_EXPECTATION_CASES
            .iter()
            .map(|case| case.title)
            .collect::<HashSet<_>>();

        for title in output_titles {
            assert!(
                output_case_titles.contains(title),
                "missing CFC output case for guideline: {title}"
            );
        }

        for title in upstream_titles {
            assert!(
                strict_case_titles.contains(title),
                "missing strict CFC expectation case for guideline: {title}"
            );
        }

        assert_eq!(
            preset_titles,
            vec!["Lines should be around 110 characters long at the most"]
        );
    }

    #[test]
    fn cfc_preset_map_matches_expected_cfc_settings() {
        let expected = BTreeMap::from([
            (
                String::from("break_multiline_call_expression_list"),
                String::from("true"),
            ),
            (
                String::from("end_statement_with_semicolon"),
                String::from("replace_with_newline"),
            ),
            (String::from("indent_size"), String::from("4")),
            (String::from("indent_style"), String::from("space")),
            (
                String::from("line_space_after_function_statement"),
                String::from("fixed(2)"),
            ),
            (
                String::from("line_space_after_local_or_assign_statement"),
                String::from("fixed(2)"),
            ),
            (String::from("max_line_length"), String::from("110")),
            (String::from("quote_style"), String::from("double")),
            (
                String::from("remove_redundant_condition_parentheses"),
                String::from("true"),
            ),
            (
                String::from("space_after_comment_dash"),
                String::from("true"),
            ),
            (
                String::from("space_around_table_field_list"),
                String::from("true"),
            ),
            (
                String::from("space_inside_function_call_parentheses"),
                String::from("true"),
            ),
            (
                String::from("space_inside_function_param_list_parentheses"),
                String::from("true"),
            ),
            (
                String::from("space_inside_square_brackets"),
                String::from("false"),
            ),
            (String::from("tab_width"), String::from("4")),
        ]);

        assert_eq!(cfc_preset_map(), expected);
    }

    #[test]
    fn cfc_examples_reformat_to_expected_output() {
        for case in CFC_OUTPUT_CASES {
            let actual = format_with_cfc_preset(case.input);
            assert_eq!(
                actual, case.expected,
                "unexpected formatting for CFC guideline: {}\ninput:\n{}\nactual:\n{}\nexpected:\n{}",
                case.title, case.input, actual, case.expected
            );
        }
    }

    #[test]
    fn formatter_without_cfc_toggles_does_not_apply_cfc_only_rules() {
        let default_style_map = BTreeMap::from([
            (
                String::from("break_multiline_call_expression_list"),
                String::from("false"),
            ),
            (
                String::from("remove_redundant_condition_parentheses"),
                String::from("false"),
            ),
        ]);

        let paren_formatted =
            format_with_style_map("if ( x == y ) then\nend\n", &default_style_map);
        assert!(
            paren_formatted.contains("if ("),
            "formatter should keep condition parentheses when the CFC toggle is disabled: {paren_formatted:?}"
        );

        let call_formatted = format_with_style_map(
            "myFunc( \"First arg\",\nsecondArg, { third, arg } )\n",
            &default_style_map,
        );
        assert!(
            call_formatted.contains("secondArg, { third, arg }"),
            "formatter should not force CFC multiline call layout when the CFC toggle is disabled: {call_formatted:?}"
        );
    }

    #[test]
    fn non_standard_and_operator_is_preserved_during_formatting() {
        let formatted = format_with_style_map("if foo&&bar then\nend\n", &BTreeMap::new());
        assert!(
            formatted.contains("&&"),
            "formatter should preserve the GLua non-standard && operator: {formatted:?}"
        );
        assert!(
            !formatted.contains(" or "),
            "formatter should not rewrite && as `or`: {formatted:?}"
        );
    }

    #[test]
    fn table_append_operator_spacing_respects_enabled_setting() {
        let formatted = format_with_style_map(
            "tbl[#tbl+1] = value\n",
            &BTreeMap::from([(
                String::from("space_around_table_append_operator"),
                String::from("true"),
            )]),
        );
        assert!(
            formatted.contains("#tbl + 1"),
            "formatter should add spaces around the table append operator when enabled: {formatted:?}"
        );
    }

    #[test]
    fn cfc_max_line_length_keeps_output_within_110_columns() {
        let input = "local value = someFunction(firstArgument, secondArgument, thirdArgument, fourthArgument, fifthArgument, sixthArgument, seventhArgument)\n";
        let actual = format_with_cfc_preset(input);

        assert!(
            actual.lines().count() > 1,
            "expected CFC max_line_length to wrap the long line, but output stayed on one line:\n{actual}"
        );
        assert!(
            actual.lines().all(|line| line.chars().count() <= 110),
            "expected all formatted lines to stay within the CFC 110-column guideline, but got:\n{actual}"
        );
    }

    #[test]
    fn cfc_strict_expectation_cases_define_current_formatter_gaps() {
        let mut failures = Vec::new();

        for case in CFC_STRICT_EXPECTATION_CASES {
            let actual = format_with_cfc_preset(case.input);
            if actual != case.expected {
                failures.push(format!(
                    "guideline: {}\ninput:\n{}\nactual:\n{}\nexpected:\n{}",
                    case.title, case.input, actual, case.expected
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "current formatter does not yet satisfy all strict CFC expectations:\n\n{}",
            failures.join("\n\n---\n\n")
        );
    }

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
            break_multiline_call_expression_list: Some(false),
            remove_redundant_condition_parentheses: Some(false),
            ..Default::default()
        });

        let style_map = build_style_map(&emmyrc);
        assert_eq!(
            style_map.get("space_inside_function_call_parentheses"),
            Some(&String::from("false"))
        );
        assert_eq!(
            style_map.get("break_multiline_call_expression_list"),
            Some(&String::from("false"))
        );
        assert_eq!(
            style_map.get("remove_redundant_condition_parentheses"),
            Some(&String::from("false"))
        );
    }

    #[test]
    fn style_overrides_support_backend_supported_always_modes() {
        let mut emmyrc = Emmyrc::default();
        emmyrc.format.preset = EmmyrcFormatPreset::Custom;
        emmyrc.format.style_overrides = Some(
            serde_json::from_value(serde_json::json!({
                "call_arg_parentheses": "always",
                "align_continuous_rect_table_field": "always",
                "align_array_table": "always"
            }))
            .expect("style overrides should deserialize backend-supported modes"),
        );

        let style_map = build_style_map(&emmyrc);
        assert_eq!(
            style_map.get("call_arg_parentheses"),
            Some(&String::from("always"))
        );
        assert_eq!(
            style_map.get("align_continuous_rect_table_field"),
            Some(&String::from("always"))
        );
        assert_eq!(
            style_map.get("align_array_table"),
            Some(&String::from("always"))
        );
    }

    #[test]
    fn style_overrides_support_bool_operator_spacing_values() {
        let mut emmyrc = Emmyrc::default();
        emmyrc.format.preset = EmmyrcFormatPreset::Custom;
        emmyrc.format.style_overrides = Some(
            serde_json::from_value(serde_json::json!({
                "space_around_concat_operator": false,
                "space_around_assign_operator": true
            }))
            .expect("style overrides should deserialize bool operator spacing values"),
        );

        let style_map = build_style_map(&emmyrc);
        assert_eq!(
            style_map.get("space_around_concat_operator"),
            Some(&String::from("false"))
        );
        assert_eq!(
            style_map.get("space_around_assign_operator"),
            Some(&String::from("true"))
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

    #[test]
    fn prefer_gluarc_generated_style_outranks_nested_editorconfig() {
        let _guard = formatter_test_lock()
            .lock()
            .expect("formatter test mutex should not be poisoned");
        let workspace_root = make_temp_workspace_root();
        let workspace_root_normalized = normalize_path(&workspace_root);
        let nested_root = workspace_root.join("nested");
        let nested_root_normalized = normalize_path(&nested_root);
        fs::create_dir_all(&nested_root).expect("should create nested formatter workspace");

        let nested_editorconfig = nested_root.join(".editorconfig");
        fs::write(
            &nested_editorconfig,
            render_editorconfig(&BTreeMap::from([(
                String::from("remove_redundant_condition_parentheses"),
                String::from("false"),
            )])),
        )
        .expect("should write nested editorconfig override");

        let lua_path = nested_root.join("sample.lua");
        let lua_uri = normalize_path(&lua_path);
        fs::write(&lua_path, "if ( x == y ) then\nend\n").expect("should write lua sample");

        let mut emmyrc = Emmyrc::default();
        emmyrc.format.preset = EmmyrcFormatPreset::Cfc;
        emmyrc.format.config_precedence = EmmyrcFormatConfigPrecedence::PreferGluarc;

        remove_code_style(&workspace_root_normalized);
        remove_code_style(&nested_root_normalized);

        let workspaces = vec![WorkspaceFolder::new(workspace_root.clone(), false)];
        apply_workspace_code_style(&workspaces, &emmyrc);

        let formatted = reformat_code(
            "if ( x == y ) then\nend\n",
            &lua_uri,
            FormattingOptions {
                indent_size: 4,
                use_tabs: false,
                insert_final_newline: true,
                non_standard_symbol: true,
            },
        )
        .replace("\r\n", "\n");

        assert_eq!(formatted, "if x == y then\nend\n");

        remove_code_style(&workspace_root_normalized);
        remove_code_style(&nested_root_normalized);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn cfc_inventory_categories_are_stable() {
        assert_eq!(
            guideline_titles_for(CfcGuidelineCoverageKind::CfcOutputCase).len(),
            7
        );
        assert_eq!(
            guideline_titles_for(CfcGuidelineCoverageKind::CfcPresetSetting).len(),
            1
        );
        assert_eq!(
            guideline_titles_for(CfcGuidelineCoverageKind::UpstreamFormatterBehavior).len(),
            8
        );
        assert_eq!(
            guideline_titles_for(CfcGuidelineCoverageKind::NonFormatterPolicy).len(),
            24
        );
    }
}
