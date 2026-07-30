use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

fn temp_workspace(name: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "glua_doc_cli_{name}_{}_{}_{}",
        std::process::id(),
        timestamp,
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn json_export_prunes_directory_matching_exclude_path() {
    let workspace = temp_workspace("exclude");
    let excluded = workspace.join(".claude").join("worktrees").join("copy");
    fs::create_dir_all(&excluded).expect("excluded test directory should be created");
    fs::write(workspace.join("main.lua"), "MainOnly = true")
        .expect("main test file should be written");
    fs::write(
        excluded.join("duplicate.lua"),
        "DuplicateShouldNotLoad = true",
    )
    .expect("excluded test file should be written");
    let json_path = workspace.join("api.json");

    let output = Command::new(env!("CARGO_BIN_EXE_glua_doc_cli"))
        .args([
            "--no-config",
            "--exclude",
            ".claude",
            "--output-format",
            "json",
            "--output",
        ])
        .arg(&json_path)
        .arg(&workspace)
        .output()
        .expect("documentation CLI should run");
    assert!(
        output.status.success(),
        "documentation CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let data: serde_json::Value = serde_json::from_slice(
        &fs::read(&json_path).expect("documentation JSON should be written"),
    )
    .expect("documentation output should be valid JSON");
    let global_names = data["globals"]
        .as_array()
        .expect("globals should be an array")
        .iter()
        .filter_map(|global| global["name"].as_str())
        .collect::<Vec<_>>();

    fs::remove_dir_all(&workspace).expect("test workspace should be removed");

    assert_eq!(global_names, vec!["MainOnly"]);
}

#[test]
fn invalid_config_field_is_ignored_without_discarding_valid_settings() {
    let workspace = temp_workspace("invalid_config");
    let ignored = workspace.join(".claude").join("worktrees").join("copy");
    fs::create_dir_all(&ignored).expect("ignored test directory should be created");
    fs::write(workspace.join("main.lua"), "MainOnly = true")
        .expect("main test file should be written");
    fs::write(
        ignored.join("duplicate.lua"),
        "DuplicateShouldNotLoad = true",
    )
    .expect("ignored test file should be written");
    fs::write(
        workspace.join(".luarc.json"),
        r#"{
            "workspace.encoding": {"unexpected": true},
            "workspace.ignoreDir": [".claude"]
        }"#,
    )
    .expect("invalid config fixture should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_glua_doc_cli"))
        .args(["--output-format", "json", "--output", "stdout"])
        .arg(&workspace)
        .output()
        .expect("documentation CLI should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stderr.contains(".luarc.json")
            && stderr.contains("workspace.encoding")
            && stderr.contains("invalid type: map, expected a string"),
        "unexpected CLI result (status {}): {stderr}",
        output.status
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    let global_names = json["globals"]
        .as_array()
        .expect("globals should be an array")
        .iter()
        .filter_map(|global| global["name"].as_str())
        .collect::<Vec<_>>();

    fs::remove_dir_all(&workspace).expect("test workspace should be removed");

    assert_eq!(global_names, vec!["MainOnly"]);
}

#[test]
fn invalid_higher_priority_config_value_preserves_valid_lower_priority_value() {
    let workspace = temp_workspace("config_precedence");
    fs::create_dir_all(&workspace).expect("test workspace should be created");
    fs::write(
        workspace.join(".luarc.json"),
        r#"{"workspace.encoding": "windows-1252"}"#,
    )
    .expect("lower-priority config fixture should be written");
    fs::write(
        workspace.join(".emmyrc.json"),
        r#"{"workspace.encoding": {"unexpected": true}}"#,
    )
    .expect("higher-priority config fixture should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_glua_doc_cli"))
        .args(["--output-format", "json", "--output", "stdout"])
        .arg(&workspace)
        .output()
        .expect("documentation CLI should run");
    assert!(
        output.status.success(),
        "documentation CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain valid JSON");
    let encoding = json["config"]["workspace"]["encoding"]
        .as_str()
        .expect("exported config encoding should be a string")
        .to_string();

    fs::remove_dir_all(&workspace).expect("test workspace should be removed");

    assert_eq!(encoding, "windows-1252");
}
