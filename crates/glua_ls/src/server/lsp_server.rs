use lsp_server::{Message, Response};
use lsp_types::InitializeParams;
use std::error::Error;
use tokio::sync::oneshot;

use crate::context;

use super::connection::AsyncConnection;
use super::message_processor::ServerMessageProcessor;

/// LSP Server manages the entire server lifecycle
pub(super) struct LspServer {
    pub(super) connection: AsyncConnection,
    pub(super) server_context: context::ServerContext,
    pub(super) processor: ServerMessageProcessor,
}

impl LspServer {
    /// Create a new LSP server instance
    pub(super) fn new(
        connection: AsyncConnection,
        params: &InitializeParams,
        init_rx: oneshot::Receiver<()>,
    ) -> Self {
        let server_context = context::ServerContext::new(
            lsp_server::Connection {
                sender: connection.connection.sender.clone(),
                receiver: connection.connection.receiver.clone(),
            },
            params.capabilities.clone(),
        );

        Self {
            connection,
            server_context,
            processor: ServerMessageProcessor::new(init_rx),
        }
    }

    /// Run the main server loop
    pub(super) async fn run(mut self) -> Result<(), Box<dyn Error + Sync + Send>> {
        // First, wait for initialization to complete while handling allowed messages
        self.wait_for_initialization().await?;

        // Process all pending messages after initialization
        if self
            .processor
            .process_pending_messages(&mut self.connection, &mut self.server_context)
            .await?
        {
            self.server_context.close().await;
            return Ok(()); // Shutdown requested during pending message processing
        }

        // Now focus on normal message processing
        while let Some(msg) = self.connection.recv().await {
            if self
                .processor
                .process_message(msg, &mut self.connection, &mut self.server_context)
                .await?
            {
                break; // Shutdown requested
            }
        }

        self.server_context.close().await;
        Ok(())
    }

    /// Wait for initialization to complete while handling initialization-allowed messages
    async fn wait_for_initialization(&mut self) -> Result<(), Box<dyn Error + Sync + Send>> {
        loop {
            // Check if initialization is complete
            if self.processor.check_initialization_complete()? {
                break; // Initialization completed
            }

            // Use a short timeout to check for messages during initialization
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(50),
                self.connection.recv(),
            )
            .await
            {
                Ok(Some(msg)) => {
                    // Process message if allowed during initialization, otherwise queue it
                    if self.processor.can_process_during_init(&msg) {
                        self.processor
                            .handle_message(msg, &mut self.connection, &mut self.server_context)
                            .await?;
                    } else {
                        match msg {
                            Message::Request(request) => {
                                if should_fail_fast_request_during_init(&request.method) {
                                    // During startup, fail fast for editor data requests instead
                                    // of queueing them behind full workspace initialization.
                                    // Clients will re-request after initialization and avoid
                                    // perceived 10-20s startup request stalls.
                                    let response = Response::new_err(
                                        request.id,
                                        lsp_server::ErrorCode::ContentModified as i32,
                                        "server initializing".to_owned(),
                                    );
                                    self.connection.send(response.into())?;
                                } else {
                                    // Preserve one-shot/critical request semantics by
                                    // deferring them until initialization completes.
                                    self.processor
                                        .pending_messages
                                        .push(Message::Request(request));
                                }
                            }
                            other => {
                                self.processor.pending_messages.push(other);
                            }
                        }
                    }
                }
                Ok(None) => {
                    // Connection closed during initialization
                    return Ok(());
                }
                Err(_) => {
                    // Timeout - continue checking for initialization completion
                    continue;
                }
            }
        }
        Ok(())
    }
}

fn should_fail_fast_request_during_init(method: &str) -> bool {
    matches!(
        method,
        "textDocument/hover"
            | "textDocument/completion"
            | "textDocument/documentSymbol"
            | "textDocument/foldingRange"
            | "textDocument/documentColor"
            | "textDocument/documentLink"
            | "textDocument/codeLens"
            | "textDocument/inlayHint"
            | "textDocument/semanticTokens/full"
            | "textDocument/diagnostic"
            | "workspace/diagnostic"
            | "workspace/symbol"
            | "gluals/annotator"
            | "gluals/gmodScriptedClasses"
            | "gluals/gmodScriptedClassesV2"
            | "gluals/docSearch"
            | "emmy/annotator"
    )
}
