#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn diagnostics_for_code(
        ws: &mut VirtualWorkspace,
        diagnostic_code: DiagnosticCode,
        code: &str,
    ) -> Vec<lsp_types::Diagnostic> {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = ws.def(code);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        diagnostic_code.get_name().to_string(),
                    ))
            })
            .collect()
    }

    fn def_isvalid_type_guard(ws: &mut VirtualWorkspace) {
        ws.def(
            r#"
            ---@class Entity
            ---@class NULL : Entity
            "#,
        );
        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            function IsValid(value) end
            "#,
        );
    }

    fn def_convar_role_fixture(ws: &mut VirtualWorkspace) {
        ws.def(
            r#"
            ---@meta
            ---@class ConVar
            ---@field GetInt fun(self: ConVar): number
            ---@field GetBool fun(self: ConVar): boolean
            ---@field SetBool fun(self: ConVar, value: boolean)

            ---@[call_arg("gmod.convar", "exists")]
            ---@param name string
            ---@return boolean
            function ConVarExists(name) end

            ---@[call_arg("gmod.convar", "reference")]
            ---@param name string
            ---@return ConVar?
            function GetConVar(name) end

            ---@[call_arg("gmod.vgui_panel", "exists")]
            ---@param name string
            ---@return boolean
            function PanelExists(name) end

            ---@[call_arg("gmod.vgui_panel", "reference")]
            ---@param name string
            ---@return ConVar?
            function GetPanelLikeConVar(name) end
            "#,
        );
    }

    fn def_swep_self_call_valid_fixture(ws: &mut VirtualWorkspace) {
        ws.def(
            r#"
            ---@meta
            ---@attribute self_call_valid(method: string)

            ---@class Entity
            ---@field SetHealth fun(self: Entity, health: number)

            ---@class NULL : Entity

            ---@class Weapon : Entity
            ---@return Entity|NULL
            function Weapon:GetOwner() end
            ---@return Entity|NULL
            function Weapon:GetActiveWeapon() end

            ---@[self_call_valid("GetOwner")]
            function Weapon:PrimaryAttack() end

            ---@class SWEP : Weapon
            SWEP = {}

            ---@return Entity|NULL
            function maybeOwner() end

            timer = {}
            ---@param delay number
            ---@param callback fun()
            function timer.Simple(delay, callback) end
            "#,
        );
    }

    #[test]
    fn test_issue_245() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local a --- @type table?
        local _ = (a and a.type == 'change') and a.field
        "#
        ));
    }
    #[test]
    fn test_issue_402() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class A
            local a = {}

            ---@param self table?
            function a.new(self)
                if self then
                    self.a = 1
                end
            end
        "#
        ));
    }

    #[test]
    fn test_issue_474() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Range4
            ---@class TSNode: userdata
            ---@field range fun(self: TSNode): Range4

            ---@param node_or_range TSNode|Range4
            ---@return Range4
            function foo(node_or_range)
                if type(node_or_range) == 'table' then
                    return node_or_range
                else
                    return node_or_range:range()
                end
            end
            "#
        ));
    }

    #[test]
    fn test_notification_pairs_nil_deletion_keeps_panel_fields_non_nullable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Panel
            local PanelBase = {}

            ---@class NoticePanel: Panel
            ---@field fx number
            ---@field fy number
            ---@field VelX number
            ---@field VelY number
            local NoticePanel = {}

            ---@generic T: Panel
            ---@param className `T`
            ---@return T
            function vgui.Create(className) end

            local Notices = {}

            function AddProgress(uid)
                local Panel = vgui.Create("NoticePanel")
                Panel.VelX = -5
                Panel.VelY = 0
                Panel.fx = 200
                Panel.fy = 100
                Notices[uid] = Panel
            end

            function AddLegacy()
                local Panel = vgui.Create("NoticePanel")
                Panel.VelX = -5
                Panel.VelY = 0
                Panel.fx = 200
                Panel.fy = 100
                table.insert(Notices, Panel)
            end

            local function UpdateNotice(pnl, total_h)
                local x = pnl.fx
                local y = pnl.fy
                local spd = 15
                y = y + pnl.VelY * spd
                x = x + pnl.VelX * spd
                pnl.fx = x
                pnl.fy = y
                return total_h + 1
            end

            local h = 0
            for _, pnl in pairs(Notices) do
                h = UpdateNotice(pnl, h)
            end

            for k, Panel in pairs(Notices) do
                if Panel:KillSelf() then
                    Notices[k] = nil
                end
            end
            "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[test]
    fn test_pairs_heterogeneous_record_missing_field_stays_nullable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::UndefinedField,
            r#"
            ---@class OptionalXRecord
            ---@field nested { x: number }|nil

            ---@type table<integer, OptionalXRecord>
            local records = {}

            for _, rec in pairs(records) do
                local nested = rec.nested
                local x = nested.x
                x = x + 1
            end
            "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[test]
    fn test_indexed_read_after_nil_assignment_stays_nullable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local records = {}
            local key = 1
            records[key] = { x = 1 }
            records[key] = nil

            local rec = records[key]
            local x = rec.x
            x = x + 1
            "#,
        );

        assert_that!(diagnostics, not(is_empty()));
    }

    #[test]
    fn test_pairs_call_site_param_inference_does_not_cross_shadowed_local_function() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local records = {}
            records[1] = { x = 1 }

            local function Update(rec)
                local x = rec.missing
                x = x + 1
            end

            for _, rec in pairs(records) do
                Update(rec)
            end

            do
                local function Update(rec)
                    local x = rec.missing
                    x = x + 1
                end

                Update({})
            end
            "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[test]
    fn test_non_pairs_generic_loop_does_not_drive_call_site_param_inference() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::UndefinedField,
            r#"
            ---@return fun(): integer, { x: number }
            local function iter() end

            local function Update(rec)
                local x = rec.missing
                x = x + 1
            end

            for _, rec in iter() do
                Update(rec)
            end
            "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[test]
    fn test_pairs_yield_strips_only_top_level_nil_not_optional_fields() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class NestedRecord
            ---@field x? number

            ---@type table<integer, { nested: NestedRecord }|nil>
            local records = {}

            for _, rec in pairs(records) do
                local nested = rec.nested
                local x = nested.x
                x = x + 1
            end
            "#,
        );

        assert_that!(diagnostics, not(is_empty()));
    }

    #[test]
    fn test_non_nil_union_branch_missing_method_is_not_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            local Entity = {}

            function Entity:DeleteOnRemove(ent) end

            ---@class phys_hinge: Entity

            ---@return phys_hinge|false
            local function Axis()
                return false
            end

            local axis = Axis()
            axis:DeleteOnRemove({})
            "#,
        ));
    }

    #[test]
    fn test_optional_callable_member_on_non_nil_receiver_still_needs_nil_check() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class CallbackOwner
            ---@field callback? fun()

            ---@type CallbackOwner
            local owner = {}
            owner.callback()
            "#,
        );

        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_optional_callable_member_on_guarded_receiver_still_needs_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class CallbackOwner
            ---@field callback? fun()

            ---@return CallbackOwner?
            function maybeOwner() end

            local owner = maybeOwner()
            if IsValid(owner) then
                owner.callback()
            end
            "#,
        );

        assert_that!(diagnostics.len(), eq(1_usize));
        assert_that!(
            diagnostics[0].message.as_str(),
            contains_substring("owner.callback")
        );
    }

    #[test]
    fn test_getconvar_without_existence_guard_still_needs_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            GetConVar("cl_drawhud"):GetInt()
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_load_ordered_convar_registration_suppresses_cached_getconvar_nil() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        def_convar_role_fixture(&mut ws);
        ws.def_file(
            "lua/includes/util.lua",
            r#"
            local ConVarCache = {}

            function GetConVar(name)
                local c = ConVarCache[name]
                if not c then
                    c = GetConVar_Internal(name)
                    if not c then
                        return
                    end

                    ConVarCache[name] = c
                end

                return c
            end
            "#,
        );

        ws.def_file(
            "gamemodes/sandbox/gamemode/cl_init.lua",
            r#"include("cl_spawnmenu.lua")"#,
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/cl_spawnmenu.lua",
            r#"include("spawnmenu/spawnmenu.lua")"#,
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/spawnmenu.lua",
            r#"
            CreateConVar("spawnmenu_toggle", "1")
            include("contextmenu.lua")
            "#,
        );
        let contextmenu_file = ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/contextmenu.lua",
            r#"
            local spawnmenu_toggle = GetConVar("spawnmenu_toggle")

            GM = {}

            function GM:OnContextMenuOpen()
                if spawnmenu_toggle:GetBool() then return end
            end

            function GM:OnContextMenuClose()
                if spawnmenu_toggle:GetBool() then
                    spawnmenu_toggle:SetBool(false)
                end
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(contextmenu_file, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::NeedCheckNil.get_name().to_string(),
                    ))
            })
            .collect::<Vec<_>>();

        assert_that!(diagnostics, is_empty());
    }

    #[test]
    fn test_unregistered_cached_getconvar_still_needs_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local missing_toggle = GetConVar("missing_toggle")
            missing_toggle:GetBool()
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_load_ordered_convar_does_not_suppress_shadowed_getconvar() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        def_convar_role_fixture(&mut ws);

        ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/spawnmenu.lua",
            r#"
            CreateConVar("spawnmenu_toggle", "1")
            include("contextmenu.lua")
            "#,
        );
        let contextmenu_file = ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/contextmenu.lua",
            r#"
            ---@return ConVar?
            local function GetConVar(name) end

            local spawnmenu_toggle = GetConVar("spawnmenu_toggle")
            spawnmenu_toggle:GetBool()
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(contextmenu_file, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::NeedCheckNil.get_name().to_string(),
                    ))
            })
            .collect::<Vec<_>>();

        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_metadata_exists_guard_suppresses_matching_lookup_call_in_then_branch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            if ConVarExists("cl_drawhud") then
                GetConVar("cl_drawhud"):GetInt()
            end
            "#,
        ));
    }

    #[test]
    fn test_metadata_exists_guard_suppresses_matching_lookup_call_after_early_return() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            if not ConVarExists("cl_drawhud") then
                return
            end
            GetConVar("cl_drawhud"):GetInt()
            "#,
        ));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_suppress_cached_lookup_local_yet() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local cv = GetConVar("cl_drawhud")
            if ConVarExists("cl_drawhud") then
                cv:GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_suppress_different_lookup_key() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            if ConVarExists("cl_drawhud") then
                GetConVar("sv_cheats"):GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_suppress_dynamic_lookup_key() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local name = "cl_drawhud"
            if ConVarExists("cl_drawhud") then
                GetConVar(name):GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_suppress_dynamic_guard_key() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local name = "cl_drawhud"
            if ConVarExists(name) then
                GetConVar("cl_drawhud"):GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_suppress_shadowed_functions() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function ConVarExists(name)
                return true
            end
            ---@return ConVar?
            local function GetConVar(name)
                return maybeConVar
            end
            if ConVarExists("cl_drawhud") then
                GetConVar("cl_drawhud"):GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_cross_registry_domains() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            if PanelExists("cl_drawhud") then
                GetConVar("cl_drawhud"):GetInt()
            end
            if ConVarExists("panel_name") then
                GetPanelLikeConVar("panel_name"):GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(2_usize));
    }

    #[test]
    fn test_metadata_exists_guard_does_not_suppress_else_branch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_convar_role_fixture(&mut ws);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            if ConVarExists("cl_drawhud") then
                return
            else
                GetConVar("cl_drawhud"):GetInt()
            end
            "#,
        );
        assert_that!(diagnostics.len(), eq(1_usize));
    }

    #[test]
    fn test_optional_callable_member_on_gmod_null_guarded_receiver_still_needs_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL
            ---@param value any
            ---@return TypeGuard<Entity>
            ---@return_cast value -NULL
            function IsValid(value) end

            ---@class CallbackOwner : Entity
            ---@field callback? fun()

            ---@return CallbackOwner|NULL
            function maybeOwner() end

            local owner = maybeOwner()
            if IsValid(owner) then
                owner.callback()
            end
            "#,
        );

        assert_that!(diagnostics.len(), eq(1_usize));
        assert_that!(
            diagnostics[0].message.as_str(),
            contains_substring("owner.callback")
        );
    }

    #[test]
    fn test_cast() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Cast1
            ---@field get fun(self: self, a: number): Cast1?
            ---@field get2 fun(self: self, a: number): Cast1?
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
                ---@type Cast1
                local A

                local a = A:get(1) --[[@cast -?]]
                    :get2(2)
            "#
        ));
    }

    #[test]
    fn test_issue_895_891() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local t = {
        123,
        234,
        345,
        }

        ---@param id number
        function test(id) end

        for i = 1, #t do
            test(t[i]) -- expected 'number' but found (123|234|345)?
        end
        "#,
        ));
    }

    #[test]
    fn test_plain_table_missing_field_strict_use_prefers_undefined_field_over_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
                local test = {}
                local value = test.meow + 1
            "#
        ));
    }

    #[test]
    fn test_issue_886() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        ---@type string[]
        local a = {}

        -- if #a == 0 then return end
        if not a[1] then return end

        -- ---@type string
        -- local s = a[1]

        ---@type string
        local s = a[#a]
        "#,
        ));
    }

    #[test]
    fn test_no_false_positive_deferred_local_function_call() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local RefreshPanel
            local button = {}

            button.DoClick = function()
                RefreshPanel()
            end

            RefreshPanel = function()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_unchecked_nil_access_for_opaque_table_chained_index() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@type table
            local tbl = {}
            print(tbl.someKey.test)
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_unchecked_nil_access_for_opaque_table_member_call() {
        let mut ws = VirtualWorkspace::new();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@type table
                local tbl = {}
                tbl.someKey()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_unchecked_nil_access_for_opaque_table_method_call() {
        let mut ws = VirtualWorkspace::new();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@type table
                local tbl = {}
                tbl:someMethod()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_dynamic_table_guarded_member_call_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@class SoundPatch
                ---@field Stop fun(self: SoundPatch)

                ---@return SoundPatch
                local function CreateSound()
                end

                local sounds = {}

                ---@param id string
                local function CreateLoopingSound(id)
                    sounds[id] = CreateSound()
                end

                CreateLoopingSound("start")

                if sounds.start then
                    sounds.start:Stop()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_tableof_dynamic_table_guarded_member_call_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@class SoundPatch
                ---@field Stop fun(self: SoundPatch)

                ---@return SoundPatch
                local function CreateSound()
                end

                ---@class TestEntity
                local TestEntity = {}

                ---@generic T
                ---@param ent T
                ---@return tableof<T>
                local function GetTable(ent)
                end

                function TestEntity:Initialize()
                    self.sounds = {}
                end

                ---@param id string
                function TestEntity:CreateLoopingSound(id)
                    local snd = self.sounds[id]

                    if not snd then
                        snd = CreateSound()
                        self.sounds[id] = snd
                    end

                    return snd
                end

                function TestEntity:InternalDeactivateSounds()
                    for id in pairs(self.sounds) do
                        self.sounds[id] = nil
                    end
                end

                function TestEntity:Update()
                    local selfTbl = GetTable(self)
                    local sounds = selfTbl.sounds

                    if sounds.start then
                        sounds.start:Stop()
                    end
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_direct_opaque_table_member_read_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@type table
                local tbl = {}
                local x = tbl.someKey
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_nullable_table_access_stays_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@type table|nil
            local x
            print(x.foo)
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_nullable_any_name_prefix_stays_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@type any?
            local x
            print(x.foo)
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_nullable_entity_method_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any
            "#,
        );
        let code = r#"
            ---@type Entity?
            local ent
            ent:GetPos()
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
    }

    #[gtest]
    fn test_null_method_requires_isvalid() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );
        let code = r#"
            local ent = GetEntityOrNULL()
            ent:GetPos()
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_truthy_check_does_not_narrow_null() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = GetEntityOrNULL()
                if ent then
                    ent:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_isvalid_narrows_null() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
                local ent = GetEntityOrNULL()
                if IsValid(ent) then
                    ent:GetPos()
                end
                "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_isvalid_narrows_explicit_null_param() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            "#,
        );

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
                ---@param ent NULL
                local function takes_null(ent)
                    if IsValid(ent) then
                        ent:GetPos()
                    end
                end
                "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_isentity_does_not_narrow_null_to_valid_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_type_predicates();
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = GetEntityOrNULL()
                if isentity(ent) then
                    ent:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_isentity_and_method_isvalid_chain_guards_valid_entity_methods() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_type_predicates();
        ws.def(
            r#"
            ---@meta

            ---@class Entity
            ---@field Team fun(self: Entity): any
            ---@field Name fun(self: Entity): any

            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@class Player : Entity
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = GetEntityOrNULL()
                if isentity(ent) and ent:IsValid() and ent:IsPlayer() then
                    ent:Team()
                    ent:Name()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_isentity_and_method_isvalid_chain_guards_string_or_entity_union() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_type_predicates();
        ws.def(
            r#"
            ---@meta

            ---@class Entity
            ---@field Team fun(self: Entity): any
            ---@field Name fun(self: Entity): any
            ---@field GetClass fun(self: Entity): string

            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@class Player : Entity

            ---@return string|Entity|nil
            function readNameOrEntity() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local attacker = readNameOrEntity()
                if isentity(attacker) and attacker:IsValid() and attacker:IsPlayer() then
                    attacker = attacker:Name()
                end

                if isentity(attacker) and attacker:IsValid() then
                    attacker = attacker:GetClass()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_explicit_entity_null_union_method_requires_isvalid() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity

            ---@return Entity|NULL
            function GetEntityOrNULL() end
            "#,
        );
        let code = r#"
            local ent = GetEntityOrNULL()
            ent:GetPos()
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_truthy_check_does_not_narrow_explicit_entity_null_union() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity

            ---@return Entity|NULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = GetEntityOrNULL()
                if ent then
                    ent:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_isvalid_narrows_player_or_null_to_player() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@class Player : Entity
            ---@field Nick fun(self: Player): string

            ---@class NULL : Entity

            ---@return Player|NULL
            function GetPlayerOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ply = GetPlayerOrNULL()
                if IsValid(ply) then
                    ply:Nick()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_isentity_does_not_narrow_player_or_null_to_player() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_type_predicates();
        ws.def(
            r#"
            ---@class Entity
            ---@class Player : Entity
            ---@field Nick fun(self: Player): string

            ---@class NULL : Entity

            ---@return Player|NULL
            function GetPlayerOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ply = GetPlayerOrNULL()
                if isentity(ply) then
                    ply:Nick()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_null_member_access_without_call_does_not_require_isvalid() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = GetEntityOrNULL()
                local get_pos = ent.GetPos
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_direct_null_truthy_check_still_requires_isvalid() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity

            ---@type NULL
            NULL = nil
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                if NULL then
                    NULL:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_direct_null_truthy_check_reports_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity

            ---@type NULL
            NULL = nil
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::GmodNullCheck,
                r#"
                if NULL then
                    NULL:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_nil_comparison_does_not_promote_entity_or_null() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        let code = r#"
            local ent = GetEntityOrNULL()
            if ent ~= nil then
                ent:GetPos()
            end
            "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::GmodNullCheck, code),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
    }

    #[gtest]
    fn test_nil_sentinel_branch_before_isvalid_elseif_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::GmodNullCheck,
                r#"
                local owner = {}
                owner.Founder = GetEntityOrNULL()
                if ( owner.Founder == nil ) then
                    return NULL
                elseif ( IsValid(owner.Founder) ) then
                    owner.Founder:GetPos()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_negated_conjunction_early_return_guards_each_stable_operand() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param r number?
            ---@param g number?
            ---@param b number?
            ---@param a number?
            local function make_color(r, g, b, a)
                if not (r and g and b and a) then
                    return error("invalid color")
                end

                return r * 16 + g, b * 16 + a
            end
            "#,
        );
        assert_that!(
            diagnostics,
            is_empty(),
            "negated conjunction early return should guard arithmetic operands"
        );
    }

    #[gtest]
    fn test_global_type_guard_on_optional_field_guards_same_field_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Panel
            ---@field Clear fun(self: Panel)

            ---@param value any
            ---@return TypeGuard<any>
            function IsValid(value) end
            "#,
        );

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Node
            ---@field ChildNodes? Panel

            ---@type Node
            local node = {}

            if IsValid(node.ChildNodes) then
                node.ChildNodes:Clear()
            end

            if not IsValid(node.ChildNodes) then return end
            node.ChildNodes:Clear()
            "#,
        );

        assert_that!(
            diagnostics,
            is_empty(),
            "IsValid on an optional object field should guard that same field"
        );
    }

    #[gtest]
    fn test_outparam_self_receiver_method_effect_guards_optional_field_after_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class DListLayout
            local DListLayout = {}
            function DListLayout:Add() end

            ---@class DTree_Node
            ---@field ChildNodes? DListLayout
            local DTree_Node = {}

            ---@outparam self.ChildNodes DListLayout
            function DTree_Node:CreateChildNodes() end

            ---@type DTree_Node
            local node = {}

            node:CreateChildNodes()
            node.ChildNodes:Add()
            "#,
        );

        assert_that!(
            diagnostics,
            is_empty(),
            "receiver-rooted outparam should narrow the annotated receiver field after the call"
        );
    }

    #[gtest]
    fn test_outparam_self_receiver_library_method_effect_guards_optional_field_after_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_receiver_outparam");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("dtree_node.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@class DListLayout
            local DListLayout = {}
            function DListLayout:Add() end

            ---@class DTree_Node
            ---@field ChildNodes? DListLayout
            local DTree_Node = {}

            ---@outparam self.ChildNodes DListLayout
            function DTree_Node:CreateChildNodes() end
            "#
                .to_string(),
            ),
        );

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type DTree_Node
            local node = {}

            node:CreateChildNodes()
            node.ChildNodes:Add()
            "#,
        );

        assert_that!(
            diagnostics,
            is_empty(),
            "receiver-rooted outparam from a library workspace should narrow main-workspace calls"
        );
    }

    #[gtest]
    fn test_outparam_self_receiver_method_effect_guards_self_field_after_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class DListLayout
            local DListLayout = {}
            function DListLayout:Add() end

            ---@class DTree_Node
            ---@field ChildNodes? DListLayout
            local DTree_Node = {}

            ---@outparam self.ChildNodes DListLayout
            function DTree_Node:CreateChildNodes() end

            function DTree_Node:AddPanel()
                self:CreateChildNodes()
                self.ChildNodes:Add()
            end
            "#,
        );

        assert_that!(
            diagnostics,
            is_empty(),
            "receiver-rooted outparam should narrow self.<field> after self:<method>()"
        );
    }

    #[gtest]
    fn test_outparam_self_receiver_effect_is_receiver_specific() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class DListLayout
            local DListLayout = {}
            function DListLayout:Add() end

            ---@class DTree_Node
            ---@field ChildNodes? DListLayout
            local DTree_Node = {}

            ---@outparam self.ChildNodes DListLayout
            function DTree_Node:CreateChildNodes() end

            ---@type DTree_Node
            local node = {}
            ---@type DTree_Node
            local other = {}

            node:CreateChildNodes()
            other.ChildNodes:Add()
            "#,
        );

        assert_that!(
            diagnostics,
            not(is_empty()),
            "receiver-rooted outparam should only narrow the receiver used in the call"
        );
    }

    #[gtest]
    fn test_outparam_self_receiver_requires_annotation() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class DListLayout
            local DListLayout = {}
            function DListLayout:Add() end

            ---@class DTree_Node
            ---@field ChildNodes? DListLayout
            local DTree_Node = {}

            function DTree_Node:CreateChildNodes() end

            ---@type DTree_Node
            local node = {}

            node:CreateChildNodes()
            node.ChildNodes:Add()
            "#,
        );

        assert_that!(
            diagnostics,
            not(is_empty()),
            "calling an unannotated method must not globally suppress optional-field diagnostics"
        );
    }

    #[gtest]
    fn test_outparam_self_receiver_does_not_leak_from_same_named_unrelated_method() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class DListLayout
            local DListLayout = {}
            function DListLayout:Add() end

            ---@class AnnotatedNode
            ---@field ChildNodes? DListLayout
            local AnnotatedNode = {}

            ---@outparam self.ChildNodes DListLayout
            function AnnotatedNode:CreateChildNodes() end

            ---@class RuntimeNode
            ---@field ChildNodes? DListLayout
            local RuntimeNode = {}

            function RuntimeNode:CreateChildNodes() end

            function RuntimeNode:AddPanel()
                self:CreateChildNodes()
                self.ChildNodes:Add()
            end
            "#,
        );

        assert_that!(
            diagnostics,
            not(is_empty()),
            "same-named unrelated methods must not donate receiver-rooted outparams"
        );
    }

    #[gtest]
    fn test_isvalid_check_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::GmodNullCheck,
                r#"
                local ent = GetEntityOrNULL()
                if IsValid(ent) then
                    ent:GetPos()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_truthy_and_isvalid_guard_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        let code = r#"
            local ent = GetEntityOrNULL()
            if ent and IsValid(ent) then
                ent:GetPos()
            end
            "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::GmodNullCheck, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_not_truthy_or_not_isvalid_guard_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::GmodNullCheck,
                r#"
                local ent = GetEntityOrNULL()
                if not ent or not IsValid(ent) then
                    return
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_nil_comparison_and_isvalid_guard_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        let code = r#"
            local ent = GetEntityOrNULL()
            if ent ~= nil and IsValid(ent) then
                ent:GetPos()
            end
            "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::GmodNullCheck, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_nil_comparison_or_not_isvalid_guard_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::GmodNullCheck,
                r#"
                local ent = GetEntityOrNULL()
                if ent == nil or not IsValid(ent) then
                    return
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_entity_or_null_param_requires_isvalid_inside_function() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@param ent Entity|NULL
                local function use_entity(ent)
                    ent:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_truthy_opaque_table_member_narrows_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@type table
                local tbl = {}
                if tbl.someKey then
                    print(tbl.someKey.test)
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_reverse_len_for_loop_index_on_plain_table_has_no_nil_access_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@param myWeapons table
            local function clear(myWeapons)
                if not myWeapons then
                    return
                end

                for i = #myWeapons, 1, -1 do
                    myWeapons[i]:OnRemove()
                    myWeapons[i] = nil
                end
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_reverse_len_for_loop_index_on_plain_table_has_no_nil_access_diagnostic_in_strict_array_mode()
     {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.strict.array_index = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@param myWeapons table
            local function clear(myWeapons)
                if not myWeapons then
                    return
                end

                for i = #myWeapons, 1, -1 do
                    myWeapons[i]:OnRemove()
                    myWeapons[i] = nil
                end
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_reverse_len_for_loop_index_on_guarded_class_table_field_has_no_nil_access_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.strict.array_index = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class Vehicle
            ---@field weapons table?
            local Vehicle = {}

            function Vehicle:ClearWeapons()
                local myWeapons = self.weapons
                if not myWeapons then
                    return
                end

                for i = #myWeapons, 1, -1 do
                    myWeapons[i]:OnRemove()
                    myWeapons[i] = nil
                end
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_reverse_len_for_loop_index_on_empty_table_const_alias_has_no_nil_access_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.strict.array_index = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            local ENT = {}

            function ENT:Initialize()
                self.weapons = {}
            end

            function ENT:ClearWeapons()
                local myWeapons = self.weapons
                if not myWeapons then
                    return
                end

                for i = #myWeapons, 1, -1 do
                    myWeapons[i]:OnRemove()
                    myWeapons[i] = nil
                end
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_numeric_for_populated_table_field_constant_index_has_no_nil_access_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class DButton
            ---@field SetText fun(self: DButton, text: any)

            ---@return DButton
            local function make_button() end

            local KP_ENTER = 11

            local PANEL = {}

            function PANEL:Init()
                self.Buttons = {}

                for i = 0, 15 do
                    self.Buttons[i] = make_button()
                end

                self.Buttons[KP_ENTER]:SetText("")
            end
        "#;

        assert_that!(
            diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code),
            is_empty()
        );
    }

    #[gtest]
    fn test_numeric_for_populated_table_field_out_of_range_constant_stays_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class DButton
            ---@field SetText fun(self: DButton, text: any)

            ---@return DButton
            local function make_button() end

            local KP_OUT_OF_RANGE = 20

            local PANEL = {}

            function PANEL:Init()
                self.Buttons = {}

                for i = 0, 15 do
                    self.Buttons[i] = make_button()
                end

                self.Buttons[KP_OUT_OF_RANGE]:SetText("")
            end
        "#;

        assert_that!(
            diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code),
            len(eq(1))
        );
    }

    #[gtest]
    fn test_reverse_len_for_loop_index_with_zero_bound_still_reports_nil_access() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@param myWeapons table
            local function clear(myWeapons)
                for i = #myWeapons, 0, -1 do
                    myWeapons[i]:OnRemove()
                end
            end
        "#;

        let has_need_check_nil = !ws.check_code_for(DiagnosticCode::NeedCheckNil, code);
        let has_unchecked_nil_access = !ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code);
        assert_that!(has_need_check_nil || has_unchecked_nil_access, eq(true));
    }

    #[gtest]
    fn test_assignment_chain_initialized_tables_do_not_require_nil_check() {
        let mut ws = VirtualWorkspace::new();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                WepHolster = {}
                WepHolster.defData = {}
                WepHolster.defData["weapon_pistol"] = {}
                WepHolster.defData["weapon_pistol"].Model = "models/weapons/W_pistol.mdl"
                WepHolster.defData["weapon_pistol"].BoneOffset = { Vector(0, 0, 0), Angle(0, 0, 0) }
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_nullable_assignment_lhs_still_requires_nil_check() {
        let mut ws = VirtualWorkspace::new();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@type table?
                local maybe
                maybe.foo = 1
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_isvalid_narrows_nil_from_nullable_type() {
        // Bug repro: annotated IsValid(maybe) should narrow away nil in the true branch.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL
            ---@return Entity?
            function maybeEntity() end

            local maybe = maybeEntity()
            if IsValid(maybe) then
                maybe:GetEditingData()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_nil_negative_branch() {
        // Bug repro: if not IsValid(x) then return end; x should be non-nil after.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@return Entity?
            function maybeEntity() end

            local maybe = maybeEntity()
            if not IsValid(maybe) then
                return
            end
            maybe:GetEditingData()
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_valid_guard_entity_typeguard_suppresses_panel_field_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@meta
            ---@attribute valid_guard()

            ---@class Entity
            ---@param value any
            ---@return TypeGuard<Entity>
            ---@[valid_guard]
            function _G.IsValid(value) end

            ---@class DVScrollBar
            ---@field Remove fun(self: DVScrollBar)
            ---@field SetZPos fun(self: DVScrollBar, z: number)
            ---@class Panel
            ---@generic T: Panel
            ---@param className `T`
            ---@return T
            function vgui.Create(className, parent) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local PANEL = {}
            function PANEL:Init()
                self.VBar = vgui.Create("DVScrollBar", self)
                self.VBar:SetZPos(20)
            end
            function PANEL:DisableScrollbar()
                if ( IsValid( self.VBar ) ) then
                    self.VBar:Remove()
                end
                self.VBar = nil
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_unstable_lvalue_guards_still_report_nil_diagnostics() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Entity
                ---@field Foo fun(self: Entity)
                ---@return Entity?
                function getEnt() end

                IsValid(getEnt()) && getEnt():Foo()
                "#,
            ),
            eq(false)
        );
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Entity
                ---@field Foo fun(self: Entity)
                ---@return table<string, Entity?>
                function getTable() end
                local i = "x"

                IsValid(getTable()[i]) && getTable()[i]:Foo()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_isvalid_narrows_reassigned_clientside_model_negative_branch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field SetModel fun(self: Entity, model: string)
            ---@field SetNoDraw fun(self: Entity, noDraw: boolean)
            ---@class NULL : Entity

            ---@return Entity|NULL
            function ClientsideModel(model, renderGroup) end

            ---@param ent Entity?
            local function Draw(ent)
                if ent == nil then
                    ent = ClientsideModel("error.mdl", 0)
                end

                if ( !IsValid( ent ) ) then return end

                ent:SetModel("error.mdl")
                ent:SetNoDraw(true)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_net_read_entity_negative_branch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            net = {}

            ---@return EntityOrNULL
            function net.ReadEntity() end

            local ent = net.ReadEntity()

            if ( !IsValid( ent ) ) then return end

            local editor = ent:GetEditingData()[ "key" ]
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_loop_assigned_local_negative_branch() {
        // GMod pattern: find an object in a loop, then guard it with IsValid before use.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::UncheckedNilAccess,
            r#"
            ---@class Entity
            ---@class Player : Entity
            ---@field ExitVehicle fun(self: Player)
            ---@return Player[]
            function getPlayers() end

            local bot
            for _, candidate in ipairs(getPlayers()) do
                bot = candidate
                break
            end

            if not IsValid(bot) then
                return
            end

            bot:ExitVehicle()
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_prior_guard_does_not_apply_after_reassignment() {
        // A prior IsValid guard only proves the value held at the guard. If the
        // local is assigned again before use, the later value still needs a check.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)

                ---@type Player?
                local bot
                if not IsValid(bot) then
                    return
                end

                bot = nil
                bot:ExitVehicle()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_shadowed_isvalid_prior_guard_does_not_suppress_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)
                ---@return Player?
                function maybePlayer() end

                local function IsValid(_)
                    return true
                end

                local bot = maybePlayer()
                if not IsValid(bot) then
                    return
                end

                bot:ExitVehicle()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_cross_file_plain_isvalid_prior_guard_does_not_suppress_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/isvalid.lua",
            r#"
            function IsValid(_)
                return true
            end
            "#,
        );

        assert_that!(
            ws.check_file_for(
                DiagnosticCode::NeedCheckNil,
                "lua/autorun/use.lua",
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)
                ---@return Player?
                function maybePlayer() end

                local bot = maybePlayer()
                if not IsValid(bot) then
                    return
                end

                bot:ExitVehicle()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_custom_type_guard_prior_guard_suppresses_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)
                ---@return Player?
                function maybePlayer() end

                ---@param value any
                ---@return TypeGuard<Player>
                function check_player(value) end

                local bot = maybePlayer()
                if not check_player(bot) then
                    return
                end

                bot:ExitVehicle()
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_method_type_guard_prior_guard_suppresses_null_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Entity
                ---@field GetEditingData fun(self: Entity): table
                ---@class NULL : Entity
                ---@alias EntityOrNULL Entity|NULL

                ---@return TypeGuard<Entity>
                ---@return_cast self -NULL
                function Entity:IsValid() end

                ---@return EntityOrNULL
                function getEntity() end

                local ent = getEntity()
                if not ent:IsValid() then
                    return
                end

                ent:GetEditingData()
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_method_type_guard_repeated_call_receiver_still_reports_null_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return TypeGuard<Entity>
            ---@return_cast self -NULL
            function Entity:IsValid() end

            ---@return EntityOrNULL
            function getEntity() end

            if not getEntity():IsValid() then
                return
            end

            getEntity():GetEditingData()
            "#,
        );

        assert_that!(diagnostics.is_empty(), eq(false));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("getEntity()"))
        );
    }

    #[gtest]
    fn test_custom_annotated_colon_self_guard_suppresses_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library file: declare the `self_guard` attribute and the Entity class/method.
        // Must be in a ---@meta file so ---@attribute is recognised.
        // NOTE: `---@[self_guard(...)]` must come AFTER `---@return` so the attribute-use
        // ownership scan (`attribute_find_doc`) does not attach it to the `@return` tag.
        ws.def(
            r#"
            ---@meta
            ---@attribute self_guard(member: string)

                ---@class Entity
                ---@field GetEditingData fun(self: Entity): table
                ---@class NULL : Entity
                ---@alias EntityOrNULL Entity|NULL

                ---@return boolean
                ---@[self_guard("gmod.entity")]
                function Entity:IsAlive() end

                ---@return EntityOrNULL
                function getEntity() end
            "#,
        );

        // Checked code: a non-meta file that uses the `self_guard`-annotated method as a
        // guard. The receiver is Entity|NULL rather than Entity? because a colon call on
        // nil is not safe; this models the common GMod NULL validity pattern.
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = getEntity()
                if not ent:IsAlive() then
                    return
                end

                ent:GetEditingData()
                "#,
            ),
            eq(true)
        );
    }

    /// R4 branch 2: a method with ONLY `---@return_cast self Entity` (no `self_guard`
    /// attribute, no `TypeGuard` return) should also suppress `NeedCheckNil` on the
    /// receiver after a `if not ent:Method() then return end` guard.
    #[gtest]
    fn test_colon_call_return_cast_self_suppresses_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library: Entity class with a method annotated with `return_cast self Entity`
        // only — no self_guard attribute, no TypeGuard return.
        ws.def(
            r#"
            ---@meta

            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table

            ---@return_cast self Entity
            ---@return boolean
            function Entity:IsValid() end

            ---@return EntityOrNULL
            function getEntity() end
            "#,
        );

        // The `return_cast self` path must recognise the method as a NULL-excluding guard.
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = getEntity()
                if not ent:IsValid() then
                    return
                end

                ent:GetEditingData()
                "#,
            ),
            eq(true)
        );
    }

    /// R4 branch 3: a method returning `TypeGuard<Entity>` as a colon-call should
    /// suppress `NeedCheckNil` on the receiver after a guard clause.
    #[gtest]
    fn test_colon_call_typeguard_return_suppresses_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library: Entity class with a method that returns TypeGuard<Entity>.
        ws.def(
            r#"
            ---@meta

            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return TypeGuard<Entity>
            function Entity:IsEntity() end

            ---@return EntityOrNULL
            function getEntity() end
            "#,
        );

        // The `TypeGuard<T>` path applied to a colon-call method must recognise the
        // method as a NULL-excluding guard.
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local ent = getEntity()
                if not ent:IsEntity() then
                    return
                end

                ent:GetEditingData()
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_isvalid_prior_guard_does_not_apply_after_else_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)

                ---@type Player?
                local bot
                if not IsValid(bot) then
                    return
                else
                    bot = nil
                end

                bot:ExitVehicle()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_isvalid_prior_guard_does_not_apply_after_elseif_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)

                ---@type Player?
                local bot
                if not IsValid(bot) then
                    return
                elseif maybeReset then
                    bot = nil
                end

                bot:ExitVehicle()
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_shadowed_isvalid_alias_prior_guard_does_not_suppress_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                ---@class Player
                ---@field ExitVehicle fun(self: Player)
                ---@return Player?
                function maybePlayer() end

                local function IsValid(_)
                    return true
                end

                do
                    local IsValid = IsValid
                    local bot = maybePlayer()
                    if not IsValid(bot) then
                        return
                    end

                    bot:ExitVehicle()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_cached_annotated_isvalid_prior_guard_suppresses_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            function IsValid(value) end

            ---@return Entity?
            function maybeEntity() end

            local IsValid = IsValid
            local ent = maybeEntity()
            if not IsValid(ent) then
                return
            end

            ent:GetEditingData()
            "#,
        ));
    }

    #[gtest]
    fn test_isfunction_narrows_nil_from_nullable_type() {
        // Bug repro: annotated isfunction(func) should narrow away nil in the true branch
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_type_predicates();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type function?
            local func = function() end
            if isfunction(func) then
                func()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_with_glua_library_annotations() {
        // Test that simulates production: load an IsValid annotation from a "library" file
        // and verify the TypeGuard annotation drives nil narrowing.
        let mut ws = VirtualWorkspace::new();

        let library_root = ws.virtual_url_generator.new_path("__test_library_isvalid");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity

            ---@param toBeValidated any The table or object to be validated.
            ---@return TypeGuard<any> # True if the object is valid.
            ---@return_cast toBeValidated -NULL
            function _G.IsValid(toBeValidated) end
            "#
                .to_string(),
            ),
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@return Entity?
            function maybeEntity() end

            local ent = maybeEntity()
            if IsValid(ent) then
                ent:GetEditingData()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_library_isvalid_prior_guard_suppresses_entity_or_null_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_isvalid_entity_or_null");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("global.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
                ---@class Entity
                ---@field GetEditingData fun(self: Entity): table
                ---@class NULL : Entity
                ---@alias EntityOrNULL Entity|NULL

                ---@param value any
                ---@return TypeGuard<any>
                ---@return_cast value -NULL
                function _G.IsValid(value) end
                "#
                .to_string(),
            ),
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            net = {}

            ---@return EntityOrNULL
            function net.ReadEntity() end

            local ent = net.ReadEntity()
            if ( !IsValid( ent ) ) then return end

            local editor = ent:GetEditingData()[ "key" ]
            "#,
        ));
    }

    #[gtest]
    fn test_indexed_annotation_isvalid_prior_guard_suppresses_entity_or_null_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "annotations/global.lua",
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            function _G.IsValid(value) end
            "#,
        );

        assert!(ws.check_file_for(
            DiagnosticCode::NeedCheckNil,
            "lua/includes/extensions/entity.lua",
            r#"
            net = {}

            ---@return EntityOrNULL
            function net.ReadEntity() end

            local ent = net.ReadEntity()
            if ( !IsValid( ent ) ) then return end

            local editor = ent:GetEditingData()[ "key" ]
            "#,
        ));
    }

    #[gtest]
    fn test_local_cached_isvalid_narrows_nil() {
        // Regression: `local IsValid = IsValid` is common in GMod addons for performance.
        // The cached local must still narrow away nil.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        def_isvalid_type_guard(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table

            ---@return Entity?
            function maybeEntity() end

            local IsValid = IsValid
            local maybe = maybeEntity()
            if IsValid(maybe) then
                maybe:GetEditingData()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_renamed_local_cached_isvalid_prior_guard_suppresses_null_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetEditingData fun(self: Entity): table
            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function getEntity() end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            function IsValid(value) end

            local isValid = IsValid
            local ent = getEntity()
            if not isValid(ent) then
                return
            end

            local editor = ent:GetEditingData()
            "#,
        ));
    }

    #[gtest]
    fn test_local_cached_isfunction_narrows_nil() {
        // Regression: `local isfunction = isfunction` is common in GMod addons for performance.
        // The cached local must still narrow away nil.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_type_predicates();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local isfunction = isfunction
            ---@type function?
            local func = function() end
            if isfunction(func) then
                func()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isstring_narrows_nil_from_nullable_type() {
        // Regression: `isstring(x)` should narrow nil from `string?` via TypeGuard annotations.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_type_predicates();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if isstring(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isfunction_member_narrows_to_subtype_nullable_return() {
        // After narrowing Entity→base_glide via isfunction(vehicle.GetFreeSeat),
        // GetFreeSeat returns Entity?, so accessing seat fields needs nil check.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_type_predicates();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        // isfunction(vehicle.GetFreeSeat) narrows vehicle to base_glide.
        // vehicle:GetFreeSeat() returns Entity?  →  seat:IsValid() needs nil check.
        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                if isfunction(vehicle.GetFreeSeat) then
                    local seat = vehicle:GetFreeSeat()
                    seat:IsValid()
                end
            end
            "#,
        );
        assert_that!(
            no_diag,
            eq(false),
            "Expected NeedCheckNil: GetFreeSeat returns Entity?"
        );
    }

    #[gtest]
    fn test_field_truthiness_plus_isfunction_member_narrows() {
        // Combined: vehicle.IsGlideVehicle AND isfunction(vehicle.GetFreeSeat)
        // Both conditions narrow vehicle from Entity to base_glide.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_type_predicates();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                if vehicle.IsGlideVehicle and isfunction(vehicle.GetFreeSeat) then
                    local seat = vehicle:GetFreeSeat()
                    seat:IsValid()
                end
            end
            "#,
        );
        assert_that!(
            no_diag,
            eq(false),
            "Expected NeedCheckNil with AND narrowing"
        );
    }

    #[gtest]
    fn test_field_truthiness_narrows_to_subtype_with_field() {
        // vehicle.IsGlideVehicle alone (truthiness of a field) should narrow
        // vehicle from Entity to base_glide (the subtype that owns that field).
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                if vehicle.IsGlideVehicle then
                    local seat = vehicle:GetFreeSeat()
                    seat:IsValid()
                end
            end
            "#,
        );
        assert_that!(
            no_diag,
            eq(false),
            "Expected NeedCheckNil after field truthiness narrowing"
        );
    }

    #[gtest]
    fn test_field_narrow_with_class_hierarchy_no_nil_on_method() {
        // When parent is narrowed via field check, methods inherited from the base should not show nil
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity

            ---@class BaseVehicle: Entity
            ---@field IsSpecialVehicle boolean
            ---@field GetLockState fun(self: BaseVehicle): boolean

            ---@class CarVehicle: BaseVehicle

            ---@class BoatVehicle: BaseVehicle
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param seat Entity
            function test(seat)
                local parent = seat:GetParent()
                if not IsValid(parent) then return end
                if not parent.IsSpecialVehicle then return end
                parent:GetLockState()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_field_narrow_false_branch_no_nil() {
        // In the false branch (field doesn't exist), variable should retain original type
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class Animal: Entity
            ---@field IsDog boolean

            ---@class Dog: Animal
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param ent Entity
            function test(ent)
                if not IsValid(ent) then return end
                if ent.IsDog then return end
                -- ent is still Entity here (false branch), NOT nil
                ent:GetPos()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_then_field_narrow_no_nil() {
        // IsValid + field check combo should work without nil issues
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class SpecialEnt: Entity
            ---@field IsSpecial boolean
            ---@field DoSpecialThing fun(self: SpecialEnt): boolean
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param ent Entity
            function test(ent)
                if not IsValid(ent) then return end
                if not ent.IsSpecial then return end
                ent:DoSpecialThing()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_no_narrowing_no_diagnostic_on_unknown_method() {
        // Without narrowing, vehicle is Entity which has no GetFreeSeat.
        // Calling vehicle:GetFreeSeat() without a guard should NOT produce
        // NeedCheckNil (it would produce a different diagnostic like undefined-field).
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                local seat = vehicle:GetFreeSeat()
                seat:IsValid()
            end
            "#,
        );
        assert_that!(
            no_diag,
            eq(true),
            "No NeedCheckNil expected: Entity has no GetFreeSeat"
        );
    }

    #[gtest]
    fn test_missing_receiver_field_method_call_reports_unchecked_nil_access() {
        // Repro: unresolved receiver field in method call should still produce
        // an unchecked nil access diagnostic on the receiver expression.
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@class VSWEP
            ---@field SlotIndex integer
            local VSWEP = {}

            function VSWEP:PrimaryAttackInternal()
                local allowDefaultBehaviour = self.Vehicle:OnWeaponFire(self, self.SlotIndex)
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(false),
            "expected unchecked-nil-access for self.Vehicle receiver in colon call"
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true),
            "definite nil receiver should escalate to unchecked-nil-access, not need-check-nil"
        );
    }

    #[gtest]
    fn test_missing_self_field_local_alias_method_call_reports_unchecked_nil_access() {
        // Repro: `local vehicle = self.Vehicle` followed by `vehicle:...` should
        // still report nil access for the local receiver alias.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class VSWEP
            local VSWEP = {}

            function VSWEP:PrimaryAttackInternal()
                local vehicle = self.Vehicle
                vehicle:FireBullet({})
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(false),
            "expected unchecked-nil-access for local alias of self.Vehicle in colon call"
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true),
            "definite nil alias receiver should escalate to unchecked-nil-access, not need-check-nil"
        );
    }

    #[gtest]
    fn test_field_narrow_direct_definer_no_nil_on_method() {
        // After narrowing to the direct field definer, methods on that type should not trigger nil
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity

            ---@class BaseGlide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetIsLocked fun(self: BaseGlide): boolean

            ---@class GlideCar: BaseGlide

            ---@class GlideAirboat: BaseGlide
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param seat Entity
            function test(seat)
                local parent = seat:GetParent()
                if not IsValid(parent) then return end
                if not parent.IsGlideVehicle then return end
                parent:GetIsLocked()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_no_duplicate_nil_check_for_call_on_uninit_local() {
        // Regression: `local test; test.meow()` should not produce two need-check-nil
        // diagnostics. The call-site check (check_call_expr) covers it, and the
        // index-expr check (check_index_expr) is suppressed when the IndexExpr is
        // a call prefix and itself nullable.
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            local test
            test.meow()
        "#;

        let need_check_nil_diagnostics =
            diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            need_check_nil_diagnostics.len(),
            eq(0_usize),
            "definite-nil receiver call should be unchecked-nil-access, not need-check-nil"
        );
    }

    #[gtest]
    fn test_single_unchecked_nil_access_for_call_on_uninit_local() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            local test
            test.meow()
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics.len(), eq(1_usize));

        let diagnostic = &diagnostics[0];
        assert_that!(
            diagnostic.message.as_str(),
            contains_substring("may be nil")
        );
        assert_that!(
            diagnostic.range.end.character - diagnostic.range.start.character,
            eq(4_u32),
            "unchecked nil receiver diagnostic should target `test`"
        );
    }

    #[gtest]
    fn test_nil_check_for_field_access_on_uninit_local_still_emits() {
        // `local test; local x = test.meow` — no call, so check_call_expr does
        // NOT fire. check_index_expr must still emit because `test` is nil.
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            local test
            local x = test.meow
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(false)
        );
    }

    #[gtest]
    fn test_isvalid_and_method_call_on_indexed_receiver_has_no_nil_access_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Seat
            ---@field GetDriver fun(self: Seat): Entity?
            "#,
        );

        let code = r#"
            ---@param seats Seat[]
            ---@param count integer
            local function test(seats, count)
                for i = count, 1, -1 do
                    local driver = IsValid(seats[i]) and seats[i]:GetDriver()
                end
            end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
    }

    #[gtest]
    fn test_dynamic_field_alias_receiver_guard_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class CSoundPatch
            local CSoundPatch = {}

            function CSoundPatch:Stop() end
            function CSoundPatch:ChangeVolume(volume) end

            ---@return CSoundPatch
            function CreateSound(parent, path) end

            local ENT = {}

            function ENT:Initialize()
                self.sounds = {}
                self.sounds.turbo = CreateSound(self, "turbo.wav")
            end

            function ENT:OnUpdateSounds()
                local sounds = self.sounds

                if sounds.turbo then
                    sounds.turbo:Stop()
                    sounds.turbo:ChangeVolume(0.5)
                end
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_callback_parameter_identity_field_guard_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class DPanel
            local DPanel = {}

            function DPanel:UpdateValue(value) end

            ---@return DPanel
            local function makePanel()
                return {}
            end

            local row = {}
            row.field = makePanel()
            row.Bind = function(s, newData)
                if s.field then
                    s.field:UpdateValue(newData)
                end
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_nested_short_circuit_receiver_guard_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class ProblemData
            ---@field title string|nil

            ---@class ProblemPanel
            ---@field Problem ProblemData|nil

            ---@param self ProblemPanel
            local function Paint(self)
                if not self.Problem then return end

                if self.Problem.title and self.Problem.title:len() > 0 then
                    local title = self.Problem.title
                end
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_derma_menu_spacer_assignment_guard_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Panel
            local Panel = {}

            function Panel:SetZPos(pos) end

            ---@class DPanel : Panel
            local DPanel = {}

            ---@class DMenu : Panel
            local DMenu = {}

            ---@return DPanel
            function DMenu:AddSpacer() end

            ---@return DMenu
            function DermaMenu() end

            local function createMenu()
                local menu = DermaMenu()
                if not menu.ToggleSpacer then menu.ToggleSpacer = menu:AddSpacer() end
                menu.ToggleSpacer:SetZPos(500)
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_derma_menu_spacer_unannotated_helper_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Panel
            local Panel = {}

            function Panel:SetZPos(pos) end

            ---@class DPanel : Panel
            local DPanel = {}

            ---@class DMenu : Panel
            local DMenu = {}

            ---@return DPanel
            function DMenu:AddSpacer() end

            ---@return DMenu
            function DermaMenu() end

            local function AddToggleOption(data, menu, ent, ply, tr)
                if not menu.ToggleSpacer then
                    menu.ToggleSpacer = menu:AddSpacer()
                    menu.ToggleSpacer:SetZPos(500)
                end
            end

            local menu = DermaMenu()
            AddToggleOption({}, menu)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_derma_menu_spacer_transitive_unannotated_helpers_have_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Panel
            local Panel = {}

            function Panel:SetZPos(pos) end

            ---@class DPanel : Panel
            local DPanel = {}

            ---@class DMenu : Panel
            local DMenu = {}

            ---@return DPanel
            function DMenu:AddSpacer() end

            ---@return DMenu
            function DermaMenu() end

            local function AddToggleOption(data, menu, ent, ply, tr)
                if not menu.ToggleSpacer then
                    menu.ToggleSpacer = menu:AddSpacer()
                    menu.ToggleSpacer:SetZPos(500)
                end
            end

            local function AddOption(data, menu, ent, ply, tr)
                AddToggleOption(data, menu, ent, ply, tr)
            end

            local function OpenEntityMenu(ent, ply, tr)
                local menu = DermaMenu()
                AddOption({}, menu, ent, ply, tr)
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_local_helper_nullable_call_site_preserves_need_check_nil() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class DMenu
            ---@field ToggleSpacer DPanel
            function DMenu:Open() end

            ---@class DPanel
            function DPanel:SetZPos(pos) end

            ---@return DMenu
            function DermaMenu() end

            ---@return DMenu?
            local function MaybeMenu() end

            local function UseMenu(menu)
                menu.ToggleSpacer:SetZPos(500)
            end

            local menu = MaybeMenu()
            UseMenu(DermaMenu())
            UseMenu(menu)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(diagnostics, not(is_empty()));
    }

    #[gtest]
    fn test_local_helper_direct_zero_arg_call_site_param_inference() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class DPanel
            function DPanel:SetZPos(pos) end

            ---@class DMenu
            ---@field ToggleSpacer DPanel
            local DMenu = {}

            ---@return DMenu
            function DermaMenu() end

            local function UseMenu(menu)
                menu.ToggleSpacer:SetZPos(500)
            end

            UseMenu(DermaMenu())
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_local_helper_call_site_param_inference_does_not_cross_files() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let helper_file = ws.def_file(
            "lua/autorun/helper.lua",
            r#"
            ---@class DMenu
            function DMenu:Open() end

            local function UseMenu(menu)
                menu:Open()
            end
            "#,
        );

        ws.def_file(
            "lua/autorun/caller.lua",
            r#"
            ---@class DMenu
            function DMenu:Open() end

            ---@class DPanel

            ---@return DMenu
            function DermaMenu() end

            UseMenu(DermaMenu())
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(helper_file, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_local_helper_self_forwarding_call_site_param_inference_is_safe() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class DMenu
            ---@field ToggleSpacer DPanel
            function DMenu:Open() end

            ---@class DPanel

            local function f(menu)
                f(menu)
                local spacer = menu.ToggleSpacer
            end


            ---@return DMenu
            function DermaMenu() end

            f(DermaMenu())
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_local_helper_mutual_forwarding_call_site_param_inference_is_safe() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class DMenu
            ---@field ToggleSpacer DPanel

            ---@class DPanel

            local function f(menu)
                g(menu)
                local spacer = menu.ToggleSpacer
            end

            local function g(menu)
                f(menu)
            end

            ---@return DMenu
            function DermaMenu() end

            f(DermaMenu())
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_local_helper_call_site_param_inference_respects_shadowed_local_function() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class DMenu
            ---@field ToggleSpacer DPanel

            ---@class DPanel
            ---@return DMenu
            function DermaMenu() end

            local function UseMenu(menu)
                menu:Open()
            end

            do
                local function UseMenu(value) end
                UseMenu(DermaMenu())
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_problem_lua_constructor_table_title_short_circuit_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class ProblemData
            ---@field title string
            ---@field text string

            ---@return ProblemData
            local function CreateProblem()
                return {
                    title = "Missing addon dependency",
                    text = "Install the required workshop item before joining."
                }
            end

            ---@class ProblemPanel
            ---@field Problem ProblemData|nil
            local PANEL = {}

            function PANEL:Init()
                self.Problem = CreateProblem()

                self.Paint = function()
                    if not self.Problem then return end

                    if self.Problem.title and self.Problem.title:len() > 0 then
                        local title = self.Problem.title
                    end
                end
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_vgui_callback_row_field_guard_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Panel
            local Panel = {}

            function Panel:UpdateVector(value) end

            ---@return Panel
            local function makePanel()
                return {}
            end

            local data = Vector(1, 2, 3)
            local row = {}
            row.offsetRow = makePanel()
            row.Bind = function(s, data)
                if s.offsetRow then
                    s.offsetRow:UpdateVector(data)
                end
            end

            row:Bind(data)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_trace_result_hit_pos_method_call_has_no_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Vector
            local Vector = {}

            ---@param other Vector
            ---@return number
            function Vector:DistToSqr(other) end

            ---@class TraceResult
            ---@field HitPos Vector

            ---@class Player
            local Player = {}

            ---@return TraceResult
            function Player:GetEyeTraceNoCursor() end

            ---@return Vector
            function Player:GetPos() end


            ---@param ply Player
            local function CanUseWeapon(ply)
                local tr = ply:GetEyeTraceNoCursor()
                local ok = tr.HitPos:DistToSqr(ply:GetPos()) <= 10000
                return ok
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_isvalid_not_narrows_entity_null_early_return() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field SetModel fun(self: Entity, model: string)

            ---@class prop_vehicle_prisoner_pod : Entity

            ---@class NULL : Entity

            ---@return prop_vehicle_prisoner_pod|NULL
            function ents_Create() end
            "#,
        );

        // After "if not IsValid(seat) then return end", seat should be narrowed
        // to prop_vehicle_prisoner_pod (no NULL)
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local seat = ents_Create()
                if not IsValid(seat) then
                    return
                end
                seat:SetModel("models/nova/airboat_seat.mdl")
                "#,
            ),
            eq(true)
        );
    }

    /// Regression test: IsValid must narrow Instance(T|NULL) to Instance(T).
    ///
    /// Production pattern from `ents.Create` which uses `@generic T : Entity`
    /// and `@return (instance) T|NULL`. The `(instance)` modifier wraps the
    /// return type in a LuaInstanceType, so the NULL is inside the Instance
    /// wrapper: `Instance(T|NULL)`. The `remove_type` function must recurse
    /// into Instance types to remove NULL from the inner union.
    #[gtest]
    fn test_isvalid_narrows_instance_type_union_null() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field SetModel fun(self: Entity, modelName: string)

            ---@class prop_vehicle_prisoner_pod : Entity

            ---@class NULL : Entity

            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents_Create(class) end
            "#,
        );

        // `if not IsValid(seat) then return end` should narrow
        // Instance(prop_vehicle_prisoner_pod|NULL) to Instance(prop_vehicle_prisoner_pod)
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local seat = ents_Create("prop_vehicle_prisoner_pod")

                if not IsValid(seat) then
                    return
                end

                seat:SetModel("models/nova/airboat_seat.mdl")
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_isvalid_type_guard_entity_excludes_null_after_early_return() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field Spawn fun(self: Entity)

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end
            "#,
        );

        let guarded_file = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            if not IsValid(balloon) then return NULL end
            balloon:Spawn()
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(guarded_file, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics,
            is_empty(),
            "`if not IsValid(balloon) then return NULL end` should prove `balloon` is not NULL."
        );

        let unguarded_file = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            balloon:Spawn()
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(unguarded_file, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("balloon may be NULL")),
            eq(true),
            "Unguarded Entity|NULL factory results must still warn. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_isvalid_type_guard_entity_excludes_null_in_nested_descendant_after_early_return() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            ---@class Entity
            ---@field Spawn fun(self: Entity)

            ---@class NULL : Entity
            ---@type NULL
            NULL = nil

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end

            ---@param ent any
            ---@return TypeGuard<Entity>
            function IsValid(ent) end
            "#,
        );

        let guarded_file = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            if not IsValid(balloon) then return NULL end

            if enabled then
                balloon:Spawn()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(guarded_file, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics,
            is_empty(),
            "Early-return IsValid guards should narrow inferred Entity|NULL locals inside descendant blocks."
        );
    }

    #[gtest]
    fn test_nested_descendant_entity_null_access_without_isvalid_guard_still_reports() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            ---@class Entity
            ---@field Spawn fun(self: Entity)

            ---@class NULL : Entity
            ---@type NULL
            NULL = nil

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end
            "#,
        );

        let unguarded_file = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            if enabled then
                balloon:Spawn()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(unguarded_file, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("balloon may be NULL")),
            eq(true),
            "Nested access without an IsValid guard must still warn. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_nested_descendant_isvalid_guard_in_conditional_sibling_does_not_dominate_later_access()
    {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field Spawn fun(self: Entity)

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end
            "#,
        );

        let file_id = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            if should_check then
                if not IsValid(balloon) then return NULL end
            end

            if enabled then
                balloon:Spawn()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("balloon may be NULL")),
            eq(true),
            "A guard nested inside a conditional sibling does not dominate later access. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_nested_descendant_isvalid_guard_invalidated_by_same_block_reassignment_before_access() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field Spawn fun(self: Entity)

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end
            "#,
        );

        let file_id = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            if not IsValid(balloon) then return NULL end

            if enabled then
                balloon = ents.Create("gmod_balloon")
                balloon:Spawn()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("balloon may be NULL")),
            eq(true),
            "A later nullable reassignment in the access block must invalidate an outer IsValid guard. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_nested_descendant_isvalid_guard_invalidated_by_intermediate_block_reassignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field Spawn fun(self: Entity)

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end
            "#,
        );

        let file_id = ws.def(
            r#"
            local balloon = ents.Create("gmod_balloon")
            if not IsValid(balloon) then return NULL end

            if outer then
                balloon = ents.Create("gmod_balloon")
                if inner then
                    balloon:Spawn()
                end
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("balloon may be NULL")),
            eq(true),
            "A later nullable reassignment in an intermediate block must invalidate an outer IsValid guard. Diagnostics: {diagnostics:#?}"
        );
    }

    /// Same as above but using `if IsValid(seat) then` (positive branch).
    #[gtest]
    fn test_isvalid_narrows_instance_type_union_null_positive_branch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);
        ws.def(
            r#"
            ---@class Entity
            ---@field SetModel fun(self: Entity, modelName: string)

            ---@class prop_vehicle_prisoner_pod : Entity

            ---@class NULL : Entity

            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents_Create(class) end
            "#,
        );

        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local seat = ents_Create("prop_vehicle_prisoner_pod")

                if IsValid(seat) then
                    seat:SetModel("models/nova/airboat_seat.mdl")
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_swep_callback_self_getowner_chained_not_null() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_swep_self_call_valid_fixture(&mut ws);

        let file_id = ws.def(
            r#"
            function SWEP:PrimaryAttack()
                self:GetOwner():SetHealth(1)
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_swep_callback_local_owner_not_null() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_swep_self_call_valid_fixture(&mut ws);

        let file_id = ws.def(
            r#"
            function SWEP:PrimaryAttack()
                local owner = self:GetOwner()
                owner:SetHealth(1)
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_swep_callback_owner_reassigned_still_flagged() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_swep_self_call_valid_fixture(&mut ws);

        let file_id = ws.def(
            r#"
            function SWEP:PrimaryAttack()
                local owner = self:GetOwner()
                owner = maybeOwner()
                owner:SetHealth(1)
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("owner may be NULL")),
            eq(true),
            "Reassigned owner must still be diagnosed. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_unmarked_method_self_getowner_still_flagged() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_swep_self_call_valid_fixture(&mut ws);

        let file_id = ws.def(
            r#"
            function SWEP:CheckLimit()
                self:GetOwner():SetHealth(1)
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("self:GetOwner() may be NULL")),
            eq(true),
            "Unmarked helper method must still be diagnosed. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_marked_callback_other_nullable_self_method_still_flagged() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_swep_self_call_valid_fixture(&mut ws);

        let file_id = ws.def(
            r#"
            function SWEP:PrimaryAttack()
                self:GetActiveWeapon():SetHealth(1)
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("self:GetActiveWeapon() may be NULL")),
            eq(true),
            "Marker only covers GetOwner. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_getowner_in_nested_deferred_closure_still_flagged() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_swep_self_call_valid_fixture(&mut ws);

        let file_id = ws.def(
            r#"
            function SWEP:PrimaryAttack()
                timer.Simple(0, function()
                    self:GetOwner():SetHealth(1)
                end)
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("self:GetOwner() may be NULL")),
            eq(true),
            "Nested/deferred closures must not inherit callback owner validity. Diagnostics: {diagnostics:#?}"
        );
    }

    /// Instance(T|NULL) without IsValid guard should still produce a diagnostic.
    #[gtest]
    fn test_instance_type_union_null_without_isvalid_still_diagnoses() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            ---@class Entity
            ---@field SetModel fun(self: Entity, modelName: string)

            ---@class prop_vehicle_prisoner_pod : Entity

            ---@class NULL : Entity

            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents_Create(class) end
            "#,
        );

        // Without IsValid guard, diagnostic should fire
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::NeedCheckNil,
                r#"
                local seat = ents_Create("prop_vehicle_prisoner_pod")
                seat:SetModel("models/nova/airboat_seat.mdl")
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_bracket_index_need_check_nil_range_covers_full_prefix_name() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            local lastNick
            local nick = "x"
            local i = 1
            local _ = lastNick[i] ~= nick
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);
        assert_that!(diagnostics.len(), eq(1_usize));

        let diagnostic = &diagnostics[0];
        assert_that!(
            diagnostic.message.as_str(),
            contains_substring("lastNick may be nil")
        );
        assert_that!(
            diagnostic.range.end.character - diagnostic.range.start.character,
            eq(8_u32),
            "need-check-nil on `lastNick[i]` should span full `lastNick`"
        );
    }

    #[gtest]
    fn test_reassigned_table_literal_field_is_not_unchecked_nil_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/autorun/sh_glide.lua",
            r#"
            Glide = Glide or {}

            function Glide.FromJSON(s)
                if type(s) ~= "string" or s == "" then
                    return {}
                end

                return util.JSONToTable(s) or {}
            end

            function Glide.ToJSON(t, prettyPrint)
                return util.TableToJSON(t, prettyPrint)
            end
            "#,
        );
        ws.def_file(
            "lua/glide/sh_utils.lua",
            r#"
            function Glide.ValidateStreamData(data)
                if type(data) ~= "table" then
                    return false, "Preset is not a table!"
                end

                local layers = data.layers
                if type(layers) ~= "table" then
                    return false, "Preset does not have valid layer data!"
                end

                return true
            end
            "#,
        );

        let engine_stream_source = r#"
                local EngineStream = {}

                function EngineStream:LoadJSON(data)
                    data = Glide.FromJSON(data)

                    local success, errorMessage = Glide.ValidateStreamData(data)
                    if not success then
                        return
                    end

                    for id, layer in SortedPairs(data.layers) do
                        self:AddLayer(id, layer.path, layer.controllers, layer.redline == true)
                    end

                    if self.isWebAudio then
                        data = {
                            kv = data.kv,
                            layers = {}
                        }

                        for id, layer in pairs(self.layers) do
                            data.layers[id] = {
                                path = layer.path,
                                redline = layer.redline,
                                controllers = layer.controllers
                            }
                        end

                        self.updateWebJSON = Glide.ToJSON(data, false)
                    end
                end
                "#;

        assert_that!(
            ws.check_file_for(
                DiagnosticCode::UncheckedNilAccess,
                "lua/glide/client/engine_stream.lua",
                engine_stream_source,
            ),
            eq(true)
        );
        assert_that!(
            ws.check_file_for(
                DiagnosticCode::NeedCheckNil,
                "lua/glide/client/engine_stream_need_check.lua",
                engine_stream_source,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_istable_short_circuit_guards_indexed_table_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<table>
            function istable(value) end
            "#,
        );
        let code = r#"
            local MODES = {
                {
                    function(client)
                        return false
                    end,
                    "Off."
                }
            }

            ---@return integer
            local function getMode() end

            local mode = getMode() or 1
            local client

            return istable(MODES[mode]) and MODES[mode][1](client)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::UncheckedNilAccess, code);

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "MODES[mode] may be nil"),
            eq(false)
        );
    }

    #[gtest]
    fn test_istable_short_circuit_guards_after_prior_index_key_invalidation() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<table>
            function istable(value) end
            "#,
        );
        let code = r#"
            local MODES = {
                {
                    function(client)
                        return false
                    end,
                    "Off."
                }
            }

            ---@return integer
            local function getMode() end

            local mode = getMode() or 1
            local client
            if not MODES[mode] then
                return
            end
            mode = mode + 1

            return istable(MODES[mode]) and MODES[mode][1](client)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "MODES[mode] may be nil"),
            eq(false)
        );
    }

    #[gtest]
    fn test_shadowed_istable_short_circuit_does_not_guard_indexed_table_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<table>
            function istable(value) end
            "#,
        );
        let code = r#"
            local MODES
            ---@return integer
            local function getMode() end

            local mode = getMode() or 1
            local client
            local istable = function()
                return true
            end

            return istable(MODES[mode]) and MODES[mode][1](client)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "MODES may be nil"),
            eq(true)
        );
    }

    #[gtest]
    fn test_nullable_type_guard_does_not_suppress_receiver_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Widget
            ---@field Run fun(self: Widget)

            ---@param value any
            ---@return TypeGuard<Widget?>
            function maybe_widget(value) end
            "#,
        );
        let code = r#"
            ---@type any
            local widget

            return maybe_widget(widget) and widget:Run()
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "widget may be nil"),
            eq(true)
        );
    }

    #[gtest]
    fn test_contextual_table_argument_field_function_param_suppresses_nullable_receiver() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class ModelEntity
            ---@field GetModel fun(self: ModelEntity): string


            ---@class SkeletonConvertor
            ---@field IsApplicable fun(self: SkeletonConvertor, ent: ModelEntity): boolean

            ---@class listlib
            list = {}

            ---@overload fun(identifier: "SkeletonConvertor", key: string, item: SkeletonConvertor)
            ---@param identifier string
            ---@param key any
            ---@param item any
            function list.Set(identifier, key, item) end
            "#,
        );
        let code = r#"
            local Builder = {
                IsApplicable = function(self, ent)
                    local mdl = ent:GetModel()
                    return mdl:EndsWith(".mdl")
                end
            }

            list.Set("SkeletonConvertor", "TF2_engineer", Builder)
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "mdl may be nil"),
            eq(false)
        );
    }

    #[gtest]
    fn test_cached_nested_annotated_field_is_not_nullable() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@class TestColor
            ---@field r number
            ---@field g number
            ---@field b number
            ---@field a number

            ---@class TestSkinProperties
            ---@field Border TestColor

            ---@class TestSkinColours
            ---@field Properties TestSkinProperties

            ---@class TestSkin
            ---@field Colours TestSkinColours

            ---@class TestPanel
            local PANEL = {}

            ---@return TestSkin
            function PANEL:GetSkin() end

            function PANEL:Paint()
                local skinColor = self:GetSkin().Colours.Properties.Border
                draw(skinColor.r, skinColor.g, skinColor.b, skinColor.a)
            end
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            diagnostics,
            is_empty(),
            "cached non-null annotated field should not require a nil check"
        );
    }

    #[gtest]
    fn test_vgui_register_table_cached_nested_annotated_field_is_not_nullable() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
            ---@class TestColor
            ---@field r number
            ---@field g number
            ---@field b number
            ---@field a number

            ---@class TestSkinProperties
            ---@field Border TestColor

            ---@class TestSkinColours
            ---@field Properties TestSkinProperties

            ---@class TestSkin
            ---@field Colours TestSkinColours

            ---@class Panel

            ---@return TestSkin
            function Panel:GetSkin() end

            vgui = {}

            ---@generic T: table
            ---@[call_arg("gmod.vgui_panel", "register_table")]
            ---@param panel T
            ---@[call_arg("gmod.vgui_panel", "base")]
            ---@param base? string
            ---@return T
            function vgui.RegisterTable(panel, base) end
        "#,
        );
        let code = r#"
            local tblRow = vgui.RegisterTable({
                Paint = function(self)
                    local skinColor = self:GetSkin().Colours.Properties.Border
                    draw(skinColor.r, skinColor.g, skinColor.b, skinColor.a)
                end
            }, "Panel")
        "#;

        let diagnostics = diagnostics_for_code(&mut ws, DiagnosticCode::NeedCheckNil, code);

        assert_that!(
            diagnostics,
            is_empty(),
            "VGUI table callback self should preserve non-null annotated skin fields"
        );
    }

    #[test]
    fn test_doc_type_global_not_widened_by_cross_file_nil_assignment() {
        // When a global is declared with `---@type T` in a library/annotation file,
        // and a Main workspace file assigns `g_Global = nil` followed by
        // `g_Global = vgui.Create("T")` inside a function, the global's type
        // should remain `T` (from the DocType annotation) when read from
        // other files — not be widened to `T|nil` or `nil`.
        //
        // Root cause: `infer_global_type_from_decl_ids` doesn't recognize
        // `LuaType::Instance` (returned by `vgui.Create("T")`) as a Def/Ref
        // type, so it gets silently dropped from the multi-decl merge. When
        // the only other decl has `InferType(Nil)`, the tier returns
        // `Err(None)` which hard-fails instead of falling through to the
        // library tier that has `DocType(T)`.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        // Load the annotation file as a library workspace (separate tier)
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_doctype_global");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("globals.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
                    ---@type SpawnMenu
                    g_SpawnMenu = nil
                "#
                .to_string(),
            ),
        );

        // Main workspace file with nil + create lifecycle
        ws.def_file(
            "gamemodes/mygamemode/gamemode/spawnmenu.lua",
            r#"
                local function CreateSpawnMenu()
                    if IsValid(g_SpawnMenu) then
                        g_SpawnMenu:Remove()
                        g_SpawnMenu = nil
                    end
                    g_SpawnMenu = vgui.Create("SpawnMenu")
                end
            "#,
        );

        let main_file = ws.def_file(
            "gamemodes/mygamemode/gamemode/custom.lua",
            r#"
                function Update()
                    g_SpawnMenu.CustomizableSpawnlistNode = nil
                end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(main_file, CancellationToken::new())
            .unwrap_or_default();

        // The global `g_SpawnMenu` should resolve to `SpawnMenu` (DocType from
        // the library annotation), NOT nil. If the type widened to nil,
        // `g_SpawnMenu` would trigger `need-check-nil`.
        let nil_diagnostics: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("g_SpawnMenu may be nil"))
            .collect();

        assert_that!(
            nil_diagnostics,
            is_empty(),
            "g_SpawnMenu should retain its DocType (SpawnMenu) from the annotation, \
             not be widened to nil by the cross-file `= nil` assignment. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_all_nil_multidecl_global_returns_nil_for_fallback() {
        // When a global has multiple decls that are ALL nil in the same tier,
        // infer_global_type_from_decl_ids should return Ok(Nil) so the
        // nil_fallback_type mechanism in infer_global_type can consult
        // lower-priority tiers. A library DocType annotation should win.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_all_nil_multidecl");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("globals.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
                    ---@meta

                    ---@class MyType
                    ---@field field number

                    ---@type MyType
                    g_AllNil = nil
                "#
                .to_string(),
            ),
        );

        ws.def_file(
            "main.lua",
            r#"
                local function reset()
                    if g_AllNil then
                        g_AllNil = nil
                    end
                    g_AllNil = nil
                end
            "#,
        );

        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
                local ok = g_AllNil.field
                local bad = g_AllNil.nonexistent
            "#,
        );

        // Truth table for `g_AllNil.<field>`:
        //   MyType  → `.field` no warning, `.nonexistent` undefined-field
        //   Unknown → `.field` no warning, `.nonexistent` no warning
        //   Nil     → `.field` warning,   `.nonexistent` warning
        //
        // By asserting BOTH fields we distinguish MyType from both Unknown
        // (hard-fail regression) and Nil (early-return-without-library regression).
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);
        let diagnostics = ws
            .analysis
            .diagnose_file(consumer_file, CancellationToken::new())
            .unwrap_or_default();

        let field_undefined = diagnostics.iter().any(|d| d.message.contains("`field`"));
        let nonexistent_undefined = diagnostics
            .iter()
            .any(|d| d.message.contains("nonexistent"));

        // `.field` must NOT be undefined — it exists on MyType.
        // If this fires, g_AllNil resolved to Nil (not MyType).
        assert_that!(
            field_undefined,
            is_false(),
            "g_AllNil.field should NOT be undefined — `field` exists on MyType. \
             If this fires, g_AllNil resolved to Nil (the all-nil tier returned \
             Ok(Nil) but the library DocType was NOT consulted). \
             Diagnostics: {diagnostics:#?}"
        );
        // `.nonexistent` MUST be undefined — it does not exist on MyType.
        // If this does NOT fire, g_AllNil resolved to Unknown (hard-fail regression).
        assert_that!(
            nonexistent_undefined,
            is_true(),
            "g_AllNil.nonexistent should be undefined — it does not exist on MyType. \
             If this does not fire, g_AllNil resolved to Unknown (the all-nil tier \
             hard-failed instead of returning Ok(Nil) for fallback). \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_library_class_not_shadowed_by_main_empty_table_global() {
        // Faithful reproduction of the gmod_tool / ToolObj false positives.
        //
        // The annotation library declares `---@class ToolObj : Tool` (with a
        // non-nil `SWEP` field) and binds it via `ToolObj = ToolObj or {}`.
        // The Main workspace (shipped GMod code) then RE-declares the same
        // global with a plain table literal `ToolObj = {}` and adds runtime
        // methods, exactly like garrysmod stool.lua:
        //   ToolObj = {}
        //   function ToolObj:GetWeapon() return self.SWEP end
        //
        // Because Main-workspace decls outrank library decls in the global
        // priority tiers, `infer_global_type` resolves `ToolObj` to the
        // Main-tier `TableConst` ({}) and never consults the library tier
        // holding `@class ToolObj : Tool`. As a result `self.SWEP` is an
        // undefined field, the runtime `GetWeapon()` return widens to nil,
        // and `self:GetWeapon():...` produces a spurious nil-access warning.
        //
        // The annotation IS authoritative and correct here — the fix belongs
        // in the LS global merge: a higher-priority bare-table decl must not
        // shadow a lower-priority annotation `@class` (Def/Ref) for the same
        // global; the class type should win (or merge).
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // --- Annotation library tier ---
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_class_shadow");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("toolobj.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
                    ---@meta

                    ---@class Weapon
                    ---@field GetOwner fun(self: Weapon): any

                    ---@class Tool
                    ---@field SWEP Weapon

                    ---@class ToolObj : Tool
                    ToolObj = ToolObj or {}
                "#
                .to_string(),
            ),
        );

        // --- Main workspace: shipped runtime pattern (garrysmod stool.lua) ---
        // Includes the `o.SWEP = nil` initializer in Create() and the
        // `ToolObj = nil` teardown, both present in the real file, since the
        // nil initializer is what widens the inferred SWEP/GetWeapon type.
        let _stool_file = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stool.lua",
            r#"
                ToolObj = {}

                function ToolObj:Create()
                    local o = {}
                    setmetatable( o, self )
                    self.__index = self
                    o.SWEP = nil
                    return o
                end

                function ToolObj:GetWeapon() return self.SWEP end

                ToolObj = nil
            "#,
        );

        let main_file = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/object.lua",
            r#"
                function ToolObj:SetStage(i)
                    self:GetWeapon():GetOwner()
                end
            "#,
        );

        // The real CLI run reports this as unchecked-nil-access on the
        // `self:GetWeapon()` receiver.
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UncheckedNilAccess);
        let diagnostics = ws
            .analysis
            .diagnose_file(main_file, CancellationToken::new())
            .unwrap_or_default();

        // `self:GetWeapon()` returns the non-nil `Weapon` field `SWEP`, so the
        // method-call receiver is never nil. No nil diagnostic should fire.
        let nil_diagnostics: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("GetWeapon"))
            .collect();

        assert_that!(
            nil_diagnostics,
            is_empty(),
            "ToolObj from the Main-workspace `ToolObj = {{}}` must still inherit the \
             library `@class ToolObj : Tool`, so `self.SWEP` is the non-nil `Weapon` \
             field and `self:GetWeapon()` is non-nil. A higher-priority empty-table \
             global must NOT shadow a lower-priority annotation class. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_mixed_nil_and_primitive_global_does_not_widen_to_nil() {
        // When a global has both a nil decl and a non-nil primitive decl
        // (e.g. Integer) in the same tier, infer_global_type_from_decl_ids
        // should NOT return Ok(Nil). The primitive is an unhandled non-nil
        // type, so the all-nil fallback must not fire. The tier should
        // return Err(None) which hard-fails, preventing a lower-priority
        // library DocType from incorrectly overriding a real same-tier
        // primitive assignment.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_mixed_nil_primitive");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("globals.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
                    ---@meta

                    ---@class MyType
                    ---@field field number

                    ---@type MyType
                    g_Mixed = nil
                "#
                .to_string(),
            ),
        );

        ws.def_file(
            "main_nil.lua",
            r#"
                g_Mixed = nil
            "#,
        );

        ws.def_file(
            "main_int.lua",
            r#"
                g_Mixed = 123
            "#,
        );

        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
                local ok = g_Mixed.field
                local bad = g_Mixed.nonexistent
            "#,
        );

        // Truth table for `g_Mixed.<field>`:
        //   MyType  → `.field` no warning, `.nonexistent` undefined-field
        //   Unknown → `.field` no warning, `.nonexistent` no warning
        //   Nil     → `.field` warning,   `.nonexistent` warning
        //
        // The correct behavior is that the mixed nil+integer tier hard-fails,
        // the library is NOT consulted, and g_Mixed resolves to Unknown — so
        // BOTH `.field` and `.nonexistent` should have no undefined-field.
        // If the all-nil fallback incorrectly fires, the library DocType leaks
        // through and BOTH would trigger undefined-field.
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);
        let diagnostics = ws
            .analysis
            .diagnose_file(consumer_file, CancellationToken::new())
            .unwrap_or_default();

        let field_undefined = diagnostics.iter().any(|d| d.message.contains("`field`"));
        let nonexistent_undefined = diagnostics
            .iter()
            .any(|d| d.message.contains("`nonexistent`"));

        // Neither `.field` nor `.nonexistent` should be undefined — g_Mixed
        // resolves to Unknown (both suppressed), NOT MyType (`.nonexistent`
        // would fire) or Nil (both would fire).
        assert_that!(
            field_undefined,
            is_false(),
            "g_Mixed.field must NOT be undefined — g_Mixed should resolve to \
             Unknown (not MyType/Nil). If this fires, the library DocType \
             leaked through. Diagnostics: {diagnostics:#?}"
        );
        assert_that!(
            nonexistent_undefined,
            is_false(),
            "g_Mixed.nonexistent must NOT be undefined — g_Mixed should resolve \
             to Unknown (not MyType). If this fires, the all-nil fallback \
             fired incorrectly and the library DocType leaked through. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negated_index_expr_guard_suppresses_need_check_nil() {
        // When an index expression like `t[k]` is guarded by a prior
        // `if (!t[k]) then return end` (or `if not t[k] then return end`),
        // the subsequent access `t[k].field` should NOT emit need-check-nil
        // because the nil case already returned early.
        //
        // This is the pattern from gmod_tool/object.lua:
        //   if ( !self.Objects[i] ) then return NULL end
        //   return self.Objects[i].Ent
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_id = ws.def(
            r#"
            ---@class Container
            ---@field Objects table<integer, {Ent: Entity}>?
            local Container = {}

            ---@param i integer
            ---@return Entity
            function Container:GetEnt(i)
                if not self.Objects[i] then return nil end
                return self.Objects[i].Ent
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let nil_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("may be nil"))
            .collect();

        // The `if not self.Objects[i]` condition itself accesses `self.Objects`,
        // which IS nullable — that diagnostic is correct and expected.
        // The Lua code (after def() adds one prefix line) has:
        //   line 8: `if not self.Objects[i] then return nil end`
        //   line 9: `return self.Objects[i].Ent`
        let guard_condition_warnings: Vec<_> = nil_warnings
            .iter()
            .filter(|d| d.range.start.line == 8)
            .collect();
        let post_guard_warnings: Vec<_> = nil_warnings
            .iter()
            .filter(|d| d.range.start.line >= 9)
            .collect();

        assert_that!(
            guard_condition_warnings.len(),
            eq(1),
            "Expected exactly one nil diagnostic on the guard condition line \
             (accessing nullable `self.Objects` inside `if not self.Objects[i]`). \
             Diagnostics: {guard_condition_warnings:#?}"
        );
        assert_that!(
            post_guard_warnings.is_empty(),
            is_true(),
            "self.Objects[i] should be narrowed to non-nil after `if not self.Objects[i] then return end`. \
             Diagnostics after guard: {post_guard_warnings:#?}"
        );
    }

    #[test]
    fn test_tool_object_index_operator_slots_are_non_nil() {
        // Faithful reproduction of the gmod_tool/object.lua pattern. ToolObj has
        // an annotated Objects field whose ToolObjects alias maps integer keys to
        // non-nil ToolObjectSlot values, while runtime Create/SetObject methods
        // also write table literals into the same field. The dynamic writes must
        // not erase the explicit indexed-slot contract.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_tool_objects_index_operator");
        ws.analysis.add_library_workspace(library_root.clone());
        let tool_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("tool.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &tool_uri,
            Some(
                r#"
                    ---@meta

                    ---@class Entity
                    ---@field EntIndex fun(self: Entity): number

                    ---@class PhysObj
                    ---@field WorldToLocal fun(self: PhysObj, value: any): any

                    ---@param value any
                    ---@return TypeGuard<Entity>
                    function IsValid(value) end

                    ---@class ToolObjectSlot
                    ---@field Ent Entity
                    ---@field Phys PhysObj|nil
                    ---@field Pos any
                    ---@field Normal any

                    ---@alias ToolObjects table<integer, ToolObjectSlot>

                    ---@class Tool
                    ---@field Objects ToolObjects

                    ---@param id number
                    ---@return any
                    function Tool:GetPos(id) end
                "#
                .to_string(),
            ),
        );
        let custom_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("custom_classes.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &custom_uri,
            Some(
                r#"
                    ---@meta

                    ---@class ToolObj : Tool
                    ---@field Objects ToolObjects
                    ---@field SetObject fun(self: ToolObj, id: number, ent: Entity, pos: any, phys: PhysObj|nil)
                    ToolObj = ToolObj or {}
                "#
                .to_string(),
            ),
        );

        ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stool.lua",
            r#"
                ToolObj = {}

                function ToolObj:Create()
                    local o = {}
                    setmetatable(o, self)
                    self.__index = self
                    o.Objects = {}
                    return o
                end

                ToolObj = nil
            "#,
        );

        let object_file = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/object.lua",
            r#"
                function ToolObj:GetPos(i)
                    if self.Objects[i].Ent:EntIndex() == 0 then
                        return self.Objects[i].Pos
                    end
                    return self.Objects[i].Ent
                end

                function ToolObj:SetObject(i, ent, pos, phys, norm)
                    self.Objects[i] = {}
                    self.Objects[i].Ent = ent
                    self.Objects[i].Pos = pos
                    self.Objects[i].Phys = phys

                    if IsValid(phys) then
                        self.Objects[i].Normal = self.Objects[i].Phys:WorldToLocal(norm)
                        self.Objects[i].Pos = self.Objects[i].Phys:WorldToLocal(pos)
                    else
                        self.Objects[i].Normal = self.Objects[i].Ent:WorldToLocal(norm)
                    end
                end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(object_file, CancellationToken::new())
            .unwrap_or_default();

        let nil_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| {
                d.message.contains("Objects")
                    || d.message.contains("Ent")
                    || d.message.contains("Phys")
            })
            .collect();

        assert_that!(
            nil_warnings,
            is_empty(),
            "ToolObjects integer index operator should make self.Objects[i] a non-nil ToolObjectSlot. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_assigned_value_type_guard_is_invalidated_by_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@field GetPos fun(self: Entity): any

                ---@param value any
                ---@return TypeGuard<Entity>
                function IsValid(value) end

                ---@class Slot
                ---@field Ent Entity|nil

                ---@type Slot
                local slot = {}

                ---@param ent Entity|nil
                ---@param other Entity|nil
                local function set_ent(ent, other)
                    slot.Ent = ent
                    ent = other

                    if IsValid(ent) then
                        slot.Ent:GetPos()
                    end
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("slot.Ent")),
            eq(true),
            "Reassigning the source expression after `slot.Ent = ent` must invalidate the assignment-backed guard. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_assigned_value_type_guard_does_not_use_if_condition_for_elseif_body() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@field GetPos fun(self: Entity): any

                ---@param value any
                ---@return TypeGuard<Entity>
                function IsValid(value) end

                ---@class Slot
                ---@field Ent Entity|nil

                ---@type Slot
                local slot = {}

                ---@param ent Entity|nil
                ---@param cond boolean
                local function set_ent(ent, cond)
                    slot.Ent = ent

                    if IsValid(ent) then
                        slot.Ent:GetPos()
                    elseif cond then
                        slot.Ent:GetPos()
                    end
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("slot.Ent")),
            eq(true),
            "The main `if IsValid(ent)` condition is false inside the elseif body and must not guard `slot.Ent`. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_assigned_value_type_guard_is_invalidated_by_nested_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@field GetPos fun(self: Entity): any

                ---@param value any
                ---@return TypeGuard<Entity>
                function IsValid(value) end

                ---@class Slot
                ---@field Ent Entity|nil

                ---@type Slot
                local slot = {}

                ---@param ent Entity|nil
                ---@param other Entity|nil
                local function set_ent(ent, other)
                    slot.Ent = ent
                    do
                        ent = other
                    end

                    if IsValid(ent) then
                        slot.Ent:GetPos()
                    end
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("slot.Ent")),
            eq(true),
            "Nested reassignment after `slot.Ent = ent` must invalidate the assignment-backed guard. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_assigned_value_type_guard_is_invalidated_by_index_key_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@field GetPos fun(self: Entity): any

                ---@param value any
                ---@return TypeGuard<Entity>
                function IsValid(value) end

                ---@class SlotEntry
                ---@field Phys Entity|nil

                ---@class Slot
                ---@field Objects table<integer, SlotEntry>

                ---@type Slot
                local slot = {}

                ---@param i integer
                ---@param phys Entity|nil
                local function set_phys(i, phys)
                    slot.Objects[i].Phys = phys
                    i = i + 1

                    if IsValid(phys) then
                        slot.Objects[i].Phys:GetPos()
                    end
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("slot.Objects[i].Phys")),
            eq(true),
            "Reassigning the index key after `slot.Objects[i].Phys = phys` must invalidate the assignment-backed guard. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_assigned_value_type_guard_is_invalidated_by_then_block_receiver_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@field GetPos fun(self: Entity): any

                ---@param value any
                ---@return TypeGuard<Entity>
                function IsValid(value) end

                ---@class SlotEntry
                ---@field Phys Entity|nil

                ---@class Slot
                ---@field Objects table<integer, SlotEntry>

                ---@type Slot
                local slot = {}

                ---@param i integer
                ---@param phys Entity|nil
                ---@param other Entity|nil
                local function set_phys(i, phys, other)
                    slot.Objects[i].Phys = phys

                    if IsValid(phys) then
                        slot.Objects[i].Phys = other
                        slot.Objects[i].Phys:GetPos()
                    end
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("slot.Objects[i].Phys")),
            eq(true),
            "Reassigning the guarded receiver from a different nullable source before the access inside the then-block must invalidate the assignment-backed guard. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_assigned_value_type_guard_is_invalidated_by_loop_back_edge_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@field GetPos fun(self: Entity): any

                ---@param value any
                ---@return TypeGuard<Entity>
                function IsValid(value) end

                ---@class SlotEntry
                ---@field Phys Entity|nil

                ---@class Slot
                ---@field Objects table<integer, SlotEntry>

                ---@type Slot
                local slot = {}

                ---@param i integer
                ---@param phys Entity|nil
                ---@param other Entity|nil
                ---@param cond boolean
                local function set_phys(i, phys, other, cond)
                    slot.Objects[i].Phys = phys

                    while cond do
                        if IsValid(phys) then
                            slot.Objects[i].Phys:GetPos()
                        end
                        slot.Objects[i].Phys = other
                    end
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("slot.Objects[i].Phys")),
            eq(true),
            "A receiver reassignment after the guarded if in the same loop body can feed the next iteration and must invalidate the assignment-backed guard. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_does_not_suppress_gmod_null() {
        // NULL is truthy in GLua, so `not ent` does NOT prove entity validity.
        // Only IsValid-based guards should suppress NULL diagnostics.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity

            ---@type NULL
            NULL = nil
            "#,
        );

        let file_id = ws.def(
            r#"
            ---@param ent Entity|NULL
            local function useEntity(ent)
                if not ent then return end
                ent:GetPos()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        // `ent:GetPos()` should still produce a NULL diagnostic because
        // `not ent` only proves non-nil, not validity.
        let null_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("NULL"))
            .collect();

        assert_that!(
            null_warnings.is_empty(),
            is_false(),
            "`not ent` should NOT suppress NULL diagnostics — NULL is truthy in GLua, \
             so IsValid is still required. Diagnostics: {null_warnings:#?}"
        );
    }

    #[test]
    fn test_negation_guard_prefix_match_is_one_directional() {
        // The reverse prefix match (condition shorter than guarded) is
        // intentionally NOT supported: `not self.Objects` does NOT suppress
        // diagnostics on `self.Objects[i]`, because checking the table exists
        // does not prove the indexed value exists.
        //
        // We verify by confirming that `not self.Objects` only suppresses
        // diagnostics on `self.Objects` (exact match), not on deeper accesses
        // like `self.Objects[i]`. Since the analyzer doesn't flag
        // `table<integer, T>[i]` as nullable, we test with a direct nullable
        // field instead.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_id = ws.def(
            r#"
            ---@class Container2
            ---@field Objects Entity?
            local Container2 = {}

            ---@param i integer
            local function GetEnt(self: Container2, i)
                if not self.Objects then return end
                self.Objects:GetPos()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        // `self.Objects:GetPos()` after `if not self.Objects` — the exact
        // match on `self.Objects` SHOULD suppress this (correct behavior).
        // This confirms the exact match still works and the one-directional
        // prefix match doesn't accidentally break the simple case.
        let post_guard_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("may be nil"))
            .filter(|d| d.range.start.line >= 8)
            .collect();

        assert_that!(
            post_guard_warnings.is_empty(),
            is_true(),
            "Exact match `not self.Objects` should suppress nil diagnostic on `self.Objects`. \
             Diagnostics: {post_guard_warnings:#?}"
        );
    }

    #[test]
    fn test_negation_guard_reverse_prefix_does_not_suppress_deeper_access() {
        // The reverse prefix match (condition shorter than guarded) is
        // intentionally NOT supported: `not self.Objects` should NOT suppress
        // diagnostics on `self.Objects.Ent`, because proving the outer object
        // exists does not prove a specific nested field is non-nil.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class Inner
            ---@field Ent Entity?
            "#,
        );

        let file_id = ws.def(
            r#"
            ---@class Container3
            ---@field Objects Inner?
            local Container3 = {}

            function Container3:GetEnt()
                if not self.Objects then return end
                self.Objects.Ent:GetPos()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        // `self.Objects.Ent:GetPos()` — the guarded expression (`self.Objects.Ent`)
        // extends the condition (`self.Objects`). The prefix match is one-directional:
        // the condition must be the guarded expression OR a longer chained version.
        // A shorter condition must NOT suppress the deeper access, so this must
        // produce a nil diagnostic on the nullable `Ent` field.
        let post_guard_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("may be nil"))
            // The guarded access is on line 7 (function body line 2); guard line 6
            // starts after line 0 class/field defs.
            .filter(|d| d.range.start.line == 7)
            .collect();

        assert_that!(
            post_guard_warnings,
            not(is_empty()),
            "`not self.Objects` should NOT suppress nil diagnostic on `self.Objects.Ent` — \
             the condition is shorter than the guarded expression. \
             Diagnostics: {post_guard_warnings:#?}"
        );
    }

    #[test]
    fn test_negation_guard_invalidated_by_reassignment() {
        // If the guarded expression is reassigned between the guard and the
        // access, the guard is invalidated and should NOT suppress.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any
            "#,
        );

        ws.def(
            r#"
            ---@class Container4
            ---@field Objects Entity?
            local Container4 = {}

            function Container4:GetEnt()
                if not self.Objects then return end
                self.Objects = nil
                self.Objects:GetPos()
            end
            "#,
        );

        // After `self.Objects = nil`, the guard is invalidated.
        // The subsequent `self.Objects:GetPos()` should produce an
        // unchecked-nil-access diagnostic since the type narrows to nil.
        assert_that!(
            ws.check_code_for(
                DiagnosticCode::UncheckedNilAccess,
                r#"
                function Container4:GetEnt()
                    if not self.Objects then return end
                    self.Objects = nil
                    self.Objects:GetPos()
                end
                "#,
            ),
            eq(false)
        );
    }

    #[test]
    fn test_negation_guard_invalidated_by_key_mutation() {
        // If the index key variable is reassigned between the guard and the
        // access, the guard is invalidated (it proved the old key, not the new one).
        // We use a COMPOUND key `self.Objects[i + 1]` so the descendant-walk
        // logic is exercised — the old exact-key implementation would NOT catch
        // this because `i + 1` is not the same expression as `i`.
        // We use `table<integer, Entity?>` so indexed access returns `Entity?`
        // (nullable), which triggers need-check-nil unless guarded.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any
            "#,
        );

        let file_id = ws.def(
            r#"
            ---@class Container5
            ---@field Objects table<integer, Entity?>?
            local Container5 = {}

            ---@param i integer
            function Container5:GetEnt(i)
                if not self.Objects[i + 1] then return end
                i = i + 1
                self.Objects[i + 1]:GetPos()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        // After `i = i + 1`, the guard on `self.Objects[i + 1]` (old i) is
        // invalidated. The subsequent `self.Objects[i + 1]:GetPos()` (new i)
        // should produce a diagnostic on `self.Objects[i + 1]` (nullable since
        // Entity? value type).
        //
        // We filter for diagnostics on the post-mutation line specifically,
        // not the guard condition line, to prove the guard was invalidated.
        // The access `self.Objects[i + 1]:GetPos()` is on the last line.
        let all_lines: Vec<_> = diagnostics.iter().map(|d| d.range.start.line).collect();
        let post_mutation_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("may be nil"))
            // Filter for the post-mutation access line (after `i = i + 1`).
            // The function body starts around line 7-8, the access is 2 lines
            // after the guard.
            .filter(|d| {
                // Find the max diagnostic line — the post-mutation access should
                // be on a later line than the guard condition.
                let max_line = all_lines.iter().copied().max().unwrap_or(0);
                d.range.start.line == max_line
            })
            .collect();

        assert_that!(
            post_mutation_warnings,
            not(is_empty()),
            "After `i = i + 1`, `self.Objects[i + 1]:GetPos()` should produce a nil diagnostic \
             on the post-mutation line. All diagnostic lines: {all_lines:?}. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_does_not_suppress_mixed_nil_null() {
        // NULL is truthy in GLua, so `not ent` only proves non-nil, not validity.
        // When the type is `Entity|NULL|nil`, the guard must NOT suppress the
        // NULL diagnostic, because `not ent` doesn't prove IsValid.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity

            ---@type NULL
            NULL = nil
            "#,
        );

        let file_id = ws.def(
            r#"
            ---@param ent Entity|NULL|nil
            local function useEntity(ent)
                if not ent then return end
                ent:GetPos()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        // `ent:GetPos()` should still produce a NULL diagnostic because
        // `not ent` only proves non-nil, not validity. The type contains
        // NULL, so the general negation guard must NOT suppress it.
        let null_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("NULL"))
            .collect();

        assert_that!(
            null_warnings.is_empty(),
            is_false(),
            "`not ent` on type `Entity|NULL|nil` should NOT suppress NULL diagnostics — \
             NULL is truthy in GLua, so IsValid is still required. \
             Diagnostics: {null_warnings:#?}"
        );
    }

    #[test]
    fn test_pairs_loop_variable_not_nullable_after_loop_exit() {
        // After a `for k, v in pairs(t) do ... end` loop completes normally,
        // the loop variable `v` is out of scope. But if a value extracted
        // from the loop is used after, it should not be incorrectly flagged
        // as nullable due to the pairs-loop's internal nil-check flow.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Player
            ---@field GetName fun(self: Player): string
            ---@field SteamID string

            ---@type table<integer, Player>
            local players = {}

            local function ProcessAll()
                local last = nil
                for _, ply in pairs(players) do
                    last = ply
                end
                -- After the loop, `last` holds the last player iterated.
                -- It should NOT be flagged as nullable just because the
                -- pairs loop could have had zero iterations.
                -- (This is a known flow-narrowing gap: the analyzer may
                -- treat `last` as potentially nil from the loop's empty path.)
                if last then
                    return last:GetName()
                end
            end
            "#,
        );

        // The `if last then` guard should suppress any need-check-nil.
        // If the analyzer incorrectly flags `last` inside the guard, that's a bug.
        let guarded_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("may be nil"))
            .collect();
        assert_that!(
            guarded_warnings,
            is_empty(),
            "After `if last then` guard, `last:GetName()` should not produce nil warnings. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_unrelated_boolean_guard_does_not_narrow_other_variable() {
        // A boolean guard `if some_bool then` should only narrow `some_bool`,
        // not other variables in scope. If `ent` is already typed as `Entity`,
        // accessing `ent:GetPos()` inside the boolean guard block should not
        // produce a nil warning.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): Vector

            ---@param ent Entity
            local function doSomething(ent, someBool)
                if someBool then
                    -- `ent` is `Entity` (non-nullable), `someBool` is the guard.
                    -- Accessing `ent:GetPos()` should NOT produce a nil warning.
                    ent:GetPos()
                end
            end
            "#,
        );

        assert_that!(
            diagnostics,
            is_empty(),
            "Boolean guard `if someBool then` should not narrow unrelated `ent` variable. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_does_not_suppress_repeated_call_expr() {
        // A negated call expression `not maybeEnt()` only proves the FIRST call
        // returned non-nil. A subsequent `maybeEnt()` call may return nil, so
        // the guard must NOT suppress the nil diagnostic.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): Vector

            ---@return Entity?
            local function maybeEnt() end

            local function useEntity()
                if not maybeEnt() then return end
                -- This second call may return nil; the guard on the first call
                -- does NOT prove this call is safe.
                maybeEnt():GetPos()
            end
            "#,
        );

        let call_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("may be nil"))
            .collect();
        assert_that!(
            call_warnings,
            not(is_empty()),
            "`not maybeEnt()` guard should NOT suppress nil diagnostic on the \
             second `maybeEnt()` call — each call may return a different value. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_invalidated_by_indexed_key_reassignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): Vector

            local function process()
                local keys = { 1 }
                ---@type table<number, Entity?>
                local t = {}

                if not t[keys[1]] then return end
                keys[1] = 2
                t[keys[1]]:GetPos()
            end
            "#,
        );

        let nil_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("may be nil"))
            .collect();

        assert_that!(
            nil_warnings,
            not(is_empty()),
            "`keys[1] = 2` changes the dynamic key proven by `if not t[keys[1]]`, \
             so the later `t[keys[1]]:GetPos()` access must still require a nil check. \
             Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_invalidated_by_local_key_shadow() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): Vector

            ---@param i integer
            local function process(i)
                ---@type table<number, Entity?>
                local t = {}

                if not t[i] then return end
                local i = 2
                t[i]:GetPos()
            end
            "#,
        );

        let nil_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("may be nil"))
            .collect();

        assert_that!(
            nil_warnings,
            not(is_empty()),
            "`local i = 2` shadows the key proven by `if not t[i]`, so the later \
             `t[i]:GetPos()` access must still require a nil check. Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_invalidated_by_local_function_key_shadow() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): Vector

            ---@param i integer
            local function process(i)
                ---@type table<any, Entity?>
                local t = {}

                if not t[i] then return end
                local function i() end
                t[i]:GetPos()
            end
            "#,
        );

        let nil_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("may be nil"))
            .collect();

        assert_that!(
            nil_warnings,
            not(is_empty()),
            "`local function i()` shadows the key proven by `if not t[i]`, so the later \
             `t[i]:GetPos()` access must still require a nil check. Diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn test_negation_guard_matches_parenthesized_dynamic_index_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field Ent Entity

            ---@class Container
            ---@field Objects table<integer, Entity?>?

            ---@param i integer
            local function process(self, i)
                if not self.Objects[i] then return end
                return (self.Objects[i]).Ent
            end
            "#,
        );

        let post_guard_warnings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("may be nil"))
            .collect();

        assert_that!(
            post_guard_warnings,
            is_empty(),
            "`if not self.Objects[i] then return end` must guard the equivalent \
             parenthesized access `(self.Objects[i]).Ent`. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_isvalid_prior_guard_not_invalidated_by_guarded_root_field_assignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field GetTable fun(self: Entity): table
            ---@class NULL : Entity

            ---@param value any
            ---@return TypeGuard<Entity>
            ---@[valid_guard]
            function _G.IsValid(value) end

            ---@return Entity|NULL
            function makeEntity() end

            local ent = makeEntity()
            if ( !IsValid( ent ) ) then return end

            ent.StoredValue = 1
            ent:GetTable()
            "#,
        );

        assert_that!(
            diagnostics,
            is_empty(),
            "Assigning a field on a guarded root local must not invalidate the root validity guard. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_stool_helper_pattern_valid_guard() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "annotations/global.lua",
            r#"
            ---@class Entity
            ---@field SetAngles fun(self: Entity, angles: any)
            ---@field GetTable fun(self: Entity): table

            ---@class base_gmodentity : Entity

            ---@class gmod_hoverball : base_gmodentity

            ---@class NULL : Entity

            ---@param value any
            ---@return TypeGuard<Entity>
            ---@[valid_guard]
            function _G.IsValid(value) end
            "#,
        );

        ws.def_file(
            "lua/includes/util.lua",
            r#"
            function IsValid(object)
                if ( !object ) then return false end

                local isvalid = object.IsValid
                if ( !isvalid ) then return false end

                return isvalid( object )
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let file_id = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stools/hoverball.lua",
            r#"
            ---@class ents
            ents = {}

            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end

            ---@class Tool
            TOOL = {}

            ---@class Player : Entity

            duplicator = {}
            ---@param name string
            ---@param _function fun(ply: Player, ...: any):(ent: Entity)
            ---@param ... any
            function duplicator.RegisterEntityClass(name, _function, ...) end

            function TOOL:LeftClick()
                if ( CLIENT ) then return true end

                local ball = MakeHoverBall(nil, nil, nil, nil, nil, nil, nil, nil, nil, nil)
                if ( !IsValid( ball ) ) then return false end

                ball:SetAngles(nil)
                return true
            end

            if SERVER then
                function MakeHoverBall(ply, pos, key_d, key_u, speed, resistance, strength, model, nocollide, key_o, Data)
                    local ball = ents.Create("gmod_hoverball")
                    if ( !IsValid( ball ) ) then return NULL end

                    ball:SetAngles(nil)
                    ball.NumDown = 1
                    ball:GetTable()
                    return ball
                end
                duplicator.RegisterEntityClass("gmod_hoverball", MakeHoverBall, "Pos", "key_d", "key_u", "speed", "resistance", "strength", "model", "nocollide", "key_o", "Data")
            end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert_that!(
            diagnostics,
            is_empty(),
            "Expected no NeedCheckNil warnings with valid STOOL helper pattern. Diagnostics: {diagnostics:#?}"
        );
    }

    #[gtest]
    fn test_utf8_optional_param_defaulted_via_assignment() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param startPos? integer
            function decode(str, startPos)
                startPos = startPos or 1
                local endPos = startPos + 1
            end
            "#,
        );
        assert_that!(
            diagnostics,
            is_empty(),
            "Optional parameter defaulted via assignment then used arithmetically should not report NeedCheckNil"
        );
    }

    #[gtest]
    fn test_utf8_multi_return_correlated_early_return() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function decode(str, pos)
                if pos > 10 then
                    return nil
                end
                return 1, 2, 3
            end

            local seqStartPos, seqEndPos = decode("abc", 1)
            if not seqStartPos then
                error("error")
            end
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            is_empty(),
            "Early error/return proving the first return is present should also clear need-check-nil on the correlated return"
        );
    }

    #[gtest]
    fn test_utf8_multi_return_correlated_return_guard() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function decode(pos)
                if pos > 10 then
                    return nil
                end
                return 1, 2, 3
            end

            local seqStartPos, seqEndPos = decode(1)
            if not seqStartPos then
                return
            end
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            is_empty(),
            "Return guard on the discriminant slot should prove correlated sibling returns non-nil"
        );
    }

    #[gtest]
    fn test_utf8_independent_optional_returns_negative_soundness() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@return integer? a
            ---@return integer? b
            local function decode() end

            local a, b = decode()
            if not a then
                return
            end
            local x = b + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Independent optional returns should still report NeedCheckNil on b after guarding a"
        );
    }

    #[gtest]
    fn test_utf8_mixed_success_shape_negative_soundness() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function decode(pos)
                if pos == 1 then
                    return nil
                elseif pos == 2 then
                    return 1, nil
                else
                    return 1, 2
                end
            end

            local seqStartPos, seqEndPos = decode(3)
            if not seqStartPos then
                error("invalid")
            end
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Using slot 2 after guarding slot 1 should still report if one return statement has a nil in slot 2"
        );
    }

    #[gtest]
    fn test_utf8_nullable_param_passthrough_negative_soundness() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param value? integer
            local function decode(value)
                if math.random() > 0.5 then
                    return nil
                end
                return 1, value
            end

            ---@type integer?
            local maybe
            local seqStartPos, seqEndPos = decode(maybe)
            if not seqStartPos then
                return
            end
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Guarding slot 1 must not prove a sibling slot non-nil when that slot passes through a nullable parameter"
        );
    }

    #[gtest]
    fn test_utf8_reassignment_between_guard_and_use_negative() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function decode()
                if math.random() > 0.5 then
                    return nil
                end
                return 1, 2
            end

            local seqStartPos, seqEndPos = decode()
            if not seqStartPos then
                error("invalid")
            end
            seqEndPos = nil
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Reassigning seqEndPos to nil after the guard should report NeedCheckNil"
        );
    }

    #[gtest]
    fn test_utf8_discriminant_reassignment_between_guard_and_use_negative() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function decode()
                if math.random() > 0.5 then
                    return nil
                end
                return 1, 2
            end

            local seqStartPos, seqEndPos = decode()
            if not seqStartPos then
                return
            end
            seqStartPos = nil
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Reassigning the guarded discriminant after the guard should invalidate sibling return correlation"
        );
    }

    #[gtest]
    fn test_utf8_recall_after_guard_negative() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function decode()
                if math.random() > 0.5 then
                    return nil
                end
                return 1, 2
            end

            local seqStartPos = decode()
            if not seqStartPos then
                return
            end
            local _, seqEndPos = decode()
            local startPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Guarding a different decode() call must not prove a later re-call's sibling return non-nil"
        );
    }

    #[gtest]
    fn test_utf8_str_rel_to_abs_vararg_unpack() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function strRelToAbs(str, ...)
                local args = { ... }
                return unpack(args)
            end
            local function decode(str, startPos)
                startPos = strRelToAbs(str, startPos or 1)
                local endPos = startPos + 1
                return startPos, endPos
            end
            local seqStartPos, seqEndPos = decode('abc')
            if not seqStartPos then return end
            local nextPos = seqEndPos + 1
            "#,
        );
        assert_that!(
            diagnostics,
            is_empty(),
            "unpack(args) passthrough should preserve non-nil tuple elements and not produce NeedCheckNil"
        );
    }

    #[gtest]
    fn test_utf8_vararg_unpack_soundness_negative() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function passthrough(...)
                local args = { ... }
                return unpack(args)
            end

            ---@type integer?
            local maybe_int = nil
            local n1 = passthrough(maybe_int)
            local x = n1 + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "unpack(args) should not clear nil/nullable type when a genuinely nullable value is passed in"
        );
    }

    #[gtest]
    fn test_unpack_explicit_nil_element_soundness_negative() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local _, maybe_int = unpack({ 1, nil, 3 })
            local next_int = maybe_int + 1
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "unpack precision must preserve explicit nil elements"
        );
    }

    fn def_canconstrain_valid_guard_annotation(ws: &mut VirtualWorkspace) {
        ws.def(
            r#"
            ---@meta
            ---@attribute valid_guard()

            ---@class Entity
            ---@field GetPhysicsObjectNum fun(self: Entity, bone: integer): any
            ---@class NULL : Entity

            ---@param value any
            ---@param bone integer
            ---@return TypeGuard<any>
            ---@[valid_guard]
            function _G.CanConstrain(value, bone) end
            "#,
        );
    }

    #[gtest]
    fn test_source_shadowed_global_valid_guard_annotation_suppresses_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_canconstrain_valid_guard_annotation(&mut ws);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            function CanConstrain(value, bone)
                return value ~= nil
            end

            ---@type Entity?
            local Ent1
            local Bone1 = 0
            if not CanConstrain(Ent1, Bone1) then return false end
            Ent1:GetPhysicsObjectNum(Bone1)
            "#,
        );

        assert_that!(diagnostics, is_empty());
    }

    #[gtest]
    fn test_local_shadowed_valid_guard_annotation_still_reports_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_canconstrain_valid_guard_annotation(&mut ws);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            local function CanConstrain(value, bone)
                return true
            end

            ---@type Entity?
            local Ent1
            local Bone1 = 0
            if not CanConstrain(Ent1, Bone1) then return false end
            Ent1:GetPhysicsObjectNum(Bone1)
            "#,
        );

        assert_that!(diagnostics, not(is_empty()));
    }

    #[gtest]
    fn test_server_only_shadowed_valid_guard_does_not_suppress_client_nil_check() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/autorun/server/sv_canconstrain.lua",
            r#"
            ---@meta
            ---@realm server
            ---@attribute valid_guard()

            ---@class Entity
            ---@field GetPhysicsObjectNum fun(self: Entity, bone: integer): any
            ---@class NULL : Entity

            ---@param value any
            ---@param bone integer
            ---@return TypeGuard<any>
            ---@[valid_guard]
            function _G.CanConstrain(value, bone) end
            "#,
        );

        assert_that!(
            ws.check_file_for(
                DiagnosticCode::NeedCheckNil,
                "lua/autorun/client/cl_use.lua",
                r#"
                function CanConstrain(value, bone)
                    return value ~= nil
                end

                ---@type Entity?
                local Ent1
                local Bone1 = 0
                if not CanConstrain(Ent1, Bone1) then return false end
                Ent1:GetPhysicsObjectNum(Bone1)
                "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn test_constraint_repro_valid_guard_ent_x_pos() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field Constraints table?
            local ent = {}
            local i = 1
            local entX = ent["Ent" .. i]
            if IsValid(entX) and entX.Constraints then
                table.RemoveByValue(entX.Constraints, ent)
            end
            "#,
        );
        assert_that!(
            diagnostics,
            is_empty(),
            "IsValid followed by and-short-circuit access of a nullable field must guard that object and field without NeedCheckNil"
        );
    }

    #[gtest]
    fn test_constraint_repro_valid_guard_ent_x_neg() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        def_isvalid_type_guard(&mut ws);

        let diagnostics = diagnostics_for_code(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field Constraints table?
            local ent = {}
            local i = 1
            local entX = ent["Ent" .. i]
            if entX.Constraints then
                table.RemoveByValue(entX.Constraints, ent)
            end
            "#,
        );
        assert_that!(
            diagnostics,
            not(is_empty()),
            "Without IsValid guard, accessing Constraints on nullable entX must trigger NeedCheckNil"
        );
    }
}
