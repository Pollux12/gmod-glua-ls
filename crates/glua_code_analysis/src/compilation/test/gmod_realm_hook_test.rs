#[cfg(test)]
mod test {
    use crate::{
        Emmyrc, EmmyrcGmodRealm, EmmyrcGmodScriptedClassDefinition,
        EmmyrcGmodScriptedClassScopeEntry, GmodConVarKind, GmodHookKind, GmodHookNameIssue,
        GmodRealm, GmodTimerKind, VirtualWorkspace,
    };
    use googletest::prelude::*;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    #[gtest]
    fn test_realm_inference_with_filename_and_dependency_hints() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let helper_file_id = ws.def_file("lua/helper.lua", "return {}");
        let payload_file_id = ws.def_file("lua/payload.lua", "return {}");
        let server_file_id = ws.def_file(
            "lua/sv_boot.lua",
            r#"
            include("helper.lua")
            AddCSLuaFile("payload.lua")
            "#,
        );
        let shared_file_id = ws.def_file("lua/sh_shared.lua", "return {}");
        let unknown_file_id = ws.def_file("lua/plain.lua", "return {}");

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

    #[gtest]
    fn test_realm_metadata_updates_after_dependency_removed() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let payload_file_id = ws.def_file("lua/payload.lua", "return {}");
        let server_file_name = "lua/sv_boot.lua";
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

    #[gtest]
    fn test_shared_filename_hint_wins_over_addcsluafile_client_hint() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let shared_file_id = ws.def_file("lua/sh_utf8.lua", "return {}");
        ws.def_file(
            "lua/sv_boot.lua",
            r#"
            AddCSLuaFile("sh_utf8.lua")
            "#,
        );

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&shared_file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
    }

    #[gtest]
    fn test_require_dependency_marks_module_shared_not_caller() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let module_file_id = ws.def_file("lua/dep.lua", "return {}");
        let caller_file_id = ws.def_file("lua/plain.lua", r#"local dep = require("dep")"#);

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

    #[gtest]
    fn test_meta_file_without_annotation_defaults_to_shared() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let meta_file_id = ws.def_file("lua/sv_meta.lua", "---@meta\n");
        ws.def_file(
            "lua/sv_boot.lua",
            r#"
            AddCSLuaFile("sv_meta.lua")
            include("sv_meta.lua")
            local dep = require("sv_meta")
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&meta_file_id)
            .cloned()
            .expect("expected realm metadata");

        // Meta files without annotation default to Shared since they define cross-realm APIs
        assert_eq!(metadata.inferred_realm, GmodRealm::Shared);
        assert_eq!(metadata.annotation_realm, None);
        assert_eq!(metadata.filename_hint, None);
        assert!(metadata.dependency_hints.is_empty());
    }

    #[gtest]
    fn test_meta_file_uses_only_realm_annotation() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let meta_file_id = ws.def_file("lua/cl_meta_annotated.lua", "---@meta\n---@realm server\n");
        ws.def_file(
            "lua/cl_boot.lua",
            r#"
            AddCSLuaFile("cl_meta_annotated.lua")
            include("cl_meta_annotated.lua")
            local dep = require("cl_meta_annotated")
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&meta_file_id)
            .cloned()
            .expect("expected realm metadata");

        assert_eq!(metadata.inferred_realm, GmodRealm::Server);
        assert_eq!(metadata.annotation_realm, Some(GmodRealm::Server));
        assert_eq!(metadata.filename_hint, None);
        assert!(metadata.dependency_hints.is_empty());
    }

    #[gtest]
    fn test_meta_file_without_annotation_defaults_to_shared_when_detection_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.detect_realm_from_filename = Some(false);
        emmyrc.gmod.detect_realm_from_calls = Some(false);
        ws.update_emmyrc(emmyrc);

        let meta_file_id = ws.def_file("lua/sv_meta_disabled.lua", "---@meta\n");

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&meta_file_id)
            .cloned()
            .expect("expected realm metadata");

        assert_eq!(metadata.inferred_realm, GmodRealm::Shared);
        assert_eq!(metadata.annotation_realm, None);
    }

    #[gtest]
    fn test_hook_detection_for_add_emit_and_gamemode_methods() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            hook.Add("Think", "id", function() end)
            hook.Add("PlayerUse", "CityRP.VehicleTrunkAccess", function(client, entity) end)
            hook.Add("", "id", function() end)
            hook.Run("CustomEmit")
            hook.Call(123, GAMEMODE)
            function GM:PlayerSpawn(ply) end
            function GAMEMODE:EntityTakeDamage(ent, dmg) end
            function PLUGIN:PlayerDisconnected(client) end
            function SANDBOX:PlayerSpawnSENT(ply, class_name) end
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
            site.kind == GmodHookKind::Add
                && site.hook_name.as_deref() == Some("PlayerUse")
                && site.callback_params == vec!["client".to_string(), "entity".to_string()]
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
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("PlayerDisconnected")
                && site.callback_params == vec!["client".to_string()]
        }));
        assert!(metadata.sites.iter().any(|site| {
            site.kind == GmodHookKind::GamemodeMethod
                && site.hook_name.as_deref() == Some("PlayerSpawnSENT")
                && site.callback_params == vec!["ply".to_string(), "class_name".to_string()]
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

    #[gtest]
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

    #[gtest]
    fn test_realm_inference_respects_default_realm_config() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.default_realm = crate::EmmyrcGmodRealm::Server;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file("lua/plain.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Server)
        );
    }

    #[gtest]
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

    #[gtest]
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

    #[gtest]
    fn test_hook_detection_with_schema_scope_config() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        // Add SCHEMA scope with hook_owner = true
        emmyrc.gmod.scripted_class_scopes.include =
            vec![EmmyrcGmodScriptedClassScopeEntry::Definition(Box::new(
                EmmyrcGmodScriptedClassDefinition {
                    id: "helix-schema".to_string(),
                    class_global: Some("SCHEMA".to_string()),
                    include: Some(vec!["schema/**".to_string()]),
                    label: Some("Helix Schema".to_string()),
                    path: Some(vec!["schema".to_string()]),
                    root_dir: Some("schema".to_string()),
                    fixed_class_name: Some("SCHEMA".to_string()),
                    is_global_singleton: Some(true),
                    strip_file_prefix: None,
                    hide_from_outline: None,
                    aliases: Some(vec!["Schema".to_string()]),
                    super_types: Some(vec!["GM".to_string()]),
                    hook_owner: Some(true),
                    exclude: None,
                    parent_id: None,
                    icon: None,
                    scaffold: None,
                    disabled: None,
                },
            ))];
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "schema/sh_schema.lua",
            r#"
            function SCHEMA:PlayerSpawn(client) end
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
                && site.callback_params == vec!["client".to_string()]
        }));
    }

    #[gtest]
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

    #[gtest]
    fn test_hook_detection_normalizes_builtin_owner_prefixed_names() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@hook SANDBOX:PlayerSpawnSENT
            function GM:PlayerSpawnSENT(ply, class_name) end

            ---@hook GM:PlayerSpawnSENT
            function SANDBOX:PlayerSpawnSENT(ply, class_name) end
            "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id)
            .cloned()
            .expect("expected hook metadata");

        let spawn_sent_sites = metadata
            .sites
            .iter()
            .filter(|site| {
                site.kind == GmodHookKind::GamemodeMethod
                    && site.hook_name.as_deref() == Some("PlayerSpawnSENT")
            })
            .count();

        assert_eq!(spawn_sent_sites, 2);
    }

    #[gtest]
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

    #[gtest]
    fn test_branch_realm_ranges_persist_after_other_file_update() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let shared_file_id = ws.def_file(
            "lua/sh_branch_scope.lua",
            r#"
            if SERVER then
                function BranchServerOnly() return true end
            end
        "#,
        );

        let has_server_range_before = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&shared_file_id)
            .map(|metadata| {
                metadata
                    .branch_realm_ranges
                    .iter()
                    .any(|r| r.realm == GmodRealm::Server)
            })
            .unwrap_or(false);
        assert!(
            has_server_range_before,
            "Expected Server branch range before unrelated file updates"
        );

        ws.def_file(
            "lua/autorun/client/cl_use_branch_scope.lua",
            r#"
            BranchServerOnly()
        "#,
        );

        let has_server_range_after = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&shared_file_id)
            .map(|metadata| {
                metadata
                    .branch_realm_ranges
                    .iter()
                    .any(|r| r.realm == GmodRealm::Server)
            })
            .unwrap_or(false);

        assert!(
            has_server_range_after,
            "Expected Server branch range to persist after unrelated file updates"
        );
    }

    #[gtest]
    fn test_branch_realm_narrowing_if_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_test.lua",
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
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Client),
            "Expected Client realm range"
        );
    }

    #[gtest]
    fn test_branch_realm_narrowing_if_server_else_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_test2.lua",
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

    #[gtest]
    fn test_branch_realm_narrowing_not_client_is_server() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_not_test.lua",
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

    #[gtest]
    fn test_branch_realm_get_realm_at_offset() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_offset_test.lua",
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

    #[gtest]
    fn test_branch_realm_narrowing_with_parenthesised_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_paren_client.lua",
            r#"
            if (CLIENT) then
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
                .any(|r| r.realm == GmodRealm::Client),
            "Expected Client realm range from `if (CLIENT)`"
        );
    }

    #[gtest]
    fn test_branch_realm_narrowing_with_parenthesised_not_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_paren_not_client.lua",
            r#"
            if (not CLIENT) then
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
            "Expected Server realm range from `if (not CLIENT)` with parentheses"
        );
    }

    #[gtest]
    fn test_branch_realm_narrowing_with_parenthesised_server() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_paren_server.lua",
            r#"
            if (SERVER) then
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
            "Expected Server realm range from `if (SERVER)`",
        );
    }

    #[gtest]
    fn test_branch_realm_narrowing_with_nested_parentheses() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_nested_paren.lua",
            r#"
            if ((CLIENT)) then
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
                .any(|r| r.realm == GmodRealm::Client),
            "Expected Client realm range from `if ((CLIENT))`",
        );
    }

    #[gtest]
    fn test_directory_over_filename_precedence_client_init_lua() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        // The bug case: pac3/lua/pac3/client/init.lua should be Client, not Server
        let file_id = ws.def_file("lua/pac3/client/init.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Client),
            "client/init.lua should be detected as Client (directory over filename precedence)"
        );
    }

    #[gtest]
    fn test_directory_over_filename_precedence_server_shared_lua() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        // server/shared.lua should be Server, not Shared
        let file_id = ws.def_file("lua/pac3/server/shared.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Server),
            "server/shared.lua should be detected as Server (directory over filename precedence)"
        );
    }

    #[gtest]
    fn test_filename_prefix_takes_precedence_over_directory() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        // cl_something.lua in server directory should still be Client (prefix > directory)
        let file_id = ws.def_file("lua/pac3/server/cl_something.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Client),
            "cl_something.lua should be detected as Client (prefix takes precedence over directory)"
        );
    }

    #[gtest]
    fn test_no_lua_anchor_does_not_infer_from_unrelated_parent_directory_names() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        // No /lua/ anchor and no known GMod content-tree hint (addons/gamemodes),
        // so `/server/` in the path must NOT force Server realm.
        let file_id = ws.def_file("workspace/server/plain.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared)
        );
    }

    #[gtest]
    fn test_cl_init_lua_is_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        // cl_init.lua should still be detected as Client
        let file_id = ws.def_file("lua/entities/cl_init.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Client),
            "cl_init.lua should be detected as Client"
        );
    }

    #[gtest]
    fn test_shared_directory_detection() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        // Files in shared/ directory should be detected as Shared
        let file_id = ws.def_file(
            "lua/pac3/core/shared/hash.lua",
            "function pac.Hash(obj) end",
        );

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared),
            "shared/hash.lua should be detected as Shared (directory detection)"
        );
    }

    #[gtest]
    fn test_sh_directory_detection() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        // Files in sh/ directory should be detected as Shared
        let file_id = ws.def_file("lua/pac3/sh/util.lua", "local x = true");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Shared),
            "sh/util.lua should be detected as Shared (directory detection)"
        );
    }

    #[gtest]
    fn test_workspace_root_addon_and_garrysmod_root_paths_match() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let addon_root_id = ws.def_file("lua/autorun/server/sv_boot.lua", "return true");
        let garrysmod_root_id = ws.def_file("lua/autorun/server/sv_boot.lua", "return true");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&addon_root_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&garrysmod_root_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
        );
    }

    #[gtest]
    fn test_workspace_root_gamemode_and_garrysmod_root_paths_match() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let gamemode_root_init = ws.def_file("gamemode/init.lua", "return true");
        let gamemode_root_cl = ws.def_file("gamemode/cl_init.lua", "return true");
        let garrysmod_root_init = ws.def_file("gamemodes/test/gamemode/init.lua", "return true");
        let garrysmod_root_cl = ws.def_file("gamemodes/test/gamemode/cl_init.lua", "return true");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&gamemode_root_init)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&gamemode_root_cl)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&garrysmod_root_init)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&garrysmod_root_cl)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
        );
    }

    #[gtest]
    fn test_gamemode_root_entities_and_effects_infer_without_lua_anchor() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let stool_id = ws.def_file(
            "entities/weapons/gmod_tool/stools/rope.lua",
            "TOOL.Category = 'Constraints'",
        );
        let effect_id = ws.def_file("entities/effects/spark.lua", "return true");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&stool_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&effect_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
        );
    }

    #[gtest]
    fn test_early_return_realm_narrowing_not_client_then_return() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_early_return_test.lua",
            r#"
            print("shared")
            if not CLIENT then return end
            print("client-only")
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
        // But there should be a Client branch range covering code after the early-return guard
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Client),
            "Expected Client realm range from `if not CLIENT then return end` early-return guard"
        );
    }

    #[gtest]
    fn test_early_return_realm_narrowing_not_server_then_return() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_early_return_server.lua",
            r#"
            print("shared")
            if not SERVER then return end
            print("server-only")
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
        // But there should be a Server branch range covering code after the early-return guard
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Server),
            "Expected Server realm range from `if not SERVER then return end` early-return guard"
        );
    }

    #[gtest]
    fn test_early_return_realm_narrowing_client_then_return() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_early_return_client.lua",
            r#"
            print("shared")
            if CLIENT then return end
            print("server-only")
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
        // But there should be a Server branch range covering code after the early-return guard
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Server),
            "Expected Server realm range from `if CLIENT then return end` early-return guard"
        );
    }

    #[gtest]
    fn test_early_return_realm_narrowing_nested_blocks() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "lua/sh_nested_early_return.lua",
            r#"
            print("shared outer")
            if true then
                if not CLIENT then return end
                print("client-only inner")
            end
            print("shared after nested")
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
        // Should have a Client branch range for the nested early-return
        assert!(
            metadata
                .branch_realm_ranges
                .iter()
                .any(|r| r.realm == GmodRealm::Client),
            "Expected Client realm range from nested `if not CLIENT then return end`"
        );
    }

    /// Test with a larger workspace to check if include propagation leaks server realm.
    #[gtest]
    fn test_stool_file_realm_with_full_gamemode_structure() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        // Gamemode core files
        let _gm_init = ws.def_file(
            "gamemodes/sandbox/gamemode/init.lua",
            r#"AddCSLuaFile("cl_init.lua")
            AddCSLuaFile("shared.lua")
            include("shared.lua")"#,
        );
        let _gm_cl_init = ws.def_file(
            "gamemodes/sandbox/gamemode/cl_init.lua",
            r#"include("shared.lua")"#,
        );
        let _gm_shared = ws.def_file(
            "gamemodes/sandbox/gamemode/shared.lua",
            "GM.Name = 'Sandbox'",
        );

        // Weapon entity files
        let _weapon_init = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/init.lua",
            r#"include("shared.lua")
            AddCSLuaFile("cl_init.lua")
            AddCSLuaFile("shared.lua")"#,
        );
        let _weapon_cl = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/cl_init.lua",
            r#"include("shared.lua")"#,
        );
        let _weapon_shared = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/shared.lua",
            "SWEP.PrintName = 'Tool Gun'",
        );

        // Stool file - auto-loaded by engine, no includes pointing to it
        let stool_file = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stools/duplicator.lua",
            r##"
            TOOL.Category = "Construction"
            TOOL.Name = "#tool.duplicator.name"

            if CLIENT then
                language.Add("tool.duplicator.name", "Duplicator")
            end
            "##,
        );

        // Also add some other entity files for a realistic workspace
        let _ent_init = ws.def_file(
            "gamemodes/sandbox/entities/entities/gmod_hands/init.lua",
            r#"include("shared.lua")"#,
        );
        let _ent_cl = ws.def_file(
            "gamemodes/sandbox/entities/entities/gmod_hands/cl_init.lua",
            r#"include("shared.lua")"#,
        );
        let _ent_shared = ws.def_file(
            "gamemodes/sandbox/entities/entities/gmod_hands/shared.lua",
            "ENT.Type = 'anim'",
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        let stool_metadata = infer_index
            .get_realm_file_metadata(&stool_file)
            .expect("stool file should have realm metadata");

        // The stool file should be Shared (default), NOT Server
        assert_eq!(
            stool_metadata.inferred_realm,
            GmodRealm::Shared,
            "Stool file should be Shared realm. filename_hint={:?}, dependency_hints={:?}",
            stool_metadata.filename_hint,
            stool_metadata.dependency_hints,
        );

        let weapon_init_metadata = infer_index
            .get_realm_file_metadata(&_weapon_init)
            .expect("weapon init should have realm metadata");
        let weapon_shared_metadata = infer_index
            .get_realm_file_metadata(&_weapon_shared)
            .expect("weapon shared should have realm metadata");

        // Verify the weapon init is correctly Server (filename detection: init.lua)
        assert_eq!(weapon_init_metadata.inferred_realm, GmodRealm::Server);
        // Verify the weapon shared is correctly Shared (filename detection: shared.lua)
        assert_eq!(weapon_shared_metadata.inferred_realm, GmodRealm::Shared);
    }

    /// Reproduce: fuzzy include resolution can create phantom dependency edges.
    /// When a server-realm file does `include("duplicator.lua")` intending to include
    /// a sibling file, fuzzy resolution can match a completely unrelated file like a stool
    /// with the same base name, leaking "Server" realm to the innocent file.
    #[gtest]
    fn test_fuzzy_include_resolution_leaks_realm_to_unrelated_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        // A server-side addon file that includes its own "duplicator.lua" helper
        let _server_init = ws.def_file("lua/server/sv_init.lua", r#"include("duplicator.lua")"#);
        // The sibling file it INTENDS to include
        let _server_helper = ws.def_file(
            "lua/server/duplicator.lua",
            "-- server-side duplicator helper",
        );

        // An unrelated stool file with the same base name — should NOT be affected
        let stool_file = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stools/duplicator.lua",
            r##"
            TOOL.Category = "Construction"
            TOOL.Name = "#tool.duplicator.name"

            if CLIENT then
                language.Add("tool.duplicator.name", "Duplicator")
            end
            "##,
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        let metadata = infer_index
            .get_realm_file_metadata(&stool_file)
            .expect("stool file should have realm metadata");

        // The stool file should be Shared (default realm), NOT Server.
        // If fuzzy resolution incorrectly matches the include("duplicator.lua") call
        // in sv_init.lua to this stool file, include propagation would leak Server realm.
        assert_eq!(
            metadata.inferred_realm,
            GmodRealm::Shared,
            "Stool file should not be affected by unrelated include(). Got dependency_hints={:?}",
            metadata.dependency_hints,
        );
    }

    /// Tests dependency kind coexistence: when a single source file both
    /// `include()`s and `AddCSLuaFile()`s the same target, both dependency edges must
    /// survive — the second call must not silently overwrite the first.
    /// The target should therefore be Shared (Server via include propagation + Client via AddCSLuaFile).
    #[gtest]
    fn test_include_and_addcsluafile_same_target_both_survive() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let utils_file_id = ws.def_file("lua/utils.lua", "return {}");
        let sv_init_file_id = ws.def_file(
            "lua/sv_init.lua",
            r#"
            include("utils.lua")
            AddCSLuaFile("utils.lua")
            "#,
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        // sv_init.lua has Server filename prefix — must stay Server
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&sv_init_file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "sv_init.lua should be Server (filename hint)",
        );

        // utils.lua receives both a Server hint (via include propagation) and a Client hint
        // (via AddCSLuaFile) — the two hints must both be kept → Shared
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&utils_file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "utils.lua should be Shared (Server from include propagation + Client from AddCSLuaFile target hint)",
        );
    }

    /// Tests no-arg AddCSLuaFile: `AddCSLuaFile()` with no arguments is a
    /// self-reference that marks the calling file for client download.
    /// A plain file (no filename hint) calling it gets a Shared dependency hint
    /// (the file runs on both server as caller and client as target).
    #[gtest]
    fn test_addcsluafile_no_args_marks_self_shared() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let utils_file_id = ws.def_file("lua/utils.lua", r#"AddCSLuaFile()"#);

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&utils_file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "utils.lua calling AddCSLuaFile() with no args should be Shared (self-ref → Shared hint)",
        );
    }

    /// Tests Fix #2 variant: when a file already has a Server filename hint, that hint
    /// takes priority over any dependency-derived hints.
    /// `sv_boot.lua` calling `AddCSLuaFile()` with no args should still be Server.
    #[gtest]
    fn test_addcsluafile_no_args_in_server_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let sv_boot_file_id = ws.def_file("lua/sv_boot.lua", r#"AddCSLuaFile()"#);

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&sv_boot_file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "sv_boot.lua should remain Server — filename hint wins over dependency hints",
        );
    }

    /// Tests Fix #4 (3+ hints): when a file accumulates three or more dependency hints
    /// covering multiple realms, the resolution must still produce a valid result (Shared)
    /// rather than panicking or picking arbitrarily.
    #[gtest]
    fn test_three_dependency_hints_resolve_to_shared() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let utils_file_id = ws.def_file("lua/utils.lua", "return {}");
        // sv_main.lua: include → Server hint on utils.lua, AddCSLuaFile → Client hint on utils.lua
        ws.def_file(
            "lua/sv_main.lua",
            r#"
            include("utils.lua")
            AddCSLuaFile("utils.lua")
            "#,
        );
        // A second caller via require → Shared hint on utils.lua
        ws.def_file("lua/sh_loader.lua", r#"require("utils")"#);

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&utils_file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "utils.lua with Server + Client + Shared hints should resolve to Shared",
        );
    }

    /// Tests Fix #5 (iteration cap): include propagation must reach at least 5 levels deep
    /// so that a chain sv_root → a → b → c → d → e all resolve to Server.
    #[gtest]
    fn test_include_chain_deeper_than_three_levels() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        // Define files from deepest to shallowest so that each include("x.lua") call
        // is resolved against an already-registered file, creating all include edges.
        let e_id = ws.def_file("lua/e.lua", "return {}");
        let d_id = ws.def_file("lua/d.lua", r#"include("e.lua")"#);
        let c_id = ws.def_file("lua/c.lua", r#"include("d.lua")"#);
        let b_id = ws.def_file("lua/b.lua", r#"include("c.lua")"#);
        let a_id = ws.def_file("lua/a.lua", r#"include("b.lua")"#);
        // Root has a Server filename hint and starts the chain
        ws.def_file("lua/sv_root.lua", r#"include("a.lua")"#);

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        for (name, id) in [
            ("a.lua", &a_id),
            ("b.lua", &b_id),
            ("c.lua", &c_id),
            ("d.lua", &d_id),
            ("e.lua", &e_id),
        ] {
            assert_eq!(
                infer_index
                    .get_realm_file_metadata(id)
                    .map(|m| m.inferred_realm),
                Some(GmodRealm::Server),
                "{name} should be Server — propagated from sv_root.lua through 5-level include chain",
            );
        }
    }

    /// Tests Menu default realm: when `default_realm` is set to Menu, files with no
    /// other hints should have `GmodRealm::Unknown` as their inferred realm
    /// (Menu maps to Unknown in the inference engine).
    #[gtest]
    fn test_default_realm_menu_falls_back_correctly() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.default_realm = EmmyrcGmodRealm::Menu;
        ws.update_emmyrc(emmyrc);

        let plain_id = ws.def_file("lua/menu/plain.lua", "local x = 1");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&plain_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Unknown),
            "plain.lua with Menu default_realm should have Unknown inferred realm",
        );
    }

    /// Tests annotation priority: a file-level `---@realm` annotation overrides
    /// filename hints — demonstrating that annotations have the highest priority.
    #[gtest]
    fn test_realm_annotation_overrides_filename_hint() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file("lua/sv_annotated.lua", "---@realm client\n");

        let metadata = ws
            .get_db_mut()
            .get_gmod_infer_index()
            .get_realm_file_metadata(&file_id)
            .cloned()
            .expect("expected realm metadata");

        // Annotation (client) must override the sv_ filename hint (server)
        assert_eq!(
            metadata.inferred_realm,
            GmodRealm::Client,
            "---@realm client annotation should override sv_ filename hint",
        );
        assert_eq!(
            metadata.annotation_realm,
            Some(GmodRealm::Client),
            "annotation_realm should record the annotation value",
        );
    }

    /// Tests cycle handling: when two files mutually include each other and a server-realm
    /// file includes one of them, realm propagation through the cycle must converge to
    /// Server for both files without looping forever.
    #[gtest]
    fn test_cyclic_include_graph_realm_convergence() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        // Use batch addition so all files exist before analysis, ensuring
        // cyclic include("b.lua") ↔ include("a.lua") edges both resolve.
        let ids = ws.def_files(vec![
            ("lua/a.lua", r#"include("b.lua")"#),
            ("lua/b.lua", r#"include("a.lua")"#),
            // sv_entry.lua has Server filename hint and seeds the cycle
            ("lua/sv_entry.lua", r#"include("a.lua")"#),
        ]);
        let a_id = ids[0];
        let b_id = ids[1];

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        assert_eq!(
            infer_index
                .get_realm_file_metadata(&a_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "a.lua should be Server (propagated from sv_entry.lua through cycle)",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&b_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "b.lua should be Server (propagated through a.lua ↔ b.lua cycle)",
        );
    }

    /// Tests bidirectional propagation merge: when both a server-realm and a client-realm
    /// file include the same plain file, that file should become Shared — not arbitrarily
    /// locked to whichever dependency was processed first.
    #[gtest]
    fn test_include_from_both_server_and_client_target_is_shared() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let utils_id = ws.def_file("lua/utils.lua", "return {}");
        ws.def_file("lua/sv_main.lua", r#"include("utils.lua")"#);
        ws.def_file("lua/cl_main.lua", r#"include("utils.lua")"#);

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&utils_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "utils.lua included by both sv_main.lua and cl_main.lua should be Shared",
        );
    }

    /// Tests branch-aware AddCSLuaFile inside an `if SERVER then` block.
    /// AddCSLuaFile marks only the target as Client — no hint is added to the source.
    /// loader.lua has no filename hint and no dependency hints, so it defaults to Shared.
    #[gtest]
    fn test_addcsluafile_inside_if_server_branch() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let helpers_id = ws.def_file("lua/helpers.lua", "return {}");
        let loader_id = ws.def_file(
            "lua/loader.lua",
            r#"
            if SERVER then
                AddCSLuaFile("helpers.lua")
            end
            "#,
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        // AddCSLuaFile no longer adds a Server hint to the source file.
        // loader.lua has no filename hint and no dependency hints → default Shared.
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&loader_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "loader.lua calling AddCSLuaFile should be Shared (no source hint, default realm)",
        );

        // The target is always marked Client by AddCSLuaFile
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&helpers_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "helpers.lua targeted by AddCSLuaFile should be Client",
        );
    }

    /// Tests AddCSLuaFile inside the `else` branch of an `if CLIENT` block.
    /// AddCSLuaFile marks only the target as Client — no hint is added to the source.
    /// loader.lua has no filename hint and no dependency hints, so it defaults to Shared.
    #[gtest]
    fn test_addcsluafile_inside_if_client_else_block() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let helpers_id = ws.def_file("lua/helpers.lua", "return {}");
        let loader_id = ws.def_file(
            "lua/loader.lua",
            r#"
            if CLIENT then
                print("client init")
            else
                AddCSLuaFile("helpers.lua")
            end
            "#,
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        // AddCSLuaFile no longer adds hints to the source file.
        // loader.lua defaults to Shared (no filename hint, no dependency hints).
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&loader_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "loader.lua with AddCSLuaFile in else-block should be Shared (default, no source hints)",
        );

        // helpers.lua is always Client (targeted by AddCSLuaFile)
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&helpers_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "helpers.lua targeted by AddCSLuaFile should be Client",
        );
    }

    /// Tests include from an impossible branch: `sv_main.lua` (Server by filename)
    /// calls `include("helpers.lua")` inside an `if CLIENT then` block.
    /// The filename hint means sv_main.lua stays Server regardless.
    /// Include edges are resolved at the file level (not branch level), so helpers.lua
    /// receives a Server dependency hint from sv_main.lua's include propagation.
    #[gtest]
    fn test_include_cl_helpers_inside_if_client_of_server_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let helpers_id = ws.def_file("lua/helpers.lua", "return {}");
        let sv_main_id = ws.def_file(
            "lua/sv_main.lua",
            r#"
            if CLIENT then
                include("helpers.lua")
            end
            "#,
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        // sv_main.lua retains Server from its filename hint
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&sv_main_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "sv_main.lua should be Server (filename hint wins over branch content)",
        );

        // helpers.lua receives a Server hint via include propagation from sv_main.lua;
        // include edges are processed at file level, not restricted to branch realm
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&helpers_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "helpers.lua should be Server (include propagation from sv_main.lua)",
        );
    }

    /// Tests forward-only propagation: sv_main.lua → helper.lua → sh_config.lua
    /// helper.lua gets Server (forward from sv_main). sh_config keeps Shared (sh_ prefix).
    /// Backward propagation is disabled — including a shared file does NOT make the
    /// includer shared. A server file can legitimately include shared files.
    #[gtest]
    fn test_forward_only_propagation_no_backward_from_shared() -> googletest::Result<()> {
        let mut ws = VirtualWorkspace::new();

        let sh_config_id = ws.def_file(
            "gamemodes/test/gamemode/sh_config.lua",
            "return { version = 1 }",
        );
        let helper_id = ws.def_file(
            "gamemodes/test/gamemode/helper.lua",
            "include(\"sh_config.lua\")\nlocal cfg = sh_config",
        );
        let sv_main_id = ws.def_file(
            "gamemodes/test/gamemode/sv_main.lua",
            "include(\"helper.lua\")",
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        assert_eq!(
            infer_index
                .get_realm_file_metadata(&sv_main_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "sv_main.lua should be Server (filename hint wins)",
        );

        assert_eq!(
            infer_index
                .get_realm_file_metadata(&sh_config_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "sh_config.lua should be Shared (sh_ filename hint)",
        );

        // helper.lua receives Server via forward propagation from sv_main.
        // No backward propagation from sh_config — a server file can include shared files.
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&helper_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "helper.lua should be Server (forward-only propagation from sv_main)",
        );

        Ok(())
    }

    /// Tests GMod autorun directory detection: files in autorun/ are Shared
    /// (loaded on both client and server by the engine).
    #[gtest]
    fn test_autorun_directory_is_shared() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file("lua/autorun/myaddon.lua", "return {}");

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "autorun files should be Shared (engine loads on both realms)",
        );
    }

    /// Tests that autorun/server/ is detected as Server (the /server/ directory
    /// check fires before the /autorun/ check).
    #[gtest]
    fn test_autorun_server_is_server() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file("lua/autorun/server/sv_init.lua", "return {}");
        let plain_id = ws.def_file("lua/autorun/server/myhelper.lua", "return {}");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "autorun/server/ files should be Server",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&plain_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "plain file in autorun/server/ should be Server via /server/ directory detection",
        );
    }

    /// Tests that autorun/client/ is detected as Client.
    #[gtest]
    fn test_autorun_client_is_client() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file("lua/autorun/client/cl_hud.lua", "return {}");
        let plain_id = ws.def_file("lua/autorun/client/myhelper.lua", "return {}");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&file_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "autorun/client/ files should be Client",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&plain_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "plain file in autorun/client/ should be Client via /client/ directory detection",
        );
    }

    /// Tests GMod effects directory detection: effects/ files are Shared
    /// (loaded on both client and server per official GMod loading order).
    /// Critically, effects/init.lua must be Shared, NOT Server (the effects/
    /// check overrides the init.lua → Server special filename check).
    #[gtest]
    fn test_effects_directory_is_shared() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let single_id = ws.def_file("lua/effects/explosion.lua", "return {}");
        let init_id = ws.def_file("lua/effects/fire/init.lua", "return {}");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&single_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "effects/ single-file should be Shared",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&init_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "effects/init.lua should be Shared (effects/ overrides init.lua → Server)",
        );
    }

    /// Tests client-only GMod directories: vgui/, postprocess/, matproxy/, skins/.
    #[gtest]
    fn test_client_only_directories() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let vgui_id = ws.def_file("lua/vgui/dpanel.lua", "return {}");
        let postprocess_id = ws.def_file("lua/postprocess/bloom.lua", "return {}");
        let matproxy_id = ws.def_file("lua/matproxy/player_color.lua", "return {}");
        let skins_id = ws.def_file("lua/skins/default.lua", "return {}");

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&vgui_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "vgui/ files should be Client",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&postprocess_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "postprocess/ files should be Client",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&matproxy_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "matproxy/ files should be Client",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&skins_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Client),
            "skins/ files should be Client",
        );
    }

    /// Tests shared GMod directories: includes/ and stools/.
    #[gtest]
    fn test_shared_directories() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let includes_id = ws.def_file("lua/includes/init.lua", "return {}");
        let stools_id = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stools/rope.lua",
            "TOOL.Category = 'Constraints'",
        );

        let infer_index = ws.get_db_mut().get_gmod_infer_index();
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&includes_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "includes/ files should be Shared (loaded on both realms)",
        );
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&stools_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "stools/ files should be Shared (loaded on both realms)",
        );
    }

    /// Tests IncludeCS handling: IncludeCS is equivalent to AddCSLuaFile + include.
    /// It should add a Client hint to the target AND create an include edge for propagation.
    #[gtest]
    fn test_includecs_adds_client_hint_and_include_edge() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let target_id = ws.def_file("lua/helpers.lua", "return {}");
        let sv_main_id = ws.def_file("lua/sv_main.lua", r#"IncludeCS("helpers.lua")"#);

        let infer_index = ws.get_db_mut().get_gmod_infer_index();

        // sv_main.lua keeps Server from filename hint
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&sv_main_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Server),
            "sv_main.lua should be Server (filename hint)",
        );

        // helpers.lua gets Client (AddCSLuaFile component) + Server (include propagation from sv_main)
        // {Client, Server} → Shared
        assert_eq!(
            infer_index
                .get_realm_file_metadata(&target_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "helpers.lua via IncludeCS should be Shared (Client from AddCSLuaFile + Server from include propagation)",
        );
    }

    /// Tests that AddCSLuaFile does not add any hint to the source file.
    /// A plain file that only calls AddCSLuaFile on another file should remain
    /// at the default realm (Shared), not become Server.
    #[gtest]
    fn test_addcsluafile_does_not_hint_source() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let _target_id = ws.def_file("lua/target.lua", "return {}");
        let source_id = ws.def_file("lua/source.lua", r#"AddCSLuaFile("target.lua")"#);

        assert_eq!(
            ws.get_db_mut()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&source_id)
                .map(|m| m.inferred_realm),
            Some(GmodRealm::Shared),
            "source.lua should be Shared (AddCSLuaFile does not hint the caller)",
        );
    }
}
