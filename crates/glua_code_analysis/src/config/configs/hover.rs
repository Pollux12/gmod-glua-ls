use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcHover {
    /// Enable showing documentation on hover.
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub enable: bool,
}

impl Default for EmmyrcHover {
    fn default() -> Self {
        Self {
            enable: default_true(),
        }
    }
}

fn default_true() -> bool {
    true
}
