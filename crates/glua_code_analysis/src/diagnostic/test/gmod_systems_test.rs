#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
    }

    fn has_duplicate_diagnostic(ws: &VirtualWorkspace, file_id: crate::FileId) -> bool {
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodDuplicateSystemRegistration
                .get_name()
                .to_string(),
        ));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    #[gtest]
    fn test_reports_unknown_static_net_start_message() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::GmodUnknownNetMessage,
            r#"
            net.Start("missing_message")
            "#,
        ));
    }

    #[gtest]
    fn test_ignores_known_or_dynamic_net_start_message() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::GmodUnknownNetMessage,
            r#"
            util.AddNetworkString("known_message")
            net.Start("known_message")
            local message_name = "missing_message"
            net.Start(message_name)
            "#,
        ));
    }

    #[gtest]
    fn test_ignores_server_registered_message_started_from_shared_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "gamemodes/terrortown/gamemode/sync.lua",
            r#"
            util.AddNetworkString("TTT_ClearEntityProperty")
            SYNC = {}

            function SYNC:ClearEntityProperty(ent, propertyName, targets)
                if ent[propertyName] == nil then return end

                ent[propertyName] = nil

                net.Start("TTT_ClearEntityProperty")
                net.WriteEntity(ent)
                net.WriteString(propertyName)
                if targets then
                    net.Send(targets)
                else
                    net.Broadcast()
                end
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodUnknownNetMessage.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[gtest]
    fn test_duplicate_system_registration_enabled_by_default() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            util.AddNetworkString("dup_name")
            util.AddNetworkString("dup_name")
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodDuplicateSystemRegistration
                .get_name()
                .to_string(),
        ));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_when_enabled() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::GmodDuplicateSystemRegistration,
            r#"
            util.AddNetworkString("dup_name")
            util.AddNetworkString("dup_name")
            concommand.Add("dup_cmd", function() end)
            concommand.Add("dup_cmd", function() end)
            CreateConVar("dup_cvar", "1")
            CreateClientConVar("dup_cvar", "1")
            "#,
        ));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_across_files() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "lua/sv_init.lua",
            r#"
            util.AddNetworkString("cross_file_dup")
            "#,
        );
        let second_file = ws.def_file(
            "lua/sv_second.lua",
            r#"
            util.AddNetworkString("cross_file_dup")
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(second_file, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodDuplicateSystemRegistration
                .get_name()
                .to_string(),
        ));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_across_compatible_server_files() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "lua/autorun/server/sv_first.lua",
            r#"
            concommand.Add("server_dup", function() end)
            "#,
        );
        let second_file = ws.def_file(
            "lua/autorun/server/sv_second.lua",
            r#"
            concommand.Add("server_dup", function() end)
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, second_file));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_for_shared_and_server_realms() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "lua/autorun/sh_first.lua",
            r#"
            concommand.Add("shared_server_dup", function() end)
            "#,
        );
        let server_file = ws.def_file(
            "lua/autorun/server/sv_second.lua",
            r#"
            concommand.Add("shared_server_dup", function() end)
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, server_file));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_for_unknown_realm_conservatively() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "lua/custom/first.lua",
            r#"
            concommand.Add("unknown_dup", function() end)
            "#,
        );
        let server_file = ws.def_file(
            "lua/autorun/server/sv_second.lua",
            r#"
            concommand.Add("unknown_dup", function() end)
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, server_file));
    }

    #[gtest]
    fn test_suppresses_duplicate_system_registration_for_disjoint_file_realms() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "lua/autorun/client/cl_cmd.lua",
            r#"
            concommand.Add("realm_split_cmd", function() end)
            "#,
        );
        let server_file = ws.def_file(
            "lua/autorun/server/sv_cmd.lua",
            r#"
            concommand.Add("realm_split_cmd", function() end)
            "#,
        );

        assert!(!has_duplicate_diagnostic(&ws, server_file));
    }

    #[gtest]
    fn test_suppresses_duplicate_system_registration_for_disjoint_branch_realms() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            if CLIENT then
                concommand.Add("branch_split_cmd", function() end)
            end
            if SERVER then
                concommand.Add("branch_split_cmd", function() end)
            end
            "#,
        );

        assert!(!has_duplicate_diagnostic(&ws, file_id));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_across_library_and_main_compatible_realms() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/sh_lib_cmd.lua",
            r#"
            concommand.Add("library_main_dup", function() end)
            "#,
        );
        let main_file = ws.def_file(
            "lua/autorun/sh_main_cmd.lua",
            r#"
            concommand.Add("library_main_dup", function() end)
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, main_file));
    }

    #[gtest]
    fn test_suppresses_duplicate_network_string_from_shadowed_library_gamemode_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_RoundState")
            "#,
        );
        let main_file = ws.def_file(
            "gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_RoundState")
            "#,
        );

        assert!(!has_duplicate_diagnostic(&ws, main_file));
    }

    #[gtest]
    fn test_suppresses_duplicate_scripted_entity_systems_from_shadowed_library_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/entities/entities/ttt_shadow/init.lua",
            r#"
            concommand.Add("ttt_shadow_cmd", function() end)
            CreateConVar("ttt_shadow_cvar", "1")
            "#,
        );
        let main_file = ws.def_file(
            "entities/entities/ttt_shadow/init.lua",
            r#"
            concommand.Add("ttt_shadow_cmd", function() end)
            CreateConVar("ttt_shadow_cvar", "1")
            "#,
        );

        assert!(!has_duplicate_diagnostic(&ws, main_file));
    }

    #[gtest]
    fn test_reports_library_and_main_duplicate_when_virtual_paths_differ() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_PathDiff")
            "#,
        );
        let main_file = ws.def_file(
            "gamemodes/darkrp/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_PathDiff")
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, main_file));
    }

    #[gtest]
    fn test_reports_duplicate_for_same_virtual_path_with_both_files_in_main_workspace() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "addon_a/gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_MainMain")
            "#,
        );
        let second_file = ws.def_file(
            "addon_b/gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_MainMain")
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, second_file));
    }

    #[gtest]
    fn test_reports_duplicate_for_same_virtual_path_when_precedence_is_unknown() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/lua/custom/same_virtual_path.lua",
            r#"
            util.AddNetworkString("TTT_UnknownPrecedence")
            "#,
        );
        let second_file = ws.def_file(
            "lua/custom/same_virtual_path.lua",
            r#"
            util.AddNetworkString("TTT_UnknownPrecedence")
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, second_file));
    }

    #[gtest]
    fn test_reports_duplicate_when_library_file_is_not_shadowed_by_main() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_NotShadowed")
            "#,
        );
        let main_file = ws.def_file(
            "gamemodes/darkrp/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_NotShadowed")
            "#,
        );

        assert!(has_duplicate_diagnostic(&ws, main_file));
    }

    #[gtest]
    fn test_suppresses_shadowed_library_duplicate_with_case_normalized_virtual_path() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/GAMEMODES/TerrorTown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_CasePath")
            "#,
        );
        let main_file = ws.def_file(
            "gamemodes/terrortown/gamemode/init.lua",
            r#"
            util.AddNetworkString("TTT_CasePath")
            "#,
        );

        assert!(!has_duplicate_diagnostic(&ws, main_file));
    }

    #[gtest]
    fn test_duplicate_system_registration_keeps_kinds_separate() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            concommand.Add("same_system_name", function() end)
            CreateConVar("same_system_name", "1")
            "#,
        );

        assert!(!has_duplicate_diagnostic(&ws, file_id));
    }

    #[gtest]
    fn test_gmod_systems_checker_is_disabled_with_gmod_off() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        assert!(ws.check_code_for(
            DiagnosticCode::GmodUnknownNetMessage,
            r#"
            net.Start("missing_message")
            "#,
        ));
    }
}
