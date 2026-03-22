#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_issue_250() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            --- @class A
            --- @field field any
            local A = {}

            function A:method()
            pcall(function()
                return self.field
            end)
            end
            "#
        ));
    }

    #[test]
    fn test_guarded_undefined_global_if_truthy() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            if invalidVar then
                print(invalidVar)
            end
            "#
        ));
    }

    #[test]
    fn test_guarded_undefined_global_if_isvalid() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            function _G.IsValid(_) end

            if IsValid(entMaybe) then
                print(entMaybe)
            end
            "#
        ));
    }

    #[test]
    fn test_unguarded_undefined_global_still_reports() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            print(invalidVar)
            "#
        ));
    }
}
