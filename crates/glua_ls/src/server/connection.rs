use lsp_server::{Connection, Message, Response};
use std::error::Error;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::error::ExitError;

/// Async wrapper for LSP Connection with tokio support
pub struct AsyncConnection {
    pub(crate) connection: Arc<Connection>,
    receiver: mpsc::UnboundedReceiver<Message>,
    _receiver_task: tokio::task::JoinHandle<()>,
}

impl AsyncConnection {
    /// Create async version from sync Connection
    pub fn from_sync(connection: Connection) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let connection = Arc::new(connection);

        // Spawn blocking task to convert sync receiver to async
        let connection_clone = connection.clone();
        let receiver_task = tokio::task::spawn_blocking(move || {
            for msg in &connection_clone.receiver {
                if tx.send(msg).is_err() {
                    break; // Receiver closed
                }
            }
        });

        Self {
            connection,
            receiver: rx,
            _receiver_task: receiver_task,
        }
    }

    /// Receive message asynchronously
    pub async fn recv(&mut self) -> Option<Message> {
        self.receiver.recv().await
    }

    /// Send message to client
    pub fn send(&self, msg: Message) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.connection
            .sender
            .send(msg)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }

    /// Handle shutdown request
    pub async fn handle_shutdown(
        &mut self,
        req: &lsp_server::Request,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        if req.method != "shutdown" {
            return Ok(false);
        }
        let resp = Response::new_ok(req.id.clone(), ());
        let _ = self.connection.sender.send(resp.into());
        match tokio::time::timeout(std::time::Duration::from_secs(30), self.receiver.recv()).await {
            Ok(Some(Message::Notification(n))) if n.method == "exit" => (),
            Ok(Some(msg)) => {
                return Err(Box::new(ExitError(format!(
                    "unexpected message during shutdown: {msg:?}"
                ))));
            }
            Ok(None) => {
                return Err(Box::new(ExitError(
                    "channel closed while waiting for exit notification".to_owned(),
                )));
            }
            Err(_) => {
                return Err(Box::new(ExitError(
                    "timed out waiting for exit notification".to_owned(),
                )));
            }
        }
        Ok(true)
    }
}
