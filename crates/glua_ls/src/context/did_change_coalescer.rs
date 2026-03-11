use lsp_types::{DidChangeTextDocumentParams, Uri};
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::ServerContextSnapshot;
use crate::handlers::on_did_change_text_document;

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
    tx: mpsc::UnboundedSender<DidChangeTextDocumentParams>,
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
    pub fn enqueue(&self, params: DidChangeTextDocumentParams) {
        // If the worker died we silently drop — nothing useful to do.
        let _ = self.tx.send(params);
    }

    /// Worker loop: wait for at least one message, then drain all pending
    /// messages, keep only the latest per URI, and process the batch.
    async fn worker(
        mut rx: mpsc::UnboundedReceiver<DidChangeTextDocumentParams>,
        context: ServerContextSnapshot,
    ) {
        loop {
            // Wait for at least one message.
            let first = match rx.recv().await {
                Some(params) => params,
                None => return, // channel closed
            };

            // Drain remaining messages without blocking.
            let mut latest: HashMap<Uri, DidChangeTextDocumentParams> = HashMap::new();
            latest.insert(first.text_document.uri.clone(), first);

            while let Ok(params) = rx.try_recv() {
                latest.insert(params.text_document.uri.clone(), params);
            }

            // Process only the latest version for each URI.
            for (_uri, params) in latest {
                on_did_change_text_document(context.clone(), params).await;
            }
        }
    }
}
