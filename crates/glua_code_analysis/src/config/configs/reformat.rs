use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcReformat {
    /// Whether to enable external tool formatting.
    #[serde(default)]
    pub external_tool: Option<EmmyrcExternalTool>,

    /// Whether to enable external tool range formatting.
    #[serde(default)]
    pub external_tool_range_format: Option<EmmyrcExternalTool>,

    /// Whether to use the diff algorithm for formatting.
    #[serde(default = "default_false")]
    pub use_diff: bool,

    /// Preset formatting profile for the built-in formatter.
    #[serde(default)]
    #[schemars(extend("x-vscode-setting" = true))]
    pub preset: EmmyrcFormatPreset,

    /// Precedence between formatter settings from .gluarc and discovered .editorconfig files.
    #[serde(default)]
    #[schemars(extend("x-vscode-setting" = true))]
    pub config_precedence: EmmyrcFormatConfigPrecedence,

    /// EmmyLuaCodeStyle overrides applied on top of the selected preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("x-vscode-setting" = true))]
    pub style_overrides: Option<EmmyrcFormatStyleOverrides>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[schemars(description = "Typed EmmyLuaCodeStyle overrides for built-in formatting.")]
#[serde(rename_all = "snake_case")]
pub struct EmmyrcFormatStyleOverrides {
    /// `tab` or `space` indentation mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indent_style: Option<EmmyrcFormatIndentStyle>,
    /// Number of spaces used for one indentation level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indent_size: Option<u32>,
    /// Visual width of a tab character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_width: Option<u32>,
    /// Preferred quote style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_style: Option<EmmyrcFormatQuoteStyle>,
    /// Indentation width for continuation lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_indent: Option<u32>,
    /// Maximum line length before wrapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_line_length: Option<EmmyrcFormatMaxLineLength>,
    /// Line ending preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_of_line: Option<EmmyrcFormatEndOfLine>,
    /// Table field separator style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_separator_style: Option<EmmyrcFormatTableSeparatorStyle>,
    /// Trailing separator policy for table literals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_table_separator: Option<EmmyrcFormatTrailingTableSeparator>,
    /// Parenthesis handling for single table/string call arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_arg_parentheses: Option<EmmyrcFormatCallArgParentheses>,
    /// Detect line endings from file content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_end_of_line: Option<bool>,
    /// Ensure file ends with a newline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_final_newline: Option<bool>,
    /// Add spaces inside table braces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_table_field_list: Option<bool>,
    /// Add space before attributes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_attribute: Option<bool>,
    /// Add space before `(` in function declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_function_open_parenthesis: Option<bool>,
    /// Add space before `(` in function calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_function_call_open_parenthesis: Option<bool>,
    /// Add space before `(` in closure/lambda syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_closure_open_parenthesis: Option<bool>,
    /// Space style for single function call arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_function_call_single_arg: Option<EmmyrcFormatSpaceBeforeFunctionCallSingleArg>,
    /// Add space before `[`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_open_square_bracket: Option<bool>,
    /// Add spaces inside call parentheses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_inside_function_call_parentheses: Option<bool>,
    /// Add spaces inside function parameter list parentheses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_inside_function_param_list_parentheses: Option<bool>,
    /// Add spaces inside square brackets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_inside_square_brackets: Option<bool>,
    /// Add spaces around table append operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_table_append_operator: Option<bool>,
    /// Ignore spaces already present inside function calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_spaces_inside_function_call: Option<bool>,
    /// Spaces before inline comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_inline_comment: Option<EmmyrcFormatSpaceBeforeInlineComment>,
    /// Insert one space after comment dashes (`--`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after_comment_dash: Option<bool>,
    /// Add spaces around math operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_math_operator: Option<bool>,
    /// Add a space after commas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after_comma: Option<bool>,
    /// Add a space after commas in `for` statements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after_comma_in_for_statement: Option<bool>,
    /// Add spaces around concatenation operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_concat_operator: Option<EmmyrcFormatSpaceAroundConcatOperator>,
    /// Add spaces around logical operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_logical_operator: Option<bool>,
    /// Add spaces around assignment operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_assign_operator: Option<EmmyrcFormatSpaceAroundAssignOperator>,
    /// Align function call arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_call_args: Option<bool>,
    /// Align function parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_function_params: Option<bool>,
    /// Align continuous assignment statements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_assign_statement: Option<EmmyrcFormatAlignContinuousAssignStatement>,
    /// Align continuous rectangular table fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_rect_table_field: Option<bool>,
    /// Maximum blank-line gap considered part of one aligned block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_line_space: Option<u32>,
    /// Align `if`/`elseif`/`else` branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_if_branch: Option<bool>,
    /// Align array table items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_array_table: Option<EmmyrcFormatAlignArrayTable>,
    /// Align similar consecutive call arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_similar_call_args: Option<bool>,
    /// Align inline comments in consecutive lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_inline_comment: Option<bool>,
    /// Align chained expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_chain_expr: Option<EmmyrcFormatAlignChainExpr>,
    /// Do not indent before `if` conditions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub never_indent_before_if_condition: Option<bool>,
    /// Do not indent comments on `if` branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub never_indent_comment_on_if_branch: Option<bool>,
    /// Preserve indentation on empty lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_indents_on_empty_lines: Option<bool>,
    /// Allow top-level comments to remain non-indented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_non_indented_comments: Option<bool>,
    /// Line spacing after `if` statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_if_statement: Option<String>,
    /// Line spacing after `do` statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_do_statement: Option<String>,
    /// Line spacing after `while` statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_while_statement: Option<String>,
    /// Line spacing after `repeat` statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_repeat_statement: Option<String>,
    /// Line spacing after `for` statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_for_statement: Option<String>,
    /// Line spacing after local/assignment statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_local_or_assign_statement: Option<String>,
    /// Line spacing after function statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_function_statement: Option<String>,
    /// Line spacing after expression statements (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_expression_statement: Option<String>,
    /// Line spacing after comments (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_after_comment: Option<String>,
    /// Line spacing around blocks (`keep`, `fixed(n)`, `min(n)`, `max(n)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_space_around_block: Option<String>,
    /// Break all list items once max line length is exceeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_all_list_when_line_exceed: Option<bool>,
    /// Collapse short multi-line expressions into one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_collapse_lines: Option<bool>,
    /// Move opening braces onto a new line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_before_braces: Option<bool>,
    /// Ignore spaces after colons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_space_after_colon: Option<bool>,
    /// Remove trailing commas in call expression argument lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_call_expression_list_finish_comma: Option<bool>,
    /// Semicolon handling at statement end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_statement_with_semicolon: Option<EmmyrcFormatEndStatementWithSemicolon>,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcFormatIndentStyle {
    Tab,
    Space,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcFormatQuoteStyle {
    None,
    Single,
    Double,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyrcFormatMaxLineLength {
    Count(u32),
    Keyword(EmmyrcFormatUnsetKeyword),
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcFormatEndOfLine {
    Crlf,
    Lf,
    Cr,
    Auto,
    Unset,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatTableSeparatorStyle {
    None,
    Comma,
    Semicolon,
    OnlyKvColon,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcFormatTrailingTableSeparator {
    Keep,
    Never,
    Always,
    Smart,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatCallArgParentheses {
    Keep,
    Remove,
    RemoveTableOnly,
    RemoveStringOnly,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyrcFormatSpaceBeforeFunctionCallSingleArg {
    Bool(bool),
    BoolKeyword(EmmyrcFormatBoolKeyword),
    Mode(EmmyrcFormatSpaceBeforeFunctionCallSingleArgMode),
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatSpaceBeforeFunctionCallSingleArgMode {
    Always,
    OnlyString,
    OnlyTable,
    None,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyrcFormatAlignArrayTable {
    Bool(bool),
    BoolKeyword(EmmyrcFormatBoolKeyword),
    Mode(EmmyrcFormatAlignArrayTableMode),
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatAlignArrayTableMode {
    None,
    Always,
    ContainCurly,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyrcFormatSpaceAroundConcatOperator {
    Bool(bool),
    BoolKeyword(EmmyrcFormatBoolKeyword),
    Mode(EmmyrcFormatAsymmetricOperatorSpacing),
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyrcFormatSpaceAroundAssignOperator {
    Bool(bool),
    BoolKeyword(EmmyrcFormatBoolKeyword),
    Mode(EmmyrcFormatAsymmetricOperatorSpacing),
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
pub enum EmmyrcFormatBoolKeyword {
    #[serde(rename = "true")]
    #[schemars(rename = "true")]
    True,
    #[serde(rename = "false")]
    #[schemars(rename = "false")]
    False,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatAsymmetricOperatorSpacing {
    None,
    Always,
    NoSpaceAsym,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatAlignChainExpr {
    None,
    Always,
    OnlyCallStmt,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
pub enum EmmyrcFormatAlignContinuousAssignStatement {
    #[serde(rename = "true")]
    #[schemars(rename = "true")]
    True,
    #[serde(rename = "false")]
    #[schemars(rename = "false")]
    False,
    #[serde(rename = "always")]
    #[schemars(rename = "always")]
    Always,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmmyrcFormatEndStatementWithSemicolon {
    Keep,
    Always,
    SameLine,
    ReplaceWithNewline,
    Never,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EmmyrcFormatSpaceBeforeInlineComment {
    Count(u32),
    Keyword(EmmyrcFormatKeepKeyword),
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcFormatKeepKeyword {
    Keep,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcFormatUnsetKeyword {
    Unset,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcExternalTool {
    /// The command to run the external tool.
    #[serde(default)]
    pub program: String,
    /// The arguments to pass to the external tool.
    #[serde(default)]
    pub args: Vec<String>,
    /// The timeout for the external tool in milliseconds.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EmmyrcFormatPreset {
    /// Preserve EmmyLuaCodeStyle defaults.
    #[default]
    Default,
    /// Apply the built-in CFC-oriented formatting preset.
    Cfc,
    /// Use only style_overrides.
    Custom,
}

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EmmyrcFormatConfigPrecedence {
    /// Prefer discovered .editorconfig files over .gluarc formatter overrides.
    #[default]
    PreferEditorconfig,
    /// Prefer .gluarc formatter overrides over discovered .editorconfig files.
    PreferGluarc,
}

fn default_timeout() -> u64 {
    5000
}

fn default_false() -> bool {
    false
}
