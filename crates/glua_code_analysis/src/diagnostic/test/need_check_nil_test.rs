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
                if IsValid(ent) then
                    ent:GetPos()
                end
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_isvalid_narrows_explicit_null_param() {
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
                ---@param ent NULL
                local function takes_null(ent)
                    if IsValid(ent) then
                        ent:GetPos()
                    end
                end
                "#,
            ),
            eq(true)
        );
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
    fn test_explicit_entity_null_union_method_requires_isvalid() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
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
    fn test_isvalid_check_does_not_report_gmod_null_check() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
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
        // Bug repro: IsValid(maybe) should narrow away nil in the true branch
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_nil_negative_branch() {
        // Bug repro: if not IsValid(x) then return end — x should be non-nil after
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if not IsValid(maybe) then
                return
            end
            maybe:reverse()
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_reassigned_clientside_model_negative_branch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
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
        assert!(ws.check_code_for(
            DiagnosticCode::UncheckedNilAccess,
            r#"
            ---@class Player
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
    fn test_cached_builtin_isvalid_prior_guard_suppresses_nil_diagnostic() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Player
            ---@field ExitVehicle fun(self: Player)
            ---@return Player?
            function maybePlayer() end

            local IsValid = IsValid
            local bot = maybePlayer()
            if not IsValid(bot) then
                return
            end

            bot:ExitVehicle()
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
        // This tests whether loading IsValid with @return boolean (as in output/global.lua)
        // conflicts with the hardcoded try_narrow_isvalid fallback
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        // Simulate the GLua annotation library defining IsValid with boolean return
        let library_root = ws.virtual_url_generator.new_path("__test_library_isvalid");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@param toBeValidated any The table or object to be validated.
            ---@return boolean # True if the object is valid.
            function _G.IsValid(toBeValidated) end
            "#
                .to_string(),
            ),
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                maybe:reverse()
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
                ---@return boolean
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
            ---@return boolean
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
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local IsValid = IsValid
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                maybe:reverse()
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

    /// Same as above but using `if IsValid(seat) then` (positive branch).
    #[gtest]
    fn test_isvalid_narrows_instance_type_union_null_positive_branch() {
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
}
