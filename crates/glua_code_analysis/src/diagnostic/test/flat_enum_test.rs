#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    const FLAT_ENUM: &str = r#"
        EF_BONEMERGE = 1
        EF_NODRAW = 32

        ---@enum EF
        ---| EF_BONEMERGE # Performs bone merge on client side
        ---| EF_NODRAW # Don't draw the entity

        ---@param effect EF
        function AddEffects(effect) end
    "#;

    fn check_call(argument: &str) -> bool {
        let mut ws = VirtualWorkspace::new();
        ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            &format!("{FLAT_ENUM}\nAddEffects({argument})"),
        )
    }

    #[test]
    fn flat_enum_accepts_an_integer_literal() {
        assert!(check_call("1"));
    }

    #[test]
    fn flat_enum_accepts_a_member_reference() {
        assert!(check_call("EF_BONEMERGE"));
    }

    #[test]
    fn flat_enum_accepts_a_plain_integer_variable() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            &format!("{FLAT_ENUM}\n---@type integer\nlocal effect\nAddEffects(effect)"),
        ));
    }

    #[test]
    fn flat_enum_accepts_a_bitwise_combination() {
        assert!(check_call("bit.bor(EF_BONEMERGE, EF_NODRAW)"));
    }

    #[test]
    fn flat_enum_accepts_an_unlisted_number() {
        // Matches the behaviour of the `@alias` form this replaces: numeric enums
        // are combined at runtime, so an unlisted value is not an error.
        assert!(check_call("999"));
    }

    #[test]
    fn flat_enum_rejects_a_string() {
        assert!(!check_call("\"EF_BONEMERGE\""));
    }

    #[test]
    fn flat_enum_reports_a_comparison_against_an_unlisted_value() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::EnumValueMismatch,
            r#"
            LEVEL_LOW = 1
            LEVEL_HIGH = 2

            ---@enum LEVEL
            ---| LEVEL_LOW # Low
            ---| LEVEL_HIGH # High

            ---@type LEVEL
            local level

            if level == 999 then
            end
            "#,
        ));
    }
}
