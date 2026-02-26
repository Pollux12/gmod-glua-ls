use lsp_types::{
    DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

pub async fn on_pull_document_diagnostic(
    context: ServerContextSnapshot,
    params: DocumentDiagnosticParams,
    token: CancellationToken,
) -> DocumentDiagnosticReportResult {
    let uri = params.text_document.uri;

    // Wait for any pending debounced reindex to finish before diagnosing
    let file_id = {
        let analysis = context.analysis().read().await;
        analysis.get_file_id(&uri)
    };
    if let Some(file_id) = file_id {
        context.debounced_analysis().wait_for_reindex(file_id, token.clone()).await;
    }

    let diagnostics = context
        .file_diagnostic()
        .pull_file_diagnostics(uri, token)
        .await;

    DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: None,
            items: diagnostics,
        },
    })
    .into()
}
