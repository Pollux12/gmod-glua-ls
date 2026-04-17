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

    fn has_undefined_global_argument_name(
        ws: &mut VirtualWorkspace,
        file_path: &str,
        content: &str,
        name: &str,
    ) -> bool {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedGlobalArgument);
        let file_id = ws.def_file(file_path, content);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobalArgument
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
            DiagnosticCode::UndefinedGlobalArgument,
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
            DiagnosticCode::UndefinedGlobalArgument,
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
        assert!(has_undefined_global_argument_name(
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
            DiagnosticCode::UndefinedGlobalArgument,
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

        assert!(has_undefined_global_argument_name(
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
            DiagnosticCode::UndefinedGlobalArgument,
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
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("plugins/**")];
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
            DiagnosticCode::UndefinedGlobalArgument,
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
            DiagnosticCode::UndefinedGlobalArgument,
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
            DiagnosticCode::UndefinedGlobalArgument,
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
            DiagnosticCode::UndefinedGlobalArgument,
        ));
    }
}
