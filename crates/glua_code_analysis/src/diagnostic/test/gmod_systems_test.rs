#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    #[gtest]
    fn test_reports_unknown_static_net_start_message() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::GmodUnknownNetMessage,
            r#"
            net.Start("missing_message")
            "#,
        ));
    }

    #[gtest]
    fn test_ignores_known_or_dynamic_net_start_message() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::GmodUnknownNetMessage,
            r#"
            util.AddNetworkString("known_message")
            net.Start("known_message")
            local message_name = "missing_message"
            net.Start(message_name)
            "#,
        ));
    }

    #[gtest]
    fn test_duplicate_system_registration_enabled_by_default() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            util.AddNetworkString("dup_name")
            util.AddNetworkString("dup_name")
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodDuplicateSystemRegistration
                .get_name()
                .to_string(),
        ));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[gtest]
    fn test_reports_duplicate_system_registration_when_enabled() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::GmodDuplicateSystemRegistration,
            r#"
            util.AddNetworkString("dup_name")
            util.AddNetworkString("dup_name")
            concommand.Add("dup_cmd", function() end)
            concommand.Add("dup_cmd", function() end)
            CreateConVar("dup_cvar", "1")
            CreateClientConVar("dup_cvar", "1")
            "#,
        ));
    }

    #[gtest]
    fn test_gmod_systems_checker_is_disabled_with_gmod_off() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        assert!(ws.check_code_for(
            DiagnosticCode::GmodUnknownNetMessage,
            r#"
            net.Start("missing_message")
            "#,
        ));
    }
}
