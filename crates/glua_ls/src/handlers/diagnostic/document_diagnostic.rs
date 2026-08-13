use lsp_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
    RelatedUnchangedDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport,
};
use tokio_util::sync::CancellationToken;

use super::diagnostic_result_id;
use crate::context::ServerContextSnapshot;

fn full_report(
    result_id: Option<String>,
    items: Vec<Diagnostic>,
) -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
        full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id, items },
    })
    .into()
}

fn unchanged_report(result_id: String) -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
        related_documents: None,
        unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id },
    })
    .into()
}

/// Answer without touching what the client already shows.
///
/// `unchanged` is only legal once the client has an id to compare against, so
/// without one the honest answer is an empty set — which only happens for a
/// document we have no diagnostics for.
fn keep_client_state(previous_result_id: Option<String>) -> DocumentDiagnosticReportResult {
    match previous_result_id {
        Some(result_id) => unchanged_report(result_id),
        None => full_report(None, Vec::new()),
    }
}

pub async fn on_pull_document_diagnostic(
    context: ServerContextSnapshot,
    params: DocumentDiagnosticParams,
    token: CancellationToken,
) -> DocumentDiagnosticReportResult {
    let uri = params.text_document.uri;
    let previous_result_id = params.previous_result_id;

    // LSP 3.17: "The server must compute document diagnostics against the
    // currently synchronized document version." So wait for the reindex
    // rather than answering from an older state — a full report replaces
    // everything the client shows, so a stale one is a visible repaint, not a
    // harmless approximation.
    //
    // Waiting is safe: until this request resolves the client keeps the
    // diagnostics it has and moves their ranges along with the edits itself.
    if !context
        .debounced_analysis()
        .wait_until_fresh_for(&token, "textDocument/diagnostic")
        .await
    {
        // Cancelled. The dispatcher turns this into RequestCancelled, which
        // the client reschedules without clearing; this value is only a
        // fallback if it ever reaches the wire.
        return keep_client_state(previous_result_id);
    }

    let Some(diagnostics) = context
        .file_diagnostic()
        .pull_file_diagnostics(uri.clone(), token.clone())
        .await
    else {
        return if token.is_cancelled() {
            keep_client_state(previous_result_id)
        } else {
            // The file is not in the index, so it genuinely has no
            // diagnostics — reporting `unchanged` here would strand whatever
            // the client is still showing for it.
            full_report(None, Vec::new())
        };
    };

    context
        .file_diagnostic()
        .cache_fresh_file_diagnostics(&uri, &diagnostics)
        .await;

    let result_id = diagnostic_result_id(&diagnostics);
    if previous_result_id.as_deref() == Some(result_id.as_str()) {
        return unchanged_report(result_id);
    }

    full_report(Some(result_id), diagnostics)
}
