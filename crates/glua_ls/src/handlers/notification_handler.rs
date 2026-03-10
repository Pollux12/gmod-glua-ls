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

use crate::context::ServerContext;

use super::{
    configuration::on_did_change_configuration,
    text_document::{
        on_did_change_text_document, on_did_change_watched_files, on_did_close_document,
        on_did_open_text_document, on_did_save_text_document, on_set_trace,
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
    // When document content changes, proactively cancel ALL in-flight requests.
    // They are working on stale data — the editor will resend fresh ones.
    // This breaks the RwLock convoy: stale readers bail out of their
    // `tokio::select!` and release (or never acquire) the read lock, allowing
    // the didChange write to proceed immediately.
    if notification.method == <DidChangeTextDocument as LspNotification>::METHOD {
        server_context.cancel_all_requests().await;
        // Mark analysis dirty BEFORE spawning the didChange handler so that
        // any request handler dispatched next sees the flag and waits for
        // reindex instead of computing on stale data (which causes flickering
        // semantic tokens / inlay hints).
        server_context.snapshot().debounced_analysis().mark_dirty();
    }

    dispatch_notification!(notification, server_context, {
        sync: {
            // Intentionally empty - async to keep the message for $/cancelRequest processing.
        }
        async: {
            DidChangeTextDocument => on_did_change_text_document,
            DidOpenTextDocument => on_did_open_text_document,
            DidSaveTextDocument => on_did_save_text_document,
            DidCloseTextDocument => on_did_close_document,
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
