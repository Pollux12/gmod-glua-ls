#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_disabled_when_gmod_off() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file("cl_test.lua", r#"AddCSLuaFile("shared.lua")"#);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[test]
    fn test_reports_when_enabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMisuse);
        let file_id = ws.def_file("cl_test.lua", r#"AddCSLuaFile("shared.lua")"#);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
        ));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }

    #[test]
    fn test_disabled_when_gmod_off_even_if_diagnostic_enabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMisuse);
        let file_id = ws.def_file("cl_test.lua", r#"AddCSLuaFile("shared.lua")"#);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMisuse.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == code));
    }
}
