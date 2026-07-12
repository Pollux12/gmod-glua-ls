#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

    fn diagnostics(ws: &mut VirtualWorkspace, source: &str) -> Vec<lsp_types::Diagnostic> {
        let file_id = ws.def(source);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
    }

    fn has_code(diagnostics: &[lsp_types::Diagnostic], code: DiagnosticCode) -> bool {
        let code = Some(NumberOrString::String(code.get_name().to_string()));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    #[test]
    fn unknown_colon_call_reports_undefined_method_error_without_undefined_field() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Entity
            local Entity = {}

            ---@type MethodTest.Entity
            local entity
            entity:MissingMethod()
            "#,
        );

        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .expect("undefined-method diagnostic");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.message, "Undefined method `MissingMethod`. ");
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedField));
    }

    #[test]
    fn unknown_colon_call_in_condition_reports_undefined_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Conditional
            local Conditional = {}

            ---@type MethodTest.Conditional
            local value
            if value:MissingMethod() then
                print("unreachable")
            end
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn known_method_does_not_report_undefined_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Known
            local Known = {}
            function Known:PresentMethod() end

            ---@type MethodTest.Known
            local value
            value:PresentMethod()
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn short_circuit_guarded_optional_method_does_not_report() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Optional
            local Optional = {}

            ---@type MethodTest.Optional
            local value
            if value.OptionalMethod and value:OptionalMethod() then
                print("optional")
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }
}
