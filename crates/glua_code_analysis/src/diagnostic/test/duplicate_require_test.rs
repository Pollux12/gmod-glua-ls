#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test() {
        let mut ws = VirtualWorkspace::new();
        // Different scopes
        ws.def_gmod_call_arg_builtins();
        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateRequire,
            r#"
            if true then
                require("a")
            else
                require("a")
            end
            "#,
        ));

        // Already present in the parent scope
        assert!(!ws.check_code_for(
            DiagnosticCode::DuplicateRequire,
            r#"
            require("a")
            if true then
                require("a")
            else
                require("a")
            end
            "#,
        ));
    }

    #[test]
    fn test_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateRequire,
            r#"
            require("a").a
            require("a")
            "#,
        ));
    }
}
