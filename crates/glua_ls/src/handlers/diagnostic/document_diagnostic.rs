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

/// Answer without touching what the client already shows.
///
/// The client applies a `full` report by replacing its whole set for the URI,
/// so an empty one asserts "this file is clean". That must never stand in for
/// "I don't know yet": the client drops its `resultId` whenever it rewrites a
/// response, and answering the next id-less pull with an empty full report
/// re-clears the file and keeps the id dropped — a blank that sustains itself
/// until the analysis happens to go fresh.
///
/// So: prefer `unchanged`, the one kind the client applies without touching its
/// collection. Failing that, replay the last full report we sent. Only claim
/// "clean" for a document we have never had diagnostics for, where the client
/// is displaying nothing anyway.
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

    // This wait is a correctness requirement, not a latency knob. `didChange`
    // applies the new text and syntax tree to the VFS but deliberately leaves
    // the index alone until the debounced `reindex_files` runs — see
    // `update_file_text_only`: "the index remains stale but functional". In
    // that window the index still describes the *previous* tree, so a semantic
    // model built over the new one resolves almost nothing and the file fills
    // with undefined-global errors that clear a moment later.
    //
    // Answering late is safe; answering early is not. Until this resolves the
    // client keeps the diagnostics it has and moves their ranges with the edits
    // itself.
    //
    // On cancellation the value built below never reaches the wire —
    // `keep_stale_editor_data_on_cancel` deliberately excludes this method, so
    // the dispatcher discards it and sends `ServerCancelled`. It is a fallback
    // for that path and the live answer for the `!is_workspace_loaded()` one.
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
            // The file is genuinely not in the index, so it has no
            // diagnostics — reporting `unchanged` here would strand whatever
            // the client is still showing for it.
            full_report(None, Vec::new())
        };
    };

    let result_id = diagnostic_result_id(&diagnostics);
    if previous_result_id.as_deref() == Some(result_id.as_str()) {
        return unchanged_report(result_id);
    }

    // Remember the report so `keep_client_state` has something truthful to
    // replay when the client comes back without a result id. Only a changed
    // set reaches here, so this costs one clone per actual change rather than
    // one per request.
    //
    // Skip it once the document is closed. Because we advertise
    // `workspace_diagnostics`, the client issues one final document pull after
    // `didClose`; caching its result would re-insert the entry that
    // `on_did_close_document` just dropped and leave it there for good.
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

    /// The loop that turns a one-frame flicker into a file that stays blank:
    /// the client drops its result id, comes back without one, and an empty
    /// full report re-clears the file and keeps the id dropped.
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

    /// With an id in hand, `unchanged` is the only kind the client applies
    /// without replacing its set.
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

    /// A document we have never produced diagnostics for is the one case where
    /// an empty full report is an accurate statement rather than a guess.
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
