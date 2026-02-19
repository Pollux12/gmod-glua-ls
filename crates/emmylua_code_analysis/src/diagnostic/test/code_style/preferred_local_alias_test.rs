#[cfg(test)]
mod test {
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::DiagnosticCode;

    #[test]
    fn test_feat_724() {
        let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();

        assert!(!ws.check_code_for(
            DiagnosticCode::PreferredLocalAlias,
            r#"
            local gsub = string.gsub
            print(string.gsub("hello", "l", "0"))
            "#,
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::PreferredLocalAlias,
            r#"
            local t = {
                a = ""
            }
            local h = t.a
            t.a = 'h'
            print(t.a)
            "#,
        ));
    }

    #[test]
    fn test_reports_alias_once_per_access_path() {
        let mut ws = crate::VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::PreferredLocalAlias);
        let file_id = ws.def(
            r#"
                local t = { a = "" }
                local h = t.a
                print(t.a)
                print(t.a)
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::PreferredLocalAlias.get_name().to_string(),
        ));
        let count = diagnostics.iter().filter(|d| d.code == code).count();
        assert_eq!(count, 1);
    }
}
