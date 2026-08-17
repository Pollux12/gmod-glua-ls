use lsp_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
    RelatedUnchangedDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport, Uri,
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

/// Answer without touching what the client already shows: `unchanged` if it
/// has an id, else replay the last report. An empty full report means "clean"
/// to the client and must never stand in for "not ready yet".
async fn keep_client_state(
    context: &ServerContextSnapshot,
    uri: &Uri,
    previous_result_id: Option<String>,
) -> DocumentDiagnosticReportResult {
    if let Some(result_id) = previous_result_id {
        return unchanged_report(result_id);
    }

    if let Some(items) = context.file_diagnostic().cached_file_diagnostics(uri).await {
        let result_id = diagnostic_result_id(&items);
        return full_report(Some(result_id), items);
    }

    full_report(None, Vec::new())
}

pub async fn on_pull_document_diagnostic(
    context: ServerContextSnapshot,
    params: DocumentDiagnosticParams,
    token: CancellationToken,
) -> DocumentDiagnosticReportResult {
    let uri = params.text_document.uri;
    let previous_result_id = params.previous_result_id;

    // Correctness, not latency: the index stays stale between didChange and
    // the debounced reindex, and diagnostics computed then are wrong.
    if !context
        .debounced_analysis()
        .wait_until_fresh_for(&token, "textDocument/diagnostic")
        .await
    {
        return keep_client_state(&context, &uri, previous_result_id).await;
    }

    let Some(diagnostics) = context
        .file_diagnostic()
        .pull_file_diagnostics(uri.clone(), token.clone())
        .await
    else {
        return if token.is_cancelled() || !context.file_diagnostic().is_workspace_loaded() {
            keep_client_state(&context, &uri, previous_result_id).await
        } else {
            // Not in the index: genuinely no diagnostics.
            full_report(None, Vec::new())
        };
    };

    let result_id = diagnostic_result_id(&diagnostics);
    if previous_result_id.as_deref() == Some(result_id.as_str()) {
        return unchanged_report(result_id);
    }

    // Cache for `keep_client_state` replay — but not for a closed document,
    // whose final pull would re-insert the entry `didClose` just dropped.
    if !context.is_document_closed(&uri).await {
        context
            .file_diagnostic()
            .cache_fresh_file_diagnostics(&uri, &diagnostics)
            .await;
    }

    full_report(Some(result_id), diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ServerContext;
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::{ClientCapabilities, DiagnosticSeverity, Range};
    use std::str::FromStr;

    fn diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            message: message.to_string(),
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            ..Default::default()
        }
    }

    fn as_empty_full_report(result: &DocumentDiagnosticReportResult) -> bool {
        let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) = result
        else {
            return false;
        };
        report.full_document_diagnostic_report.items.is_empty()
    }

    #[gtest]
    fn id_less_pull_replays_the_last_report_instead_of_claiming_clean() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();

        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///test.lua").unwrap();
            let items = vec![diagnostic("undefined global")];

            snapshot
                .file_diagnostic()
                .cache_fresh_file_diagnostics(&uri, &items)
                .await;

            let result = keep_client_state(&snapshot, &uri, None).await;

            verify_that!(as_empty_full_report(&result), eq(false))?;
            let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) =
                &result
            else {
                return fail!("expected a full report replaying the cached diagnostics");
            };
            verify_that!(report.full_document_diagnostic_report.items.len(), eq(1))?;
            verify_that!(
                report.full_document_diagnostic_report.result_id.is_some(),
                eq(true)
            )?;
            Ok(())
        })
    }

    #[gtest]
    fn pull_with_a_result_id_answers_unchanged() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();

        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///test.lua").unwrap();

            let result = keep_client_state(&snapshot, &uri, Some("abc".to_string())).await;

            verify_that!(
                matches!(
                    result,
                    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(_))
                ),
                eq(true)
            )?;
            Ok(())
        })
    }

    #[gtest]
    fn unseen_document_may_still_report_empty() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();

        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///never-seen.lua").unwrap();

            let result = keep_client_state(&snapshot, &uri, None).await;

            verify_that!(as_empty_full_report(&result), eq(true))?;
            Ok(())
        })
    }
}
