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

        assert_that!(ws.check_code_for(DiagnosticCode::GmodNullCheck, code), eq(false));
        assert_that!(ws.check_code_for(DiagnosticCode::NeedCheckNil, code), eq(false));
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

        assert_that!(ws.check_code_for(DiagnosticCode::GmodNullCheck, code), eq(true));
        assert_that!(ws.check_code_for(DiagnosticCode::NeedCheckNil, code), eq(true));
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

        assert_that!(ws.check_code_for(DiagnosticCode::GmodNullCheck, code), eq(true));
        assert_that!(ws.check_code_for(DiagnosticCode::NeedCheckNil, code), eq(true));
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
    fn test_isfunction_narrows_nil_from_nullable_type() {
        // Bug repro: isfunction(func) should narrow away nil in the true branch
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
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
    fn test_local_cached_isfunction_narrows_nil() {
        // Regression: `local isfunction = isfunction` is common in GMod addons for performance.
        // The cached local must still narrow away nil.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
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
        // Regression: `isstring(x)` should narrow nil from `string?` via try_narrow_istype_function.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
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
}
