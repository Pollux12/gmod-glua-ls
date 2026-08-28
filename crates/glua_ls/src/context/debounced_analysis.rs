use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use lsp_types::Uri;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, file_diagnostic::SharedDiagnosticDataCache};

const FRESHNESS_STUCK_WARN_AFTER: Duration = Duration::from_secs(5);

/// See implementation.
const IDLE_WORKSPACE_DIAGNOSTIC_DELAY: Duration = Duration::from_millis(2000);

/// How long the user must stay idle before the dependency ripple runs.
const RIPPLE_QUIET: Duration = Duration::from_millis(1000);

/// Cap on how long one typing burst can hold the ripple off, so diagnostics
/// still settle during sustained typing.
const MAX_RIPPLE_DEFERRAL: Duration = Duration::from_secs(5);

/// How long the ripple gives requests released by the self-index to take their
const READER_HANDOFF_GRACE: Duration = Duration::from_millis(250);

pub struct DebouncedAnalysis {
    pending_files: Mutex<HashSet<FileId>>,
    reindexing_files: Mutex<HashSet<FileId>>,
    /// Documents whose own index entries do not match their text yet, either
    blocked_documents: Mutex<HashMap<FileId, Uri>>,
    /// See implementation.
    has_pending_changes: AtomicBool,
    in_flight_changes: AtomicUsize,
    /// Requests aimed at one document that are waiting for, or reading against,
    pending_readers: AtomicUsize,
    readers_idle_notify: Notify,
    notify: Notify,
    reindex_notify: Notify,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shared_diagnostic_data_cache: SharedDiagnosticDataCache,
    debounce_duration: Duration,
    shutdown: CancellationToken,
    client: Arc<ClientProxy>,
    workspace_diagnostic_level: Arc<AtomicU8>,
    lsp_features: Arc<crate::context::LspFeatures>,
    /// Entries into [`Self::wait_until_fresh_for`], so a test can synchronise
    /// on a handler having reached the wait instead of on a deadline.
    #[cfg(test)]
    freshness_waits: AtomicUsize,
}

impl DebouncedAnalysis {
    pub(crate) fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        debounce_ms: u64,
        shutdown: CancellationToken,
        client: Arc<ClientProxy>,
        shared_diagnostic_data_cache: SharedDiagnosticDataCache,
        workspace_diagnostic_level: Arc<AtomicU8>,
        lsp_features: Arc<crate::context::LspFeatures>,
    ) -> Self {
        Self {
            pending_files: Mutex::new(HashSet::new()),
            reindexing_files: Mutex::new(HashSet::new()),
            blocked_documents: Mutex::new(HashMap::new()),
            has_pending_changes: AtomicBool::new(false),
            in_flight_changes: AtomicUsize::new(0),
            pending_readers: AtomicUsize::new(0),
            readers_idle_notify: Notify::new(),
            notify: Notify::new(),
            reindex_notify: Notify::new(),
            analysis,
            shared_diagnostic_data_cache,
            debounce_duration: Duration::from_millis(debounce_ms),
            shutdown,
            client,
            workspace_diagnostic_level,
            lsp_features,
            #[cfg(test)]
            freshness_waits: AtomicUsize::new(0),
        }
    }

    pub async fn schedule(&self, file_id: FileId, uri: Uri) {
        {
            let mut pending = self.pending_files.lock().await;
            pending.insert(file_id);
        }
        {
            let mut blocked = self.blocked_documents.lock().await;
            blocked.insert(file_id, uri);
        }
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Signal that document changes are in-flight but not yet scheduled.
    pub fn begin_in_flight_change(self: &Arc<Self>) -> InFlightChangeGuard {
        self.in_flight_changes.fetch_add(1, Ordering::AcqRel);
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
        InFlightChangeGuard::new(self.clone(), 1)
    }

    pub async fn finish_in_flight_changes(&self, count: usize) {
        if count == 0 {
            return;
        }

        let mut current = self.in_flight_changes.load(Ordering::Acquire);
        loop {
            let next = current.saturating_sub(count);
            match self.in_flight_changes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) => {
                    if previous < count {
                        log::error!(
                            "LS_INFLIGHT_UNDERFLOW attempted to finish {} in-flight changes with only {} pending",
                            count,
                            previous
                        );
                    }
                    break;
                }
                Err(observed) => current = observed,
            }
        }

        self.refresh_dirty_state().await;
        self.reindex_notify.notify_waiters();
    }

    /// See implementation.
    pub fn is_dirty(&self) -> bool {
        self.has_pending_changes.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn in_flight_change_count(&self) -> usize {
        self.in_flight_changes.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn freshness_wait_count(&self) -> usize {
        self.freshness_waits.load(Ordering::Acquire)
    }

    /// Register that a request is waiting on, or reading against, one
    pub fn begin_reader_handoff(self: &Arc<Self>) -> ReaderHandoff {
        self.pending_readers.fetch_add(1, Ordering::AcqRel);
        ReaderHandoff {
            analysis: self.clone(),
        }
    }

    /// Let outstanding [`ReaderHandoff`]s take their read lock before the
    async fn await_reader_handoff(&self) {
        let deadline = Instant::now() + READER_HANDOFF_GRACE;

        loop {
            // Register before testing, or a drop landing in between is lost.
            let idle = self.readers_idle_notify.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();

            if self.pending_readers.load(Ordering::Acquire) == 0 {
                return;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }

            tokio::select! {
                _ = idle => {}
                _ = tokio::time::sleep(remaining) => return,
                _ = self.shutdown.cancelled() => return,
            }
        }
    }

    /// See implementation.
    pub async fn wait_until_fresh_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
    ) -> bool {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;

        loop {
            // Register (`enable()`) before testing the condition: unpolled
            // Notified futures are invisible to `notify_waiters()`.
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if !self.has_pending_changes.load(Ordering::Acquire) {
                return true;
            }

            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            tokio::select! {
                _ = notified => {} // re-check
                _ = cancel_token.cancelled() => return false,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                }
            }
        }
    }

    /// Wait until the document at `uri` has index entries matching its text.
    pub async fn wait_until_file_fresh_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        uri: &Uri,
    ) -> bool {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;

        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.file_is_answerable(uri).await {
                return true;
            }

            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            tokio::select! {
                _ = notified => {}
                _ = cancel_token.cancelled() => return false,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                }
            }
        }
    }

    /// Whether a request aimed at `uri` can be answered against entries that
    pub async fn file_is_answerable(&self, uri: &Uri) -> bool {
        // An edit whose text has not been applied yet would have the request
        // resolve a position against the previous tree.
        if self.in_flight_changes.load(Ordering::Acquire) > 0 {
            return false;
        }
        if !self.has_pending_changes.load(Ordering::Acquire) {
            return true;
        }
        // Only ever a handful of documents are mid-edit at once.
        !self
            .blocked_documents
            .lock()
            .await
            .values()
            .any(|blocked| blocked == uri)
    }

    async fn log_freshness_stuck(&self, request_method: &'static str, started_at: Instant) {
        let in_flight = self.in_flight_changes.load(Ordering::Acquire);
        let pending_count = self.pending_files.lock().await.len();
        let reindexing_count = self.reindexing_files.lock().await.len();
        log::warn!(
            "LS_FRESHNESS_STUCK request={} waited_ms={} in_flight={} pending_files={} reindexing_files={}",
            request_method,
            started_at.elapsed().as_millis(),
            in_flight,
            pending_count,
            reindexing_count
        );
    }

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

    /// See implementation.
    async fn self_index_without_queuing(
        &self,
        file_ids: Vec<FileId>,
    ) -> Option<(Vec<FileId>, Vec<FileId>)> {
        let analysis = self.analysis.clone();
        let cache = self.shared_diagnostic_data_cache.clone();

        tokio::select! {
            _ = self.shutdown.cancelled() => None,
            result = tokio::task::spawn_blocking(move || {
                let mut guard = analysis.blocking_write();
                // Change-aware: only expand to dependents when the file's
                let (changed, expansion) = guard.self_index_files_and_get_ripple_with_changed(file_ids);
                cache.invalidate();
                (changed, expansion)
            }) => match result {
                Ok(result) => Some(result),
                Err(err) => {
                    log::error!("self-index task failed: {}", err);
                    None
                }
            }
        }
    }

    async fn reindex_files_without_queuing(
        &self,
        file_ids: Vec<FileId>,
        expansion: Vec<FileId>,
    ) -> bool {
        let analysis = self.analysis.clone();
        let cache = self.shared_diagnostic_data_cache.clone();

        tokio::select! {
            _ = self.shutdown.cancelled() => false,
            result = tokio::task::spawn_blocking(move || {
                let mut guard = analysis.blocking_write();
                guard.reindex_expanded_files(file_ids, expansion);
                // Invalidate under the write lock so no reader can observe the
                cache.invalidate();
            }) => {
                if let Err(err) = result {
                    log::error!("reindex task failed: {}", err);
                    return false;
                }
                true
            }
        }
    }

    /// Hold the owed ripple until typing has stopped for [`RIPPLE_QUIET`].
    async fn ripple_quiet_elapsed(&self, burst_started_at: Instant) -> bool {
        let extra = RIPPLE_QUIET.saturating_sub(self.debounce_duration);
        let deferral_left = MAX_RIPPLE_DEFERRAL.saturating_sub(burst_started_at.elapsed());
        if extra.is_zero() || deferral_left.is_zero() {
            return true;
        }

        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => return true,
            _ = self.notify.notified() => return false,
            _ = tokio::time::sleep(extra.min(deferral_left)) => {}
        }

        // A notify landing before the select registered would be lost, so the
        // timer expiring is not on its own proof that nothing arrived.
        self.pending_files.lock().await.is_empty()
    }

    /// See implementation.
    pub async fn run(&self) {
        let mut idle_workspace_diagnostic_token: Option<CancellationToken> = None;
        // The ripple owed by the self-indexes run so far in this typing burst,
        // and the union of the expansions each of them captured.
        let mut owed_files: HashSet<FileId> = HashSet::new();
        let mut owed_expansion: HashSet<FileId> = HashSet::new();
        let mut burst_started_at: Option<Instant> = None;
        loop {
            // Register before testing the condition: `notify_waiters()` stores
            // no permit, so a signal landing in between would be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let needs_work = !self.pending_files.lock().await.is_empty()
                || self.has_pending_changes.load(Ordering::Acquire)
                || !owed_files.is_empty();
            if !needs_work {
                tokio::select! {
                    _ = notified => {}
                    _ = self.shutdown.cancelled() => return,
                }
            }

            // Debounce: keep resetting the timer while new events arrive.
            loop {
                tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => return,
                    _ = self.notify.notified() => continue,
                    _ = tokio::time::sleep(self.debounce_duration) => break,
                }
            }

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

                let Some((changed_files, expansion)) =
                    self.self_index_without_queuing(file_ids.clone()).await
                else {
                    if self.shutdown.is_cancelled() {
                        return;
                    }
                    let mut reindexing = self.reindexing_files.lock().await;
                    let mut blocked = self.blocked_documents.lock().await;
                    for id in &file_ids {
                        reindexing.remove(id);
                        blocked.remove(id);
                    }
                    drop(blocked);
                    drop(reindexing);
                    self.refresh_dirty_state().await;
                    self.reindex_notify.notify_waiters();
                    continue;
                };

                {
                    let mut blocked = self.blocked_documents.lock().await;
                    for file_id in &file_ids {
                        blocked.remove(file_id);
                    }
                }
                self.reindex_notify.notify_waiters();

                // The requests just released still have to be polled before
                self.await_reader_handoff().await;

                if expansion.is_empty() {
                    // No exports changed - the self-index already makes these
                    {
                        let mut reindexing = self.reindexing_files.lock().await;
                        for id in &file_ids {
                            reindexing.remove(id);
                        }
                    }
                    self.refresh_dirty_state().await;
                    self.reindex_notify.notify_waiters();
                    if owed_files.is_empty() {
                        continue;
                    }
                } else {
                    // Only the files whose exports actually changed need a
                    let changed_set: HashSet<FileId> = changed_files.iter().copied().collect();
                    {
                        let mut reindexing = self.reindexing_files.lock().await;
                        for id in &file_ids {
                            if !changed_set.contains(id) {
                                reindexing.remove(id);
                            }
                        }
                    }
                    owed_files.extend(changed_files.iter().copied());
                    owed_expansion.extend(expansion);
                    burst_started_at.get_or_insert_with(Instant::now);
                }
            }

            if owed_files.is_empty() {
                self.refresh_dirty_state().await;
                self.reindex_notify.notify_waiters();
                continue;
            }

            // Hold the ripple until typing has genuinely stopped. Another edit
            let burst_started_at_instant = burst_started_at.unwrap_or_else(Instant::now);
            if !self.ripple_quiet_elapsed(burst_started_at_instant).await {
                continue;
            }

            {
                let ripple_files: Vec<FileId> = {
                    let mut ids: Vec<FileId> = owed_files.iter().copied().collect();
                    ids.sort();
                    ids
                };
                let ripple_expansion: Vec<FileId> = {
                    let mut ids: Vec<FileId> = owed_expansion.iter().copied().collect();
                    ids.sort();
                    ids
                };
                log::info!(
                    "ripple: {} edited file(s) over {} file(s) after {}ms quiet",
                    ripple_files.len(),
                    ripple_expansion.len(),
                    RIPPLE_QUIET.as_millis()
                );

                let reindex_completed = self
                    .reindex_files_without_queuing(ripple_files.clone(), ripple_expansion)
                    .await;

                {
                    let mut reindexing = self.reindexing_files.lock().await;
                    for id in &ripple_files {
                        reindexing.remove(id);
                    }
                }
                owed_files.clear();
                owed_expansion.clear();
                burst_started_at = None;

                self.reindex_notify.notify_waiters();
                if !reindex_completed {
                    if self.shutdown.is_cancelled() {
                        return;
                    }
                    log::error!(
                        "LS_REINDEX_FAILED reindex of {} file(s) did not complete; continuing so freshness waiters are released",
                        ripple_files.len()
                    );
                }

                if self.lsp_features.supports_semantic_tokens_refresh() {
                    self.client.refresh_semantic_tokens();
                }
                if self.lsp_features.supports_inlay_hint_refresh() {
                    self.client.refresh_inlay_hints();
                }

                // Arm an idle workspace diagnostic refresh so closed files hit
                // by cross-file changes get re-diagnosed once typing pauses.
                if let Some(token) = idle_workspace_diagnostic_token.take() {
                    token.cancel();
                }
                let cancel_token = CancellationToken::new();
                idle_workspace_diagnostic_token = Some(cancel_token.clone());

                let client = self.client.clone();
                let status = self.workspace_diagnostic_level.clone();
                let lsp_features = self.lsp_features.clone();
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(IDLE_WORKSPACE_DIAGNOSTIC_DELAY) => {
                            if !cancel_token.is_cancelled() && !shutdown.is_cancelled() {
                                // Raise, never lower: don't drop a pending Slow sweep.
                                status.fetch_max(
                                    crate::context::WorkspaceDiagnosticLevel::Fast.to_u8(),
                                    Ordering::AcqRel,
                                );
                                if lsp_features.supports_refresh_diagnostic() {
                                    client.refresh_workspace_diagnostics();
                                }
                            }
                        }
                        _ = cancel_token.cancelled() => {}
                        _ = shutdown.cancelled() => {}
                    }
                });
            }

            self.refresh_dirty_state().await;

            // Always notify waiters so they can re-check the condition.
            self.reindex_notify.notify_waiters();
        }
    }

    async fn refresh_dirty_state(&self) {
        // Publish while holding both locks, or concurrent callers can
        let pending = self.pending_files.lock().await;
        let reindexing = self.reindexing_files.lock().await;

        let has_pending_file_work = !pending.is_empty() || !reindexing.is_empty();
        let has_in_flight_changes = self.in_flight_changes.load(Ordering::Acquire) > 0;

        self.has_pending_changes.store(
            has_pending_file_work || has_in_flight_changes,
            Ordering::Release,
        );

        // `in_flight_changes` is not covered by the locks above, so a
        if self.in_flight_changes.load(Ordering::Acquire) > 0 {
            self.has_pending_changes.store(true, Ordering::Release);
        }
    }
}

/// Keeps the ripple off the write lock while one request takes its read lock.
pub struct ReaderHandoff {
    analysis: Arc<DebouncedAnalysis>,
}

impl Drop for ReaderHandoff {
    fn drop(&mut self) {
        if self.analysis.pending_readers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.analysis.readers_idle_notify.notify_waiters();
        }
    }
}

pub struct InFlightChangeGuard {
    analysis: Option<Arc<DebouncedAnalysis>>,
    count: usize,
}

impl InFlightChangeGuard {
    fn new(analysis: Arc<DebouncedAnalysis>, count: usize) -> Self {
        Self {
            analysis: Some(analysis),
            count,
        }
    }

    pub async fn finish(mut self) {
        if let Some(analysis) = self.analysis.take() {
            analysis.finish_in_flight_changes(self.count).await;
        }
    }
}

impl Drop for InFlightChangeGuard {
    fn drop(&mut self) {
        let Some(analysis) = self.analysis.take() else {
            return;
        };
        let count = self.count;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    analysis.finish_in_flight_changes(count).await;
                });
            }
            Err(err) => {
                log::error!(
                    "LS_INFLIGHT_GUARD_DROP_FAILED could not settle {} in-flight changes without a Tokio runtime: {}",
                    count,
                    err
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;
    use std::time::{Duration, Instant};

    use super::{MAX_RIPPLE_DEFERRAL, READER_HANDOFF_GRACE};

    use glua_code_analysis::{DiagnosticCode, EmmyLuaAnalysis, FileId, file_path_to_uri};
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::Uri;
    use lsp_types::{ClientCapabilities, Diagnostic, NumberOrString};
    use std::str::FromStr;
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    use crate::context::{ClientProxy, FileDiagnostic, LspFeatures, StatusBar};

    use super::DebouncedAnalysis;

    fn test_lsp_features() -> Arc<LspFeatures> {
        Arc::new(LspFeatures::new(ClientCapabilities::default()))
    }

    fn test_debounced_analysis() -> Arc<DebouncedAnalysis> {
        let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
        let (connection, _peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone(), true));
        let file_diagnostic = FileDiagnostic::new(analysis.clone(), status_bar, client.clone());
        Arc::new(DebouncedAnalysis::new(
            analysis,
            0,
            CancellationToken::new(),
            client,
            file_diagnostic.shared_diagnostic_data_cache(),
            Arc::new(AtomicU8::new(0)),
            test_lsp_features(),
        ))
    }

    fn has_diagnostic_code(diagnostics: &[Diagnostic], code: DiagnosticCode) -> bool {
        let code = Some(NumberOrString::String(code.get_name().to_string()));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    #[gtest]
    fn in_flight_guard_finish_clears_dirty_state() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let guard = debounced_analysis.begin_in_flight_change();

            verify_that!(debounced_analysis.in_flight_change_count(), eq(1))?;
            verify_that!(debounced_analysis.is_dirty(), eq(true))?;

            guard.finish().await;

            verify_that!(debounced_analysis.in_flight_change_count(), eq(0))?;
            verify_that!(debounced_analysis.is_dirty(), eq(false))?;
            Ok(())
        })
    }

    #[gtest]
    fn in_flight_guard_drop_eventually_clears_dirty_state() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let guard = debounced_analysis.begin_in_flight_change();

            verify_that!(debounced_analysis.in_flight_change_count(), eq(1))?;
            drop(guard);

            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if debounced_analysis.in_flight_change_count() == 0
                        && !debounced_analysis.is_dirty()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("drop guard should settle in-flight change promptly");

            verify_that!(debounced_analysis.in_flight_change_count(), eq(0))?;
            verify_that!(debounced_analysis.is_dirty(), eq(false))?;
            Ok(())
        })
    }

    /// Typing must send the ripple back rather than let it start: while it
    /// runs, no keystroke can even be applied.
    #[gtest]
    fn a_new_edit_sends_the_ripple_back() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let uri = Uri::from_str("file:///workspace/edited.lua").expect("uri should parse");

            let scheduler = debounced_analysis.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                scheduler.schedule(FileId { id: 1 }, uri).await;
            });

            let run_the_ripple = tokio::time::timeout(
                Duration::from_millis(500),
                debounced_analysis.ripple_quiet_elapsed(Instant::now()),
            )
            .await
            .expect("an edit should send the ripple back well inside the quiet window");

            verify_that!(run_the_ripple, eq(false))?;
            Ok(())
        })
    }

    /// Sustained typing must not hold diagnostics off indefinitely.
    #[gtest]
    fn a_long_burst_cannot_hold_the_ripple_off_forever() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let uri = Uri::from_str("file:///workspace/edited.lua").expect("uri should parse");
            debounced_analysis.schedule(FileId { id: 1 }, uri).await;

            let started_at = Instant::now()
                .checked_sub(MAX_RIPPLE_DEFERRAL)
                .expect("the deferral cap should fit before now");

            let run_the_ripple = tokio::time::timeout(
                Duration::from_millis(100),
                debounced_analysis.ripple_quiet_elapsed(started_at),
            )
            .await
            .expect("the cap should release the ripple immediately");

            verify_that!(run_the_ripple, eq(true))?;
            Ok(())
        })
    }

    /// The ripple must yield to a request the self-index just released, or the
    /// request queues behind the write lock and waits out the ripple anyway.
    #[gtest]
    fn the_ripple_waits_for_an_outstanding_reader() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();

            // Nothing outstanding: the ripple must not pay the grace.
            tokio::time::timeout(
                Duration::from_millis(50),
                debounced_analysis.await_reader_handoff(),
            )
            .await
            .expect("no readers should let the ripple straight through");

            let handoff = debounced_analysis.begin_reader_handoff();
            let held = tokio::time::timeout(
                Duration::from_millis(50),
                debounced_analysis.await_reader_handoff(),
            )
            .await;
            verify_that!(held.is_err(), eq(true))?;

            drop(handoff);
            tokio::time::timeout(
                Duration::from_millis(250),
                debounced_analysis.await_reader_handoff(),
            )
            .await
            .expect("dropping the last handoff should release the ripple");

            Ok(())
        })
    }

    /// A stream of requests must not starve the ripple.
    #[gtest]
    fn an_outstanding_reader_only_delays_the_ripple_by_the_grace() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let _never_dropped = debounced_analysis.begin_reader_handoff();

            let started_at = Instant::now();
            debounced_analysis.await_reader_handoff().await;

            verify_that!(started_at.elapsed() >= READER_HANDOFF_GRACE, eq(true))?;
            verify_that!(started_at.elapsed() < READER_HANDOFF_GRACE * 4, eq(true))?;
            Ok(())
        })
    }

    /// The point of the per-file gate: an edit to one document must not park
    #[gtest]
    fn a_pending_edit_blocks_only_its_own_document() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let edited = Uri::from_str("file:///workspace/edited.lua").expect("uri should parse");
            let untouched =
                Uri::from_str("file:///workspace/untouched.lua").expect("uri should parse");

            debounced_analysis
                .schedule(FileId { id: 1 }, edited.clone())
                .await;

            let cancel = CancellationToken::new();
            let untouched_answered = tokio::time::timeout(
                Duration::from_millis(250),
                debounced_analysis.wait_until_file_fresh_for(
                    &cancel,
                    "textDocument/completion",
                    &untouched,
                ),
            )
            .await;
            verify_that!(untouched_answered.unwrap_or(false), eq(true))?;

            let edited_answered = tokio::time::timeout(
                Duration::from_millis(250),
                debounced_analysis.wait_until_file_fresh_for(
                    &cancel,
                    "textDocument/completion",
                    &edited,
                ),
            )
            .await;
            verify_that!(edited_answered.is_err(), eq(true))?;
            Ok(())
        })
    }

    #[gtest]
    fn finish_in_flight_changes_saturates_underflow() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();

            debounced_analysis.finish_in_flight_changes(1).await;

            verify_that!(debounced_analysis.in_flight_change_count(), eq(0))?;
            verify_that!(debounced_analysis.is_dirty(), eq(false))?;
            Ok(())
        })
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
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
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
                Arc::new(AtomicU8::new(0)),
                test_lsp_features(),
            );
            verify_that!(
                debounced_analysis
                    .reindex_files_without_queuing(vec![api_file_id], vec![api_file_id])
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
