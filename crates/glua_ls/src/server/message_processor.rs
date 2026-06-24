use lsp_server::Message;
use std::error::Error;
use tokio::sync::oneshot;

use crate::context;
use crate::handlers::{on_notification_handler, on_request_handler, on_response_handler};

use super::connection::AsyncConnection;
use super::main_loop::InitResult;

/// Outcome of server initialization observed by the message loop.
///
/// `Ready` means initialization succeeded and normal message processing may
/// continue. `Failed(reason)` means the spawned `initialized_handler` returned
/// an error (currently: GMod annotations validation); the server loop should
/// surface the reason (the handler already sent a `window/showMessage`) and
/// abort. `Closed` means the init task ended without sending a result (e.g.
/// it panicked or was dropped).
pub(super) enum InitOutcome {
    Ready,
    Failed(String),
    Closed,
}

/// Server initialization and message processing state
pub(super) struct ServerMessageProcessor {
    initialization_complete: bool,
    pub(super) pending_messages: Vec<Message>,
    pub(super) init_rx: oneshot::Receiver<InitResult>,
}

impl ServerMessageProcessor {
    pub(super) fn new(init_rx: oneshot::Receiver<InitResult>) -> Self {
        Self {
            initialization_complete: false,
            pending_messages: Vec::new(),
            init_rx,
        }
    }

    /// Check if message can be processed during initialization
    pub(super) fn can_process_during_init(&self, msg: &Message) -> bool {
        match msg {
            // Allow all responses (including configuration responses)
            Message::Response(_) => true,
            // Allow specific notifications
            Message::Notification(notify) => {
                matches!(notify.method.as_str(), "$/cancelRequest" | "initialized")
            }
            // Don't process other requests during initialization
            Message::Request(_) => false,
        }
    }

    /// Process message during normal operation (after initialization)
    pub(super) async fn process_message(
        &mut self,
        msg: Message,
        connection: &mut AsyncConnection,
        server_context: &mut context::ServerContext,
    ) -> Result<bool, Box<dyn Error + Sync + Send>> {
        // During normal operation, process all messages
        self.handle_message(msg, connection, server_context).await
    }

    /// Poll the initialization task. Returns `Some(outcome)` once the init
    /// task has reported (or its channel closed), at which point
    /// `initialization_complete` is latched and the caller should act on the
    /// outcome. Returns `None` while initialization is still in progress.
    pub(super) fn poll_initialization(&mut self) -> Option<InitOutcome> {
        if self.initialization_complete {
            return None;
        }
        match self.init_rx.try_recv() {
            Ok(Ok(())) => {
                self.initialization_complete = true;
                Some(InitOutcome::Ready)
            }
            Ok(Err(reason)) => {
                self.initialization_complete = true;
                Some(InitOutcome::Failed(reason))
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                // Initialization task closed unexpectedly
                self.initialization_complete = true;
                Some(InitOutcome::Closed)
            }
        }
    }

    /// Process all pending messages after initialization
    pub(super) async fn process_pending_messages(
        &mut self,
        connection: &mut AsyncConnection,
        server_context: &mut context::ServerContext,
    ) -> Result<bool, Box<dyn Error + Sync + Send>> {
        let messages = std::mem::take(&mut self.pending_messages);
        for msg in messages {
            if self.handle_message(msg, connection, server_context).await? {
                return Ok(true); // Shutdown requested
            }
        }
        Ok(false)
    }

    /// Handle individual message
    pub(super) async fn handle_message(
        &self,
        msg: Message,
        connection: &mut AsyncConnection,
        server_context: &mut context::ServerContext,
    ) -> Result<bool, Box<dyn Error + Sync + Send>> {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req).await? {
                    server_context.close().await;
                    return Ok(true); // Shutdown requested
                }
                on_request_handler(req, server_context).await?;
            }
            Message::Notification(notify) => {
                on_notification_handler(notify, server_context).await?;
            }
            Message::Response(response) => {
                on_response_handler(response, server_context).await?;
            }
        }
        Ok(false)
    }
}
