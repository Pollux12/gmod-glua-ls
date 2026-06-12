#[cfg(test)]
mod test {
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
}
