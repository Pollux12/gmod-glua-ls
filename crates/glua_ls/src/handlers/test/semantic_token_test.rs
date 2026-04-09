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
        let method_idx = token_type_index(SemanticTokenType::METHOD);
        let readonly_declaration = modifier_bitset(&[
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::DECLARATION,
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
                    method_idx,
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
    fn test_variable_and_builtin_tokens_follow_vscode_semantics() -> Result<()> {
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
                &[
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    SemanticTokenModifier::READONLY,
                ],
            ),
            eq(true)
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
    fn test_builtin_library_namespaces_are_not_plain_globals() -> Result<()> {
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
            eq(true)
        )?;

        verify_that!(
            has_token(
                &tokens,
                0,
                13,
                5,
                SemanticTokenType::METHOD,
                &[CustomSemanticTokenModifier::CALLABLE],
            ),
            eq(true)
        )?;

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
    fn test_hook_name_strings_use_event_tokens() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let main = ws.def_file(
            "main.lua",
            r#"hook.Add("Think", "demo", function() end)
hook.Run("Think")
"#,
        );

        let data = ws.get_semantic_token_data_for_file(main)?;
        let tokens = decode(&data);

        verify_that!(
            has_token(&tokens, 0, 9, 7, SemanticTokenType::EVENT, &[]),
            eq(true)
        )?;
        verify_that!(
            has_token(&tokens, 1, 9, 7, SemanticTokenType::EVENT, &[]),
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
            has_token(&tokens, 0, 0, 3, SemanticTokenType::CLASS, &[]),
            eq(true)
        )?;
        verify_that!(
            has_token(&tokens, 1, 9, 3, SemanticTokenType::CLASS, &[]),
            eq(true)
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
            eq(true)
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
                    CustomSemanticTokenModifier::LOCAL
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
    fn test_single_method_global_member_stays_property() -> Result<()> {
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
            has_token(
                &tokens,
                1,
                9,
                9,
                SemanticTokenType::NAMESPACE,
                &[CustomSemanticTokenModifier::GLOBAL]
            ),
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
