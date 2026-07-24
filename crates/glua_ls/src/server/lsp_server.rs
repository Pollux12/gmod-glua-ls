use lsp_server::{Message, Response};
use lsp_types::InitializeParams;
use std::error::Error;
use tokio::sync::oneshot;

use crate::context;

use super::connection::AsyncConnection;
use super::error::ExitError;
use super::main_loop::InitResult;
use super::message_processor::{InitOutcome, ServerMessageProcessor};

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
        init_rx: oneshot::Receiver<InitResult>,
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
        // First, wait for initialization to complete while handling allowed messages.
        // Returns the init outcome if it failed; otherwise the server is ready.
        if let Some(failure_reason) = self.wait_for_initialization().await? {
            // Initialization failed (e.g. GMod annotations validation). The
            // `initialized_handler` already sent a `window/showMessage` with
            // the reason. Close the context and abort the server cleanly
            // instead of proceeding without required API metadata.
            log::error!("language server initialization failed: {failure_reason}");
            self.server_context.close().await;

            // Give the user-visible `window/showMessage` a bounded opportunity
            // to be serialized and flushed by the LSP writer thread before the
            // process tears down.
            //
            // `ClientProxy::show_message` enqueues the notification onto the
            // LSP `Connection::sender` channel. Both stdio and TCP transports
            // use `crossbeam_channel::bounded::<Message>(0)` (a rendezvous
            // channel), so `send` only completes once the writer thread has
            // *received* the message — but the writer's `Message::write`
            // (which flushes to stdout/socket) runs *after* the rendezvous,
            // and `run_ls` returns without `threads.join()` on this abort
            // path. Without a grace window the writer thread can be killed
            // mid-flush when the process exits, silently dropping the reason
            // the user is supposed to see.
            //
            // We do NOT call `threads.join()` here because the reader thread
            // blocks on client stdin until the client sends `exit`/closes the
            // stream, which would hang the abort indefinitely. A short sleep is
            // the correct bounded drain: the writer needs only milliseconds to
            // flush a single small notification, and we cap the wait so a
            // slow client can never stall the abort.
            drain_outbound_before_abort().await;

            return Err(ExitError(failure_reason).into());
        }

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

    /// Wait for initialization to complete while handling initialization-allowed messages.
    ///
    /// Returns `Ok(Some(reason))` when initialization failed and the server
    /// should abort, `Ok(None)` when initialization succeeded and the server
    /// may continue, and `Err` for connection-level errors.
    async fn wait_for_initialization(
        &mut self,
    ) -> Result<Option<String>, Box<dyn Error + Sync + Send>> {
        loop {
            // Check if initialization is complete
            if let Some(outcome) = self.processor.poll_initialization() {
                match outcome {
                    InitOutcome::Ready => return Ok(None),
                    InitOutcome::Failed(reason) => return Ok(Some(reason)),
                    InitOutcome::Closed => {
                        // Init task ended without a result; treat as a fatal
                        // startup failure so we don't silently limp on.
                        return Ok(Some(
                            "language server initialization task terminated unexpectedly"
                                .to_string(),
                        ));
                    }
                }
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
                    return Ok(None);
                }
                Err(_) => {
                    // Timeout - continue checking for initialization completion
                    continue;
                }
            }
        }
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
            | "gluals/hoverExpand"
            | "emmy/annotator"
    )
}

/// Bounded upper bound on how long `drain_outbound_before_abort` waits for the
/// LSP writer thread to flush the queued `window/showMessage` before the
/// initialization-failure abort returns.
///
/// The writer only needs to serialize and flush a single small JSON-RPC
/// notification (typically a few hundred bytes), which is sub-millisecond
/// work once it has the message. 200ms is a generous ceiling that covers
/// scheduler latency on a loaded machine while never perceptibly delaying
/// the abort path for the user. It is deliberately *not* unbounded: a slow
/// or stuck client can never stall server teardown on this path.
const OUTBOUND_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Give the LSP writer thread a bounded opportunity to flush the
/// `window/showMessage` notification queued on the initialization-failure
/// path before `run_ls` returns and the process tears down.
///
/// See the call site in [`LspServer::run`] for the full rationale. This is a
/// best-effort happens-before-ish drain, not a hard guarantee: we cap the
/// wait so a misbehaving transport can't hang the abort.
async fn drain_outbound_before_abort() {
    tokio::time::sleep(OUTBOUND_DRAIN_GRACE).await;
}
