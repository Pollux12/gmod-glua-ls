use std::error::Error;

use log::warn;
use lsp_server::Notification;
use lsp_types::{
    CancelParams, NumberOrString,
    notification::{
        Cancel, DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles,
        DidChangeWorkspaceFolders, DidCloseTextDocument, DidOpenTextDocument, DidRenameFiles,
        DidSaveTextDocument, Notification as LspNotification, SetTrace,
    },
};

use crate::context::{ServerContext, ServerContextSnapshot};

use super::{
    configuration::on_did_change_configuration,
    text_document::{
        on_did_change_watched_files, on_did_close_document, on_did_open_text_document,
        on_did_save_text_document, on_set_trace,
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
            let uri = params.text_document.uri.clone();
            let snapshot = server_context.snapshot();
            snapshot.note_document_seen_version(&uri, params.text_document.version);
            // Exempted requests wait for fresh data instead of being
            // cancelled: a cancelled executeCommand drops the user's command.
            server_context
                .cancel_all_requests_except(&[
                    "textDocument/codeLens",
                    "textDocument/inlayHint",
                    "workspace/executeCommand",
                ])
                .await;
            // Mark analysis dirty BEFORE handing the update to the coalescer so
            // follow-up requests see the stale state immediately.
            let in_flight = snapshot.debounced_analysis_arc().begin_in_flight_change();
            // Profiling: capture the notification receipt (version + seq +
            // clock) so the coalescer worker can report `coalescer_wait_ms`
            // and `total_ms` spanning notification through schedule. `None`
            // when unset: zero clock reads, zero output.
            let enqueue_profile =
                crate::util::ls_profile::DidChangeEnqueue::capture(params.text_document.version);
            server_context
                .did_change_coalescer()
                .enqueue(params, in_flight, enqueue_profile);
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
            let snapshot = server_context.snapshot();
            snapshot.note_document_seen_version(&uri, params.text_document.version);
            {
                let mut workspace = snapshot.workspace_manager().write().await;
                workspace.current_open_files.insert(uri.clone());
            }
            server_context.cancel_text_requests_for_uri(&uri).await;
            let in_flight = snapshot.debounced_analysis_arc().begin_in_flight_change();
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
            let snapshot = server_context.snapshot();
            snapshot.mark_document_closed(&uri);
            {
                let mut workspace = snapshot.workspace_manager().write().await;
                workspace.current_open_files.remove(&uri);
            }
            server_context.cancel_text_requests_for_uri(&uri).await;
            let in_flight = snapshot.debounced_analysis_arc().begin_in_flight_change();
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
            spawn_pending_file_change(server_context, DidChangeWatchedFiles::METHOD, |snapshot| {
                on_did_change_watched_files(snapshot, params)
            });
        }
        return Ok(());
    }

    if notification.method == <DidRenameFiles as LspNotification>::METHOD {
        if let Ok(params) = notification
            .extract::<<DidRenameFiles as LspNotification>::Params>(DidRenameFiles::METHOD)
        {
            spawn_pending_file_change(server_context, DidRenameFiles::METHOD, |snapshot| {
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
/// from the moment it arrives: a diagnostics pull answered before the handler
/// applies it would report the files as they were.
fn spawn_pending_file_change<F>(
    server_context: &ServerContext,
    method: &'static str,
    handler: impl FnOnce(ServerContextSnapshot) -> F + Send + 'static,
) where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let snapshot = server_context.snapshot();
    let in_flight = snapshot.debounced_analysis_arc().begin_in_flight_change();
    tokio::spawn(async move {
        let handle = tokio::spawn(handler(snapshot));
        if let Err(err) = handle.await {
            log::error!("LS_FILE_CHANGE_PANIC {method} handler failed: {err}");
        }
        in_flight.finish().await;
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
        ClientCapabilities, DidChangeWatchedFilesParams, FileChangeType, FileEvent, FileRename,
        RenameFilesParams, Uri, WorkspaceClientCapabilities,
    };

    use super::*;

    /// A watched-file change is pending from the moment it arrives, so a
    /// diagnostics pull waits for it instead of reporting the files as they
    /// were before it.
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

    /// A file rename is pending from the moment it arrives, as a watched-file
    /// change is.
    #[test]
    fn a_file_rename_is_pending_once_received() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let capabilities = ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    apply_edit: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mut context = ServerContext::new(connection, capabilities);
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
