#[cfg(test)]
mod test {
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Test
            local Test = {}

            ---@param a string
            function Test.name(a)
            end

            Test:name("")
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Test2
            local Test = {}

            ---@param a string
            function Test.name(a)
            end

            Test.name("", "")
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class A
            ---@field event fun()

            ---@type A
            local a = {
                event = function(aaa)
                end,
            }
        "#
        ));
    }

    fn inherited_callable_suppresses_redundant_parameter(enable_isolation: bool) -> bool {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.workspace.enable_isolation = enable_isolation;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::RedundantParameter);

        let other_workspace = ws
            .virtual_url_generator
            .base
            .parent()
            .expect("virtual workspace parent")
            .join("other_workspace");
        ws.analysis.add_main_workspace(other_workspace.clone());
        let other_uri = lsp_types::Uri::parse_from_file_path(
            &other_workspace.join("lua/foreign_arity_edge.lua"),
        )
        .expect("other workspace uri");
        ws.analysis
            .update_file_by_uri(
                &other_uri,
                Some(
                    r#"
                ---@class ArityChild : ArityBase
                local ArityChild = {}
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);
        let file_id = ws.def_file(
            "lua/current_arity.lua",
            r#"
            ---@class ArityBase
            ---@field Run fun(first: string, second: string)
            local ArityBase = {}

            ---@class ArityChild
            local ArityChild = {}

            function ArityChild.Run(first) end
            ArityChild.Run("first", "second")
            "#,
        );

        let code = Some(NumberOrString::String(
            DiagnosticCode::RedundantParameter.get_name().to_string(),
        ));
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .iter()
            .all(|diagnostic| diagnostic.code != code)
    }

    #[test]
    fn isolated_inherited_callable_does_not_suppress_redundant_parameter() {
        assert!(inherited_callable_suppresses_redundant_parameter(false));
        assert!(!inherited_callable_suppresses_redundant_parameter(true));
    }

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
                ---@type fun(...)[]
                local a = {}

                a[1] = function(ccc, ...)
                end
        "#
        ));
    }

    #[test]
    fn test_dots() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Test
            local Test = {}

            ---@param a string
            ---@param ... any
            function Test.dots(a, ...)
                print(a, ...)
            end

            Test.dots(1, 2, 3)
            Test:dots(1, 2, 3)
        "#
        ));
    }

    #[test]
    fn test_issue_360() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
                ---@alias buz number

                ---@param a buz
                ---@overload fun(): number
                function test(a)
                end

                local c = test({'test'})
        "#
        ));
    }

    #[test]
    fn test_function_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
                ---@class D30
                local M = {}

                ---@param callback fun()
                local function with_local(callback)
                end

                function M:add_local_event()
                    with_local(function(local_player) end)
                end
        "#
        ));
    }

    #[test]
    fn test_inferred_callback_params_with_fewer_explicit_params() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
                ---@param cb fun(self: table, width: number, height: number)
                local function set_paint(cb)
                end

                set_paint(function(self)
                end)
        "#
        ));
    }

    #[test]
    fn test_generic_infer_function() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias Parameters<T extends function> T extends (fun(...: infer P): any) and P or never

            ---@alias Procedure fun(...: any[]): any

            ---@alias MockParameters<T> T extends Procedure and Parameters<T> or never

            ---@class Mock<T>
            ---@field calls MockParameters<T>[]
            ---@overload fun(...: MockParameters<T>...)

            ---@generic T: Procedure
            ---@param a T
            ---@return Mock<T>
            function fn(a)
            end

            sum = fn(function(a, b)
                return a + b
            end)
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            sum(1, 2, 3)
        "#
        ));
    }

    #[test]
    fn test_issue_894() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            _nop = function() end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            function a(...) _nop(...) end
        "#
        ));
    }

    // When a colon-defined method annotation is assigned via dot syntax, the user's
    // closure should be allowed to include an explicit `self` as the first parameter
    // without being flagged as a redundant parameter.
    #[test]
    fn test_colon_method_dot_assign_with_explicit_self() {
        let mut ws = VirtualWorkspace::new();

        // panel.Paint = function(self, w, h) should NOT trigger redundant-parameter
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Panel
            local Panel = {}

            ---@param width number
            ---@param height number
            function Panel:Paint(width, height) end

            ---@type Panel
            local panel = {}

            panel.Paint = function(self, w, h)
            end
            "#
        ));

        // With fewer params (no self) also OK
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Panel2
            local Panel2 = {}

            ---@param width number
            ---@param height number
            function Panel2:Paint(width, height) end

            ---@type Panel2
            local panel2 = {}

            panel2.Paint = function(w, h)
            end
            "#
        ));
    }

    #[test]
    fn test_vgui_registered_table_inherits_panel_method_optional_param() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
            ---@class Panel
            local Panel = {}

            ---@param layoutNow? boolean
            function Panel:InvalidateLayout(layoutNow) end

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

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            local panelType = vgui.RegisterTable({
                Layout = function(self)
                    self:InvalidateLayout()
                    self:InvalidateLayout(true)
                end
            }, "Panel")
            "#
        ));
    }

    #[test]
    fn test_instance_dynamic_panel_field_does_not_shadow_inherited_panel_method() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
            ---@class Panel
            local Panel = {}

            ---@param layoutNow? boolean
            function Panel:InvalidateLayout(layoutNow) end

            ---@class DPanel: Panel
            local DPanel = {}
            "#,
        );

        ws.def_file(
            "lua/vgui/dpanel.lua",
            r#"
            local PANEL = {}

            vgui.Register("DPanel", PANEL, "Panel")
            "#,
        );

        ws.def_file(
            "lua/vgui/dpanellist.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                self.pnlCanvas = vgui.Create("DPanel", self)
                self.pnlCanvas.InvalidateLayout = function()
                    self:InvalidateLayout()
                end
            end

            vgui.Register("DPanelList", PANEL, "Panel")
            "#,
        );

        assert!(ws.check_file_for(
            DiagnosticCode::RedundantParameter,
            "lua/vgui/dtree_node.lua",
            r#"
            local PANEL = {}

            function PANEL:Layout()
                self:InvalidateLayout(true)
            end

            vgui.Register("DTreeNode", PANEL, "Panel")
            "#
        ));
    }

    #[test]
    fn test_dynamic_callable_member_preserves_inherited_callable_arity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class BasePanel
            local BasePanel = {}

            ---@param immediate? boolean
            function BasePanel:InvalidateLayout(immediate) end

            ---@class ChildPanel: BasePanel
            local ChildPanel = {}

            ---@type ChildPanel
            local canvas = {}
            canvas.InvalidateLayout = function() end

            ---@type ChildPanel
            local panel = {}
            panel:InvalidateLayout(true)
            "#
        ));
    }

    // Real-world repro: `Vector(seat.GlideExitPos[1], -seat.GlideExitPos[2], seat.GlideExitPos[3])`
    // where `seat.GlideExitPos` is inferred as `Vector | nil` after an `if cond then ... else nil end`
    // on an unannotated (unknown) parameter. The 3-arg call must NOT be matched against a 1-arg
    // overload of Vector. Two related bugs:
    //   1) Field type collapses to `nil` instead of `Vector?` when condition has type `unknown`.
    //   2) Overload resolution falls back to the first overload (1-arg Vector) when args are nil,
    //      bypassing the count-based filter that should keep the 3-param base signature.
    #[test]
    fn test_vector_overload_with_optional_field_assigned_in_branches() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Vector
            local Vector = {}

            ---@overload fun(vector: Vector): Vector
            ---@overload fun(vectorString: string): Vector
            ---@param x? number
            ---@param y? number
            ---@param z? number
            ---@return Vector
            function _G.Vector(x, y, z) end

            ---@class Seat
            ---@field GlideExitPos? Vector
            local seat = {}

            -- unannotated param => `unknown`. Branches assign Vector / nil.
            local function set_exit(exitPos)
                if exitPos then
                    seat.GlideExitPos = Vector(exitPos[1], exitPos[2], exitPos[3])
                else
                    seat.GlideExitPos = nil
                end
            end

            local pos = Vector(seat.GlideExitPos[1], -seat.GlideExitPos[2], seat.GlideExitPos[3])
            "#
        ));
    }

    // Even simpler: no annotation on Seat at all. Field type comes purely from the if/else
    // assignments. This is the closest to the user's actual scenario.
    #[test]
    fn test_vector_overload_with_field_collapsing_to_nil_in_branches() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Vector
            local Vector = {}

            ---@overload fun(vector: Vector): Vector
            ---@overload fun(vectorString: string): Vector
            ---@param x? number
            ---@param y? number
            ---@param z? number
            ---@return Vector
            function _G.Vector(x, y, z) end

            local seat = {}

            local function set_exit(exitPos)
                if exitPos then
                    seat.GlideExitPos = Vector(exitPos[1], exitPos[2], exitPos[3])
                else
                    seat.GlideExitPos = nil
                end
            end

            local pos = Vector(seat.GlideExitPos[1], -seat.GlideExitPos[2], seat.GlideExitPos[3])
            "#
        ));
    }

    // Minimal repro: even without the if/else flow, calling Vector(a, b, c) where the args are
    // typed `nil` must not pick the 1-arg `fun(vector: Vector): Vector` overload.
    #[test]
    fn test_vector_three_nil_args_does_not_pick_one_arg_overload() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Vector
            local Vector = {}

            ---@overload fun(vector: Vector): Vector
            ---@overload fun(vectorString: string): Vector
            ---@param x? number
            ---@param y? number
            ---@param z? number
            ---@return Vector
            function _G.Vector(x, y, z) end

            ---@type nil
            local n = nil
            local pos = Vector(n, n, n)
            "#
        ));
    }

    // Repro where args come from indexing a `nil` value, producing `Never`-typed args.
    #[test]
    fn test_vector_three_never_args_does_not_pick_one_arg_overload() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ---@class Vector
            local Vector = {}

            ---@overload fun(vector: Vector): Vector
            ---@overload fun(vectorString: string): Vector
            ---@param x? number
            ---@param y? number
            ---@param z? number
            ---@return Vector
            function _G.Vector(x, y, z) end

            ---@type nil
            local nilVal = nil
            local pos = Vector(nilVal[1], -nilVal[2], nilVal[3])
            "#
        ));
    }

    /// StarfallEx's `Privilege` literal carries a 0-arg `check`
    /// placeholder, and every real writer is `self.check =
    /// function(instance, target)` inside `buildcheck = function(self)` —
    /// an unannotated table-field param, so the write resolves to no owner
    /// and never joins the member item.
    #[test]
    fn test_member_with_unattributable_writers_is_not_called_redundant() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "lua/writer.lua",
            r#"
            local Privilege = {
                buildcheck = function(self)
                    self.check = function(instance, target)
                        return instance, target
                    end
                end,
            }
            return Privilege
            "#,
        );

        assert!(ws.check_file_for(
            DiagnosticCode::RedundantParameter,
            "lua/reader.lua",
            r#"
            local privilege = { check = function() error("not set") end }
            privilege.check(1, 2)
            "#,
        ));
    }

    /// StarfallEx `permissions/core.lua` builds `checks` with
    /// `checks[#checks+1] = check`, an append write under a computed key.
    #[test]
    fn test_computed_key_collection_element_is_not_called_redundant() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_file_for(
            DiagnosticCode::RedundantParameter,
            "lua/collection.lua",
            r#"
            ---@param src any
            local function build(src, key, instance, target)
                local checks = {}
                local check = src.checks[key]
                if check == "block" then
                    check = function() return false, "blocked!" end
                end
                if check ~= "allow" then
                    checks[#checks+1] = check
                end
                for _, v in ipairs(checks) do
                    local ok, reason = v(instance, target)
                    if not ok then return false, reason end
                end
                return true
            end
            return build
            "#,
        ));
    }

    /// A container with only literal keys keeps its authority — nothing about it
    /// is sampled, so a real arity error still has to be reported.
    #[test]
    fn test_literal_key_collection_element_still_reports_redundant() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_file_for(
            DiagnosticCode::RedundantParameter,
            "lua/literal_collection.lua",
            r#"
            local checks = { function() return true end }
            local function run(instance, target)
                for _, v in ipairs(checks) do
                    local ok = v(instance, target)
                    if not ok then return false end
                end
                return true
            end
            return run
            "#,
        ));
    }

    /// An annotated member keeps its authority: an unattributed write to some
    /// unrelated table sharing the name must not excuse a real arity error.
    #[test]
    fn test_annotated_member_still_reports_despite_unattributable_writers() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "lua/writer.lua",
            r#"
            local Holder = {
                build = function(self)
                    self.abs = function(a, b) return a, b end
                end,
            }
            return Holder
            "#,
        );

        assert!(!ws.check_file_for(
            DiagnosticCode::RedundantParameter,
            "lua/reader.lua",
            r#"
            local fade = math.abs(1, 0)
            "#,
        ));
    }

    /// An unattributable write carries no owner, so it is matched by bare name.
    /// A declared class does not share that name space: its methods keep their
    /// arity however many untyped tables elsewhere hold a field called the same.
    #[test]
    fn test_class_method_still_reports_despite_unattributable_writers() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "lua/writer.lua",
            r#"
            local Holder = {
                build = function(self)
                    self.Think = function(a, b, c) return a, b, c end
                end,
            }
            return Holder
            "#,
        );

        assert!(!ws.check_file_for(
            DiagnosticCode::RedundantParameter,
            "lua/reader.lua",
            r#"
            ---@class ThinkerA
            local A = {}
            function A:Think(a) return a end

            ---@class ThinkerB
            local B = {}
            function B:Think(a) return a end

            A:Think(1, 2)
            "#,
        ));
    }

    /// A call with more arguments than any overload takes is resolved by how
    /// the leading arguments fit, so only the surplus argument is reported.
    #[test]
    fn test_overloaded_call_with_surplus_argument_reports_only_the_surplus() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class SurplusColor
            ---@field r number

            ---@overload fun(color: SurplusColor)
            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a number
            function SetSurplusColor(r, g, b, a) end

            SetSurplusColor(255, 255, 255, 255, 0)
            "#,
        );
        let diagnostics_for = |ws: &mut VirtualWorkspace, code: DiagnosticCode| {
            ws.analysis.diagnostic.enable_only(code);
            let expected = Some(NumberOrString::String(code.get_name().to_string()));
            ws.analysis
                .diagnose_file(file_id, CancellationToken::new())
                .unwrap_or_default()
                .into_iter()
                .filter(|diagnostic| diagnostic.code == expected)
                .collect::<Vec<_>>()
        };

        let redundant = diagnostics_for(&mut ws, DiagnosticCode::RedundantParameter);
        assert_eq!(redundant.len(), 1);
        assert_eq!(redundant[0].range.start.line, 11);
        assert!(redundant[0].range.start.character >= 40);
        assert!(diagnostics_for(&mut ws, DiagnosticCode::ParamTypeMismatch).is_empty());
    }

    fn diagnostic_count(source: &str, code: DiagnosticCode) -> usize {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(source);
        ws.analysis.diagnostic.enable_only(code);
        let expected = Some(NumberOrString::String(code.get_name().to_string()));
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == expected)
            .count()
    }

    /// The shorter overload that fits the leading arguments is chosen whatever
    /// the declaration order, and the surplus arguments are the redundant ones.
    #[test]
    fn test_surplus_call_selects_shorter_fitting_overload_in_any_order() {
        for (first, second) in [("boolean", "number"), ("number", "boolean")] {
            let source = format!(
                r#"
                ---@overload fun(value: {first})
                ---@overload fun(value: {second})
                ---@param left string
                ---@param right string
                local function choose(left, right) end

                choose(10, 20, 30)
                "#
            );
            assert_eq!(
                diagnostic_count(&source, DiagnosticCode::RedundantParameter),
                2
            );
            assert_eq!(
                diagnostic_count(&source, DiagnosticCode::ParamTypeMismatch),
                0
            );
        }
    }

    /// A candidate accepting the whole leading argument type wins over one
    /// accepting only a union component of it, whatever the declaration order.
    #[test]
    fn test_surplus_call_prefers_full_match_over_union_component_match() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@overload fun(value: number): string
            ---@param value number|string
            ---@return boolean
            local function choose(value)
                return true
            end

            ---@param value number|string
            local function caller(value)
                local result = choose(value, false)
                return result
            end
            "#,
        );
        let result = ws.local_ty(file_id, "result");
        assert_eq!(ws.humanize_type(result), "boolean");
        assert_eq!(
            diagnostic_count_in(&mut ws, file_id, DiagnosticCode::RedundantParameter),
            1
        );
        assert_eq!(
            diagnostic_count_in(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            0
        );
    }

    /// A variadic alternative matching only a union component does not beat the
    /// finite form that matches in full, although it consumes more arguments.
    #[test]
    fn test_surplus_call_prefers_full_match_over_partial_variadic() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@overload fun(...: number|boolean): string
            ---@param value number|string
            ---@return boolean
            local function choose(value)
                return true
            end

            ---@param value number|string
            local function caller(value)
                local result = choose(value, false)
                return result
            end
            "#,
        );
        let result = ws.local_ty(file_id, "result");
        assert_eq!(ws.humanize_type(result), "boolean");
    }

    /// A variadic alternative whose required fixed parameter is not supplied
    /// ranks below the finite form whose required parameters are all supplied.
    #[test]
    fn test_surplus_call_prefers_finite_form_over_variadic_missing_required_prefix() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@overload fun(a: number, b: number, required: string, ...: number): string
            ---@param value number
            ---@return boolean
            local function choose(value)
                return true
            end

            local result = choose(1, 2)
            "#,
        );
        let result = ws.local_ty(file_id, "result");
        assert_eq!(ws.humanize_type(result), "boolean");
        assert_eq!(
            diagnostic_count_in(&mut ws, file_id, DiagnosticCode::RedundantParameter),
            1
        );
        assert_eq!(
            diagnostic_count_in(&mut ws, file_id, DiagnosticCode::MissingParameter),
            0
        );
    }

    /// An optional fixed parameter left unsupplied does not demote the variadic alternative.
    #[test]
    fn test_surplus_call_keeps_variadic_with_optional_prefix() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@overload fun(a: number, b: number, required: string?, ...: number): string
            ---@param value number
            ---@return boolean
            local function choose(value)
                return true
            end

            local result = choose(1, 2)
            "#,
        );
        let result = ws.local_ty(file_id, "result");
        assert_eq!(ws.humanize_type(result), "string");
        assert_eq!(
            diagnostic_count_in(&mut ws, file_id, DiagnosticCode::RedundantParameter),
            0
        );
    }

    /// A fixed parameter whose type accepts an omitted argument through an alias or `unknown` does not demote the variadic alternative.
    #[test]
    fn test_surplus_call_keeps_variadic_with_nullable_alias_or_unknown_prefix() {
        for (prelude, param_type) in [
            ("---@alias MaybeString string|nil\n", "MaybeString"),
            ("", "unknown"),
        ] {
            let mut ws = VirtualWorkspace::new();
            let file_id = ws.def(&format!(
                r#"
                {prelude}
                ---@overload fun(a: number, b: number, extra: {param_type}, ...: number): string
                ---@param value number
                ---@return boolean
                local function choose(value)
                    return true
                end

                local result = choose(1, 2)
                "#
            ));
            let result = ws.local_ty(file_id, "result");
            assert_eq!(ws.humanize_type(result), "string", "{param_type}");
            for code in [
                DiagnosticCode::RedundantParameter,
                DiagnosticCode::MissingParameter,
            ] {
                assert_eq!(
                    diagnostic_count_in(&mut ws, file_id, code),
                    0,
                    "{param_type}"
                );
            }
        }
    }

    fn diagnostic_count_in(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> usize {
        ws.analysis.diagnostic.enable_only(code);
        let expected = Some(NumberOrString::String(code.get_name().to_string()));
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == expected)
            .count()
    }

    /// A typed variadic overload whose leading type fails does not hide the
    /// finite overload that fits.
    #[test]
    fn test_surplus_call_ignores_failing_typed_variadic_overload() {
        let source = r#"
            ---@overload fun(value: boolean)
            ---@overload fun(tag: string, ...: string)
            ---@param left number
            ---@param right number
            local function choose(left, right) end

            choose(1, 2, 3)
            "#;
        assert_eq!(
            diagnostic_count(source, DiagnosticCode::RedundantParameter),
            1
        );
        assert_eq!(
            diagnostic_count(source, DiagnosticCode::ParamTypeMismatch),
            0
        );
    }
}
