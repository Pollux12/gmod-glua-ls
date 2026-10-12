use lsp_server::{ErrorCode, Message, Response};
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
    /// Whether the client sent `shutdown`: requests are refused from then on
    /// and `exit` ends the server cleanly.
    shutdown_requested: bool,
    pub(super) pending_messages: Vec<Message>,
    pub(super) init_rx: oneshot::Receiver<InitResult>,
}

impl ServerMessageProcessor {
    pub(super) fn new(init_rx: oneshot::Receiver<InitResult>) -> Self {
        Self {
            initialization_complete: false,
            shutdown_requested: false,
            pending_messages: Vec::new(),
            init_rx,
        }
    }

    /// Check if message can be processed during initialization: responses,
    /// cancellations, and the shutdown sequence, which does not wait for the
    /// workspace to load. After `shutdown` every message is handled at once.
    pub(super) fn can_process_during_init(&self, msg: &Message) -> bool {
        match msg {
            Message::Response(_) => true,
            _ if self.shutdown_requested => true,
            Message::Notification(notify) => {
                matches!(
                    notify.method.as_str(),
                    "$/cancelRequest" | "initialized" | "exit"
                )
            }
            Message::Request(request) => request.method == "shutdown",
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

    /// Whether the client sent `shutdown` before `exit`, which makes the exit
    /// a clean one.
    pub(super) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
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
                return Ok(true); // Exit requested
            }
        }
        Ok(false)
    }

    /// Handles one message. Returns whether the client asked the server to
    /// exit. After `shutdown`, requests are answered with `InvalidRequest`
    /// and other notifications are dropped, as the protocol has it.
    pub(super) async fn handle_message(
        &mut self,
        msg: Message,
        connection: &mut AsyncConnection,
        server_context: &mut context::ServerContext,
    ) -> Result<bool, Box<dyn Error + Sync + Send>> {
        match msg {
            Message::Notification(notify) if notify.method == "exit" => return Ok(true),
            Message::Request(req) if self.shutdown_requested => {
                connection.send(
                    Response::new_err(
                        req.id,
                        ErrorCode::InvalidRequest as i32,
                        "the server is shutting down".to_owned(),
                    )
                    .into(),
                )?;
            }
            Message::Request(req) if req.method == "shutdown" => {
                self.shutdown_requested = true;
                server_context.close().await;
                // Requests held back while the workspace initialized are
                // refused as any request after `shutdown` is.
                for pending in std::mem::take(&mut self.pending_messages) {
                    if let Message::Request(pending) = pending {
                        connection.send(
                            Response::new_err(
                                pending.id,
                                ErrorCode::InvalidRequest as i32,
                                "the server is shutting down".to_owned(),
                            )
                            .into(),
                        )?;
                    }
                }
                connection.send(Response::new_ok(req.id, ()).into())?;
            }
            Message::Request(req) => {
                on_request_handler(req, server_context).await?;
            }
            Message::Notification(_) | Message::Response(_) if self.shutdown_requested => {}
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

#[cfg(test)]
mod tests {
    use lsp_server::{Connection, Notification, Request, RequestId};

    use super::*;

    fn request(id: i32, method: &str) -> Message {
        Message::Request(Request::new(
            RequestId::from(id),
            method.to_owned(),
            serde_json::Value::Null,
        ))
    }

    fn notification(method: &str) -> Message {
        Message::Notification(Notification::new(
            method.to_owned(),
            serde_json::Value::Null,
        ))
    }

    /// While the workspace initializes, `shutdown` and `exit` are handled at
    /// once, and a request held back until initialization is refused before
    /// `shutdown` is answered.
    #[test]
    fn shutdown_during_initialization_is_answered_at_once() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (handled_at_once, held_back, replies) = runtime.block_on(async {
            let (server, client) = Connection::memory();
            let mut context = context::ServerContext::new(
                Connection {
                    sender: server.sender.clone(),
                    receiver: server.receiver.clone(),
                },
                Default::default(),
            );
            let mut connection = AsyncConnection::from_sync(server);
            let (_init_tx, init_rx) = oneshot::channel();
            let mut processor = ServerMessageProcessor::new(init_rx);
            let held = request(5, "textDocument/definition");
            let held_back = !processor.can_process_during_init(&held);
            processor.pending_messages.push(held);
            let handled_at_once = [request(1, "shutdown"), notification("exit")]
                .iter()
                .all(|msg| processor.can_process_during_init(msg));
            processor
                .handle_message(request(1, "shutdown"), &mut connection, &mut context)
                .await
                .expect("shutdown is handled");
            let replies = client
                .receiver
                .try_iter()
                .filter_map(|msg| match msg {
                    Message::Response(response) => {
                        Some((response.id, response.error.map(|error| error.code)))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            (handled_at_once, held_back, replies)
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(5));

        assert_eq!(
            (handled_at_once, held_back, replies),
            (
                true,
                true,
                vec![
                    (RequestId::from(5), Some(ErrorCode::InvalidRequest as i32)),
                    (RequestId::from(1), None),
                ]
            )
        );
    }

    /// After `shutdown`, a request is refused and a notification dropped
    /// until `exit`, which ends the server cleanly.
    #[test]
    fn requests_after_shutdown_are_refused_until_exit() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (exits, replies, shutdown) = runtime.block_on(async {
            let (server, client) = Connection::memory();
            let mut context = context::ServerContext::new(
                Connection {
                    sender: server.sender.clone(),
                    receiver: server.receiver.clone(),
                },
                Default::default(),
            );
            let mut connection = AsyncConnection::from_sync(server);
            let (_init_tx, init_rx) = oneshot::channel();
            let mut processor = ServerMessageProcessor::new(init_rx);
            let mut exits = Vec::new();
            for msg in [
                request(1, "shutdown"),
                notification("textDocument/didClose"),
                request(2, "textDocument/hover"),
                notification("exit"),
            ] {
                exits.push(
                    processor
                        .handle_message(msg, &mut connection, &mut context)
                        .await
                        .expect("messages after shutdown are handled"),
                );
            }
            let replies = client
                .receiver
                .try_iter()
                .filter_map(|msg| match msg {
                    Message::Response(response) => {
                        Some((response.id, response.error.map(|error| error.code)))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            (exits, replies, processor.shutdown_requested())
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(5));

        assert_eq!(
            (exits, replies, shutdown),
            (
                vec![false, false, false, true],
                vec![
                    (RequestId::from(1), None),
                    (RequestId::from(2), Some(ErrorCode::InvalidRequest as i32)),
                ],
                true
            )
        );
    }
}
