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
    #[serde(default)]
    pub network: EmmyrcGmodNetwork,
    #[serde(default = "param_type_hints_default")]
    pub param_type_hints: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_realm_from_filename: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_realm_from_calls: Option<bool>,
    #[serde(default = "infer_dynamic_fields_default")]
    pub infer_dynamic_fields: bool,
    #[serde(default = "dynamic_fields_global_default")]
    pub dynamic_fields_global: bool,
    /// Path to GMod annotations to load as core library.
    /// When set to empty string or not provided, uses VSCode extension's auto-downloaded annotations (if enabled).
    /// Set to explicit path to override, or use `autoLoadAnnotations: false` in .emmyrc to disable entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations_path: Option<String>,
    /// Disable auto-loading of annotations (from VSCode or default path).
    /// This takes precedence over extension settings.
    #[serde(default)]
    pub auto_load_annotations: Option<bool>,
}

fn enabled_default() -> bool {
    true
}

fn infer_dynamic_fields_default() -> bool {
    true
}

fn dynamic_fields_global_default() -> bool {
    true
}

fn param_type_hints_default() -> HashMap<String, String> {
    [
        ("ply", "Player"),
        ("player", "Player"),
        ("ent", "Entity"),
        ("entity", "Entity"),
        ("veh", "Entity"),
        ("vehicle", "Entity"),
        ("wep", "Weapon"),
        ("weapon", "Weapon"),
        ("pnl", "Panel"),
        ("panel", "Panel"),
    ]
    .into_iter()
    .map(|(name, type_name)| (name.to_string(), type_name.to_string()))
    .collect()
}

impl Default for EmmyrcGmod {
    fn default() -> Self {
        Self {
            enabled: enabled_default(),
            default_realm: EmmyrcGmodRealm::default(),
            scripted_class_scopes: EmmyrcGmodScriptedClassScopes::default(),
            hook_mappings: EmmyrcGmodHookMappings::default(),
            network: EmmyrcGmodNetwork::default(),
            param_type_hints: param_type_hints_default(),
            detect_realm_from_filename: None,
            detect_realm_from_calls: None,
            infer_dynamic_fields: infer_dynamic_fields_default(),
            dynamic_fields_global: dynamic_fields_global_default(),
            annotations_path: None,
            auto_load_annotations: None,
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
        "plugins/**".to_string(),
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

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodNetwork {
    #[serde(default = "network_enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub diagnostics: EmmyrcGmodNetworkDiagnostics,
    #[serde(default)]
    pub completion: EmmyrcGmodNetworkCompletion,
}

fn network_enabled_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetwork {
    fn default() -> Self {
        Self {
            enabled: network_enabled_default(),
            diagnostics: EmmyrcGmodNetworkDiagnostics::default(),
            completion: EmmyrcGmodNetworkCompletion::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodNetworkDiagnostics {
    #[serde(default = "network_type_mismatch_default")]
    pub type_mismatch: bool,
    #[serde(default = "network_order_mismatch_default")]
    pub order_mismatch: bool,
    #[serde(default = "network_missing_counterpart_default")]
    pub missing_counterpart: bool,
}

fn network_type_mismatch_default() -> bool {
    true
}

fn network_order_mismatch_default() -> bool {
    true
}

fn network_missing_counterpart_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetworkDiagnostics {
    fn default() -> Self {
        Self {
            type_mismatch: network_type_mismatch_default(),
            order_mismatch: network_order_mismatch_default(),
            missing_counterpart: network_missing_counterpart_default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcGmodNetworkCompletion {
    #[serde(default = "network_smart_read_suggestions_default")]
    pub smart_read_suggestions: bool,
    #[serde(default = "network_mismatch_hints_default")]
    pub mismatch_hints: bool,
}

fn network_smart_read_suggestions_default() -> bool {
    true
}

fn network_mismatch_hints_default() -> bool {
    true
}

impl Default for EmmyrcGmodNetworkCompletion {
    fn default() -> Self {
        Self {
            smart_read_suggestions: network_smart_read_suggestions_default(),
            mismatch_hints: network_mismatch_hints_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    #[gtest]
    fn test_gmod_defaults() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str("{}").or_fail()?;
        verify_that!(gmod.enabled, eq(true))?;
        verify_that!(gmod.default_realm, eq(EmmyrcGmodRealm::Shared))?;
        verify_that!(
            gmod.scripted_class_scopes.include,
            eq(&vec![
                "entities/**".to_string(),
                "weapons/**".to_string(),
                "effects/**".to_string(),
                "weapons/gmod_tool/stools/**".to_string(),
                "plugins/**".to_string(),
            ])
        )?;
        verify_that!(gmod.scripted_class_scopes.exclude.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.method_to_hook.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.emitter_to_hook.is_empty(), eq(true))?;
        verify_that!(gmod.hook_mappings.method_prefixes.is_empty(), eq(true))?;
        verify_that!(gmod.network.enabled, eq(true))?;
        verify_that!(gmod.network.diagnostics.type_mismatch, eq(true))?;
        verify_that!(gmod.network.diagnostics.order_mismatch, eq(true))?;
        verify_that!(gmod.network.diagnostics.missing_counterpart, eq(true))?;
        verify_that!(gmod.network.completion.smart_read_suggestions, eq(true))?;
        verify_that!(gmod.network.completion.mismatch_hints, eq(true))?;
        verify_that!(
            gmod.param_type_hints.get("ply"),
            eq(Some(&"Player".to_string()))
        )?;
        verify_that!(
            gmod.param_type_hints.get("vehicle"),
            eq(Some(&"Entity".to_string()))
        )?;
        verify_that!(gmod.detect_realm_from_filename, eq(None))?;
        verify_that!(gmod.detect_realm_from_calls, eq(None))?;
        verify_that!(gmod.infer_dynamic_fields, eq(true))?;
        verify_that!(gmod.dynamic_fields_global, eq(true))
    }

    #[gtest]
    fn test_gmod_network_camel_case_keys() -> Result<()> {
        let gmod: EmmyrcGmod = serde_json::from_str(
            r#"{
                "network": {
                    "enabled": false,
                    "diagnostics": {
                        "typeMismatch": false,
                        "orderMismatch": false,
                        "missingCounterpart": false
                    },
                    "completion": {
                        "smartReadSuggestions": false,
                        "mismatchHints": false
                    }
                }
            }"#,
        )
        .or_fail()?;

        verify_that!(gmod.network.enabled, eq(false))?;
        verify_that!(gmod.network.diagnostics.type_mismatch, eq(false))?;
        verify_that!(gmod.network.diagnostics.order_mismatch, eq(false))?;
        verify_that!(gmod.network.diagnostics.missing_counterpart, eq(false))?;
        verify_that!(gmod.network.completion.smart_read_suggestions, eq(false))?;
        verify_that!(gmod.network.completion.mismatch_hints, eq(false))
    }
}
