#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_issue_242() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@class A
                local A = {}
                A.__index = A

                function A:method() end

                ---@return A
                function new()
                    local a = setmetatable({}, A);
                    return a
                end
        "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                local setmetatable = setmetatable
                ---@class A
                local A = {}

                function A:method() end

                ---@return A
                function new()
                    return setmetatable({}, { __index = A })
                end
        "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@class A
                local A = {}
                A.__index = A

                function A:method() end

                ---@return A
                function new()
                return setmetatable({}, A)
                end
        "#
        ));
    }

    #[test]
    fn test_issue_220() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            --- @class A

            --- @return A?, integer?
            function bar()
            end

            --- @return A?, integer?
            function foo()
            return bar()
            end
        "#
        ));
    }

    #[test]
    fn test_return_type_mismatch() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@class diagnostic.Test1
            local Test = {}

            ---@return number
            function Test.n()
                return "1"
            end
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@return string
            local test = function()
                return 1
            end
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@class diagnostic.Test2
            local Test = {}

            ---@return number
            Test.n = function ()
                return "1"
            end
        "#
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@return number
            local function test3()
                if true then
                    return ""
                else
                    return 2, 3
                end
                return 2, 3
            end
        "#
        ));
    }

    #[test]
    fn test_variadic_return_type_mismatch() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@return number, any...
            local function test()
                return 1, 2, 3
            end
        "#
        ));
    }

    #[test]
    fn test_issue_146() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            local function bar()
                return {}
            end

            ---@param _f fun():table 测试
            function foo(_f) end

            foo(function()
                return bar()
            end)
            "#
        ));
    }

    #[test]
    fn test_issue_150() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantReturnValue,
            r#"
            function bar(a)
                return tonumber(a)
            end
            "#
        ));
    }

    #[test]
    fn test_return_dots_syntax_error() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::SyntaxError,
            r#"
            function bar()
                return ...
            end
            "#
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::SyntaxError,
            r#"
            function bar()
                local args = {...}
            end
            "#
        ));
    }

    #[test]
    fn test_issue_167() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            --- @return integer?, integer?
            local function foo()
            end

            --- @return integer?, integer?
            local function bar()
                return foo()
            end
            "#
        ));
    }

    #[test]
    fn test_as_return_type() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            local function dd()
                return "11231"
            end

            ---@return integer
            local function f()

                return dd() ---@as integer
            end
        "#
        ));
    }

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@return string?
                local function a()
                    ---@type int?
                    local ccc
                    return ccc and a() or nil
                end
            "#
        ));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@return any[]
                local function a()
                    ---@type table|table<any, any>
                    local ccc
                    return ccc
                end
            "#
        ));
    }

    #[test]
    fn test_3() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@return table<string, {old: any, new: any}>
                local function test()
                    ---@type table<string, {old: any, new: any}>|table
                    local a
                    return a
                end
            "#
        ));
    }

    #[test]
    fn test_4() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        // TODO 该测试被`setmetatable`强行覆盖, 未正常诊断`debug.setmetatable`
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@generic T
            ---@param value T
            ---@param meta? table
            ---@return T value
            ---@overload fun(value: table, meta: T): T
            local setmetatable = debug.setmetatable

            ---@class switch
            ---@field cachedCases string[]
            ---@field map table<string, function>
            ---@field _default fun(...):...
            local switchMT = {}

            ---@return switch
            local function switch()
                local obj = setmetatable({
                    map = {},
                    cachedCases = {},
                }, switchMT)
                return obj
            end
            "#
        ));
    }

    #[test]
    fn test_issue_341() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            --- @return integer
            local function foo()
                local a --- @type integer?
                return a or error("a is nil")
            end
            end
            "#
        ));
    }

    #[test]
    fn test_supper() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@class key: integer

                ---@return key key
                local function get()
                    return 0
                end
            "#
        ));
    }

    #[test]
    fn test_return_self() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@class UI
            local M = {}

            ---@return self
            function M:get()
                return self
            end
            "#
        ));
    }
    #[test]
    fn test_issue_343() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                --- @return integer, integer
                function range() return 0, 0 end

                ---@class MyType
                ---@field [1] integer
                ---@field [2] integer

                --- @return MyType
                function foo()
                return { range() }
                end

                --- @return MyType
                function bar()
                return { 0, 0 }
                end
            "#
        ));
    }
    #[test]
    fn test_issue_474() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
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
    fn test_super_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@namespace Test

                ---@alias A fun()

                ---@class B<T>: A

                ---@return A
                local function subscribe()
                    ---@type B<string>
                    local a

                    return a
                end
            "#
        ));
    }

    #[test]
    fn test_generic_type_extends() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@class AnonymousObserver<T>: Observer<T>

                ---@class Observer<T>: IDisposable

                ---@class IDisposable

                ---@generic T
                ---@return IDisposable
                local function createAnonymousObserver()
                    ---@type AnonymousObserver<T>
                    local observer = {}

                    return observer
                end
            "#
        ));
    }

    #[test]
    fn test_generic_type_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Range: Observable<integer>
            ---@class Observable<T>

            ---@return Range
            function newRange()
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"

            ---@return Observable<integer>
            function range()
                return newRange()
            end

            "#
        ));
    }

    #[test]
    fn test_generic_type_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Observable<T>
                ---@class CountObservable<T>: Observable<integer>
                CountObservable = {}
                ---@return CountObservable<T>
                function CountObservable:new()
                end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@return Observable<integer>
                local function count()
                    return CountObservable:new()
                end
            "#
        ));
    }

    /// A union arm that is an unbounded variadic answers *every* slot, so the
    /// spread has to bound itself. Checking a `@return` asks for the value list
    /// with no arity to fill, and taking the unbounded arm's arity literally
    /// looped to `usize::MAX` pushing a type per iteration, which ate the
    /// machine on a ten-line file.
    #[test]
    fn unbounded_variadic_union_arm_spreads_without_an_arity_to_fill() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def_file(
            "lua/forward.lua",
            r#"
            ---@vararg integer
            local function forward(n, ...)
                if n > 0 then return forward(n - 1, ...) end
                return ...
            end

            ---@return integer
            local function outer(...)
                return forward(3, ...)
            end

            print(outer(1))
            "#,
        );
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::ReturnTypeMismatch);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        // The point is that this terminates at all, and the type it settles on
        // is what proves it: an unbounded arm taken literally cannot produce a
        // finite union, so pinning the union pins the bound.
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("`(1 ...|unknown ...)`"),
            "spread should yield one slot per unbounded arm: {messages:?}"
        );
    }
}
