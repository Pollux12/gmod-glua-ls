use lsp_types::{Uri, VersionedTextDocumentIdentifier};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use super::{InFlightChangeGuard, ServerContextSnapshot};
use crate::handlers::on_did_change_text_document;

struct QueuedDidChange {
    document: VersionedTextDocumentIdentifier,
    text: String,
    session: u64,
    in_flight: InFlightChangeGuard,
    enqueue_profile: Option<crate::util::ls_profile::DidChangeEnqueue>,
}

#[derive(Default)]
struct PendingChanges {
    latest: HashMap<Uri, QueuedDidChange>,
    closed: bool,
}

struct SharedState {
    pending: Mutex<PendingChanges>,
    notify: Notify,
}

impl SharedState {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, PendingChanges> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Coalesces rapid `textDocument/didChange` notifications.
///
/// When the user types character-by-character, VS Code sends one `didChange`
/// per keystroke.  Without coalescing, each notification spawns a concurrent
/// handler that competes for `analysis.write()`.  With 33 keystrokes this
/// creates 33 serialised write-lock acquisitions plus 33 parses.
///
/// Each queued change carries the document's full text after it, so the
/// coalescer stores only the **latest** text per URI in a latest-wins map and
/// processes them one batch at a time through a single worker task.  This
/// bounds memory to at most one buffered copy per dirty URI even when the
/// worker is stalled on a long reindex, and turns 33 write-lock acquisitions
/// into 1–3.
pub struct DidChangeCoalescer {
    shared: Arc<SharedState>,
}

impl Drop for DidChangeCoalescer {
    fn drop(&mut self) {
        let leftover = {
            let mut pending = self.shared.lock_pending();
            pending.closed = true;
            std::mem::take(&mut pending.latest)
        };
        drop(leftover);
        self.shared.notify.notify_one();
    }
}

impl DidChangeCoalescer {
    /// Create a new coalescer and spawn its background worker.
    pub fn new(context: ServerContextSnapshot) -> Self {
        let shared = Arc::new(SharedState {
            pending: Mutex::new(PendingChanges::default()),
            notify: Notify::new(),
        });
        tokio::spawn(Self::worker(shared.clone(), context));
        Self { shared }
    }

    /// Enqueue a document's full text after a didChange.
    /// Replaces any older unprocessed text for the same URI immediately so
    /// buffered memory never grows with keystroke count while the worker is
    /// busy.
    pub fn enqueue(
        &self,
        document: VersionedTextDocumentIdentifier,
        text: String,
        session: u64,
        in_flight: InFlightChangeGuard,
        enqueue_profile: Option<crate::util::ls_profile::DidChangeEnqueue>,
    ) {
        let uri = document.uri.clone();
        let queued = QueuedDidChange {
            document,
            text,
            session,
            in_flight,
            enqueue_profile,
        };
        let (closed, superseded) = {
            let mut pending = self.shared.lock_pending();
            if pending.closed {
                (true, Some(queued))
            } else {
                (false, pending.latest.insert(uri, queued))
            }
        };
        // Drop any superseded entry outside the mutex so its
        // `InFlightChangeGuard::drop` settles without holding the lock.
        drop(superseded);
        if closed {
            log::error!(
                "LS_COALESCER_SEND_FAILED didChange worker is closed; settling dropped change"
            );
            return;
        }
        self.shared.notify.notify_one();
    }

    /// Worker loop: wait for at least one pending change, take the latest
    /// map atomically, and process each URI.
    async fn worker(shared: Arc<SharedState>, context: ServerContextSnapshot) {
        let shutdown = context.debounced_analysis_arc().shutdown_token();
        loop {
            let latest = loop {
                let notified = shared.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                let taken = {
                    let mut pending = shared.lock_pending();
                    if !pending.latest.is_empty() {
                        Some(std::mem::take(&mut pending.latest))
                    } else if pending.closed {
                        log::info!("didChange coalescer stopped: closed");
                        return;
                    } else {
                        None
                    }
                };

                if let Some(batch) = taken {
                    break batch;
                }

                tokio::select! {
                    _ = notified => {}
                    _ = shutdown.cancelled() => {
                        let leftover = {
                            let mut pending = shared.lock_pending();
                            pending.closed = true;
                            std::mem::take(&mut pending.latest)
                        };
                        drop(leftover);
                        log::info!("didChange coalescer stopped: shutdown");
                        return;
                    }
                }
            };

            // Process only the latest version for each URI.
            for (uri, queued) in latest {
                if !crate::util::ls_profile::ls_profile_enabled()
                    || queued.enqueue_profile.is_none()
                {
                    let task_context = context.clone();
                    let handle = tokio::spawn(async move {
                        on_did_change_text_document(
                            task_context,
                            queued.document,
                            queued.text,
                            queued.session,
                        )
                        .await;
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
                let document = queued.document;
                let text = queued.text;
                let session = queued.session;
                let in_flight = queued.in_flight;
                let handle = tokio::spawn(crate::util::ls_profile::LS_DID_CHANGE_ACCUM.scope(
                    accum_scope,
                    async move {
                        on_did_change_text_document(task_context, document, text, session).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ServerContext;
    use lsp_server::Connection;
    use std::str::FromStr;

    #[tokio::test]
    async fn rapid_enqueues_for_same_uri_keep_only_latest_and_settle_superseded_guards() {
        let (server, _client) = Connection::memory();
        let server_context = ServerContext::new(server, lsp_types::ClientCapabilities::default());
        let snapshot = server_context.snapshot();
        let debounced = snapshot.debounced_analysis_arc();
        let uri = Uri::from_str("file:///test/rapid.lua").unwrap();

        let shared = Arc::new(SharedState {
            pending: Mutex::new(PendingChanges::default()),
            notify: Notify::new(),
        });
        let coalescer = DidChangeCoalescer {
            shared: shared.clone(),
        };

        for version in 1..=50 {
            let guard = debounced.begin_document_change(uri.clone());
            coalescer.enqueue(
                VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                format!("local x = {version}"),
                1,
                guard,
                None,
            );
        }

        let latest = {
            let mut pending = shared.lock_pending();
            assert_eq!(pending.latest.len(), 1);
            pending.latest.remove(&uri).unwrap()
        };
        assert_eq!(latest.document.version, 50);
        assert_eq!(latest.text, "local x = 50");

        latest.in_flight.finish().await;
        // Superseded guards settle asynchronously on the runtime; yield until
        // all 49 drop-spawned finishes complete.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while debounced.is_dirty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("superseded in-flight guards must settle");

        server_context.close().await;
    }
}
