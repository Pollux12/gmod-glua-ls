#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, Emmyrc, EmmyrcGmodScriptedClassScopeEntry, VirtualWorkspace};

    fn legacy_scope(pattern: &str) -> EmmyrcGmodScriptedClassScopeEntry {
        EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(pattern.to_string())
    }

    fn has_diagnostic(
        ws: &mut VirtualWorkspace,
        file_path: &str,
        content: &str,
        diagnostic_code: DiagnosticCode,
    ) -> bool {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = ws.def_file(file_path, content);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            diagnostic_code.get_name().to_string(),
        ));

        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    fn has_undefined_global_name(
        ws: &mut VirtualWorkspace,
        file_path: &str,
        content: &str,
        name: &str,
    ) -> bool {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedGlobal);
        let file_id = ws.def_file(file_path, content);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobal.get_name().to_string(),
        ));
        let message_needled = format!("undefined global variable: {name}");

        diagnostics.iter().any(|diagnostic| {
            diagnostic.code == code && diagnostic.message.contains(&message_needled)
        })
    }

    fn has_undefined_global_assignment_name(
        ws: &mut VirtualWorkspace,
        file_path: &str,
        content: &str,
        name: &str,
    ) -> bool {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedGlobalAssignment);
        let file_id = ws.def_file(file_path, content);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobalAssignment
                .get_name()
                .to_string(),
        ));
        let message_needled = format!("undefined global variable: {name}");

        diagnostics.iter().any(|diagnostic| {
            diagnostic.code == code && diagnostic.message.contains(&message_needled)
        })
    }

    #[test]
    fn test_issue_250() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            --- @class A
            --- @field field any
            local A = {}

            function A:method()
            pcall(function()
                return self.field
            end)
            end
            "#
        ));
    }

    #[test]
    fn test_guarded_undefined_global_if_truthy() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if invalidVar then
                print(invalidVar)
            end
            "#
        ));
    }

    #[test]
    fn test_guarded_undefined_global_if_isvalid() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            function _G.IsValid(_) end

            if IsValid(entMaybe) then
                print(entMaybe)
            end
            "#
        ));
    }

    #[test]
    fn test_guarded_undefined_global_if_double_not_truthy() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if not not invalidVar then
                print(invalidVar)
            end
            "#
        ));
    }

    #[test]
    fn test_guarded_undefined_global_if_isvalid_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            function _G.IsValid(_) end

            local is_valid = IsValid
            if is_valid(entMaybe) then
                print(entMaybe)
            end
            "#
        ));
    }

    #[test]
    fn test_guard_clause_not_global_suppresses_following_uses() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            local function ShouldAlwaysSit(ply)
                if not ms then return end
                if not ms.GetTheaterPlayers then return end

                print(ms.GetTheaterPlayers)
            end
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_clause_not_global_suppresses_later_direct_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if not MR then return end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_clause_not_global_suppresses_later_function_body_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if not MR then return end

            local function later_use()
                print(MR)
            end

            later_use()
            "#
        ));
    }

    #[test]
    fn test_guard_clause_istable_global_suppresses_following_uses() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            local function ShouldAlwaysSit2()
                if not istable(ms) then return end
                if not ms.GetTheaterPlayers then return end

                print(ms.GetTheaterPlayers)
            end
            "#,
            "ms",
        ));
    }

    #[test]
    fn test_helper_continuation_guard_not_isstring_suppresses_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            function _G.isstring(_) end

            if not isstring(testVar) then return end

            print(testVar)
            "#
        ));
    }

    #[test]
    fn test_guard_clause_that_implies_falsy_still_reports_undefined_global() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobalAssignment,
            r#"
            if invalidVar then
                return
            end

            print(invalidVar)
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_clause_with_elseif_suppresses_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if not MR then
                return
            elseif true then
            end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_clause_with_else_suppresses_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if not MR then
                return
            else
                local _ = true
            end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_top_level_not_guard_without_early_return_does_not_suppress_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobalAssignment,
            r#"
            if not MR then
                local _ = true
            elseif true then
            end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_truthy_if_guard_does_not_apply_to_else_scope() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"
            if testVarThen then
                print(testVarThen)
            else
                print(testVarElse)
            end
        "#;

        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            content,
            "testVarThen",
        ));
        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "test.lua",
            content,
            "testVarElse",
        ));
    }

    #[test]
    fn test_top_level_guard_clause_not_equal_nil_does_not_suppress_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobalAssignment,
            r#"
            if MR ~= nil then
                return
            end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_clause_equal_nil_suppresses_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if MR == nil then
                return
            end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_clause_nil_equal_name_suppresses_later_use() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if nil == MR then
                return
            end

            print(MR)
            "#
        ));
    }

    #[test]
    fn test_top_level_guard_suppresses_undefined_global_only_not_other_diagnostics() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"
            ---@class MRPlyType
            ---@field GetFirstSpawn integer

            ---@class MRType
            ---@field Ply MRPlyType

            if not MR then return end
            ---@cast MR MRType

            MR.Ply:GetFirstSpawn(1)
        "#;

        assert!(!has_undefined_global_name(
            &mut ws, "test.lua", content, "MR"
        ));
        assert!(has_diagnostic(
            &mut ws,
            "test.lua",
            content,
            DiagnosticCode::CallNonCallable,
        ));
    }

    #[test]
    fn test_continuation_guard_scope_before_and_after_guard() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"
            function OtherTestFunction()
                print(testVarBefore)
            end

            print(testVarBefore)

            if not testVarAfter then return end

            print(testVarAfter)

            function FinalTestFunction()
                print(testVarAfter)
            end
        "#;

        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "test.lua",
            content,
            "testVarBefore",
        ));
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            content,
            "testVarAfter",
        ));
    }

    #[test]
    fn test_unguarded_undefined_global_still_reports() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobalAssignment,
            r#"
            print(invalidVar)
            "#
        ));
    }

    #[gtest]
    fn scripted_plugin_scope_should_not_report_undefined_global_for_plugin() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include =
            vec![EmmyrcGmodScriptedClassScopeEntry::Definition(Box::new(
                crate::EmmyrcGmodScriptedClassDefinition {
                    id: "plugins".to_string(),
                    label: Some("Plugins".to_string()),
                    class_global: Some("PLUGIN".to_string()),
                    path: Some(vec!["plugins".to_string()]),
                    include: Some(vec!["plugins/**".to_string()]),
                    exclude: None,
                    fixed_class_name: None,
                    is_global_singleton: None,
                    hide_from_outline: None,
                    strip_file_prefix: None,
                    aliases: None,
                    super_types: None,
                    hook_owner: None,
                    parent_id: None,
                    icon: None,
                    root_dir: None,
                    scaffold: None,
                    disabled: None,
                },
            ))];
        ws.update_emmyrc(emmyrc);

        assert!(!has_diagnostic(
            &mut ws,
            "plugins/vehicles/sh_plugin.lua",
            r#"
            PLUGIN.Name = "Vehicles"

            function PLUGIN:OnLoad()
            end
            "#,
            DiagnosticCode::UndefinedGlobal,
        ));
    }

    #[gtest]
    fn scripted_entity_scope_should_not_depend_on_global_ent_definition() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("lua/entities/**")];
        ws.update_emmyrc(emmyrc);

        assert!(!has_diagnostic(
            &mut ws,
            "lua/entities/sent_test/init.lua",
            r#"
            ENT.Type = "anim"

            function ENT:Initialize()
            end
            "#,
            DiagnosticCode::UndefinedGlobal,
        ));
    }

    #[gtest]
    fn non_scripted_scope_should_still_report_undefined_global_for_plugin() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("plugins/**")];
        ws.update_emmyrc(emmyrc);

        assert!(has_diagnostic(
            &mut ws,
            "lua/autorun/sh_plugin.lua",
            r#"
            PLUGIN.Name = "Vehicles"
            "#,
            DiagnosticCode::UndefinedGlobal,
        ));
    }

    #[test]
    fn test_guarded_global_with_index_expr_and_condition() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        // Pattern: if ctp and ctp.Disable then ... end
        // The base name 'ctp' should be guarded when accessed within the if block
        // check_code_for returns true when NO diagnostics are found
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if ctp and ctp.Disable then
                print(ctp)
            end
            "#
        ));
    }

    #[test]
    fn test_derma_define_control_panel_not_undefined_global() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // TestPanel should NOT be reported as undefined global after derma.DefineControl
        assert!(!has_undefined_global_name(
            &mut ws,
            "lua/vgui/test_panel.lua",
            r#"
            local PANEL = {}
            function PANEL:Init() end
            derma.DefineControl("TestPanel", "Description", PANEL, "DFrame")
            -- Access the panel global - should not be undefined
            local x = TestPanel
            "#,
            "TestPanel"
        ));
    }

    // =========================================================================
    // scripted-owner multi-match suppression tests
    // =========================================================================

    /// When only one owner glob matches the file, its global and aliases must
    /// still be suppressed (backward-compatibility / single-match path).
    #[gtest]
    fn test_scripted_class_scope_fixed_class_name_suppresses_global() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include =
            vec![EmmyrcGmodScriptedClassScopeEntry::Definition(Box::new(
                crate::EmmyrcGmodScriptedClassDefinition {
                    id: "helix-schema".to_string(),
                    label: Some("Helix Schema".to_string()),
                    class_global: Some("SCHEMA".to_string()),
                    fixed_class_name: Some("SCHEMA".to_string()),
                    is_global_singleton: Some(true),
                    strip_file_prefix: None,
                    hide_from_outline: None,
                    aliases: Some(vec!["Schema".to_string()]),
                    super_types: Some(vec!["GM".to_string()]),
                    hook_owner: Some(true),
                    path: Some(vec!["schema".to_string()]),
                    include: Some(vec![
                        "schema/**".to_string(),
                        "gamemode/schema.lua".to_string(),
                    ]),
                    exclude: None,
                    parent_id: None,
                    icon: None,
                    root_dir: None,
                    scaffold: None,
                    disabled: None,
                },
            ))];
        ws.update_emmyrc(emmyrc);

        // schema/sh_schema.lua should have SCHEMA defined
        assert!(!has_undefined_global_name(
            &mut ws,
            "schema/sh_schema.lua",
            r#"SCHEMA.Name = "Test""#,
            "SCHEMA",
        ));

        // gamemode/schema.lua should also have SCHEMA defined via include fallback
        assert!(!has_undefined_global_name(
            &mut ws,
            "gamemode/schema.lua",
            r#"SCHEMA.Name = "Test""#,
            "SCHEMA",
        ));
    }

    #[gtest]
    fn test_scripted_class_scope_item_suppresses_global() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include =
            vec![EmmyrcGmodScriptedClassScopeEntry::Definition(Box::new(
                crate::EmmyrcGmodScriptedClassDefinition {
                    id: "helix-items".to_string(),
                    label: Some("Helix Items".to_string()),
                    class_global: Some("ITEM".to_string()),
                    fixed_class_name: None,
                    is_global_singleton: None,
                    hide_from_outline: None,
                    strip_file_prefix: Some(true),
                    aliases: None,
                    super_types: None,
                    hook_owner: None,
                    path: Some(vec!["items".to_string()]),
                    include: Some(vec![
                        "schema/items/**".to_string(),
                        "plugins/**/items/**".to_string(),
                    ]),
                    exclude: None,
                    parent_id: None,
                    icon: None,
                    root_dir: None,
                    scaffold: None,
                    disabled: None,
                },
            ))];
        ws.update_emmyrc(emmyrc);

        // schema/items/sh_bandage.lua should have ITEM defined
        assert!(!has_undefined_global_name(
            &mut ws,
            "schema/items/sh_bandage.lua",
            r#"ITEM.name = "Bandage""#,
            "ITEM",
        ));

        // plugins/foo/items/sh_stuff.lua should also have ITEM defined
        assert!(!has_undefined_global_name(
            &mut ws,
            "plugins/foo/items/sh_stuff.lua",
            r#"ITEM.name = "Stuff""#,
            "ITEM",
        ));
    }

    #[gtest]
    fn test_undefined_global_suppressed_in_assignment_rhs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"mysqlOO = mysqloo"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_suppressed_in_local_assignment_rhs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"local x = mysqloo"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_suppressed_in_or_fallback() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"mysqlOO = mysqloo or {}"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_suppressed_in_or_chain_both_undefined() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"local x = mysqloo or tmysql"#,
            "mysqloo",
        ));
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"local x = mysqloo or tmysql"#,
            "tmysql",
        ));
    }

    #[gtest]
    fn test_undefined_global_suppressed_in_paren_wrapped_or() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"a = (mysqloo) or {}"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_still_reported_for_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"mysqloo()"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_still_reported_for_index() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"mysqloo.connect"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_still_reported_for_index_in_assignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"a = mysqloo.connect"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_still_reported_for_or_result_indexed() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"a = (mysqloo or tmysql).connect"#,
            "mysqloo",
        ));
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"a = (mysqloo or tmysql).connect"#,
            "tmysql",
        ));
    }

    #[gtest]
    fn test_undefined_global_still_reported_for_call_in_or_rhs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"a = mysqloo or tmysql.connect()"#,
            "tmysql",
        ));
    }

    #[gtest]
    fn test_undefined_global_still_reported_for_arithmetic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"a = mysqloo + 1"#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_undefined_global_direct_argument_uses_argument_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"
        local function foo(_) end
        foo(mysqloo)
        "#;

        assert!(has_diagnostic(
            &mut ws,
            "test.lua",
            content,
            DiagnosticCode::UndefinedGlobalAssignment,
        ));
        assert!(!has_undefined_global_name(
            &mut ws, "test.lua", content, "mysqloo"
        ));
    }

    #[gtest]
    fn test_undefined_global_parenthesized_direct_argument_uses_argument_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"
        local function foo(_) end
        foo((mysqloo))
        "#;

        assert!(has_diagnostic(
            &mut ws,
            "test.lua",
            content,
            DiagnosticCode::UndefinedGlobalAssignment,
        ));
        assert!(!has_undefined_global_name(
            &mut ws, "test.lua", content, "mysqloo"
        ));
    }

    #[gtest]
    fn test_undefined_global_nested_argument_keeps_undefined_global_code() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"
        local fallback = 1
        local function foo(_) end
        foo(mysqloo or fallback)
        "#;

        assert!(has_undefined_global_name(
            &mut ws, "test.lua", content, "mysqloo"
        ));
        assert!(!has_diagnostic(
            &mut ws,
            "test.lua",
            content,
            DiagnosticCode::UndefinedGlobalAssignment,
        ));
    }

    #[gtest]
    fn test_undefined_global_call_prefix_is_not_argument_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let content = r#"mysqloo()"#;

        assert!(has_undefined_global_name(
            &mut ws, "test.lua", content, "mysqloo"
        ));
        assert!(!has_diagnostic(
            &mut ws,
            "test.lua",
            content,
            DiagnosticCode::UndefinedGlobalAssignment,
        ));
    }

    /// Regression: `if not X.Y or X.Y < N then` should guard *all* occurrences of
    /// the index-expr base `X` inside the condition, not just the first. Previously,
    /// `collect_truthy_guarded_names` only descended into known logical/equality
    /// operators, so the second `X.Y` (under `<`) leaked an undefined-global
    /// diagnostic.
    #[gtest]
    fn test_indexed_global_in_comparison_under_or_is_guarded() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if not tmysql.Version or tmysql.Version < 4.1 then
                print("old")
            end
            "#,
            "tmysql",
        ));
    }

    /// Same idea but with `and` chaining different comparison ops.
    #[gtest]
    fn test_indexed_global_in_chained_comparisons_is_guarded() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if tmysql.Version >= 4.1 and tmysql.Version < 5.0 then
                print("ok")
            end
            "#,
            "tmysql",
        ));
    }

    /// Same idea, but the indexed access is the operand of an arithmetic op.
    /// Indexing implies the prefix is non-nil, so we should guard the prefix.
    #[gtest]
    fn test_indexed_global_in_arithmetic_is_guarded() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if tmysql.Count + 1 > 0 then
                print("ok")
            end
            "#,
            "tmysql",
        ));
    }

    /// A bare NameExpr under an arithmetic op is NOT a guard - reading
    /// `mysqloo + 1` doesn't imply `mysqloo` is defined; in Lua it would error.
    /// We must not over-eagerly suppress diagnostics for naked names.
    #[gtest]
    fn test_bare_global_in_arithmetic_is_still_reported() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if mysqloo + 1 > 0 then
                print("ok")
            end
            "#,
            "mysqloo",
        ));
    }

    #[gtest]
    fn test_nested_indexed_global_in_if_condition_is_guarded() {
        // `foo.bar.baz` — the outer IndexExpr's prefix is itself an IndexExpr,
        // so the guard must recurse to reach the deepest base name (`foo`).
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if foo.bar.baz then
                print("ok")
            end
            "#,
            "foo",
        ));
    }

    #[gtest]
    fn test_nested_indexed_global_in_comparison_is_guarded() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if not foo.bar.baz or foo.bar.baz < 4.1 then
                print("ok")
            end
            "#,
            "foo",
        ));
    }

    #[gtest]
    fn test_nested_indexed_global_in_method_call_is_guarded() {
        // `foo.bar:baz()` under a comparison — prefix walk must descend through
        // the CallExpr and the nested IndexExpr to reach `foo`.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if foo.bar:baz() < 1 then
                print("ok")
            end
            "#,
            "foo",
        ));
    }

    /// Regression: `local x = UNDEF` and `x = UNDEF` are silent uses (the
    /// nil simply gets bound) so they should be reported as the demoted
    /// `UndefinedGlobalAssignment` warning, not the strict `UndefinedGlobal`
    /// error. Previously only direct call args were demoted.
    #[test]
    fn assignment_rhs_undefined_global_demoted_to_assignment_code() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "assign.lua",
            r#"
            local _ = UNDEF_LOCAL
            "#,
            "UNDEF_LOCAL",
        ));

        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "assign2.lua",
            r#"
            multistatements = CLIENT_MULTI_STATEMENTS
            "#,
            "CLIENT_MULTI_STATEMENTS",
        ));

        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "tablefield.lua",
            r#"
            local t = { k = SOME_UNDEF }
            "#,
            "SOME_UNDEF",
        ));
    }

    /// Regression: assignment-RHS undefined globals must NOT be reported under
    /// the strict `UndefinedGlobal` (Error) code — that demotion is the whole
    /// point of `UndefinedGlobalAssignment`.
    #[test]
    fn assignment_rhs_undefined_global_does_not_fire_strict_code() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "assign.lua",
            r#"
            local _ = UNDEF_LOCAL
            x = UNDEF_GLOBAL
            "#,
            "UNDEF_LOCAL",
        ));
    }

    /// Index/call/arith uses must keep firing the strict `UndefinedGlobal`
    /// error code — only silent reads (call arg, assignment RHS) get demoted.
    #[test]
    fn index_and_call_undefined_global_remain_strict_code() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "index.lua",
            r#"
            local _ = UNDEF_X.field
            "#,
            "UNDEF_X",
        ));

        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws,
            "call.lua",
            r#"
            UNDEF_FN()
            "#,
            "UNDEF_FN",
        ));
    }

    /// Regression: the `if X.Y < N then ... else USE(X) end` else-branch must
    /// also see `X` widened to Any — evaluating `X.Y` in the condition implies
    /// `X` is non-nil regardless of which branch we take.
    #[gtest]
    fn test_indexed_global_in_comparison_guards_false_branch_too() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "test.lua",
            r#"
            if tmysql.Version < 4.1 then
                tmysql.Connect()
            else
                tmysql.Other()
            end
            "#,
            "tmysql",
        ));
    }

    /// Short-circuit guard: in `if a and tmysql.Version then T else F end`,
    /// the *else* branch is reached when `a` is falsy, in which case
    /// `tmysql.Version` was never evaluated. We must NOT widen `tmysql` in
    /// the else branch, so a use there should still report undefined-global.
    /// (The true branch is fine — both operands evaluated.)
    #[gtest]
    fn test_short_circuit_and_does_not_widen_in_false_branch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "and_true.lua",
            r#"
            local a = true
            if a and tmysql.Version then
                tmysql.Connect()
            end
            "#,
            "tmysql",
        ));

        let mut ws2 = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_name(
            &mut ws2,
            "and_false.lua",
            r#"
            local a = true
            if a and tmysql.Version then
            else
                tmysql.Other()
            end
            "#,
            "tmysql",
        ));
    }

    /// Regression: table-constructor field values reached via `or`/`and` must
    /// be demoted to the warning code, not the strict error code. Previously
    /// only direct names (`{ k = UNDEF }`) were demoted.
    #[test]
    fn table_field_or_chain_undefined_global_demoted() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!has_undefined_global_name(
            &mut ws,
            "tablefield_or.lua",
            r#"
            local t = { k = ModA or ModB }
            "#,
            "ModA",
        ));

        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "tablefield_or2.lua",
            r#"
            local t = { k = ModA or ModB }
            "#,
            "ModA",
        ));
    }

    /// Self-shadow `local foo = foo` outside legacy `module(...,seeall)` files
    /// is no longer fully silenced — typos surface as the demoted
    /// `UndefinedGlobalAssignment` warning so users still see them.
    #[gtest]
    fn test_self_shadow_outside_legacy_module_demoted_not_silenced() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(has_undefined_global_assignment_name(
            &mut ws,
            "self_shadow_typo.lua",
            r#"
            local typoo = typoo
            "#,
            "typoo",
        ));
    }
}
