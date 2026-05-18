use log::warn;
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

    // When analysis is dirty (edits pending reindex), return cached
    // diagnostics immediately. This prevents flickering: returning empty
    // diagnostics or an error causes VS Code to clear ALL existing
    // diagnostics for the file. We must return the best available data
    // so the editor never shows a gap between old and new diagnostics.
    //
    // Note: cached diagnostics CAN be empty — that's valid if the previous
    // computation genuinely found zero diagnostics. We only avoid returning
    // empty as a *fallback* when we don't have computed data yet.
    if context.debounced_analysis().is_dirty() {
        if let Some(cached) = context
            .file_diagnostic()
            .cached_display_diagnostics(&uri)
            .await
        {
            return full_document_diagnostic_report(cached);
        }
        // No cached diagnostics yet (e.g. first edit on a newly opened file).
        // Fall through to wait for fresh analysis rather than returning empty.
    }

    // Wait for fresh analysis data before computing diagnostics.
    if !context.debounced_analysis().wait_until_fresh(&token).await {
        // Cancellation — return cached diagnostics if available.
        // If no cache exists, return empty (we have no data to show).
        let cached = context
            .file_diagnostic()
            .cached_display_diagnostics(&uri)
            .await
            .unwrap_or_default();
        if cached.is_empty() {
            warn!(
                "pull diagnostic cancelled with no cached diagnostics for {:?}",
                uri
            );
        }
        return full_document_diagnostic_report(cached);
    }

    let diagnostics = match context
        .file_diagnostic()
        .pull_file_diagnostics(uri.clone(), token)
        .await
    {
        Some(diagnostics) => {
            // Fresh computation succeeded — return the result regardless of
            // whether it's empty. An empty vec is valid: it means the file
            // genuinely has no diagnostics.
            context
                .file_diagnostic()
                .cache_fresh_file_diagnostics(&uri, &diagnostics)
                .await;
            diagnostics
        }
        // pull_file_diagnostics returned None (file not found or cancelled).
        // Return cached diagnostics if available. If no cache, return empty
        // since the file doesn't exist in the index.
        None => context
            .file_diagnostic()
            .cached_display_diagnostics(&uri)
            .await
            .unwrap_or_default(),
    };

    full_document_diagnostic_report(diagnostics)
}
