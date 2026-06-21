#[cfg(test)]
mod tests {
    use crate::handlers::semantic_token::{
        CustomSemanticTokenModifier, CustomSemanticTokenType, SEMANTIC_TOKEN_MODIFIERS,
        SEMANTIC_TOKEN_TYPES,
    };
    use crate::handlers::test_lib::ProviderVirtualWorkspace;
    use glua_code_analysis::EmmyrcGmodScriptedClassScopeEntry;
    use googletest::prelude::*;
    use lsp_types::{SemanticTokenModifier, SemanticTokenType};

    fn token_type_index(token_type: SemanticTokenType) -> u32 {
        SEMANTIC_TOKEN_TYPES
            .iter()
            .position(|t| t == &token_type)
            .unwrap() as u32
    }

    fn modifier_bitset(modifiers: &[SemanticTokenModifier]) -> u32 {
        modifiers.iter().fold(0, |acc, m| {
            let index = SEMANTIC_TOKEN_MODIFIERS
                .iter()
                .position(|x| x == m)
                .unwrap() as u32;
            acc | (1 << index)
        })
    }

    fn decode(data: &[u32]) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut result = Vec::new();
        let mut line = 0;
        let mut col = 0;
        for chunk in data.chunks_exact(5) {
            let delta_line = chunk[0];
            let delta_start = chunk[1];
            let length = chunk[2];
            let token_type = chunk[3];
            let token_modifiers = chunk[4];

            if delta_line > 0 {
                line += delta_line;
                col = 0;
            }
            col += delta_start;

            result.push((line, col, length, token_type, token_modifiers));
        }
        result
    }

    fn has_token(
        tokens: &[(u32, u32, u32, u32, u32)],
        line: u32,
        col: u32,
        len: u32,
        token_type: SemanticTokenType,
        modifiers: &[SemanticTokenModifier],
    ) -> bool {
        let token_type = token_type_index(token_type);
        let modifiers = modifier_bitset(modifiers);
        tokens.contains(&(line, col, len, token_type, modifiers))
    }

    fn has_token_type(
        tokens: &[(u32, u32, u32, u32, u32)],
        line: u32,
        col: u32,
        len: u32,
        token_type: SemanticTokenType,
    ) -> bool {
        let token_type = token_type_index(token_type);
        tokens
            .iter()
            .any(|(token_line, token_col, token_len, typ, _)| {
                *token_line == line && *token_col == col && *token_len == len && *typ == token_type
            })
    }

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let _ = ws.check_semantic_token(
            r#"
            ---@class Cast1
            ---@field a string      # test
        "#,
            vec![],
        );
        Ok(())
    }

    #[gtest]
    fn test_require_alias_prefix_is_namespace_in_index_expr() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file("mod.lua", "return {}");
        let main = ws.def_file(
            "main.lua",
            r#"local m = require("mod")
m.foo()
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        let namespace_idx = token_type_index(SemanticTokenType::NAMESPACE);
        let field_idx = token_type_index(CustomSemanticTokenType::FIELD);
        let readonly_declaration = modifier_bitset(&[
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::DECLARATION,
            CustomSemanticTokenModifier::LOCAL,
        ]);

        // `local m = require("mod")`
        verify_that!(
            &tokens,
            contains(eq(&(0, 6, 1, namespace_idx, readonly_declaration)))
        )?;

        // `m.foo()`
        verify_that!(
            &tokens,
            all![
                contains(eq(&(1, 0, 1, namespace_idx, 0))),
                contains(eq(&(
                    1,
                    2,
                    3,
                    field_idx,
                    modifier_bitset(&[CustomSemanticTokenModifier::CALLABLE]),
                ))),
            ]
        )?;

        Ok(())
    }

    #[gtest]
    fn test_doc_tag_realm_is_documentation_keyword() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@realm server
local x = 1
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        let keyword_idx = token_type_index(SemanticTokenType::KEYWORD);
        let doc_modifier = modifier_bitset(&[SemanticTokenModifier::DOCUMENTATION]);

        verify_that!(
            tokens.iter().any(|(_, _, len, token_type, modifiers)| {
                *token_type == keyword_idx
                    && (*modifiers & doc_modifier) == doc_modifier
                    && *len >= 5
            }),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_string_literal_segments_use_utf16_lengths() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file("main.lua", "local s = \"😀\\n\"\n");

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(&tokens, 0, 10, 3, SemanticTokenType::STRING, &[]),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_variable_and_unresolved_call_tokens_follow_vscode_semantics() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local x = 1
x = 2
global_var = x
print(global_var, x)
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                0,
                6,
                1,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                1,
                0,
                1,
                SemanticTokenType::VARIABLE,
                &[
                    CustomSemanticTokenModifier::LOCAL,
                    SemanticTokenModifier::MODIFICATION,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                2,
                0,
                10,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::GLOBAL,
                    SemanticTokenModifier::MODIFICATION,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                3,
                0,
                5,
                SemanticTokenType::FUNCTION,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                3,
                0,
                5,
                SemanticTokenType::FUNCTION,
                &[
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    SemanticTokenModifier::READONLY,
                ],
            ),
            eq(false)
        )?;
        verify_that!(
            has_token(
                &tokens,
                3,
                6,
                10,
                SemanticTokenType::VARIABLE,
                &[CustomSemanticTokenModifier::GLOBAL],
            ),
            eq(true)
        )?;
        verify_that!(
            tokens.iter().any(|(line, col, _, _, modifiers)| {
                *line == 2
                    && *col == 0
                    && (*modifiers & modifier_bitset(&[SemanticTokenModifier::STATIC])) != 0
            }),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_parameters_stay_parameters_even_when_called() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local function run(cb, value)
    cb(value)
end
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                0,
                19,
                2,
                SemanticTokenType::PARAMETER,
                &[SemanticTokenModifier::DECLARATION],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                0,
                23,
                5,
                SemanticTokenType::PARAMETER,
                &[SemanticTokenModifier::DECLARATION],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                1,
                4,
                2,
                SemanticTokenType::FUNCTION,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(&tokens, 1, 7, 5, SemanticTokenType::PARAMETER, &[]),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_unresolved_builtin_like_namespace_does_not_use_spelling_heuristic() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"print(string.lower("x"))
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);
        verify_that!(
            has_token(
                &tokens,
                0,
                6,
                6,
                SemanticTokenType::NAMESPACE,
                &[SemanticTokenModifier::DEFAULT_LIBRARY],
            ),
            eq(false)
        )?;

        verify_that!(
            has_token(
                &tokens,
                0,
                13,
                5,
                CustomSemanticTokenType::FIELD,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_doc_payload_tokens_keep_documentation_context() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@field callback fun()
---@realm server
---@namespace MyNS
---@using string
---@return string result
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);
        verify_that!(
            has_token(
                &tokens,
                0,
                10,
                8,
                CustomSemanticTokenType::FIELD,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::DOCUMENTATION,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                1,
                10,
                6,
                SemanticTokenType::ENUM_MEMBER,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::DOCUMENTATION,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                2,
                14,
                4,
                SemanticTokenType::NAMESPACE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::DOCUMENTATION,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                3,
                10,
                6,
                SemanticTokenType::NAMESPACE,
                &[SemanticTokenModifier::DOCUMENTATION],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                4,
                18,
                6,
                SemanticTokenType::VARIABLE,
                &[SemanticTokenModifier::DOCUMENTATION],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_unresolved_builtin_like_local_alias_does_not_use_spelling_heuristic() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local str = string
str.lower("demo")
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);
        verify_that!(
            has_token(
                &tokens,
                0,
                6,
                3,
                SemanticTokenType::NAMESPACE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(false)
        )?;

        verify_that!(
            has_token(
                &tokens,
                1,
                0,
                3,
                SemanticTokenType::NAMESPACE,
                &[
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_shadowed_builtin_namespace_alias_stays_local_variable() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local string = {}
local str = string
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);
        verify_that!(
            has_token(
                &tokens,
                1,
                6,
                3,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT,
                ],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                1,
                6,
                3,
                SemanticTokenType::NAMESPACE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_unresolved_gmod_realm_constants_do_not_use_spelling_heuristic() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file("main.lua", r#"print(CLIENT, SERVER, MENU_DLL)"#);

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        for (col, len) in [(6, 6), (14, 6), (22, 8)] {
            verify_that!(
                has_token(
                    &tokens,
                    0,
                    col,
                    len,
                    SemanticTokenType::ENUM_MEMBER,
                    &[
                        SemanticTokenModifier::DEFAULT_LIBRARY,
                        SemanticTokenModifier::READONLY,
                    ],
                ),
                eq(false)
            )?;
        }

        Ok(())
    }

    #[gtest]
    fn test_callable_locals_stay_variables_but_function_declarations_stay_functions() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local function helper()
end
helper()

local fn = function() end
fn()
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                0,
                15,
                6,
                SemanticTokenType::FUNCTION,
                &[SemanticTokenModifier::DECLARATION],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                2,
                0,
                6,
                SemanticTokenType::FUNCTION,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                4,
                6,
                2,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::CALLABLE,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                5,
                0,
                2,
                SemanticTokenType::FUNCTION,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_callable_union_variable_keeps_lua_identity_and_callsite_signal() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@type fun()|nil
local maybeFn
maybeFn()
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                1,
                6,
                7,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::CALLABLE,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                2,
                0,
                7,
                SemanticTokenType::FUNCTION,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_table_fields_use_custom_field_token_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local panel = {}
panel.headerPanel = 1
print(panel.headerPanel)
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                1,
                6,
                11,
                CustomSemanticTokenType::FIELD,
                &[SemanticTokenModifier::MODIFICATION],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(&tokens, 2, 12, 11, CustomSemanticTokenType::FIELD, &[],),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_callable_table_fields_stay_fields_not_methods() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local callbacks = {
    onClick = function() end
}
callbacks.onClick()
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);
        verify_that!(
            has_token(
                &tokens,
                1,
                4,
                7,
                CustomSemanticTokenType::FIELD,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::CALLABLE,
                ],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                3,
                10,
                7,
                CustomSemanticTokenType::FIELD,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(
                &tokens,
                3,
                10,
                7,
                SemanticTokenType::METHOD,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_table_locals_and_index_prefixes_get_object_modifier() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local Editor = {}
Editor.SAVE_DIR = "cityrp_glide_layouts/"
Editor.previewHookId = "Glide.VehicleLayoutEditorPreview"
Editor.sessions = Editor.sessions or {}
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        for (line, col) in [(1, 0), (2, 0), (3, 0), (3, 18)] {
            verify_that!(
                has_token(
                    &tokens,
                    line,
                    col,
                    6,
                    SemanticTokenType::VARIABLE,
                    &[
                        CustomSemanticTokenModifier::LOCAL,
                        CustomSemanticTokenModifier::OBJECT,
                    ],
                ),
                eq(true)
            )?;
        }

        verify_that!(
            has_token(
                &tokens,
                0,
                6,
                6,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT,
                ],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                1,
                7,
                8,
                CustomSemanticTokenType::FIELD,
                &[SemanticTokenModifier::MODIFICATION],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                2,
                7,
                13,
                CustomSemanticTokenType::FIELD,
                &[SemanticTokenModifier::MODIFICATION],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                1,
                7,
                8,
                CustomSemanticTokenType::FIELD,
                &[
                    SemanticTokenModifier::READONLY,
                    SemanticTokenModifier::MODIFICATION,
                ],
            ),
            eq(false)
        )?;

        verify_that!(
            has_token(&tokens, 3, 25, 8, CustomSemanticTokenType::FIELD, &[]),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_table_field_alias_keeps_local_object_signal() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local StyledTheme = { colors = {} }
local colors = StyledTheme.colors
colors.primary = "white"
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                1,
                6,
                6,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT,
                ],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                2,
                0,
                6,
                SemanticTokenType::VARIABLE,
                &[
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT,
                ],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                2,
                7,
                7,
                CustomSemanticTokenType::FIELD,
                &[SemanticTokenModifier::MODIFICATION],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_local_class_instances_stay_variables_with_object_modifier() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@class DPanel
---@return DPanel
local function create() end

local pnl = create()
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                4,
                6,
                3,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT,
                ],
            ),
            eq(true)
        )?;

        verify_that!(
            tokens.iter().any(|(line, col, len, token_type, _)| {
                *line == 4
                    && *col == 6
                    && *len == 3
                    && *token_type == token_type_index(SemanticTokenType::CLASS)
            }),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_alias_to_class_local_keeps_object_modifier() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@class Test2

---@alias TestAlias Test2

---@type TestAlias
local var
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                5,
                6,
                3,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT,
                ],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_local_class_alias_keeps_class_and_local_signal() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@class Glide.VehicleLayoutEditor
Glide = {}
Glide.VehicleLayoutEditor = {}

local Editor = Glide.VehicleLayoutEditor
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                4,
                6,
                6,
                SemanticTokenType::CLASS,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_shadowed_class_path_alias_stays_local_variable() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@class Glide.VehicleLayoutEditor
local Glide = { VehicleLayoutEditor = 1 }

local Editor = Glide.VehicleLayoutEditor
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                3,
                6,
                6,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                3,
                6,
                6,
                SemanticTokenType::CLASS,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            ),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_hook_name_strings_do_not_use_call_path_heuristic() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"local hook = { Add = function() end, Run = function() end }
hook.Add("Think", "demo", function() end)
hook.Run("Think")
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(&tokens, 1, 9, 7, SemanticTokenType::EVENT, &[]),
            eq(false)
        )?;
        verify_that!(
            has_token(&tokens, 2, 9, 7, SemanticTokenType::EVENT, &[]),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_labels_and_goto_use_label_tokens() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"::done::
goto done
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                0,
                2,
                4,
                CustomSemanticTokenType::LABEL,
                &[SemanticTokenModifier::DECLARATION],
            ),
            eq(true)
        )?;
        verify_that!(
            has_token(&tokens, 1, 5, 4, CustomSemanticTokenType::LABEL, &[]),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_scoped_gmod_class_globals_are_class_tokens() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include =
            vec![EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(
                "entities/**".to_string(),
            )];
        ws.update_emmyrc(emmyrc);

        let main = ws.def_file(
            "lua/entities/test_entity/shared.lua",
            r#"ENT.Type = "anim"
function ENT:Initialize()
end
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token_type(&tokens, 0, 0, 3, SemanticTokenType::CLASS),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_local_shadow_of_scoped_gmod_class_global_stays_local_variable() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include =
            vec![EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(
                "entities/**".to_string(),
            )];
        ws.update_emmyrc(emmyrc);

        let main = ws.def_file(
            "lua/entities/test_entity/shared.lua",
            r#"local ENT = {}
ENT.Type = "anim"
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token_type(&tokens, 0, 6, 3, SemanticTokenType::CLASS),
            eq(false)
        )?;

        verify_that!(
            has_token_type(&tokens, 1, 0, 3, SemanticTokenType::CLASS),
            eq(false)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_enrichments() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"---@deprecated
local deprecated_var = 1

---@readonly
local readonly_var = 1

---@async
local function do_work() end
do_work()
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        // Deprecated
        verify_that!(
            has_token(
                &tokens,
                1,
                6,
                14,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    SemanticTokenModifier::DEPRECATED,
                ],
            ),
            eq(true)
        )?;

        // Readonly from property metadata
        verify_that!(
            has_token(
                &tokens,
                4,
                6,
                12,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    SemanticTokenModifier::READONLY,
                ]
            ),
            eq(true)
        )?;

        // Async function
        verify_that!(
            has_token(
                &tokens,
                7,
                15,
                7,
                SemanticTokenType::FUNCTION,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::ASYNC,
                ]
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_for_loop_vars() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"for k, v in pairs(var) do end
for i = 1, 10 do end
"#,
        );
        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        // k
        verify_that!(
            has_token(
                &tokens,
                0,
                4,
                1,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::READONLY,
                    CustomSemanticTokenModifier::LOCAL,
                ]
            ),
            eq(true)
        )?;

        // v
        verify_that!(
            has_token(
                &tokens,
                0,
                7,
                1,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::READONLY,
                    CustomSemanticTokenModifier::LOCAL,
                ]
            ),
            eq(true)
        )?;

        // i
        verify_that!(
            has_token(
                &tokens,
                1,
                4,
                1,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::READONLY,
                    CustomSemanticTokenModifier::LOCAL,
                ]
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_cityrp_vehicle_namespace_and_class() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let main = ws.def_file(
            "main.lua",
            r#"cityrp = {}
cityrp.vehicle = {}
function cityrp.vehicle.drive() end
"#,
        );
        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(
                &tokens,
                1,
                0,
                6,
                SemanticTokenType::NAMESPACE,
                &[CustomSemanticTokenModifier::GLOBAL,]
            ),
            eq(false)
        )?;

        verify_that!(
            has_token(
                &tokens,
                1,
                7,
                7,
                SemanticTokenType::CLASS,
                &[SemanticTokenModifier::MODIFICATION]
            ),
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                2,
                24,
                5,
                SemanticTokenType::METHOD,
                &[SemanticTokenModifier::DECLARATION]
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_local_chained_field_not_namespace() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let main = ws.def_file(
            "main.lua",
            r#"local my_table = {}
my_table.first = {}
my_table.first.second = 1
"#,
        );
        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        // my_table
        verify_that!(
            has_token(
                &tokens,
                0,
                6,
                8,
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    CustomSemanticTokenModifier::LOCAL,
                    CustomSemanticTokenModifier::OBJECT
                ]
            ),
            eq(true)
        )?;

        // first
        verify_that!(
            has_token(
                &tokens,
                1,
                9,
                5,
                CustomSemanticTokenType::FIELD,
                &[SemanticTokenModifier::MODIFICATION]
            ),
            eq(true)
        )?;

        // second
        verify_that!(
            has_token(
                &tokens,
                2,
                15,
                6,
                CustomSemanticTokenType::FIELD,
                &[SemanticTokenModifier::MODIFICATION]
            ),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_unresolved_index_expr_uses_property_fallback() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file("main.lua", r#"local x = unknown.field"#);
        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(&tokens, 0, 18, 5, CustomSemanticTokenType::FIELD, &[]),
            eq(true)
        )?;

        Ok(())
    }

    #[gtest]
    fn test_global_table_method_owner_is_namespace_like() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"my_global = {}
function my_global.action() end
"#,
        );
        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        // my_global
        verify_that!(
            has_token(&tokens, 1, 9, 9, SemanticTokenType::NAMESPACE, &[]),
            eq(true)
        )?;

        // action
        verify_that!(
            has_token(
                &tokens,
                1,
                19,
                6,
                SemanticTokenType::METHOD,
                &[SemanticTokenModifier::DECLARATION]
            ),
            eq(true)
        )?;

        Ok(())
    }
}
