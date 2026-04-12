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
    request::{Request as LspRequest, WorkspaceDiagnosticRequest},
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
            if snapshot.lsp_features().supports_workspace_diagnostic() {
                let workspace = snapshot.workspace_manager().read().await;
                workspace.update_workspace_version(WorkspaceDiagnosticLevel::Fast, true);
            }
            // Keep inlay hint requests alive so they can wait for fresh data
            // instead of being cancelled and causing visible flicker.
            server_context
                .cancel_all_requests_except(&["textDocument/inlayHint"])
                .await;
            // Mark analysis dirty BEFORE handing the update to the coalescer so
            // follow-up requests see the stale state immediately.
            snapshot.debounced_analysis().mark_dirty();
            server_context.did_change_coalescer().enqueue(params);
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
            server_context
                .cancel_requests_by_method(WorkspaceDiagnosticRequest::METHOD)
                .await;
            snapshot.debounced_analysis().mark_dirty();
            let task_snapshot = snapshot.clone();
            tokio::spawn(async move {
                on_did_open_text_document(task_snapshot.clone(), params).await;
                task_snapshot
                    .debounced_analysis()
                    .finish_in_flight_changes(1)
                    .await;
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
            server_context
                .cancel_requests_by_method(WorkspaceDiagnosticRequest::METHOD)
                .await;
            snapshot.debounced_analysis().mark_dirty();
            let task_snapshot = snapshot.clone();
            tokio::spawn(async move {
                on_did_close_document(task_snapshot.clone(), params).await;
                task_snapshot
                    .debounced_analysis()
                    .finish_in_flight_changes(1)
                    .await;
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
