#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_undefined_doc_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedDocParam,
            r#"
            ---@param a number
            ---@param b number
            function bar(a)
            end
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedDocParam,
            r#"
            ---@param a string
            ---@param ccc string
            local d = call(function(a, b, c)
            end)
        "#
        ));
    }

    #[test]
    fn test_doc_param_applies_only_to_the_owner_closure() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedDocParam,
            r#"
            ---@param ply Player
            concommand.Add("x", function(ply) end, function(cmd, args) end)
        "#
        ));
    }
}
