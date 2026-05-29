use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, file_diagnostic::SharedDiagnosticDataCache};

/// Debounced analysis: accumulates file IDs from rapid edits and runs `reindex_files` once the user pauses typing.
pub struct DebouncedAnalysis {
    pending_files: Mutex<HashSet<FileId>>,
    reindexing_files: Mutex<HashSet<FileId>>,
    /// True when document changes have arrived but reindex has not yet completed.
    /// Set synchronously by `mark_dirty()` (called inline in the notification
    /// handler, before the didChange task is spawned) so that any request handler
    /// dispatched afterwards sees the flag immediately.
    has_pending_changes: AtomicBool,
    in_flight_changes: AtomicUsize,
    notify: Notify,
    reindex_notify: Notify,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shared_diagnostic_data_cache: SharedDiagnosticDataCache,
    debounce_duration: Duration,
    shutdown: CancellationToken,
    client: Arc<ClientProxy>,
}

impl DebouncedAnalysis {
    pub(crate) fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        debounce_ms: u64,
        shutdown: CancellationToken,
        client: Arc<ClientProxy>,
        shared_diagnostic_data_cache: SharedDiagnosticDataCache,
    ) -> Self {
        Self {
            pending_files: Mutex::new(HashSet::new()),
            reindexing_files: Mutex::new(HashSet::new()),
            has_pending_changes: AtomicBool::new(false),
            in_flight_changes: AtomicUsize::new(0),
            notify: Notify::new(),
            reindex_notify: Notify::new(),
            analysis,
            shared_diagnostic_data_cache,
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
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Signal that document changes are in-flight but not yet scheduled.
    ///
    /// Called **synchronously** from the notification handler (inline, before
    /// spawning the didChange task) so that request handlers dispatched
    /// immediately afterward see the dirty flag and wait for reindex instead
    /// of computing on stale analysis data.
    pub fn mark_dirty(&self) {
        self.in_flight_changes.fetch_add(1, Ordering::AcqRel);
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub async fn finish_in_flight_changes(&self, count: usize) {
        if count == 0 {
            return;
        }

        self.in_flight_changes.fetch_sub(count, Ordering::AcqRel);
        self.refresh_dirty_state().await;
        self.reindex_notify.notify_waiters();
    }

    /// Check whether document changes are pending reindex.
    ///
    /// Handlers that need consistent tree + index data (e.g. semantic tokens)
    /// can use this to decide whether to serve stale results or return `None`
    /// so the client keeps its previous state.
    pub fn is_dirty(&self) -> bool {
        self.has_pending_changes.load(Ordering::Acquire)
    }

    /// Wait until all pending document changes have been reindexed.
    ///
    /// Returns `true` when the analysis is fresh, `false` if the cancel token
    /// fired first.  Uses `enable()` so that `notify_waiters()` wakeups are
    /// not lost between creating the `Notified` future and polling it.
    pub async fn wait_until_fresh(&self, cancel_token: &CancellationToken) -> bool {
        loop {
            // Create and enable the Notified future BEFORE checking the
            // condition.  `enable()` ensures that a `notify_waiters()` call
            // between here and the `select!` poll is captured, avoiding a
            // missed wakeup (unpolled Notified futures are invisible to
            // `notify_waiters` without `enable`).
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if !self.has_pending_changes.load(Ordering::Acquire) {
                return true;
            }
            tokio::select! {
                _ = notified => {} // re-check
                _ = cancel_token.cancelled() => return false,
            }
        }
    }

    /// Wait until the given file is no longer pending reindex.
    pub async fn wait_for_reindex(&self, file_id: FileId, cancel_token: CancellationToken) {
        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let is_pending = {
                let pending = self.pending_files.lock().await;
                let reindexing = self.reindexing_files.lock().await;
                pending.contains(&file_id) || reindexing.contains(&file_id)
            };
            if !is_pending {
                return;
            }
            tokio::select! {
                _ = notified => {}
                _ = cancel_token.cancelled() => return,
            }
        }
    }

    async fn reindex_files_without_queuing(&self, file_ids: Vec<FileId>) -> bool {
        let mut retries = 0u32;

        loop {
            if let Ok(mut analysis) = self.analysis.try_write() {
                analysis.reindex_files(file_ids);
                self.shared_diagnostic_data_cache.invalidate();
                return true;
            }

            tokio::select! {
                _ = self.shutdown.cancelled() => return false,
                _ = async {
                    if retries <= 20 {
                        tokio::task::yield_now().await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                } => {}
            }

            retries += 1;
        }
    }

    /// Background loop: waits for events, debounces, then runs reindex.
    /// Spawn this once at server startup.
    pub async fn run(&self) {
        loop {
            // Wait for the first event, unless files were scheduled during
            // the previous reindex (the Notify signal may have been missed
            // because there was no active waiter at that point), or
            // mark_dirty() was called without a corresponding schedule().
            let needs_work = !self.pending_files.lock().await.is_empty()
                || self.has_pending_changes.load(Ordering::Acquire);
            if !needs_work {
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
                let mut ids: Vec<FileId> = pending.drain().collect();
                ids.sort();
                for id in &ids {
                    reindexing.insert(*id);
                }
                ids
            };

            if !file_ids.is_empty() {
                log::info!(
                    "debounced reindex: {} file(s) after {}ms quiet",
                    file_ids.len(),
                    self.debounce_duration.as_millis()
                );

                let reindex_completed = self.reindex_files_without_queuing(file_ids.clone()).await;

                {
                    let mut reindexing = self.reindexing_files.lock().await;
                    for id in &file_ids {
                        reindexing.remove(id);
                    }
                }

                self.reindex_notify.notify_waiters();
                if !reindex_completed {
                    return;
                }

                // Trigger diagnostic and semantic token refresh so the client
                // re-pulls with fresh data after the reindex.
                self.client.refresh_workspace_diagnostics();
                self.client.refresh_semantic_tokens();
                self.client.refresh_inlay_hints();
            }

            self.refresh_dirty_state().await;

            // Always notify waiters so they can re-check the condition.
            // Even if we didn't reindex (pending was empty), clearing the
            // dirty flag means waiters should proceed with available data.
            self.reindex_notify.notify_waiters();
        }
    }

    async fn refresh_dirty_state(&self) {
        let has_pending_file_work = {
            let pending = self.pending_files.lock().await;
            if !pending.is_empty() {
                true
            } else {
                let reindexing = self.reindexing_files.lock().await;
                !reindexing.is_empty()
            }
        };
        let has_in_flight_changes = self.in_flight_changes.load(Ordering::Acquire) > 0;
        self.has_pending_changes.store(
            has_pending_file_work || has_in_flight_changes,
            Ordering::Release,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use glua_code_analysis::{DiagnosticCode, EmmyLuaAnalysis, file_path_to_uri};
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::{Diagnostic, NumberOrString};
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    use crate::context::{ClientProxy, FileDiagnostic, StatusBar};

    use super::DebouncedAnalysis;

    fn has_diagnostic_code(diagnostics: &[Diagnostic], code: DiagnosticCode) -> bool {
        let code = Some(NumberOrString::String(code.get_name().to_string()));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    #[gtest]
    fn reindex_invalidates_cached_shared_diagnostic_data() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace =
                std::env::temp_dir().join("gmod_glua_ls_debounced_shared_diagnostic_cache");
            analysis.add_main_workspace(workspace.clone());
            analysis
                .diagnostic
                .enable_only(DiagnosticCode::DiscardReturns);

            let api_uri = file_path_to_uri(&workspace.join("lua/autorun/shared/api.lua"))
                .expect("API URI should parse");
            analysis.update_file_by_uri(
                &api_uri,
                Some("function NeedsUse() return true end".to_string()),
            );

            let user_uri = file_path_to_uri(&workspace.join("lua/autorun/shared/user.lua"))
                .expect("user URI should parse");
            analysis.update_file_by_uri(&user_uri, Some("NeedsUse()".to_string()));

            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone()));
            let analysis = Arc::new(RwLock::new(analysis));
            let file_diagnostic =
                FileDiagnostic::new(analysis.clone(), status_bar.clone(), client.clone());

            let initial_diagnostics = file_diagnostic
                .pull_file_diagnostics(user_uri.clone(), CancellationToken::new())
                .await
                .unwrap_or_default();
            verify_that!(
                has_diagnostic_code(&initial_diagnostics, DiagnosticCode::DiscardReturns),
                eq(false)
            )?;

            let api_file_id = {
                let mut analysis = analysis.write().await;
                analysis
                    .update_file_text_only(
                        &api_uri,
                        r#"
                            ---@nodiscard
                            function NeedsUse() return true end
                        "#
                        .to_string(),
                    )
                    .expect("API file should still exist")
            };

            let debounced_analysis = DebouncedAnalysis::new(
                analysis.clone(),
                0,
                CancellationToken::new(),
                client,
                file_diagnostic.shared_diagnostic_data_cache(),
            );
            verify_that!(
                debounced_analysis
                    .reindex_files_without_queuing(vec![api_file_id])
                    .await,
                eq(true)
            )?;

            let updated_diagnostics = file_diagnostic
                .pull_file_diagnostics(user_uri, CancellationToken::new())
                .await
                .unwrap_or_default();
            verify_that!(
                has_diagnostic_code(&updated_diagnostics, DiagnosticCode::DiscardReturns),
                eq(true)
            )?;

            Ok(())
        })
    }
}
