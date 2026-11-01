#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::RequireModuleNotVisible);

        // Define a module with an @export namespace restriction
        ws.def_file(
            "test.lua",
            r#"
                ---@namespace Test

                ---@export namespace
                local M = {}

                return M
                "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::RequireModuleNotVisible,
            r#"
                local a = require("test")
            "#,
        ));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::RequireModuleNotVisible);

        // `@export` without a namespace restriction
        ws.def_file(
            "test.lua",
            r#"
                ---@namespace Test

                ---@export
                local M = {}

                return M
                "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::RequireModuleNotVisible,
            r#"
                ---@namespace AA
                local a = require("test")
            "#,
        ));
    }
    #[test]
    fn test_3() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::RequireModuleNotVisible);

        // With `@export namespace` but no namespace in the current file, the module is treated as invisible
        ws.def_file(
            "test.lua",
            r#"
                ---@export namespace
                local M = {}

                return M
                "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::RequireModuleNotVisible,
            r#"
                local a = require("test")
            "#,
        ));
    }
}
