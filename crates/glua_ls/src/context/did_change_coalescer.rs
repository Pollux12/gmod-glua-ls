use lsp_types::{DidChangeTextDocumentParams, Uri};
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::{InFlightChangeGuard, ServerContextSnapshot};
use crate::handlers::on_did_change_text_document;

struct QueuedDidChange {
    params: DidChangeTextDocumentParams,
    in_flight: InFlightChangeGuard,
    enqueue_profile: Option<crate::util::ls_profile::DidChangeEnqueue>,
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
    pub fn enqueue(
        &self,
        params: DidChangeTextDocumentParams,
        in_flight: InFlightChangeGuard,
        enqueue_profile: Option<crate::util::ls_profile::DidChangeEnqueue>,
    ) {
        if let Err(err) = self.tx.send(QueuedDidChange {
            params,
            in_flight,
            enqueue_profile,
        }) {
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
                None => {
                    // Every sender is gone: shutdown.
                    log::info!("didChange coalescer stopped: channel closed");
                    return;
                }
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
                if !crate::util::ls_profile::ls_profile_enabled()
                    || queued.enqueue_profile.is_none()
                {
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
                    continue;
                }

                // Profiled path: scope one accumulator so preparse, write
                // wait/hold, and schedule timings observed inside
                // `on_did_change_text_document` accumulate into a single
                // `[profile] ls_did_change` line. Superseded (coalesced away)
                // versions never reach here and emit nothing.
                let profile = queued.enqueue_profile.expect("profile checked above");
                let accum = std::sync::Arc::new(crate::util::ls_profile::DidChangeAccum::new());
                let dequeue_at = std::time::Instant::now();
                let coalescer_wait = dequeue_at.saturating_duration_since(profile.enqueue_at);
                let task_context = context.clone();
                let accum_scope = accum.clone();
                let params = queued.params;
                let in_flight = queued.in_flight;
                let handle = tokio::spawn(crate::util::ls_profile::LS_DID_CHANGE_ACCUM.scope(
                    accum_scope,
                    async move {
                        on_did_change_text_document(task_context, params).await;
                    },
                ));
                if let Err(err) = handle.await {
                    log::error!(
                        "LS_COALESCER_ITEM_PANIC uri={:?} didChange handler failed: {}",
                        uri,
                        err
                    );
                }
                in_flight.finish().await;
                let timings = accum.snapshot();
                let total = profile.enqueue_at.elapsed();
                crate::util::ls_profile::emit_ls_did_change(
                    profile.seq,
                    profile.version,
                    coalescer_wait,
                    timings.preparse,
                    timings.write_wait,
                    timings.write_hold,
                    timings.notify_to_schedule,
                    total,
                );
            }
        }
    }
}
