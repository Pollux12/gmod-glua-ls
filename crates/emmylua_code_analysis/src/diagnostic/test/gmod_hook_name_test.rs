#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    #[test]
    fn test_reports_invalid_static_hook_names() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::GmodInvalidHookName,
            r#"
            hook.Add("", "id", function() end)
            "#,
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::GmodInvalidHookName,
            r#"
            hook.Run(123)
            "#,
        ));
    }

    #[test]
    fn test_ignores_valid_or_dynamic_hook_names() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::GmodInvalidHookName,
            r#"
            local hook_name = "Think"
            hook.Add("Think", "id", function() end)
            hook.Run(hook_name)
            "#,
        ));
    }

    #[test]
    fn test_gmod_hook_name_checker_is_disabled_with_gmod_off() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        assert!(ws.check_code_for(
            DiagnosticCode::GmodInvalidHookName,
            r#"
            hook.Run(123)
            "#,
        ));
    }
}
