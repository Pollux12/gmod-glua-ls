use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use serde::de::IntoDeserializer;
use serde_json::Value;
use serde_path_to_error::{Path as SerdePath, Segment};

use crate::{config::lua_loader::load_lua_config, read_file_with_encoding};

use super::{Emmyrc, flatten_config::FlattenConfigObject};

#[derive(Debug)]
pub enum ConfigLoadError {
    Read {
        path: PathBuf,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    Invalid {
        config_files: Vec<PathBuf>,
        message: String,
    },
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigLoadError::Read { path } => {
                write!(f, "failed to read config file \"{}\"", path.display())
            }
            ConfigLoadError::Parse { path, message } => {
                write!(
                    f,
                    "failed to parse config file \"{}\": {message}",
                    path.display()
                )
            }
            ConfigLoadError::Invalid {
                config_files,
                message,
            } => {
                if config_files.is_empty() {
                    write!(f, "failed to apply config: {message}")
                } else {
                    let paths = config_files
                        .iter()
                        .map(|path| format!("\"{}\"", path.display()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(f, "failed to apply config from {paths}: {message}")
                }
            }
        }
    }
}

impl Error for ConfigLoadError {}

struct ConfigLayer {
    value: Value,
    source: Option<PathBuf>,
}

fn load_config_file(config_file: &Path) -> Result<Value, ConfigLoadError> {
    log::info!("Loading config file: {:?}", config_file);
    let config_content =
        read_file_with_encoding(config_file, "utf-8").ok_or_else(|| ConfigLoadError::Read {
            path: config_file.to_path_buf(),
        })?;

    let config_value = if config_file.extension().and_then(|s| s.to_str()) == Some("lua") {
        load_lua_config(&config_content).map_err(|message| ConfigLoadError::Parse {
            path: config_file.to_path_buf(),
            message,
        })?
    } else {
        serde_json::from_str(&config_content).map_err(|error| ConfigLoadError::Parse {
            path: config_file.to_path_buf(),
            message: error.to_string(),
        })?
    };

    Ok(normalize_to_emmyrc_json(config_value))
}

fn load_config_layers_impl(
    config_files: &[PathBuf],
    partial_emmyrcs: Option<Vec<Value>>,
    strict: bool,
) -> Result<Vec<ConfigLayer>, ConfigLoadError> {
    let mut config_layers = Vec::new();

    for config_file in config_files {
        match load_config_file(config_file) {
            Ok(value) => config_layers.push(ConfigLayer {
                value,
                source: Some(config_file.clone()),
            }),
            Err(error) if strict => return Err(error),
            Err(error) => log::error!("{error}"),
        }
    }

    if let Some(partial_emmyrcs) = partial_emmyrcs {
        for partial_emmyrc in partial_emmyrcs {
            config_layers.push(ConfigLayer {
                value: normalize_to_emmyrc_json(partial_emmyrc),
                source: None,
            });
        }
    }

    if config_layers.is_empty() {
        log::info!("No valid config file found.");
    }

    Ok(config_layers)
}

fn merge_config_layers(config_layers: Vec<ConfigLayer>) -> Value {
    config_layers.into_iter().fold(
        Value::Object(Default::default()),
        |mut merged, config_layer| {
            merge_values(&mut merged, config_layer.value);
            merged
        },
    )
}

fn load_configs_raw_impl(
    config_files: &[PathBuf],
    partial_emmyrcs: Option<Vec<Value>>,
    strict: bool,
) -> Result<Value, ConfigLoadError> {
    let config_layers = load_config_layers_impl(config_files, partial_emmyrcs, strict)?;
    Ok(merge_config_layers(config_layers))
}

pub fn load_configs_raw(config_files: Vec<PathBuf>, partial_emmyrcs: Option<Vec<Value>>) -> Value {
    load_configs_raw_impl(&config_files, partial_emmyrcs, false).unwrap_or_else(|error| {
        log::error!("{error}");
        Value::Object(Default::default())
    })
}

pub fn try_load_configs(
    config_files: Vec<PathBuf>,
    partial_emmyrcs: Option<Vec<Value>>,
) -> Result<Emmyrc, ConfigLoadError> {
    let config_layers = load_config_layers_impl(&config_files, partial_emmyrcs, true)?;
    deserialize_config_layers_ignoring_invalid_values(config_layers)
}

pub fn load_configs(config_files: Vec<PathBuf>, partial_emmyrcs: Option<Vec<Value>>) -> Emmyrc {
    load_config_layers_impl(&config_files, partial_emmyrcs, false)
        .and_then(deserialize_config_layers_ignoring_invalid_values)
        .unwrap_or_else(|error| {
            log::error!("{error}");
            Emmyrc::default()
        })
}

fn deserialize_config_layers_ignoring_invalid_values(
    config_layers: Vec<ConfigLayer>,
) -> Result<Emmyrc, ConfigLoadError> {
    let mut merged = Value::Object(Default::default());
    let mut emmyrc = Emmyrc::default();

    for config_layer in config_layers {
        let ConfigLayer { mut value, source } = config_layer;

        loop {
            let mut candidate = merged.clone();
            merge_values(&mut candidate, value.clone());

            let result = serde_path_to_error::deserialize(candidate.clone().into_deserializer());
            let error = match result {
                Ok(candidate_emmyrc) => {
                    merged = candidate;
                    emmyrc = candidate_emmyrc;
                    break;
                }
                Err(error) => error,
            };
            let path = error.path().clone();
            let path_text = path.to_string();
            let message = error.inner().to_string();

            if !remove_value_at_path(&mut value, &path) {
                return Err(ConfigLoadError::Invalid {
                    config_files: source.into_iter().collect(),
                    message: format!("at `{path_text}`: {message}"),
                });
            }

            if let Some(source) = &source {
                log::warn!(
                    "Ignoring invalid config value at `{path_text}` from \"{}\": {message}",
                    source.display()
                );
            } else {
                log::warn!("Ignoring invalid config value at `{path_text}`: {message}");
            }
        }
    }

    Ok(emmyrc)
}

fn remove_value_at_path(config: &mut Value, path: &SerdePath) -> bool {
    let segments = path.iter().collect::<Vec<_>>();
    remove_value_at_segments(config, &segments)
}

fn remove_value_at_segments(value: &mut Value, segments: &[&Segment]) -> bool {
    let Some((segment, remaining)) = segments.split_first() else {
        return false;
    };
    let is_last = remaining.is_empty();

    match segment {
        Segment::Map { key } => {
            let Some(object) = value.as_object_mut() else {
                return false;
            };
            if is_last {
                object.remove(key).is_some()
            } else {
                object
                    .get_mut(key)
                    .is_some_and(|child| remove_value_at_segments(child, remaining))
            }
        }
        Segment::Seq { index } => {
            let Some(array) = value.as_array_mut() else {
                return false;
            };
            if is_last {
                if *index < array.len() {
                    array.remove(*index);
                    true
                } else {
                    false
                }
            } else {
                array
                    .get_mut(*index)
                    .is_some_and(|child| remove_value_at_segments(child, remaining))
            }
        }
        Segment::Enum { variant } => {
            if let Some(child) = value
                .as_object_mut()
                .and_then(|object| object.get_mut(variant))
            {
                if is_last {
                    *child = Value::Null;
                    true
                } else {
                    remove_value_at_segments(child, remaining)
                }
            } else if is_last {
                false
            } else {
                remove_value_at_segments(value, remaining)
            }
        }
        Segment::Unknown => false,
    }
}

fn normalize_to_emmyrc_json(config: Value) -> Value {
    FlattenConfigObject::parse(config).to_emmyrc()
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

    use super::{merge_values, try_load_configs};
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

        assert_eq!(base["diagnostics"]["disable"], json!(["call-non-callable"]));
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
        merge_values(&mut merged, super::normalize_to_emmyrc_json(luarc));
        merge_values(&mut merged, super::normalize_to_emmyrc_json(emmyrc));

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

        let merged = configs.into_iter().fold(json!({}), |mut acc, item| {
            merge_values(&mut acc, item);
            acc
        });

        assert_eq!(
            merged["diagnostics"]["disable"],
            json!(["call-non-callable"])
        );
    }

    #[test]
    fn try_load_configs_ignores_invalid_value_and_keeps_valid_fields() {
        let config = try_load_configs(
            Vec::new(),
            Some(vec![json!({
                "workspace": {
                    "encoding": {
                        "unexpected": true
                    },
                    "ignoreDir": [".claude"]
                }
            })]),
        )
        .expect("an invalid field should not reject the remaining config");

        assert_eq!(config.workspace.ignore_dir, vec![".claude"]);
    }

    #[test]
    fn try_load_configs_ignores_invalid_overlay_and_keeps_previous_value() {
        let config = try_load_configs(
            Vec::new(),
            Some(vec![
                json!({
                    "workspace": {
                        "encoding": "windows-1252"
                    }
                }),
                json!({
                    "workspace": {
                        "encoding": {
                            "unexpected": true
                        }
                    }
                }),
            ]),
        )
        .expect("an invalid overlay should preserve the previous valid value");

        assert_eq!(config.workspace.encoding, "windows-1252");
    }
}
