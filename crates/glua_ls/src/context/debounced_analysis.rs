use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::ClientProxy;

/// Debounced analysis: accumulates file IDs from rapid edits and runs `reindex_files` once the user pauses typing.
pub struct DebouncedAnalysis {
    pending_files: Mutex<HashSet<FileId>>,
    reindexing_files: Mutex<HashSet<FileId>>,
    notify: Notify,
    reindex_notify: Notify,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    debounce_duration: Duration,
    shutdown: CancellationToken,
    client: Arc<ClientProxy>,
}

impl DebouncedAnalysis {
    pub fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        debounce_ms: u64,
        shutdown: CancellationToken,
        client: Arc<ClientProxy>,
    ) -> Self {
        Self {
            pending_files: Mutex::new(HashSet::new()),
            reindexing_files: Mutex::new(HashSet::new()),
            notify: Notify::new(),
            reindex_notify: Notify::new(),
            analysis,
            debounce_duration: Duration::from_millis(debounce_ms),
            shutdown,
            client,
        }
    }

    /// Add a file to the pending reindex set and reset the debounce timer.
    pub async fn schedule(&self, file_id: FileId) {
        {
            let mut pending = self.pending_files.lock().await;
            pending.insert(file_id);
        }
        self.notify.notify_waiters();
    }

    /// Wait until the given file is no longer pending reindex.
    pub async fn wait_for_reindex(&self, file_id: FileId, cancel_token: CancellationToken) {
        loop {
            let is_pending = {
                let pending = self.pending_files.lock().await;
                let reindexing = self.reindexing_files.lock().await;
                pending.contains(&file_id) || reindexing.contains(&file_id)
            };
            if !is_pending {
                return;
            }
            tokio::select! {
                _ = self.reindex_notify.notified() => {}
                _ = cancel_token.cancelled() => return,
            }
        }
    }

    /// Wait until all pending reindexes are finished.
    pub async fn wait_for_all_reindex(&self, cancel_token: CancellationToken) {
        loop {
            let is_pending = {
                let pending = self.pending_files.lock().await;
                let reindexing = self.reindexing_files.lock().await;
                !pending.is_empty() || !reindexing.is_empty()
            };
            if !is_pending {
                return;
            }
            tokio::select! {
                _ = self.reindex_notify.notified() => {}
                _ = cancel_token.cancelled() => return,
            }
        }
    }

    /// Background loop: waits for events, debounces, then runs reindex.
    /// Spawn this once at server startup.
    pub async fn run(&self) {
        loop {
            // Wait for the first event, unless files were scheduled during
            // the previous reindex (the Notify signal may have been missed
            // because there was no active waiter at that point).
            if self.pending_files.lock().await.is_empty() {
                tokio::select! {
                    _ = self.notify.notified() => {}
                    _ = self.shutdown.cancelled() => return,
                }
            }

            // Debounce: keep resetting the timer while new events arrive
            loop {
                tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => return,
                    _ = self.notify.notified() => continue,
                    _ = tokio::time::sleep(self.debounce_duration) => break,
                }
            }

            // Timer expired — drain pending files and reindex
            let file_ids: Vec<FileId> = {
                let mut pending = self.pending_files.lock().await;
                let mut reindexing = self.reindexing_files.lock().await;
                let ids: Vec<FileId> = pending.drain().collect();
                for id in &ids {
                    reindexing.insert(*id);
                }
                ids
            };

            if file_ids.is_empty() {
                continue;
            }

            log::info!(
                "debounced reindex: {} file(s) after {}ms quiet",
                file_ids.len(),
                self.debounce_duration.as_millis()
            );

            let mut analysis = self.analysis.write().await;
            analysis.reindex_files(file_ids.clone());
            drop(analysis);

            {
                let mut reindexing = self.reindexing_files.lock().await;
                for id in &file_ids {
                    reindexing.remove(id);
                }
            }

            self.reindex_notify.notify_waiters();

            // Trigger diagnostic refresh so the client re-pulls with fresh index data
            self.client.refresh_workspace_diagnostics();
        }
    }
}
