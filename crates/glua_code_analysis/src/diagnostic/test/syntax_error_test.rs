#[cfg(test)]
mod test {
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, EmmyrcLuaVersion, VirtualWorkspace};

    fn syntax_error_messages_and_lines(
        ws: &mut VirtualWorkspace,
        file_name: &str,
        content: &str,
    ) -> Vec<(String, u32)> {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::SyntaxError);
        let file_id = ws.def_file(file_name, content);
        let syntax_error_code = Some(NumberOrString::String(
            DiagnosticCode::SyntaxError.get_name().to_string(),
        ));

        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == syntax_error_code)
            .map(|diagnostic| (diagnostic.message, diagnostic.range.start.line))
            .collect()
    }

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::SyntaxError,
            r#"
            local function aaa(..., n)
            end
        "#
        ));
    }

    #[test]
    fn test_luajit_ull() {
        let mut ws = VirtualWorkspace::new();
        let mut config = ws.get_emmyrc();
        config.runtime.version = EmmyrcLuaVersion::LuaJIT;
        ws.update_emmyrc(config);
        assert!(ws.check_code_for(
            DiagnosticCode::SyntaxError,
            r#"
            local d = 0xFFFFFFFFFFFFFFFFULL
        "#
        ));
    }

    #[test]
    fn syntax_error_should_not_consume_function_end_after_incomplete_statement() {
        let mut ws = VirtualWorkspace::new();
        let diagnostics = syntax_error_messages_and_lines(
            &mut ws,
            "lua/test.lua",
            r#"---@class TestClass
local testClass = {}

function testClass:MethodOne()
    self._testVar = "string"
    self._testVar -- this is causing the error
end

function testClass:MethodTwo()
    self._testVar = true
end
"#,
        );

        assert_eq!(
            diagnostics,
            vec![(
                "expected '=' for assignment or this is an incomplete statement".to_string(),
                5,
            )]
        );
    }
}
