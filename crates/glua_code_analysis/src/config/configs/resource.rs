use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Resource path settings.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcResource {
    /// File paths used to resolve document links (ctrl+click on string literals). Not used for module indexing.
    #[serde(default)]
    pub paths: Vec<String>,
}
