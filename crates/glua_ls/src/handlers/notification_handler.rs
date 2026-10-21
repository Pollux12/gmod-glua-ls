use std::error::Error;

use log::warn;
use lsp_server::Notification;
use lsp_types::{
    CancelParams, MessageType, NumberOrString, ShowMessageParams, VersionedTextDocumentIdentifier,
    notification::{
        Cancel, DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles,
        DidChangeWorkspaceFolders, DidCloseTextDocument, DidOpenTextDocument, DidRenameFiles,
        DidSaveTextDocument, Notification as LspNotification, SetTrace,
    },
};

use crate::context::{ServerContext, ServerContextSnapshot, SyncError};

use super::{
    configuration::on_did_change_configuration,
    text_document::{
        on_did_change_watched_files, on_did_close_document, on_did_open_text_document,
        on_did_save_text_document, on_set_trace, watched_change_scope,
    },
    workspace::{on_did_change_workspace_folders, on_did_rename_files_handler},
};

macro_rules! dispatch_notification {
    ($notification:expr, $context:expr, {
        sync: { $($sync_notif:ty => $sync_handler:expr),* $(,)? }
        async: { $($async_notif:ty => $async_handler:expr),* $(,)? }
    }) => {
        match $notification.method.as_str() {
            Cancel::METHOD => {
                if let Ok(params) = $notification.extract::<CancelParams>(Cancel::METHOD) {
                    handle_cancel($context, params).await;
                }
            }
            $(
                <$sync_notif>::METHOD => {
                    if let Ok(params) = $notification.extract::<<$sync_notif as LspNotification>::Params>(<$sync_notif>::METHOD) {
                        let snapshot = $context.snapshot();
                        $sync_handler(snapshot, params).await;
                    }
                }
            )*
            $(
                <$async_notif>::METHOD => {
                    if let Ok(params) = $notification.extract::<<$async_notif as LspNotification>::Params>(<$async_notif>::METHOD) {
                        let snapshot = $context.snapshot();
                        tokio::spawn(async move {
                            $async_handler(snapshot, params).await;
                        });
                    }
                }
            )*
            method => {
                warn!("Unhandled notification method: {}", method);
            }
        }
    };
}

pub async fn on_notification_handler(
    notification: Notification,
    server_context: &mut ServerContext,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    if notification.method == <DidChangeTextDocument as LspNotification>::METHOD {
        if let Ok(params) = notification
            .extract::<<DidChangeTextDocument as LspNotification>::Params>(
                DidChangeTextDocument::METHOD,
            )
        {
            let document = params.text_document;
            let uri = document.uri.clone();
            let text = match server_context
                .editor_documents()
                .change(&uri, params.content_changes)
            {
                Ok(text) => text,
                Err(err) => {
                    report_sync_error(server_context, &document, err);
                    return Ok(());
                }
            };
            let snapshot = server_context.snapshot();
            snapshot.note_document_seen_version(&uri, document.version);
            // Only this document's requests are superseded. Exempted ones
            // wait for fresh data instead of being cancelled: a cancelled
            // executeCommand drops the user's command.
            server_context
                .cancel_requests_for_uri(
                    &uri,
                    &[
                        "textDocument/codeLens",
                        "textDocument/inlayHint",
                        "workspace/executeCommand",
                    ],
                )
                .await;
            // Mark the document dirty BEFORE handing the update to the
            // coalescer so follow-up requests see the stale state immediately.
            let in_flight = snapshot.debounced_analysis_arc().begin_document_change(uri);
            // Profiling: capture the notification receipt (version + seq +
            // clock) so the coalescer worker can report `coalescer_wait_ms`
            // and `total_ms` spanning notification through schedule. `None`
            // when unset: zero clock reads, zero output.
            let enqueue_profile =
                crate::util::ls_profile::DidChangeEnqueue::capture(document.version);
            server_context.did_change_coalescer().enqueue(
                document,
                text,
                in_flight,
                enqueue_profile,
            );
        }
        return Ok(());
    }

    if notification.method == <DidOpenTextDocument as LspNotification>::METHOD {
        if let Ok(params) = notification
            .extract::<<DidOpenTextDocument as LspNotification>::Params>(
                DidOpenTextDocument::METHOD,
            )
        {
            let uri = params.text_document.uri.clone();
            server_context
                .editor_documents()
                .open(uri.clone(), params.text_document.text.clone());
            let snapshot = server_context.snapshot();
            snapshot.note_document_seen_version(&uri, params.text_document.version);
            {
                let mut workspace = snapshot.workspace_manager().write().await;
                workspace.current_open_files.insert(uri.clone());
            }
            server_context.cancel_requests_for_uri(&uri, &[]).await;
            let in_flight = snapshot
                .debounced_analysis_arc()
                .begin_document_change(uri.clone());
            let task_snapshot = snapshot.clone();
            tokio::spawn(async move {
                let handler_snapshot = task_snapshot.clone();
                let handle = tokio::spawn(async move {
                    on_did_open_text_document(handler_snapshot, params).await;
                });
                if let Err(err) = handle.await {
                    log::error!("LS_DID_OPEN_PANIC didOpen handler failed: {}", err);
                }
                in_flight.finish().await;
            });
        }
        return Ok(());
    }

    if notification.method == <DidCloseTextDocument as LspNotification>::METHOD {
        if let Ok(params) = notification
            .extract::<<DidCloseTextDocument as LspNotification>::Params>(
                DidCloseTextDocument::METHOD,
            )
        {
            let uri = params.text_document.uri.clone();
            server_context.editor_documents().close(&uri);
            let snapshot = server_context.snapshot();
            snapshot.mark_document_closed(&uri);
            {
                let mut workspace = snapshot.workspace_manager().write().await;
                workspace.current_open_files.remove(&uri);
            }
            server_context.cancel_requests_for_uri(&uri, &[]).await;
            let in_flight = snapshot
                .debounced_analysis_arc()
                .begin_document_change(uri.clone());
            let task_snapshot = snapshot.clone();
            tokio::spawn(async move {
                let handler_snapshot = task_snapshot.clone();
                let handle = tokio::spawn(async move {
                    on_did_close_document(handler_snapshot, params).await;
                });
                if let Err(err) = handle.await {
                    log::error!("LS_DID_CLOSE_PANIC didClose handler failed: {}", err);
                }
                in_flight.finish().await;
            });
        }
        return Ok(());
    }

    if notification.method == <DidChangeWatchedFiles as LspNotification>::METHOD {
        if let Ok(params) = notification
            .extract::<<DidChangeWatchedFiles as LspNotification>::Params>(
                DidChangeWatchedFiles::METHOD,
            )
        {
            let scope = watched_change_scope(&params);
            spawn_pending_file_change(
                server_context,
                DidChangeWatchedFiles::METHOD,
                scope,
                |snapshot| on_did_change_watched_files(snapshot, params),
            );
        }
        return Ok(());
    }

    if notification.method == <DidRenameFiles as LspNotification>::METHOD {
        if let Ok(params) = notification
            .extract::<<DidRenameFiles as LspNotification>::Params>(DidRenameFiles::METHOD)
        {
            // A renamed folder moves every file under it.
            spawn_pending_file_change(server_context, DidRenameFiles::METHOD, None, |snapshot| {
                on_did_rename_files_handler(snapshot, params)
            });
        }
        return Ok(());
    }

    dispatch_notification!(notification, server_context, {
        sync: {
            // Intentionally empty - async to keep the message for $/cancelRequest processing.
        }
        async: {
            DidSaveTextDocument => on_did_save_text_document,
            SetTrace => on_set_trace,
            DidChangeConfiguration => on_did_change_configuration,
            DidChangeWorkspaceFolders => on_did_change_workspace_folders,
        }
    });

    Ok(())
}

/// Runs the handler of a change to files on disk with the change pending
/// from the moment it arrives, for the documents in `scope` or for every
/// document when `None`: a request answered before the handler applies it
/// would read the files as they were.
fn spawn_pending_file_change<F>(
    server_context: &ServerContext,
    method: &'static str,
    scope: Option<Vec<lsp_types::Uri>>,
    handler: impl FnOnce(ServerContextSnapshot) -> F + Send + 'static,
) where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let snapshot = server_context.snapshot();
    let debounced_analysis = snapshot.debounced_analysis_arc();
    let in_flight = match scope {
        Some(uris) => debounced_analysis.begin_documents_change(uris),
        None => debounced_analysis.begin_in_flight_change(),
    };
    tokio::spawn(async move {
        let handle = tokio::spawn(handler(snapshot));
        if let Err(err) = handle.await {
            log::error!("LS_FILE_CHANGE_PANIC {method} handler failed: {err}");
        }
        in_flight.finish().await;
    });
}

/// A didChange that cannot be applied leaves the document's text as it was:
/// applying it anyway would corrupt every later edit. The user is told once,
/// when the document falls out of sync.
fn report_sync_error(
    server_context: &ServerContext,
    document: &VersionedTextDocumentIdentifier,
    err: SyncError,
) {
    log::error!(
        "LS_SYNC_REJECTED uri={} version={} didChange not applied: {}",
        document.uri.as_str(),
        document.version,
        err
    );
    if matches!(err, SyncError::OutOfSync | SyncError::NotOpen) {
        return;
    }
    server_context
        .snapshot()
        .client()
        .show_message(ShowMessageParams {
            typ: MessageType::ERROR,
            message: format!(
                "GLuaLS could not apply an edit to {}: {}. Close and reopen the file so its analysis follows your edits again.",
                document.uri.as_str(),
                err
            ),
        });
}

async fn handle_cancel(server_context: &mut ServerContext, params: CancelParams) {
    let req_id = match params.id {
        NumberOrString::Number(i) => i.into(),
        NumberOrString::String(s) => s.into(),
    };

    server_context.cancel(req_id).await;
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lsp_server::Connection;
    use lsp_types::{
        ClientCapabilities, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidOpenTextDocumentParams, FileChangeType, FileEvent, FileRename, Position, Range,
        RenameFilesParams, TextDocumentContentChangeEvent, TextDocumentItem, Uri,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// A watched-file change is pending from the moment it arrives, so a
    /// request waits for it instead of reading the files as they were before
    /// it.
    #[test]
    fn a_watched_file_change_is_pending_once_received() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let mut context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let params = DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Uri::from_str("file:///workspace/gone.lua").unwrap(),
                    typ: FileChangeType::DELETED,
                }],
            };
            let notification = Notification::new(DidChangeWatchedFiles::METHOD.to_string(), params);

            // Held, so the handler cannot apply the change before the check.
            let held = snapshot.analysis().write().await;
            on_notification_handler(notification, &mut context)
                .await
                .expect("the notification is handled");
            let pending = snapshot.debounced_analysis().is_dirty();
            drop(held);

            assert!(pending);
        });
    }

    fn ranged(start: (u32, u32), end: (u32, u32), text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(start.0, start.1),
                Position::new(end.0, end.1),
            )),
            range_length: None,
            text: text.to_string(),
        }
    }

    /// Runs `edits` (one notification each, from version 2) after opening
    /// `opened` over an indexed file, and returns the text the analysis holds
    /// once the latest applied version is reached.
    fn analysis_text_after_edits(
        opened: &str,
        edits: Vec<Vec<TextDocumentContentChangeEvent>>,
    ) -> Option<String> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let mut context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///workspace/lua/autorun/sync.lua").unwrap();
            snapshot
                .analysis()
                .write()
                .await
                .update_file_by_uri(&uri, Some("local a = 1\n".to_string()));

            let open = DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "lua".to_string(),
                    version: 1,
                    text: opened.to_string(),
                },
            };
            on_notification_handler(
                Notification::new(DidOpenTextDocument::METHOD.to_string(), open),
                &mut context,
            )
            .await
            .expect("the open is handled");
            for (version, content_changes) in (2..).zip(edits) {
                let change = DidChangeTextDocumentParams {
                    text_document: lsp_types::VersionedTextDocumentIdentifier {
                        uri: uri.clone(),
                        version,
                    },
                    content_changes,
                };
                on_notification_handler(
                    Notification::new(DidChangeTextDocument::METHOD.to_string(), change),
                    &mut context,
                )
                .await
                .expect("the change is handled");
            }

            let applied = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                snapshot
                    .wait_until_latest_document_version_applied(&uri, &CancellationToken::new()),
            )
            .await;
            assert_eq!(applied.ok(), Some(true), "the latest version is applied");
            let analysis = snapshot.analysis().read().await;
            let file_id = analysis.get_file_id(&uri)?;
            analysis
                .compilation
                .get_db()
                .get_vfs()
                .latest_content(&file_id)
                .cloned()
        })
    }

    /// Ranged changes sent right after the open reach the analysis applied
    /// to the opened text, in the order the editor sent them.
    #[test]
    fn ranged_changes_reach_the_analysis_in_order() {
        let held = analysis_text_after_edits(
            "local a = 2\r\n",
            vec![
                vec![ranged((0, 7), (0, 7), "b")],
                vec![ranged((0, 6), (0, 8), "😀")],
                vec![ranged((1, 0), (1, 0), "x"), ranged((0, 8), (0, 9), "")],
            ],
        );
        assert_eq!(held.as_deref(), Some("local 😀= 2\r\nx"));
    }

    /// A change that cannot be applied, and the ranged changes after it,
    /// leave the analysis on the last good text without stalling requests
    /// that wait for the latest version.
    #[test]
    fn a_bad_change_keeps_the_last_good_text() {
        let held = analysis_text_after_edits(
            "local a = 2\n",
            vec![
                vec![ranged((0, 10), (0, 11), "3")],
                vec![ranged((9, 0), (9, 0), "x")],
                vec![ranged((0, 0), (0, 0), "y")],
            ],
        );
        assert_eq!(held.as_deref(), Some("local a = 3\n"));
    }

    /// A file rename is pending from the moment it arrives, as a watched-file
    /// change is.
    #[test]
    fn a_file_rename_is_pending_once_received() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let mut context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let params = RenameFilesParams {
                files: vec![FileRename {
                    old_uri: "file:///workspace/old.lua".to_string(),
                    new_uri: "file:///workspace/new.lua".to_string(),
                }],
            };
            let notification = Notification::new(DidRenameFiles::METHOD.to_string(), params);

            // Held, so the handler cannot apply the rename before the check.
            let held = snapshot.analysis().write().await;
            on_notification_handler(notification, &mut context)
                .await
                .expect("the notification is handled");
            let pending = snapshot.debounced_analysis().is_dirty();
            drop(held);

            assert!(pending);
        });
    }
}
