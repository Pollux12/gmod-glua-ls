#[cfg(test)]
mod test {
    use crate::DiagnosticCode;

    #[test]
    fn test_issue_158() {
        let mut ws = crate::VirtualWorkspace::new();

        ws.def(
            r#"
        a = {} --- @deprecated
        "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::Deprecated,
            r#"
            ---@diagnostic disable-next-line: deprecated
            local _b = a
            "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::Deprecated,
            r#"
            local _c = a ---@diagnostic disable-next-line: deprecated
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::Deprecated,
            r#"
            local _d = a ---@diagnostic disable-line: deprecated
            "#
        ));
    }

    #[test]
    fn deprecated_prefilter_keeps_name_and_member_diagnostics() {
        let mut ws = crate::VirtualWorkspace::new();

        ws.def(
            r#"
        ---@deprecated use new_api
        old_api = {}

        api = {}

        ---@deprecated use api.new_method
        function api.old_method() end
        "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::Deprecated,
            r#"
            old_api
            api.old_method()
            "#
        ));
    }
}
