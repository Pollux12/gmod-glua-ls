use glua_code_analysis::Emmyrc;
use std::fs;

fn main() {
    let schema = schemars::schema_for!(Emmyrc);
    let mut schema_json = serde_json::to_string_pretty(&schema).unwrap();
    if !schema_json.ends_with('\n') {
        schema_json.push('\n');
    }
    let root_crates = std::env::current_dir().unwrap();
    let output_path = root_crates.join("crates/glua_code_analysis/resources/schema.json");
    println!("Output path: {:?}", output_path);
    fs::write(output_path, schema_json).expect("Unable to write file");
}
