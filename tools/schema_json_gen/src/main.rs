use glua_code_analysis::Emmyrc;
use serde_json::Value;
use std::fs;

/// Legacy diagnostic-code names that the analyzer still accepts via
/// `#[serde(alias = "...")]` but that schemars does not emit on its own.
/// Listed alongside the canonical code so editors validating `.gluarc.json`
/// against the schema continue to accept old configs after a rename.
const LEGACY_DIAGNOSTIC_ALIASES: &[(&str, &str)] =
    &[("undefined-global-assignment", "undefined-global-argument")];

fn main() {
    let schema = schemars::schema_for!(Emmyrc);
    let mut schema_value = serde_json::to_value(&schema).expect("schema to value");
    inject_legacy_diagnostic_aliases(&mut schema_value);
    let mut schema_json = serde_json::to_string_pretty(&schema_value).unwrap();
    if !schema_json.ends_with('\n') {
        schema_json.push('\n');
    }
    let root_crates = std::env::current_dir().unwrap();
    let output_path = root_crates.join("crates/glua_code_analysis/resources/schema.json");
    println!("Output path: {:?}", output_path);
    fs::write(output_path, schema_json).expect("Unable to write file");
}

fn inject_legacy_diagnostic_aliases(value: &mut Value) {
    // Probe both the JSON Schema 2020-12 (`$defs`) and draft-07 (`definitions`)
    // locations so a future schemars upgrade that flips the key still works.
    let key = ["$defs", "definitions"]
        .into_iter()
        .find(|k| value.get(*k).is_some())
        .unwrap_or_else(|| {
            panic!(
                "Schema is missing both `$defs` and `definitions`; \
                 cannot inject legacy diagnostic aliases. \
                 Has the schemars output structure changed?"
            )
        });
    let defs = value
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("Schema `{}` is not an object", key));
    let code = defs
        .get_mut("DiagnosticCode")
        .and_then(Value::as_object_mut)
        .expect(
            "Schema `DiagnosticCode` definition missing or not an object; \
             cannot inject legacy diagnostic aliases",
        );

    // schemars may emit the variant list under `oneOf` or `anyOf` depending on
    // version / future tweaks; accept either.
    let variants_key = ["oneOf", "anyOf"]
        .into_iter()
        .find(|k| code.get(*k).is_some())
        .expect(
            "Schema `DiagnosticCode` has neither `oneOf` nor `anyOf`; \
             cannot inject legacy diagnostic aliases",
        );
    let variants = code
        .get_mut(variants_key)
        .and_then(Value::as_array_mut)
        .unwrap_or_else(|| panic!("Schema `DiagnosticCode.{}` is not an array", variants_key));

    for (canonical, legacy) in LEGACY_DIAGNOSTIC_ALIASES {
        // Skip when the legacy entry is already present (idempotent).
        if variants
            .iter()
            .any(|entry| entry.get("const").and_then(Value::as_str) == Some(legacy))
        {
            continue;
        }
        let canonical_entry = variants
            .iter()
            .find(|entry| entry.get("const").and_then(Value::as_str) == Some(canonical))
            .unwrap_or_else(|| {
                panic!(
                    "Canonical diagnostic code `{}` not found in schema; \
                     cannot inject legacy alias `{}`. \
                     Was the canonical code renamed without updating \
                     LEGACY_DIAGNOSTIC_ALIASES?",
                    canonical, legacy
                )
            });
        let mut alias_entry = canonical_entry.clone();
        let obj = alias_entry.as_object_mut().unwrap_or_else(|| {
            panic!(
                "Diagnostic variant entry for `{}` is not an object",
                canonical
            )
        });
        obj.insert("const".to_string(), Value::String((*legacy).to_string()));
        let new_desc = format!(
            "Deprecated alias for `{}`. Accepted for backward compatibility.",
            canonical
        );
        obj.insert("description".to_string(), Value::String(new_desc));
        variants.push(alias_entry);
    }
}
