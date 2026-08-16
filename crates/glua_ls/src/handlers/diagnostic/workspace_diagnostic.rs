use std::collections::HashMap;

use lsp_types::{
    FullDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport, WorkspaceDiagnosticParams,
    WorkspaceDiagnosticReport, WorkspaceFullDocumentDiagnosticReport,
    WorkspaceUnchangedDocumentDiagnosticReport,
};
use tokio_util::sync::CancellationToken;

use super::diagnostic_result_id;
use crate::context::{ServerContextSnapshot, WorkspaceDiagnosticLevel};

pub async fn on_pull_workspace_diagnostic(
    context: ServerContextSnapshot,
    params: WorkspaceDiagnosticParams,
    token: CancellationToken,
) -> WorkspaceDiagnosticReport {
    // Wait for any pending/in-flight document changes to finish before diagnosing.
    if !context
        .debounced_analysis()
        .wait_until_fresh_for(&token, "workspace/diagnostic")
        .await
    {
        // Cancelled. Return an empty workspace report indicating no files
        // changed in this chunk, so the client retains its current per-URI state.
        return WorkspaceDiagnosticReport { items: vec![] };
    }

    let Some(workspace_manager) = context.read_workspace_manager(&token).await else {
        return WorkspaceDiagnosticReport { items: vec![] };
    };
    // Claim the pending level atomically — a load followed by a separate store
    // is not serialised by the read guard we hold here.
    let status = workspace_manager.claim_workspace_diagnostic_level();
    if status == WorkspaceDiagnosticLevel::None {
        return WorkspaceDiagnosticReport { items: vec![] };
    }
    let open_files = workspace_manager.current_open_files.clone();
    drop(workspace_manager);

    // let emmyrc = context.analysis().read().await.get_emmyrc();
    let file_diagnostics = match status {
        WorkspaceDiagnosticLevel::None => Vec::new(),
        WorkspaceDiagnosticLevel::Fast => {
            context
                .file_diagnostic()
                .pull_workspace_diagnostics_fast(token.clone())
                .await
        }
        WorkspaceDiagnosticLevel::Slow => {
            context
                .file_diagnostic()
                .pull_workspace_diagnostics_slow(token.clone())
                .await
        }
    };

    // The sweep was cut short, so the set above covers only part of the
    // workspace. The level it claimed is already cleared, so put it back —
    // otherwise the files this pass never reached stay stale until an unrelated
    // edit happens to re-arm one.
    if token.is_cancelled() {
        let workspace_manager = context.workspace_manager().read().await;
        workspace_manager.restore_workspace_diagnostic_level(status);
    }
    let open_file_versions = {
        let analysis = context.analysis().read().await;
        file_diagnostics
            .iter()
            .filter_map(|(uri, _)| {
                if !open_files.contains(uri) {
                    return None;
                }

                let version = analysis.get_file_id(uri).and_then(|file_id| {
                    analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_version(&file_id)
                });
                Some((uri.clone(), version))
            })
            .collect::<HashMap<_, _>>()
    };

    // A full report replaces the client's diagnostics for that URI, so report
    // `unchanged` for every file whose set still matches the id the client
    // holds. Without this every workspace pull repaints the whole workspace.
    let previous_result_ids: HashMap<_, _> = params
        .previous_result_ids
        .into_iter()
        .map(|previous| (previous.uri, previous.value))
        .collect();

    WorkspaceDiagnosticReport {
        items: file_diagnostics
            .into_iter()
            .map(|(uri, diagnostics)| {
                let version = open_file_versions
                    .get(&uri)
                    .copied()
                    .flatten()
                    .map(i64::from);
                let result_id = diagnostic_result_id(&diagnostics);

                if previous_result_ids.get(&uri) == Some(&result_id) {
                    return WorkspaceUnchangedDocumentDiagnosticReport {
                        version,
                        uri,
                        unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                            result_id,
                        },
                    }
                    .into();
                }

                WorkspaceFullDocumentDiagnosticReport {
                    version,
                    uri,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        items: diagnostics,
                        result_id: Some(result_id),
                    },
                }
                .into()
            })
            .collect(),
    }
}
