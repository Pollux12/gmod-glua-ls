use std::path::PathBuf;

use serde_json::Value;

use crate::{config::lua_loader::load_lua_config, read_file_with_encoding};

use super::{Emmyrc, flatten_config::FlattenConfigObject};

pub fn load_configs_raw(config_files: Vec<PathBuf>, partial_emmyrcs: Option<Vec<Value>>) -> Value {
    let mut config_jsons = Vec::new();

    for config_file in config_files {
        log::info!("Loading config file: {:?}", config_file);
        let config_content = match read_file_with_encoding(&config_file, "utf-8") {
            Some(content) => content,
            None => {
                log::error!(
                    "Failed to read config file: {:?}, error: File not found or unreadable",
                    config_file
                );
                continue;
            }
        };

        let config_value = if config_file.extension().and_then(|s| s.to_str()) == Some("lua") {
            match load_lua_config(&config_content) {
                Ok(value) => value,
                Err(e) => {
                    log::error!(
                        "Failed to parse lua config file: {:?}, error: {:?}",
                        &config_file,
                        e
                    );
                    continue;
                }
            }
        } else {
            match serde_json::from_str(&config_content) {
                Ok(json) => json,
                Err(e) => {
                    log::error!(
                        "Failed to parse config file: {:?}, error: {:?}",
                        &config_file,
                        e
                    );
                    continue;
                }
            }
        };

        config_jsons.push(normalize_to_emmyrc_json(config_value));
    }

    if let Some(partial_emmyrcs) = partial_emmyrcs {
        for partial_emmyrc in partial_emmyrcs {
            config_jsons.push(normalize_to_emmyrc_json(partial_emmyrc));
        }
    }

    if config_jsons.is_empty() {
        log::info!("No valid config file found.");
        Value::Object(Default::default())
    } else {
        config_jsons
            .into_iter()
            .fold(Value::Object(Default::default()), |mut acc, item| {
                merge_values(&mut acc, item);
                acc
            })
    }
}

fn normalize_to_emmyrc_json(config: Value) -> Value {
    FlattenConfigObject::parse(config).to_emmyrc()
}

pub fn load_configs(config_files: Vec<PathBuf>, partial_emmyrcs: Option<Vec<Value>>) -> Emmyrc {
    let emmyrc_json_value = load_configs_raw(config_files, partial_emmyrcs);
    serde_json::from_value(emmyrc_json_value).unwrap_or_else(|err| {
        log::error!("Failed to parse config: error: {:?}", err);
        Emmyrc::default()
    })
}

fn merge_values(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(base_value) => {
                        merge_values(base_value, overlay_value);
                    }
                    None => {
                        base_map.insert(key, overlay_value);
                    }
                }
            }
        }
        (Value::Array(base_array), Value::Array(overlay_array)) => {
            *base_array = overlay_array;
        }
        (base_slot, overlay_value) => {
            *base_slot = overlay_value;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::merge_values;
    use crate::config::flatten_config::FlattenConfigObject;

    #[test]
    fn test_merge_values_array_overlay_replaces_base() {
        let mut base = json!({
            "diagnostics": {
                "disable": ["inject-field", "duplicate-set-field"]
            }
        });
        let overlay = json!({
            "diagnostics": {
                "disable": ["call-non-callable"]
            }
        });

        merge_values(&mut base, overlay);

        assert_eq!(
            base["diagnostics"]["disable"],
            json!(["call-non-callable"])
        );
    }

    #[test]
    fn test_luarc_then_emmyrc_diagnostics_disable_prefers_emmyrc() {
        let luarc = json!({
            "diagnostics": {
                "disable": ["inject-field", "duplicate-set-field"]
            }
        });
        let emmyrc = json!({
            "diagnostics": {
                "disable": ["call-non-callable", "unnecessary-if"]
            }
        });

        let mut merged = json!({});
        merge_values(&mut merged, luarc);
        merge_values(&mut merged, emmyrc);

        let emmyrc_json = FlattenConfigObject::parse(merged).to_emmyrc();
        assert_eq!(
            emmyrc_json["diagnostics"]["disable"],
            json!(["call-non-callable", "unnecessary-if"])
        );
    }

    #[test]
    fn test_dotted_luarc_key_then_nested_emmyrc_prefers_emmyrc() {
        let luarc = json!({
            "diagnostics.disable": ["inject-field"]
        });
        let emmyrc = json!({
            "diagnostics": {
                "disable": ["call-non-callable"]
            }
        });

        let mut merged = json!({});
        merge_values(
            &mut merged,
            super::normalize_to_emmyrc_json(luarc),
        );
        merge_values(
            &mut merged,
            super::normalize_to_emmyrc_json(emmyrc),
        );

        assert_eq!(
            merged["diagnostics"]["disable"],
            json!(["call-non-callable"])
        );
    }

    #[test]
    fn test_load_configs_raw_with_dotted_and_nested_disable_prefers_later_config() {
        let configs = vec![
            super::normalize_to_emmyrc_json(json!({
                "diagnostics.disable": ["inject-field"]
            })),
            super::normalize_to_emmyrc_json(json!({
                "diagnostics": {
                    "disable": ["call-non-callable"]
                }
            })),
        ];

        let merged = configs
            .into_iter()
            .fold(json!({}), |mut acc, item| {
                merge_values(&mut acc, item);
                acc
            });

        assert_eq!(
            merged["diagnostics"]["disable"],
            json!(["call-non-callable"])
        );
    }
}
