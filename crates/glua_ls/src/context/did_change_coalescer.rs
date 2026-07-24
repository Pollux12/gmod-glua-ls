use lsp_types::{DidChangeTextDocumentParams, Uri};
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::{InFlightChangeGuard, ServerContextSnapshot};
use crate::handlers::on_did_change_text_document;

struct QueuedDidChange {
    params: DidChangeTextDocumentParams,
    in_flight: InFlightChangeGuard,
}

/// Coalesces rapid `textDocument/didChange` notifications.
///
/// When the user types character-by-character, VS Code sends one `didChange`
/// per keystroke.  Without coalescing, each notification spawns a concurrent
/// handler that competes for `analysis.write()`.  With 33 keystrokes this
/// creates 33 serialised write-lock acquisitions plus 33 parses.
///
/// The coalescer keeps only the **latest** params per URI and processes
/// them one batch at a time through a single worker task.  This turns 33
/// write-lock acquisitions into 1–3.
pub struct DidChangeCoalescer {
    /// Send a didChange to the worker.  The worker reads all pending
    /// messages before doing any work, so the channel acts as a buffer.
    tx: mpsc::UnboundedSender<QueuedDidChange>,
}

impl DidChangeCoalescer {
    /// Create a new coalescer and spawn its background worker.
    pub fn new(context: ServerContextSnapshot) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Self::worker(rx, context));
        Self { tx }
    }

    /// Enqueue a didChange notification.
    /// If the worker is busy, the params accumulate in the channel and
    /// the worker drains + deduplicates them in the next batch.
    pub fn enqueue(&self, params: DidChangeTextDocumentParams, in_flight: InFlightChangeGuard) {
        if let Err(err) = self.tx.send(QueuedDidChange { params, in_flight }) {
            log::error!(
                "LS_COALESCER_SEND_FAILED didChange worker channel is closed; settling dropped change"
            );
            drop(err);
        }
    }

    /// Worker loop: wait for at least one message, then drain all pending
    /// messages, keep only the latest per URI, and process the batch.
    async fn worker(
        mut rx: mpsc::UnboundedReceiver<QueuedDidChange>,
        context: ServerContextSnapshot,
    ) {
        loop {
            // Wait for at least one message.
            let first = match rx.recv().await {
                Some(params) => params,
                None => return, // channel closed
            };

            // Drain remaining messages without blocking.
            let mut latest: HashMap<Uri, QueuedDidChange> = HashMap::new();
            latest.insert(first.params.text_document.uri.clone(), first);

            while let Ok(params) = rx.try_recv() {
                if let Some(superseded) =
                    latest.insert(params.params.text_document.uri.clone(), params)
                {
                    superseded.in_flight.finish().await;
                }
            }

            // Process only the latest version for each URI.
            for (uri, queued) in latest {
                let task_context = context.clone();
                let handle = tokio::spawn(async move {
                    on_did_change_text_document(task_context, queued.params).await;
                });
                if let Err(err) = handle.await {
                    log::error!(
                        "LS_COALESCER_ITEM_PANIC uri={:?} didChange handler failed: {}",
                        uri,
                        err
                    );
                }
                queued.in_flight.finish().await;
            }
        }
    }
}
