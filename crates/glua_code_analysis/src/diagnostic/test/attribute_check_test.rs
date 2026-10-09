#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn missing_required_parameter() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AttributeMissingParameter,
            r#"
            ---@attribute tagged(name: string)

            ---@[tagged]
            local a = 1
        "#
        ));
    }

    #[test]
    fn redundant_parameter() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AttributeRedundantParameter,
            r#"
            ---@attribute tagged2(name: string)

            ---@[tagged2("a", "b")]
            local a = 1
        "#
        ));
    }

    #[test]
    fn param_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AttributeParamTypeMismatch,
            r#"
            ---@attribute tagged3(name: string)

            ---@[tagged3(1)]
            local a = 1
        "#
        ));
    }
}
