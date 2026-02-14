use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_realm_from_filename: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_realm_from_calls: Option<bool>,
}

fn enabled_default() -> bool {
    true
}

impl Default for EmmyrcGmod {
    fn default() -> Self {
        Self {
            enabled: enabled_default(),
            default_realm: EmmyrcGmodRealm::default(),
            scripted_class_scopes: EmmyrcGmodScriptedClassScopes::default(),
            hook_mappings: EmmyrcGmodHookMappings::default(),
            detect_realm_from_filename: None,
            detect_realm_from_calls: None,
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

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodScriptedClassScopes {
    #[serde(default = "scripted_scope_include_default")]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn scripted_scope_include_default() -> Vec<String> {
    vec![
        "entities/**".to_string(),
        "weapons/**".to_string(),
        "effects/**".to_string(),
        "weapons/gmod_tool/stools/**".to_string(),
    ]
}

impl Default for EmmyrcGmodScriptedClassScopes {
    fn default() -> Self {
        Self {
            include: scripted_scope_include_default(),
            exclude: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gmod_defaults() {
        let gmod: EmmyrcGmod = serde_json::from_str("{}").unwrap();
        assert!(gmod.enabled);
        assert_eq!(gmod.default_realm, EmmyrcGmodRealm::Shared);
        assert_eq!(
            gmod.scripted_class_scopes.include,
            vec![
                "entities/**".to_string(),
                "weapons/**".to_string(),
                "effects/**".to_string(),
                "weapons/gmod_tool/stools/**".to_string(),
            ]
        );
        assert!(gmod.scripted_class_scopes.exclude.is_empty());
        assert!(gmod.hook_mappings.method_to_hook.is_empty());
        assert!(gmod.hook_mappings.emitter_to_hook.is_empty());
        assert!(gmod.hook_mappings.method_prefixes.is_empty());
        assert_eq!(gmod.detect_realm_from_filename, None);
        assert_eq!(gmod.detect_realm_from_calls, None);
    }
}
