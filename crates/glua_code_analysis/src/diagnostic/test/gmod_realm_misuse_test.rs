#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

    #[gtest]
    fn test_disabled_when_gmod_off() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "addons/test/lua/autorun/server/sv_api.lua",
            r#"function ServerOnlyApi() return true end"#,
        );
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"ServerOnlyApi()"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[gtest]
    fn test_reports_when_enabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);
        ws.def_file(
            "addons/test/lua/autorun/server/sv_api.lua",
            r#"function ServerOnlyApi() return true end"#,
        );
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"ServerOnlyApi()"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    /// AddCSLuaFile is a shared function: it can be called from any realm and simply
    /// does nothing on the client. Calling it from a client file must not produce any
    /// realm-misuse diagnostic.
    #[gtest]
    fn test_addcsluafile_no_realm_diagnostic_in_client_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"AddCSLuaFile("shared.lua")"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let misuse_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code == misuse_code || d.code == risky_code)
        );
    }

    /// AddCSLuaFile is a shared function: calling it inside an explicit `if CLIENT then`
    /// block must not produce a realm-misuse diagnostic either.
    #[gtest]
    fn test_addcsluafile_no_realm_diagnostic_in_explicit_client_branch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_test.lua",
            r#"
                if CLIENT then
                    AddCSLuaFile("shared.lua")
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let misuse_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code == misuse_code || d.code == risky_code)
        );
    }

    #[gtest]
    fn test_strict_realm_misuse_enabled_by_default() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_test.lua",
            r#"
                if SERVER then
                    ---@realm server
                    function SuperCoolFunc() return true end
                end

                if CLIENT then
                    SuperCoolFunc()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_strict_mismatch_for_client_calling_server_branch_function() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_test.lua",
            r#"
                if SERVER then
                    ---@realm server
                    function SuperCoolFunc() return true end
                end

                if CLIENT then
                    SuperCoolFunc()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == risky_code)
        );
    }

    #[gtest]
    fn test_reports_risky_mismatch_for_filename_inferred_realms() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);

        ws.def_file(
            "addons/test/lua/autorun/server/sv_api.lua",
            r#"
                function ServerOnlyApi()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"
                ServerOnlyApi()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == risky_code)
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_unknown_realm_when_callsite_realm_is_unresolved() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.default_realm = crate::EmmyrcGmodRealm::Menu;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodUnknownRealm);

        ws.def_file(
            "addons/test/lua/autorun/sh_decl.lua",
            r#"
                ---@realm server
                function ServerOnlyApi()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/use_api.lua",
            r#"
                ServerOnlyApi()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let unknown_code = Some(NumberOrString::String(
            DiagnosticCode::GmodUnknownRealm.get_name().to_string(),
        ));
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == unknown_code);
        assert!(diagnostic.is_some());
        assert_eq!(
            diagnostic.and_then(|diagnostic| diagnostic.severity),
            Some(DiagnosticSeverity::HINT)
        );
    }

    #[gtest]
    fn test_does_not_report_unknown_realm_for_shared_callee() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.default_realm = crate::EmmyrcGmodRealm::Menu;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodUnknownRealm);

        ws.def_file(
            "addons/test/lua/autorun/sh_decl.lua",
            r#"
                function SharedApi()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/use_api.lua",
            r#"
                SharedApi()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let unknown_code = Some(NumberOrString::String(
            DiagnosticCode::GmodUnknownRealm.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == unknown_code)
        );
    }

    #[gtest]
    fn test_prefers_compatible_shared_member_over_client_member() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/client/cl_item.lua",
            r#"
                ITEM = ITEM or {}
                function ITEM:GetBase() return true end
            "#,
        );

        ws.def_file(
            "addons/test/lua/autorun/sh_item.lua",
            r#"
                ITEM = ITEM or {}
                function ITEM:GetBase() return true end
                function ITEM:GetOwner() return true end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/sv_use.lua",
            r#"
                if SERVER then
                    local item = ITEM
                    item:GetBase()
                    item:GetOwner()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_nested_branch_prefers_inner_realm_for_callsite() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_test.lua",
            r#"
                if SERVER then
                    function ServerOnlyFunc() return true end
                end

                if CLIENT then
                    if SERVER then
                        ServerOnlyFunc()
                    end
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_function_realm_tag_does_not_override_file_realm() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_item.lua",
            r#"
                ITEM = ITEM or {}

                ---@realm client
                function ITEM:ClientOnly()
                    return true
                end

                function ITEM:GetOwner()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/sv_use.lua",
            r#"
                if SERVER then
                    local item = ITEM
                    item:GetOwner()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_client_global_call_prefers_shared_decl_over_server_only_override() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_override.lua",
            r#"
                if SERVER then
                    function Color(r, g, b, a)
                        if not r or not g or not b then
                            return nil
                        end

                        return r
                    end
                end
            "#,
        );

        ws.def_file(
            "addons/test/lua/autorun/sh_shared_color.lua",
            r#"
                function Color(r, g, b, a)
                    return r
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_use_color.lua",
            r#"
                local c = Color(255, 255, 255, 255)
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_client_global_call_falls_back_to_library_shared_decl_when_main_is_server_only() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);

        ws.def_file(
            "library/sh_shared_color.lua",
            r#"
                function Color(r, g, b, a)
                    return r
                end
            "#,
        );

        ws.def_file(
            "addons/test/lua/autorun/sh_override.lua",
            r#"
                if SERVER then
                    function Color(r, g, b, a)
                        if not r or not g or not b then
                            return nil
                        end

                        return r
                    end
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_use_color.lua",
            r#"
                local c = Color(255, 255, 255, 255)
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_strict_mismatch_for_client_calling_gm_method_with_server_annotation() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_gm_decl.lua",
            r#"
                ---@realm server
                function GM:RealmReproOnly()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_gm_call.lua",
            r#"
                if CLIENT then
                    GM:RealmReproOnly()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_strict_mismatch_for_server_calling_gm_method_with_client_annotation() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_gm_decl_client.lua",
            r#"
                ---@realm client
                function GM:PreRender()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_gm_call_server.lua",
            r#"
                if SERVER then
                    GM:PreRender()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_strict_mismatch_for_server_calling_shared_file_annotated_table_method() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_table_method_decl.lua",
            r#"
                testTbl = testTbl or {}

                ---@realm client
                function testTbl:TestMethod()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_table_method_call.lua",
            r#"
                if SERVER then
                    testTbl:TestMethod()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_strict_mismatch_for_server_calling_shared_file_annotated_global_function() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_global_function_decl.lua",
            r#"
                ---@realm client
                function TestFunction()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_global_function_call.lua",
            r#"
                if SERVER then
                    TestFunction()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_redefinition_prefers_explicit_annotation_over_shared_definition() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);

        ws.def_file(
            "addons/test/lua/autorun/sh_redef_shared.lua",
            r#"
                function RealmRedefinitionTarget()
                    return true
                end
            "#,
        );

        ws.def_file(
            "addons/test/lua/autorun/sh_redef_client.lua",
            r#"
                ---@realm client
                function RealmRedefinitionTarget()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/autorun/sh_redef_call.lua",
            r#"
                if SERVER then
                    RealmRedefinitionTarget()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_reports_risky_mismatch_for_client_calling_annotated_server_ent_method() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);

        ws.def_file(
            "addons/test/lua/entities/realm_repro_entity/shared.lua",
            r#"
                ---@realm server
                function ENT:ServerRealmOnlyMethod()
                    return true
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/entities/realm_repro_entity/cl_init.lua",
            r#"
                ENT:ServerRealmOnlyMethod()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == risky_code)
        );
    }

    /// ENT method defined in server file (init.lua) AND in a CLIENT block of shared.lua.
    /// Calling it from init.lua (server) must NOT produce a realm mismatch because
    /// the server definition is realm-compatible.
    #[gtest]
    fn test_no_mismatch_for_ent_method_defined_in_both_server_and_client_realms() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Server file defines the method
        ws.def_file(
            "addons/test/lua/entities/dual_realm_ent/init.lua",
            r#"
                function ENT:GetFuelAmountUnits()
                    return self.fuelAmount or 0
                end
            "#,
        );

        // Client block in shared.lua also defines the same method
        ws.def_file(
            "addons/test/lua/entities/dual_realm_ent/shared.lua",
            r#"
                if CLIENT then
                    function ENT:GetFuelAmountUnits()
                        return self:GetNWFloat("fuel", 0)
                    end
                end
            "#,
        );

        // Server file calls the method — should see both candidates
        let file_id = ws.def_file(
            "addons/test/lua/entities/dual_realm_ent/sv_fuel.lua",
            r#"
                function ENT:ConsumeFuel()
                    local current = self:GetFuelAmountUnits()
                    return current
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let mismatch_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        let heuristic_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code == mismatch_code || d.code == heuristic_code),
            "Expected no realm mismatch for ENT method defined in both server and client, got: {:?}",
            diagnostics
                .iter()
                .filter(|d| d.code == mismatch_code || d.code == heuristic_code)
                .collect::<Vec<_>>()
        );
    }

    /// Same pattern but calling the method from the client realm — also should be fine
    /// since there's a CLIENT-block definition.
    #[gtest]
    fn test_no_mismatch_for_ent_method_defined_in_both_realms_called_from_client() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/entities/dual_realm_ent2/init.lua",
            r#"
                function ENT:GetFuelAmountUnits()
                    return self.fuelAmount or 0
                end
            "#,
        );

        ws.def_file(
            "addons/test/lua/entities/dual_realm_ent2/shared.lua",
            r#"
                if CLIENT then
                    function ENT:GetFuelAmountUnits()
                        return self:GetNWFloat("fuel", 0)
                    end
                end
            "#,
        );

        let file_id = ws.def_file(
            "addons/test/lua/entities/dual_realm_ent2/cl_init.lua",
            r#"
                function ENT:Draw()
                    local fuel = self:GetFuelAmountUnits()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let mismatch_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        let heuristic_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code == mismatch_code || d.code == heuristic_code),
            "Expected no realm mismatch for ENT method defined in both realms, got: {:?}",
            diagnostics
                .iter()
                .filter(|d| d.code == mismatch_code || d.code == heuristic_code)
                .collect::<Vec<_>>()
        );
    }

    /// Same dual-realm pattern but with a plain global table (not ENT), not in
    /// entity directory. Tests that the issue is not specific to scripted classes.
    #[gtest]
    fn test_no_mismatch_for_global_table_method_defined_in_both_realms() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Server file defines the method on a table
        ws.def_file(
            "addons/test/lua/autorun/server/sv_mylib.lua",
            r#"
                MyLib = MyLib or {}
                function MyLib:GetValue()
                    return self.value or 0
                end
            "#,
        );

        // Client file also defines the same method
        ws.def_file(
            "addons/test/lua/autorun/client/cl_mylib.lua",
            r#"
                MyLib = MyLib or {}
                function MyLib:GetValue()
                    return self.netValue or 0
                end
            "#,
        );

        // Server file calls the method — should see both definitions
        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/sv_use_mylib.lua",
            r#"
                local val = MyLib:GetValue()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let mismatch_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        let heuristic_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code == mismatch_code || d.code == heuristic_code),
            "Expected no realm mismatch for table method defined in both realms, got: {:?}",
            diagnostics
                .iter()
                .filter(|d| d.code == mismatch_code || d.code == heuristic_code)
                .collect::<Vec<_>>()
        );
    }

    #[gtest]
    fn test_disabled_when_gmod_off_even_if_diagnostic_enabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "addons/test/lua/autorun/server/sv_api.lua",
            r#"function ServerOnlyApi() return true end"#,
        );
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatch);
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"ServerOnlyApi()"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatch.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == code));

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == risky_code)
        );
    }
}
