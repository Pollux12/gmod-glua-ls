#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_issue_760() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ReadOnly,
            r#"
            ---@readonly
            local errorCode = {}

            errorCode.NOT_FOUND = 10 --- show warnings attempt to modify readonly variables.
        "#
        ));
    }

    #[test]
    fn test_config_assignment_chain_without_readonly_candidates_is_allowed() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReadOnly,
            r#"
            WepHolster = {}
            WepHolster.defData = {}
            WepHolster.defData["weapon_pistol"] = {}
            WepHolster.defData["weapon_pistol"].Model = "models/weapons/w_pistol.mdl"
            WepHolster.defData["weapon_pistol"].BoneOffset = {
                Vector(1, 2, 3),
                Angle(4, 5, 6),
            }
        "#
        ));
    }
}
