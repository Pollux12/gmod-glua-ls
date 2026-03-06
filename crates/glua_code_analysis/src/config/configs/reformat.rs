use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcReformat {
    /// External formatter tool configuration. When set, the specified program is used for formatting instead of the built-in formatter.
    #[serde(default)]
    pub external_tool: Option<EmmyrcExternalTool>,

    /// External formatter for range formatting. When set, range format requests use this tool instead of the built-in formatter.
    #[serde(default)]
    pub external_tool_range_format: Option<EmmyrcExternalTool>,

    /// Whether to use the diff algorithm for formatting.
    #[serde(default = "default_false")]
    pub use_diff: bool,

    /// Built-in formatter preset. `default` uses EmmyLuaCodeStyle defaults. `cfc` applies the CFC style preset. `custom` applies no preset — only the `styleOverrides` you configure below take effect.
    #[serde(default)]
    #[schemars(extend("x-vscode-setting" = true))]
    pub preset: EmmyrcFormatPreset,

    /// Controls which takes precedence when both `.editorconfig` and `.gluarc.json` formatter settings apply to the same option: `preferEditorconfig` (default) or `preferGluarc`.
    #[serde(default)]
    #[schemars(extend("x-vscode-setting" = true))]
    pub config_precedence: EmmyrcFormatConfigPrecedence,

    /// Per-key EmmyLuaCodeStyle style overrides applied on top of the selected preset. Keys not configured here inherit their value from the active preset.
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
    /// Display width of a tab character in spaces, used for visual alignment calculations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_width: Option<u32>,
    /// Preferred quote style for string literals. `none` preserves the original style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_style: Option<EmmyrcFormatQuoteStyle>,
    /// Number of extra spaces used for indenting continuation lines when a line is wrapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_indent: Option<u32>,
    /// Column limit before the formatter wraps items onto new lines. Set to `"unset"` to disable line length enforcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_line_length: Option<EmmyrcFormatMaxLineLength>,
    /// Line ending preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_of_line: Option<EmmyrcFormatEndOfLine>,
    /// Separator character used between table fields (`none`, `comma`, `semicolon`, `only_kv_colon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_separator_style: Option<EmmyrcFormatTableSeparatorStyle>,
    /// Whether to add or remove a trailing separator after the last field in a table (`keep`, `never`, `always`, `smart`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_table_separator: Option<EmmyrcFormatTrailingTableSeparator>,
    /// How to handle parentheses around single-argument function calls with a string or table literal (`keep`, `remove`, `remove_table_only`, `remove_string_only`, `always`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_arg_parentheses: Option<EmmyrcFormatCallArgParentheses>,
    /// Detect line endings from file content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_end_of_line: Option<bool>,
    /// Ensure file ends with a newline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_final_newline: Option<bool>,
    /// Add spaces inside table braces, e.g. `{ a = 1 }` vs `{a = 1}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_table_field_list: Option<bool>,
    /// Add a space before Lua attribute annotations, e.g. `local x <const>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_attribute: Option<bool>,
    /// Add a space before `(` in function declarations, e.g. `function foo ()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_function_open_parenthesis: Option<bool>,
    /// Add a space before `(` in function calls, e.g. `print ('hello')`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_function_call_open_parenthesis: Option<bool>,
    /// Add a space before `(` in anonymous function expressions, e.g. `function ()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_closure_open_parenthesis: Option<bool>,
    /// Space handling before a single string or table literal argument: `always`, `only_string`, `only_table`, or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_function_call_single_arg: Option<EmmyrcFormatSpaceBeforeFunctionCallSingleArg>,
    /// Add a space before `[` in table index expressions, e.g. `t [key]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_open_square_bracket: Option<bool>,
    /// Add spaces inside function call parentheses, e.g. `print( 'hello' )`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_inside_function_call_parentheses: Option<bool>,
    /// Add spaces inside function parameter list parentheses, e.g. `function foo( a, b )`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_inside_function_param_list_parentheses: Option<bool>,
    /// Add spaces inside square bracket index expressions, e.g. `t[ 1 ]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_inside_square_brackets: Option<bool>,
    /// Add spaces around the table append operator (`#t + 1` style appends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_table_append_operator: Option<bool>,
    /// When `true`, preserve existing spacing inside function call arguments instead of normalizing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_spaces_inside_function_call: Option<bool>,
    /// Number of spaces to enforce before inline comments (`--`), or `"keep"` to preserve existing spacing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before_inline_comment: Option<EmmyrcFormatSpaceBeforeInlineComment>,
    /// Ensure a single space exists after the comment dash prefix (`--`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after_comment_dash: Option<bool>,
    /// Add spaces around math operators (`+`, `-`, `*`, `/`, `%`, `^`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_math_operator: Option<bool>,
    /// Add a space after commas in expressions and argument lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after_comma: Option<bool>,
    /// Add a space after commas in `for` loop range expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after_comma_in_for_statement: Option<bool>,
    /// Spaces around the concatenation operator (`..`). Accepts `true`/`false`, or `"none"` (no spaces), `"always"` (spaces on both sides), `"no_space_asym"` (space on left side only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_concat_operator: Option<EmmyrcFormatSpaceAroundConcatOperator>,
    /// Add spaces around logical operators (`and`, `or`, `not`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_logical_operator: Option<bool>,
    /// Spaces around assignment operators (`=`). Accepts `true`/`false`, or `"none"` (no spaces), `"always"` (spaces on both sides), `"no_space_asym"` (asymmetric spacing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_around_assign_operator: Option<EmmyrcFormatSpaceAroundAssignOperator>,
    /// Vertically align arguments of multi-line function calls to the opening parenthesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_call_args: Option<bool>,
    /// Vertically align parameters in multi-line function declarations to the opening parenthesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_function_params: Option<bool>,
    /// Align consecutive assignment statements on their `=` sign (`true` aligns within a block, `always` aligns across blank lines, `false` disables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_assign_statement: Option<EmmyrcFormatAlignContinuousAssignStatement>,
    /// Align key-value fields in rectangular table literals so that `=` signs line up vertically (`true`, `false`, `always`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_rect_table_field: Option<EmmyrcFormatAlignContinuousAssignStatement>,
    /// Maximum number of blank lines between consecutive statements before they are no longer considered part of the same aligned block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_line_space: Option<u32>,
    /// Vertically align `if` / `elseif` / `else` condition keywords.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_if_branch: Option<bool>,
    /// Array table layout: `none` keeps items inline, `always` puts each item on its own line, `contain_curly` uses multiline only when a nested table is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_array_table: Option<EmmyrcFormatAlignArrayTable>,
    /// Align arguments across similar consecutive function calls so that matching argument positions line up vertically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_similar_call_args: Option<bool>,
    /// Align the `--` of inline comments across consecutive lines so they form a vertical column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_continuous_inline_comment: Option<bool>,
    /// Align chained method / field expressions: `none` disables, `always` aligns all chains, `only_call_stmt` aligns only call statement chains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_chain_expr: Option<EmmyrcFormatAlignChainExpr>,
    /// Do not add extra indentation to wrapped `if`/`elseif` condition expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub never_indent_before_if_condition: Option<bool>,
    /// Do not indent a lone comment that appears inside an `if` branch body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub never_indent_comment_on_if_branch: Option<bool>,
    /// Keep indentation whitespace on otherwise blank lines instead of stripping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_indents_on_empty_lines: Option<bool>,
    /// Allow comments that start at column 0 to remain un-indented even when inside an indented block.
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
    /// When a line exceeds `max_line_length`, break every item in the containing list onto its own line rather than only the overflowing item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_all_list_when_line_exceed: Option<bool>,
    /// Collapse short multi-statement blocks into a single line when they fit within `max_line_length`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_collapse_lines: Option<bool>,
    /// Place the opening `{` of a table constructor on a new line instead of at the end of the preceding line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_before_braces: Option<bool>,
    /// When a function call already spans multiple lines, put each argument and the closing `)` on their own lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_multiline_call_expression_list: Option<bool>,
    /// Do not normalize spacing after `:` in method call and definition syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_space_after_colon: Option<bool>,
    /// Remove trailing commas that appear at the end of function call argument lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_call_expression_list_finish_comma: Option<bool>,
    /// Remove redundant parentheses around single-line condition expressions such as `if (x) then`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_redundant_condition_parentheses: Option<bool>,
    /// Semicolon handling: `keep` preserves existing, `always` adds to all statements, `same_line` adds between same-line statements, `replace_with_newline` replaces semicolons with newlines, `never` removes all.
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
    Always,
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
    /// Use EmmyLuaCodeStyle built-in defaults. No style overrides are applied.
    #[default]
    Default,
    /// Apply the built-in CFC (Clockwork Fighters Community) style preset: 4-space indentation, spaces inside parentheses, semicolons replaced with newlines.
    Cfc,
    /// Apply no preset. Only the `styleOverrides` you configure explicitly take effect; everything else uses the formatter's compiled-in default.
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
