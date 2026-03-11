use lsp_types::{
    FullDocumentDiagnosticReport, WorkspaceDiagnosticParams, WorkspaceDiagnosticReport,
    WorkspaceFullDocumentDiagnosticReport,
};
use tokio_util::sync::CancellationToken;

use crate::context::{ServerContextSnapshot, WorkspaceDiagnosticLevel};

pub async fn on_pull_workspace_diagnostic(
    context: ServerContextSnapshot,
    _: WorkspaceDiagnosticParams,
    token: CancellationToken,
) -> WorkspaceDiagnosticReport {
    // Wait for any pending debounced reindex to finish before diagnosing
    context
        .debounced_analysis()
        .wait_for_all_reindex(token.clone())
        .await;

    let Some(workspace_manager) = context.read_workspace_manager(&token).await else {
        return WorkspaceDiagnosticReport { items: vec![] };
    };
    let status = workspace_manager.get_workspace_diagnostic_level();
    if status == WorkspaceDiagnosticLevel::None {
        return WorkspaceDiagnosticReport { items: vec![] };
    }
    let version = workspace_manager.get_workspace_version();
    let client_id = workspace_manager.client_config.client_id;
    workspace_manager.update_workspace_version(WorkspaceDiagnosticLevel::None, false);
    drop(workspace_manager);

    if client_id.is_vscode() && context.lsp_features().supports_refresh_diagnostic() {
        context.client().refresh_workspace_diagnostics();
    }

    // let emmyrc = context.analysis().read().await.get_emmyrc();
    let file_diagnostics = match status {
        WorkspaceDiagnosticLevel::None => Vec::new(),
        WorkspaceDiagnosticLevel::Fast => {
            context
                .file_diagnostic()
                .pull_workspace_diagnostics_fast(token)
                .await
        }
        WorkspaceDiagnosticLevel::Slow => {
            context
                .file_diagnostic()
                .pull_workspace_diagnostics_slow(token)
                .await
        }
    };

    WorkspaceDiagnosticReport {
        items: file_diagnostics
            .into_iter()
            .map(|(uri, diagnostics)| {
                WorkspaceFullDocumentDiagnosticReport {
                    uri,
                    version: Some(version),
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        items: diagnostics,
                        result_id: None,
                    },
                }
                .into()
            })
            .collect(),
    }
}
