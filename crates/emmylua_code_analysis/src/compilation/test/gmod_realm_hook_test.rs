#[cfg(test)]
mod test {
    use crate::{
        Emmyrc, GmodConVarKind, GmodHookKind, GmodHookNameIssue, GmodRealm, GmodTimerKind,
        VirtualWorkspace,
    };

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    #[test]
    fn test_realm_inference_with_filename_and_dependency_hints() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let helper_file_id = ws.def_file("lua/autorun/helper.lua", "return {}");
        let payload_file_id = ws.def_file("lua/autorun/payload.lua", "return {}");
        let server_file_id = ws.def_file(
            "lua/autorun/sv_boot.lua",
            r#"
            include("helper.lua")
            AddCSLuaFile("payload.lua")
            "#,
        );
        let shared_file_id = ws.def_file("lua/autorun/sh_shared.lua", "return {}");
        let unknown_file_id = ws.def_file("lua/autorun/plain.lua", "return {}");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&server_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Server)
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&helper_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Server)
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&payload_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Client)
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&shared_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&unknown_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
    }

    #[test]
    fn test_realm_metadata_updates_after_dependency_removed() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let payload_file_id = ws.def_file("lua/autorun/payload.lua", "return {}");
        let server_file_name = "lua/autorun/sv_boot.lua";
        ws.def_file(server_file_name, r#"AddCSLuaFile("payload.lua")"#);

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&payload_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Client)
        );

        ws.def_file(server_file_name, "local noop = true");
        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&payload_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
    }

    #[test]
    fn test_require_dependency_marks_module_shared_not_caller() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let module_file_id = ws.def_file("dep.lua", "return {}");
        let caller_file_id = ws.def_file("plain.lua", r#"local dep = require("dep")"#);

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&caller_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&module_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
    }

    #[test]
    fn test_hook_detection_for_add_emit_and_gamemode_methods() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            hook.Add("Think", "id", function() end)
            hook.Add("", "id", function() end)
            hook.Run("CustomEmit")
            hook.Call(123, GAMEMODE)
            function GM:PlayerSpawn(ply) end
            function GAMEMODE:EntityTakeDamage(ent, dmg) end
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id)
            .cloned()
            .expect("expected hook metadata");

        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::Add
                && site.hook_name.as_deref() == Some("Think")
                && site.name_issue.is_none()
                && site.callback_params.is_empty()
        }));
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("PlayerSpawn")
                && site.callback_params == vec!["ply".to_string()]
        }));
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("EntityTakeDamage")
                && site.callback_params == vec!["ent".to_string(), "dmg".to_string()]
        }));
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::Emit && site.hook_name.as_deref() == Some("CustomEmit")
        }));
        assert!(
            metadata
                .sites
                .iter()
                .any(|site| site.name_issue == Some(GmodHookNameIssue::Empty))
        );
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::Emit
                && site.name_issue == Some(GmodHookNameIssue::NonStringLiteral)
        }));
    }

    #[test]
    fn test_system_metadata_detection_for_network_concommand_convar_and_timer() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            util.AddNetworkString("sys.net")
            net.Start("sys.net")
            net.Receive("sys.net", function(_, _) end)
            concommand.Add("sys_cmd", function() end)
            CreateConVar("sys_enabled", "1")
            CreateClientConVar("sys_client_enabled", "1")
            timer.Create("sys_timer", 1, 0, function() end)
            timer.Simple(0.25, function() end)
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_system_file_metadata(&file_id)
            .cloned()
            .expect("expected system metadata");

        assert!(
            metadata
                .net_add_string_calls
                .iter()
                .any(|site| site.name.as_deref() == Some("sys.net"))
        );
        assert!(
            metadata
                .net_start_calls
                .iter()
                .any(|site| site.name.as_deref() == Some("sys.net"))
        );
        assert!(metadata.net_receive_calls.iter().any(|site| {
            site.message_name.as_deref() == Some("sys.net") && site.callback.syntax_id.is_some()
        }));
        assert!(metadata.concommand_add_calls.iter().any(|site| {
            site.command_name.as_deref() == Some("sys_cmd") && site.callback.syntax_id.is_some()
        }));
        assert!(metadata.convar_create_calls.iter().any(|site| {
            site.kind == GmodConVarKind::Server
                && site.convar_name.as_deref() == Some("sys_enabled")
        }));
        assert!(metadata.convar_create_calls.iter().any(|site| {
            site.kind == GmodConVarKind::Client
                && site.convar_name.as_deref() == Some("sys_client_enabled")
        }));
        assert!(metadata.timer_calls.iter().any(|site| {
            site.kind == GmodTimerKind::Create
                && site.timer_name.as_deref() == Some("sys_timer")
                && site.callback.syntax_id.is_some()
        }));
        assert!(metadata.timer_calls.iter().any(|site| {
            site.kind == GmodTimerKind::Simple
                && site.timer_name.is_none()
                && site.callback.syntax_id.is_some()
        }));
    }

    #[test]
    fn test_realm_inference_respects_default_realm_config() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.default_realm = crate::EmmyrcGmodRealm::Server;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file("lua/autorun/plain.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Server)
        );
    }

    #[test]
    fn test_hook_detection_respects_hook_mappings() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.hook_mappings.method_to_hook.insert(
            "PLUGIN:PlayerConnect".to_string(),
            "PlayerInitialSpawn".to_string(),
        );
        emmyrc
            .gmod
            .hook_mappings
            .emitter_to_hook
            .insert("myhooks.Emit".to_string(), "*".to_string());
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            function PLUGIN:PlayerConnect(ply) end
            myhooks.Emit("MappedEmit")
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id)
            .cloned()
            .expect("expected hook metadata");

        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("PlayerInitialSpawn")
        }));
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::Emit && site.hook_name.as_deref() == Some("MappedEmit")
        }));
    }

    #[test]
    fn test_hook_detection_respects_method_prefixes() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc
            .gmod
            .hook_mappings
            .method_prefixes
            .push("PLUGIN".to_string());
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            function PLUGIN:PlayerLoaded(ply) end
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id)
            .cloned()
            .expect("expected hook metadata");

        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("PlayerLoaded")
        }));
    }

    #[test]
    fn test_hook_detection_from_hook_annotation() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@hook
            function PLUGIN:PlayerSpawn(client) end

            ---@hook CustomPluginEvent
            function PLUGIN:OnCustomEvent(client) end
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id)
            .cloned()
            .expect("expected hook metadata");

        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("PlayerSpawn")
        }));
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("CustomPluginEvent")
        }));
    }

    #[test]
    fn test_hook_metadata_not_collected_when_gmod_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(r#"hook.Add("Think", "id", function() end)"#);
        assert!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_hook_file_metadata(&file_id)
                .is_none()
        );
    }

    #[test]
    fn test_branch_realm_narrowing_if_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/autorun/sh_test.lua",
            r#"
            if CLIENT then
                print("client only")
            end
        "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&file_id)
            .cloned()
            .expect("expected realm metadata");

        assert!(
            !metadata.branch_realm_ranges.is_empty(),
            "Expected at least one branch realm range"
        );
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Client),
            "Expected a Client realm range"
        );
    }

    #[test]
    fn test_branch_realm_narrowing_if_server_else_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/autorun/sh_test2.lua",
            r#"
            if SERVER then
                print("server only")
            else
                print("client only")
            end
        "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&file_id)
            .cloned()
            .expect("expected realm metadata");

        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Server),
            "Expected a Server realm range for the if block"
        );
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Client),
            "Expected a Client realm range for the else block"
        );
    }

    #[test]
    fn test_branch_realm_narrowing_not_client_is_server() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/autorun/sh_not_test.lua",
            r#"
            if not CLIENT then
                print("server only")
            end
        "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&file_id)
            .cloned()
            .expect("expected realm metadata");

        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Server),
            "Expected Server realm range from `not CLIENT`"
        );
    }

    #[test]
    fn test_branch_realm_get_realm_at_offset() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/autorun/sh_offset_test.lua",
            r#"
            if CLIENT then
                print("client")
            end
            print("shared")
        "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&file_id)
            .cloned()
            .expect("expected realm metadata");

        // File should be shared (sh_ prefix)
        assert_eq!(metadata.inferred_realm, GmodRealm::Shared);
        // But branch ranges should contain a Client range
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Client),
            "Expected Client realm range from if CLIENT block"
        );
    }
}
