#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{DiagnosticCode, VirtualWorkspace};
    #[test]
    fn test_issue_226() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                function foo(...)
                local a = { ... }
                return function(...)
                    return { a, { ... } }
                end
                end
        "#
        ));
    }

    #[test]
    fn test() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                local x = 1
                local x = 2
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
            local function aaa()
            end

            local function aaa()
            end
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
            local function aaa(a, b)
            end
            local a = 2
            local b = 2
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
            ---@class Test
            local Test = {}

            function Test:test(c)
            end

            local c = 1
        "#
        ));
    }

    #[test]
    fn test_do_end() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                do
                    local c = 1
                end

                do
                    local c = 1
                end
                local c = 1
        "#
        ));
    }

    #[test]
    fn test_for() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
            local function aaa()
                for a = 1, 1 do
                    local fora = 1
                end
                local fora = 1
            end
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                for k, v in pairs({}) do
                end
                for k, v in pairs({}) do
                end
        "#
        ));
    }

    #[test]
    fn test_issue_481() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                local a = function(a) -- 不应该报错, 参数`a`先被定义, 然后再是局部变量`a`
                end
        "#
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                local a
                a = function(a) -- 报错
                end
        "#
        ));
    }

    #[test]
    fn test_gmod_self_param_shadow_is_ignored() {
        let code = r#"
            local ENT = {}
            function ENT:Build()
                local f = function(self)
                    return self
                end

                return f(self)
            end
        "#;

        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(DiagnosticCode::RedefinedLocal, code));

        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = false;
        ws.analysis.update_config(Arc::new(emmyrc));
        assert!(!ws.check_code_for(DiagnosticCode::RedefinedLocal, code));
    }
}
