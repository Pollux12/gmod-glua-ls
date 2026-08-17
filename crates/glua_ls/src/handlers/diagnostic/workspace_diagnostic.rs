use std::collections::{HashMap, HashSet};

use glua_code_analysis::FileId;
use lsp_types::{
    Diagnostic, FullDocumentDiagnosticReport, PreviousResultId, UnchangedDocumentDiagnosticReport,
    Uri, WorkspaceDiagnosticParams, WorkspaceDiagnosticReport,
    WorkspaceFullDocumentDiagnosticReport, WorkspaceUnchangedDocumentDiagnosticReport,
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
        // Cancellation — return empty items rather than stale data,
        // since workspace diagnostics replace per-URI state and
        // returning stale could mask real issues. The client will
        // re-pull after the next refresh signal.
        return WorkspaceDiagnosticReport { items: vec![] };
    }

    let Some(workspace_manager) = context.read_workspace_manager(&token).await else {
        return WorkspaceDiagnosticReport { items: vec![] };
    };
    let status = workspace_manager.get_workspace_diagnostic_level();
    if status == WorkspaceDiagnosticLevel::None {
        return WorkspaceDiagnosticReport { items: vec![] };
    }
    let client_id = workspace_manager.client_config.client_id;
    let open_files = workspace_manager.current_open_files.clone();
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
    let analysis = context.analysis().read().await;
    let vfs = analysis.compilation.get_db().get_vfs();
    build_report(
        file_diagnostics,
        params.previous_result_ids,
        &open_files,
        |uri| vfs.get_file_id(uri),
        |file_id| vfs.get_file_version(&file_id),
    )
}

/// Builds the report, matching client-supplied URIs against the server's own by
/// `FileId` rather than by URI.
///
/// `Uri` compares as a raw string, and the two sides spell the same file
/// differently: the server emits `file:///C:/...` while VS Code sends back
/// `file:///c%3A/...`. Keying on the URI therefore never matches on Windows, so
/// every pull would repaint the whole workspace and no open file would carry a
/// version. `FileId` resolution normalises through `uri_to_file_path`.
///
/// A URI that resolves to no `FileId` is dropped: an unknown file has no cached
/// result to be unchanged against.
fn build_report(
    file_diagnostics: Vec<(Uri, Vec<Diagnostic>)>,
    previous_result_ids: Vec<PreviousResultId>,
    open_files: &HashSet<Uri>,
    resolve: impl Fn(&Uri) -> Option<FileId>,
    version_of: impl Fn(FileId) -> Option<i32>,
) -> WorkspaceDiagnosticReport {
    let mut previous_result_ids: HashMap<FileId, String> = previous_result_ids
        .into_iter()
        .filter_map(|previous| Some((resolve(&previous.uri)?, previous.value)))
        .collect();
    let open_file_ids: HashSet<FileId> = open_files.iter().filter_map(&resolve).collect();

    WorkspaceDiagnosticReport {
        items: file_diagnostics
            .into_iter()
            .map(|(uri, diagnostics)| {
                let file_id = resolve(&uri);
                let version = file_id
                    .filter(|file_id| open_file_ids.contains(file_id))
                    .and_then(&version_of)
                    .map(i64::from);
                let result_id = diagnostic_result_id(&diagnostics);
                let previous = file_id.and_then(|file_id| previous_result_ids.remove(&file_id));

                // A full report replaces the client's diagnostics for that URI,
                // so report `unchanged` whenever the set still matches the id
                // the client holds.
                if previous.as_ref() == Some(&result_id) {
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use glua_code_analysis::{Emmyrc, Vfs};
    use lsp_types::WorkspaceDocumentDiagnosticReport;

    use super::*;

    /// VS Code echoes `previousResultIds` back in its own URI spelling, which
    /// on Windows differs from the one the server emitted.
    #[cfg(windows)]
    #[test]
    fn unchanged_fires_for_the_client_uri_spelling() {
        let mut vfs = Vfs::new();
        vfs.update_config(Emmyrc::default().into());

        let server_uri = Uri::from_str("file:///C:/Source/addon/lua/autorun/init.lua").unwrap();
        let client_uri = Uri::from_str("file:///c%3A/Source/addon/lua/autorun/init.lua").unwrap();
        vfs.file_id(&server_uri);

        let diagnostics = vec![Diagnostic {
            message: "unused local".to_string(),
            ..Default::default()
        }];
        let result_id = diagnostic_result_id(&diagnostics);

        let report = build_report(
            vec![(server_uri, diagnostics)],
            vec![PreviousResultId {
                uri: client_uri,
                value: result_id.clone(),
            }],
            &HashSet::new(),
            |uri| vfs.get_file_id(uri),
            |_| None,
        );

        match &report.items[..] {
            [WorkspaceDocumentDiagnosticReport::Unchanged(unchanged)] => assert_eq!(
                unchanged.unchanged_document_diagnostic_report.result_id,
                result_id
            ),
            other => panic!("expected a single unchanged report, got {other:?}"),
        }
    }

    #[test]
    fn changed_result_id_reports_full() {
        let mut vfs = Vfs::new();
        vfs.update_config(Emmyrc::default().into());

        let uri = Uri::from_str("file:///C:/Source/addon/lua/autorun/init.lua").unwrap();
        vfs.file_id(&uri);

        let report = build_report(
            vec![(uri.clone(), vec![Diagnostic::default()])],
            vec![PreviousResultId {
                uri,
                value: "stale".to_string(),
            }],
            &HashSet::new(),
            |uri| vfs.get_file_id(uri),
            |_| None,
        );

        assert!(matches!(
            report.items[..],
            [WorkspaceDocumentDiagnosticReport::Full(_)]
        ));
    }
}
