use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

#[allow(dead_code)]
fn default_false() -> bool {
    false
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcStrict {
    /// Whether to enable strict mode require path.
    #[serde(default)]
    pub require_path: bool,
    /// Whether to enable strict mode array indexing.
    #[serde(default = "default_false")]
    pub array_index: bool,
    /// meta define overrides file define
    #[serde(default = "default_true")]
    pub meta_override_file_define: bool,
    /// Base constant types defined in doc can match base types, allowing int to match `---@alias id 1|2|3`, same for string.
    #[serde(default = "default_true")]
    pub doc_base_const_match_base_type: bool,
    /// This option limits the visibility of third-party libraries.
    ///
    /// When enabled, third-party libraries must use `---@export global` annotation to be importable (i.e., no diagnostic errors and visible in auto-import).
    #[serde(default = "default_false")]
    pub require_export_global: bool,
    /// Allow nullable types (T?) to be passed where non-nullable (T) is expected.
    #[serde(default = "default_true")]
    pub allow_nullable_as_non_nullable: bool,
}

impl Default for EmmyrcStrict {
    fn default() -> Self {
        Self {
            require_path: false,
            array_index: false,
            meta_override_file_define: true,
            doc_base_const_match_base_type: true,
            require_export_global: false,
            allow_nullable_as_non_nullable: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EmmyrcStrict;

    #[test]
    fn test_strict_defaults() {
        let strict: EmmyrcStrict = serde_json::from_str("{}").unwrap();

        assert!(!strict.require_path);
        assert!(!strict.array_index);
        assert!(strict.meta_override_file_define);
        assert!(strict.doc_base_const_match_base_type);
        assert!(!strict.require_export_global);
        assert!(strict.allow_nullable_as_non_nullable);
    }
}
