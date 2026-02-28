use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcInlayHint {
    /// Show parameter names in function calls and parameter types in function definitions.
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub param_hint: bool,
    /// Show named array indexes.
    ///
    /// Example:
    ///
    /// ```lua
    /// local array = {
    ///    [1] = 1, -- [name]
    /// }
    ///
    /// print(array[1] --[[ Hint: name ]])
    /// ```
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub index_hint: bool,
    /// Show types of local variables.
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub local_hint: bool,
    /// Show methods that override functions from base class.
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub override_hint: bool,
    /// Show hint when calling an object results in a call to
    /// its meta table's `__call` function.
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub meta_call_hint: bool,
    /// Show name of enumerator when passing a literal value to a function
    /// that expects an enum.
    ///
    /// Example:
    ///
    /// ```lua
    /// --- @enum Level
    /// local Foo = {
    ///    Info = 1,
    ///    Error = 2,
    /// }
    ///
    /// --- @param l Level
    /// function print_level(l) end
    ///
    /// print_level(1 --[[ Hint: Level.Info ]])
    /// ```
    #[serde(default = "default_false")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub enum_param_hint: bool,
    /// Show an inlay hint after closing `end` keywords indicating what block
    /// they belong to (e.g. function name). Only shown when the block spans
    /// at least `closingEndHintMinLines` lines.
    #[serde(default = "default_true")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub closing_end_hint: bool,
    /// Also show closing `end` hints for control flow blocks (if, while, do,
    /// for). Only effective when `closingEndHint` is enabled.
    #[serde(default = "default_false")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub closing_end_hint_control_flow: bool,
    /// Minimum number of lines a block must span before a closing `end` hint
    /// is shown.
    #[serde(default = "default_min_lines")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub closing_end_hint_min_lines: u32,
}

impl Default for EmmyrcInlayHint {
    fn default() -> Self {
        Self {
            param_hint: default_true(),
            index_hint: default_true(),
            local_hint: default_true(),
            override_hint: default_true(),
            meta_call_hint: default_true(),
            enum_param_hint: default_false(),
            closing_end_hint: default_true(),
            closing_end_hint_control_flow: default_false(),
            closing_end_hint_min_lines: default_min_lines(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_min_lines() -> u32 {
    15
}
