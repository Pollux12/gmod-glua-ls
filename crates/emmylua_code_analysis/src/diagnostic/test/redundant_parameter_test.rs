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
}
