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

use crate::context::{ServerContext, WorkspaceDiagnosticLevel};

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
            snapshot
                .note_document_seen_version(&uri, params.text_document.version)
                .await;
            // Keep stale-aware UI requests alive so they can wait for fresh
            // data instead of flickering while typing.
            //
            // Diagnostics are exempt for a stronger reason than flicker. VS
            // Code pulls again on every didChange and cancels its own in-flight
            // pull to do it; whatever we answer a cancelled pull with — success
            // or error — the client rewrites to an empty *full* report and
            // clears the file. Cancelling here only guarantees that response
            // arrives, and it discards a handler built to wait for fresh data
            // and answer properly. Upstream never self-cancels any request.
            //
            // `workspace/executeCommand` is exempt for a different reason: it
            // mutates (auto-require issues a `workspace/applyEdit`) and it now
            // waits for a fresh index before running. Cancelling it mid-wait
            // would drop the user's command with no visible error — and an edit
            // landing in that window is routine, since the command's own applied
            // edit or a format-on-save produces one.
            server_context
                .cancel_all_requests_except(&[
                    "textDocument/codeLens",
                    "textDocument/inlayHint",
                    "textDocument/diagnostic",
                    "workspace/diagnostic",
                    "workspace/executeCommand",
                ])
                .await;
            // Mark analysis dirty BEFORE handing the update to the coalescer so
            // follow-up requests see the stale state immediately.
            let in_flight = snapshot.debounced_analysis_arc().begin_in_flight_change();
            server_context
                .did_change_coalescer()
                .enqueue(params, in_flight);
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
            snapshot
                .note_document_seen_version(&uri, params.text_document.version)
                .await;
            {
                let mut workspace = snapshot.workspace_manager().write().await;
                workspace.current_open_files.insert(uri.clone());
                workspace.update_workspace_version(WorkspaceDiagnosticLevel::Fast, true);
            }
            server_context.cancel_text_requests_for_uri(&uri).await;
            // The in-flight workspace sweep is deliberately left to finish.
            // Cancelling it restarts a whole-workspace scan from the beginning
            // with no resume point, and VS Code re-pulls every 2s — so opening
            // files faster than a large sweep completes used to livelock on
            // partial scans. The level bump above already schedules the next
            // sweep, and the client ignores workspace results for URIs it
            // tracks by document pull, so the open file loses nothing.
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
            snapshot.mark_document_closed(&uri).await;
            {
                let mut workspace = snapshot.workspace_manager().write().await;
                workspace.current_open_files.remove(&uri);
                workspace.update_workspace_version(WorkspaceDiagnosticLevel::Fast, true);
            }
            server_context.cancel_text_requests_for_uri(&uri).await;
            // The in-flight workspace sweep is deliberately left to finish.
            // Cancelling it restarts a whole-workspace scan from the beginning
            // with no resume point, and VS Code re-pulls every 2s — so opening
            // files faster than a large sweep completes used to livelock on
            // partial scans. The level bump above already schedules the next
            // sweep, and the client ignores workspace results for URIs it
            // tracks by document pull, so the open file loses nothing.
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

    dispatch_notification!(notification, server_context, {
        sync: {
            // Intentionally empty - async to keep the message for $/cancelRequest processing.
        }
        async: {
            DidSaveTextDocument => on_did_save_text_document,
            DidChangeWatchedFiles => on_did_change_watched_files,
            SetTrace => on_set_trace,
            DidChangeConfiguration => on_did_change_configuration,
            DidChangeWorkspaceFolders => on_did_change_workspace_folders,
            DidRenameFiles => on_did_rename_files_handler,
        }
    });

    Ok(())
}

async fn handle_cancel(server_context: &mut ServerContext, params: CancelParams) {
    let req_id = match params.id {
        NumberOrString::Number(i) => i.into(),
        NumberOrString::String(s) => s.into(),
    };

    server_context.cancel(req_id).await;
}
