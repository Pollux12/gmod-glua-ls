#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_duplicate_index() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::DuplicateIndex,
            r#"
                local a = {
                    b = 1,
                    b = 1
                }
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateIndex,
            r#"
                local a = {
                    a = 1,
                    b = 1
                }
            "#
        ));
    }

    #[test]
    fn test_string_key_that_looks_like_integer_is_not_duplicate() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateIndex,
            r#"
                local a = {
                    ["[1]"] = 1,
                    [1] = 2
                }
            "#
        ));
    }

    #[test]
    fn test_repeated_integer_key_is_duplicate() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::DuplicateIndex,
            r#"
                local a = {
                    [1] = 1,
                    [1] = 2
                }
            "#
        ));
    }

    #[test]
    fn test_integer_key_over_positional_entry_is_duplicate() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::DuplicateIndex,
            r#"
                local a = {
                    "a",
                    [1] = "b"
                }
            "#
        ));
    }
}
