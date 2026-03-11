use lsp_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

fn full_document_diagnostic_report(items: Vec<Diagnostic>) -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: None,
            items,
        },
    })
    .into()
}

pub async fn on_pull_document_diagnostic(
    context: ServerContextSnapshot,
    params: DocumentDiagnosticParams,
    token: CancellationToken,
) -> DocumentDiagnosticReportResult {
    let uri = params.text_document.uri;

    if context.debounced_analysis().is_dirty()
        && let Some(cached_diagnostics) = context
            .file_diagnostic()
            .cached_display_diagnostics(&uri)
            .await
    {
        return full_document_diagnostic_report(cached_diagnostics);
    }

    if !context.debounced_analysis().wait_until_fresh(&token).await {
        return full_document_diagnostic_report(
            context
                .file_diagnostic()
                .cached_display_diagnostics(&uri)
                .await
                .unwrap_or_default(),
        );
    }

    let diagnostics = match context
        .file_diagnostic()
        .pull_file_diagnostics(uri.clone(), token)
        .await
    {
        Some(diagnostics) => {
            context
                .file_diagnostic()
                .cache_fresh_file_diagnostics(&uri, &diagnostics)
                .await;
            diagnostics
        }
        None => context
            .file_diagnostic()
            .cached_display_diagnostics(&uri)
            .await
            .unwrap_or_default(),
    };

    full_document_diagnostic_report(diagnostics)
}
