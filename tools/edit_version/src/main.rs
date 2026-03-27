use std::fs;
use toml_edit::{value, DocumentMut};

const CARGOS: &[&str] = &[
    "crates/glua_code_analysis/Cargo.toml",
    "crates/glua_diagnostic_macro/Cargo.toml",
    "crates/glua_parser_desc/Cargo.toml",
    "crates/glua_parser/Cargo.toml",
    "crates/schema_to_glua/Cargo.toml",
    "crates/glua_ls/Cargo.toml",
    "crates/glua_check/Cargo.toml",
];

fn main() {
    let mut version = std::env::args().nth(1).expect("Please provide a version");
    if version.starts_with("refs/tags/") {
        version = version.replace("refs/tags/", "");
    }
    if version.starts_with('v') {
        version = version.trim_start_matches('v').to_string();
    }

    let current_dir = std::env::current_dir().unwrap();
    // 向上查找到有crates的目录
    let workspace_dir = current_dir
        .ancestors()
        .find(|dir| dir.join("crates").exists())
        .expect("Unable to find crates directory");

    for cargo in CARGOS {
        let path = workspace_dir.join(cargo);
        let content =
            fs::read_to_string(&path).unwrap_or_else(|_| panic!("Unable to read {}", cargo));

        let mut doc = content
            .parse::<DocumentMut>()
            .unwrap_or_else(|_| panic!("Failed to parse {}", cargo));

        doc["package"]["version"] = value(version.clone());

        fs::write(&path, doc.to_string())
            .unwrap_or_else(|_| panic!("Unable to write to {}", cargo));
    }

    let workspacec_cargo = workspace_dir.join("Cargo.toml");
    let content = fs::read_to_string(&workspacec_cargo)
        .unwrap_or_else(|_| panic!("Unable to read {:?}", workspacec_cargo));
    let mut doc = content
        .parse::<DocumentMut>()
        .unwrap_or_else(|_| panic!("Failed to parse {:?}", workspacec_cargo));

    let dependencies = doc["workspace"]["dependencies"].as_table_mut().unwrap();
    for dep_name in [
        "glua_code_analysis",
        "glua_parser",
        "glua_parser_desc",
        "glua_diagnostic_macro",
        "schema_to_glua",
    ] {
        if let Some(dep) = dependencies.get_mut(dep_name) {
            dep["version"] = value(version.clone());
        }
    }

    fs::write(&workspacec_cargo, doc.to_string())
        .unwrap_or_else(|_| panic!("Unable to write to {:?}", workspacec_cargo));
}
