#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

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

    #[test]
    fn test_gmod_vgui_panel_registration_allows_panel_reuse() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                local PANEL = {}
                function PANEL:Init() end
                derma.DefineControl("FirstPanel", "", PANEL, "Panel")

                local PANEL = {}
                function PANEL:Init() end
                derma.DefineControl("SecondPanel", "", PANEL, "Panel")
        "#
        ));
    }

    #[test]
    fn test_gmod_vgui_panel_registration_initializer_allows_panel_reuse() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                local PANEL = {}
                function PANEL:Init() end

                local PANEL = derma.DefineControl("Button", "", PANEL, "DLabel")
                PANEL = table.Copy(PANEL)
        "#
        ));
    }

    #[test]
    fn test_unregistered_panel_reuse_still_reports_redefined_local() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::RedefinedLocal,
            r#"
                local PANEL = {}
                function PANEL:Init() end

                local PANEL = {}
                function PANEL:Init() end
        "#
        ));
    }
    #[test]
    fn repeated_scopes_with_many_visible_locals_diagnose_quick_smoke() {
        let mut code = String::new();
        for i in 0..300 {
            code.push_str(&format!(
                "local visible_{i} = {i}\ndo\n    local scoped_{i} = visible_{i}\nend\n"
            ));
        }

        let mut ws = VirtualWorkspace::new();
        let start = Instant::now();
        let no_redefined_local = ws.check_code_for(DiagnosticCode::RedefinedLocal, &code);
        let elapsed = start.elapsed();

        assert!(no_redefined_local);
        assert!(
            elapsed.as_millis() < 250,
            "redefined-local repeated-scope smoke took too long: {elapsed:?}"
        );
    }

    #[test]
    fn repeated_syntax_vgui_registration_reuse_diagnose_quick_smoke() {
        let mut code = String::new();
        for i in 0..80 {
            code.push_str(&format!(
                "local PANEL = {{}}\nfunction PANEL:Init() end\nvgui.Register(\"Panel{i}\", PANEL, \"Panel\")\n"
            ));
        }

        let mut ws = VirtualWorkspace::new();
        let start = Instant::now();
        let no_redefined_local = ws.check_code_for(DiagnosticCode::RedefinedLocal, &code);
        let elapsed = start.elapsed();

        assert!(no_redefined_local);
        assert!(
            elapsed.as_millis() < 250,
            "redefined-local vgui registration smoke took too long: {elapsed:?}"
        );
    }
}
