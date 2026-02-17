#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    #[gtest]
    fn test_disabled_when_gmod_off() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"AddCSLuaFile("shared.lua")"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuseRisky);
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"AddCSLuaFile("shared.lua")"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuseRisky.get_name().to_string(),
        ));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == code));
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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuse);

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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
        ));
        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuseRisky.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuseRisky);

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
            DiagnosticCode::GmodRealmMisuseRisky.get_name().to_string(),
        ));
        let strict_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
    fn test_prefers_compatible_shared_member_over_client_member() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMisuse);

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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuse);

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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuse);

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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuse);

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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
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
            .enable_only(DiagnosticCode::GmodRealmMisuse);

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
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == strict_code)
        );
    }

    #[gtest]
    fn test_disabled_when_gmod_off_even_if_diagnostic_enabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMisuse);
        let file_id = ws.def_file(
            "addons/test/lua/autorun/client/cl_test.lua",
            r#"AddCSLuaFile("shared.lua")"#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == code));

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMisuseRisky);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let risky_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuseRisky.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == risky_code)
        );
    }
}
