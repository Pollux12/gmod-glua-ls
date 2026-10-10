#[cfg(test)]
mod test {
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, VirtualWorkspace};

    /// The style checker reports Unicode scalar columns; the diagnostic must
    /// land on the same token in UTF-16 columns.
    #[test]
    fn code_style_range_after_an_emoji_is_in_utf16_units() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::CodeStyleCheck);
        let file_id = ws.def("local s = \"\u{1F642}\" local t=1\n");
        let starts = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.message.contains("after token '='"))
            .map(|diagnostic| diagnostic.range.start)
            .collect::<Vec<_>>();

        assert_eq!(starts, vec![lsp_types::Position::new(0, 22)]);
    }

    /// GLua syntax must not make the style checker's parse fail and report nothing.
    #[test]
    fn code_style_reports_in_files_using_glua_syntax() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::CodeStyleCheck);
        let file_id = ws.def("// glua comment
local a = 1
if a != 2 && a != 3 then local t=1 end
");
        let found = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .any(|diagnostic| diagnostic.message.contains("after token '='"));

        assert!(found);
    }
}
