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
    fn test_guard_clause_that_implies_falsy_still_reports_undefined_global() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if invalidVar then
                return
            end

            print(invalidVar)
            "#
        ));
    }

    #[test]
    fn test_unguarded_undefined_global_still_reports() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
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
}
