use glua_code_analysis::{EmmyLuaAnalysis, FileId, Slice, UpdateProgress};
use lsp_types::Uri;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, FileDiagnostic, file_diagnostic::SharedDiagnosticDataCache};

const FRESHNESS_STUCK_WARN_AFTER: Duration = Duration::from_secs(5);

/// How long one slice of a re-analysis holds the analysis write lock. A
/// request waits for at most about one slice.
const SLICE: Duration = Duration::from_millis(10);

/// How long the user must stay idle after a reindex before the whole workspace
/// is re-diagnosed.
const IDLE_WORKSPACE_DIAGNOSTIC_DELAY: Duration = Duration::from_millis(2000);

/// Breakdown of a freshness wait by observed state, so one summary line
/// can report where the wait went without per-iteration logging.
///
/// `in_flight` covers text applied by no task yet (`in_flight_changes > 0`);
/// `blocked` covers the self-index not having rebuilt this document's own
/// entries yet (or, for workspace waits, the ripple owed). `total` is the
/// whole wait.
#[derive(Default, Clone, Copy)]
pub struct FreshWaitBreakdown {
    pub total: Duration,
    pub in_flight: Duration,
    pub blocked: Duration,
}

/// Outcome of a freshness wait.
///
/// `Fresh` means the analysis caught up with the client's text. `Cancelled`
/// means the request's token fired first. `LoopDead` means the debounce
/// supervisor gave up ([`Self::note_debounce_loop_dead`]), so freshness can
/// never arrive and the caller must not diagnose stale data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Cancelled,
    LoopDead,
}

/// Debounced analysis: accumulates file IDs from rapid edits, takes them into
/// the re-analysis once the user pauses typing, and settles it in slices that
/// release the analysis lock between them.
pub struct DebouncedAnalysis {
    pending_files: Mutex<HashSet<FileId>>,
    reindexing_files: Mutex<HashSet<FileId>>,
    /// Documents whose own index entries do not match their text yet, either
    /// because their edit is still queued or because the batch re-indexing them
    /// has not reached them.
    ///
    /// Holds the URI the *client* used, so a request can be tested against its
    /// own params without taking the analysis lock — which the re-index holds
    /// for its whole duration, so resolving a file id first would wait out
    /// exactly what this exists to avoid. Keyed by file id so entries are
    /// cleared by identity rather than by matching that URI against the one the
    /// VFS derived from a path, which need not be spelled the same way.
    blocked_documents: Mutex<HashMap<FileId, Uri>>,
    /// True when document changes have arrived but reindex has not yet completed.
    /// Set synchronously by `begin_in_flight_change()` (called inline in the
    /// notification handler, before the didChange task is spawned) so that any
    /// request handler dispatched afterwards sees the flag immediately.
    has_pending_changes: AtomicBool,
    /// True while a re-analysis is settling in slices.
    settling: AtomicBool,
    /// Files whose diagnostics wait for them to settle: the next slice settles
    /// them first.
    prioritised: std::sync::Mutex<Vec<FileId>>,
    /// False once the supervisor has given up restarting the debounce loop
    /// (see `LS_DEBOUNCE_LOOP_DEAD`). Freshness can then never arrive, so
    /// waiters fail fast instead of parking until their request is cancelled.
    /// Set once, never re-armed: a dead loop stays dead for process lifetime.
    debounce_loop_alive: AtomicBool,
    in_flight_changes: AtomicUsize,
    /// Requests waiting for an edited document to be answerable. While any
    /// waits, a debounce ends at once: the request needs the re-analysis now,
    /// and only background propagation is worth coalescing.
    waiting_requests: AtomicUsize,
    /// A request was refused as stale while edits were pending: the client
    /// re-sends it, so the next debounce ends at once, as for a waiting one.
    refused_request: AtomicBool,
    notify: Notify,
    reindex_notify: Notify,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shared_diagnostic_data_cache: SharedDiagnosticDataCache,
    /// Re-diagnoses the files an edit's re-analysis reports as affected.
    file_diagnostic: Arc<FileDiagnostic>,
    debounce_duration: Duration,
    shutdown: CancellationToken,
    client: Arc<ClientProxy>,
    workspace_diagnostic_level: Arc<AtomicU8>,
    lsp_features: Arc<crate::context::LspFeatures>,
    /// Entries into [`Self::wait_until_fresh_for`], so a test can synchronise
    /// on a handler having reached the wait instead of on a deadline.
    #[cfg(test)]
    freshness_waits: AtomicUsize,
    /// Debounces a waiting request ended early, so a test can tell one did.
    #[cfg(test)]
    expedited_debounces: AtomicUsize,
    /// Wake-ups that restarted a debounce's timer, so a test can tell a
    /// request that did not wait left the debounce alone.
    #[cfg(test)]
    debounce_wakes: AtomicUsize,
}

/// Counts a request among [`DebouncedAnalysis::waiting_requests`] until
/// dropped.
struct WaitingRequest<'a>(&'a DebouncedAnalysis);

impl Drop for WaitingRequest<'_> {
    fn drop(&mut self) {
        self.0.waiting_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

impl DebouncedAnalysis {
    pub(crate) fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        debounce_ms: u64,
        shutdown: CancellationToken,
        client: Arc<ClientProxy>,
        file_diagnostic: Arc<FileDiagnostic>,
        shared_diagnostic_data_cache: SharedDiagnosticDataCache,
        workspace_diagnostic_level: Arc<AtomicU8>,
        lsp_features: Arc<crate::context::LspFeatures>,
    ) -> Self {
        Self {
            pending_files: Mutex::new(HashSet::new()),
            reindexing_files: Mutex::new(HashSet::new()),
            blocked_documents: Mutex::new(HashMap::new()),
            has_pending_changes: AtomicBool::new(false),
            settling: AtomicBool::new(false),
            prioritised: std::sync::Mutex::default(),
            debounce_loop_alive: AtomicBool::new(true),
            in_flight_changes: AtomicUsize::new(0),
            waiting_requests: AtomicUsize::new(0),
            refused_request: AtomicBool::new(false),
            notify: Notify::new(),
            reindex_notify: Notify::new(),
            analysis,
            shared_diagnostic_data_cache,
            file_diagnostic,
            debounce_duration: Duration::from_millis(debounce_ms),
            shutdown,
            client,
            workspace_diagnostic_level,
            lsp_features,
            #[cfg(test)]
            freshness_waits: AtomicUsize::new(0),
            #[cfg(test)]
            expedited_debounces: AtomicUsize::new(0),
            #[cfg(test)]
            debounce_wakes: AtomicUsize::new(0),
        }
    }

    /// Notes a request refused because its document is mid-edit, which the
    /// client re-sends: a debounce of the pending edits ends at once.
    pub async fn request_refused(&self) {
        if self.pending_files.lock().await.is_empty() {
            return;
        }
        self.refused_request.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Counts a request as waiting for an edited document and wakes a
    /// debounce in progress, which then ends at once.
    fn request_waiting(&self) -> WaitingRequest<'_> {
        self.waiting_requests.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
        WaitingRequest(self)
    }

    /// Add a file to the pending reindex set and reset the debounce timer.
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
    ///
    /// Called **synchronously** from the notification handler (inline, before
    /// spawning the didChange task) so that request handlers dispatched
    /// immediately afterward see the dirty flag and wait for reindex instead
    /// of computing on stale analysis data.
    pub fn begin_in_flight_change(self: &Arc<Self>) -> InFlightChangeGuard {
        self.in_flight_changes.fetch_add(1, Ordering::AcqRel);
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
        InFlightChangeGuard::new(self.clone(), 1)
    }

    /// Record that the supervising loop has given up restarting the debounce
    /// loop (`LS_DEBOUNCE_LOOP_DEAD`). Freshness can never arrive afterwards,
    /// so [`Self::wait_until_fresh_for`] and [`Self::wait_until_file_fresh_for`]
    /// fail fast instead of parking until the request is cancelled. Wakes
    /// parked waiters: a waiter past its stuck-warning timer has no active
    /// sleep left and would otherwise never re-check the flag.
    pub(crate) fn note_debounce_loop_dead(&self) {
        self.debounce_loop_alive.store(false, Ordering::Release);
        self.reindex_notify.notify_waiters();
    }

    fn debounce_loop_alive(&self) -> bool {
        self.debounce_loop_alive.load(Ordering::Acquire)
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

    /// Check whether document changes are pending reindex.
    ///
    /// Handlers that need consistent tree + index data (e.g. semantic tokens)
    /// can use this to decide whether to serve stale results or return `None`
    /// so the client keeps its previous state.
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

    /// Wait until all pending document changes have been reindexed.
    ///
    /// Returns `Fresh` when the analysis is current, `Cancelled` if the cancel
    /// token fired first, and `LoopDead` if the debounce loop is dead.
    /// Uses `enable()` so that `notify_waiters()` wakeups are
    /// not lost between creating the `Notified` future and polling it.
    pub async fn wait_until_fresh_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
    ) -> Freshness {
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
                return Freshness::Fresh;
            }

            if !self.debounce_loop_alive() {
                return Freshness::LoopDead;
            }

            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            tokio::select! {
                _ = notified => {} // re-check
                _ = cancel_token.cancelled() => return Freshness::Cancelled,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                }
            }
        }
    }

    /// Wait until the document at `uri` has index entries matching its text.
    ///
    /// A request positioned inside a file needs that file's entries to line up
    /// with the tree it is resolving against — they are keyed by position, so an
    /// edit that shifts offsets is what makes them stop matching, and answering
    /// from the old ones is what silently returns a thinner list. It does *not*
    /// need the edit's dependency ripple to have finished; that settles other
    /// files' inferences, and waiting for it costs seconds on a large gamemode
    /// for an answer that is already correct.
    ///
    /// Callers with no URI to aim at want [`wait_until_fresh_for`] instead.
    ///
    /// [`wait_until_fresh_for`]: Self::wait_until_fresh_for
    pub async fn wait_until_file_fresh_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        uri: &Uri,
    ) -> Freshness {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;
        let mut waiting = None;

        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.file_is_answerable(uri).await {
                return Freshness::Fresh;
            }

            if !self.debounce_loop_alive() {
                return Freshness::LoopDead;
            }

            // Only a request that has to wait needs the re-analysis now.
            waiting.get_or_insert_with(|| self.request_waiting());
            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            tokio::select! {
                _ = notified => {}
                _ = cancel_token.cancelled() => return Freshness::Cancelled,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                }
            }
        }
    }

    /// Wait until the document at `uri` has nothing left to settle: its own
    /// entries match its text and the re-analysis in progress has no work
    /// left in it. While it waits, its work goes first.
    ///
    /// Diagnostics need this and no more: the rest of the re-analysis can
    /// still move what they read, and reports those files affected when it
    /// settles.
    pub async fn wait_until_file_settled_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        uri: &Uri,
    ) -> Freshness {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;
        let mut waiting = None;

        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.file_is_answerable(uri).await && !self.file_is_settling(uri).await {
                return Freshness::Fresh;
            }

            if !self.debounce_loop_alive() {
                return Freshness::LoopDead;
            }

            // Only a request that has to wait needs the re-analysis now.
            waiting.get_or_insert_with(|| self.request_waiting());
            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            tokio::select! {
                _ = notified => {}
                _ = cancel_token.cancelled() => return Freshness::Cancelled,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                }
            }
        }
    }

    /// Whether the re-analysis in progress still has work in the document at
    /// `uri`; if it has, the next slice settles that work first.
    async fn file_is_settling(&self, uri: &Uri) -> bool {
        if !self.settling.load(Ordering::Acquire) {
            return false;
        }
        let analysis = self.analysis.read().await;
        let Some(file_id) = analysis.get_file_id(uri) else {
            return false;
        };
        self.file_id_is_settling(&analysis, file_id)
    }

    /// Whether `analysis`'s re-analysis in progress still has work in
    /// `file_id`; if it has, the next slice settles that work first.
    fn file_id_is_settling(&self, analysis: &EmmyLuaAnalysis, file_id: FileId) -> bool {
        let settling = analysis.compilation.is_settling(file_id);
        if settling && let Ok(mut prioritised) = self.prioritised.lock() {
            prioritised.push(file_id);
        }
        settling
    }

    /// Profiled form of [`Self::wait_until_fresh_for`]: identical wake-up
    /// behavior, plus time accumulated by observed state.
    pub async fn wait_until_fresh_for_profiled(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
    ) -> (Freshness, FreshWaitBreakdown) {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;
        let mut in_flight = Duration::ZERO;
        let mut blocked = Duration::ZERO;

        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if !self.has_pending_changes.load(Ordering::Acquire) {
                return (
                    Freshness::Fresh,
                    FreshWaitBreakdown {
                        total: started_at.elapsed(),
                        in_flight,
                        blocked,
                    },
                );
            }

            if !self.debounce_loop_alive() {
                return (
                    Freshness::LoopDead,
                    FreshWaitBreakdown {
                        total: started_at.elapsed(),
                        in_flight,
                        blocked,
                    },
                );
            }

            let iter_start = Instant::now();
            let observed_in_flight = self.in_flight_changes.load(Ordering::Acquire) > 0;
            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            let cancelled = tokio::select! {
                _ = notified => false,
                _ = cancel_token.cancelled() => true,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                    false
                }
            };
            let elapsed = iter_start.elapsed();
            if observed_in_flight {
                in_flight += elapsed;
            } else {
                blocked += elapsed;
            }
            if cancelled {
                return (
                    Freshness::Cancelled,
                    FreshWaitBreakdown {
                        total: started_at.elapsed(),
                        in_flight,
                        blocked,
                    },
                );
            }
        }
    }

    /// Profiled form of [`Self::wait_until_file_fresh_for`]: identical
    /// wake-up behavior, plus time accumulated by observed state.
    pub async fn wait_until_file_fresh_for_profiled(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        uri: &Uri,
    ) -> (Freshness, FreshWaitBreakdown) {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;
        let mut in_flight = Duration::ZERO;
        let mut blocked = Duration::ZERO;
        let mut waiting = None;

        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.file_is_answerable(uri).await {
                return (
                    Freshness::Fresh,
                    FreshWaitBreakdown {
                        total: started_at.elapsed(),
                        in_flight,
                        blocked,
                    },
                );
            }

            if !self.debounce_loop_alive() {
                return (
                    Freshness::LoopDead,
                    FreshWaitBreakdown {
                        total: started_at.elapsed(),
                        in_flight,
                        blocked,
                    },
                );
            }

            // Only a request that has to wait needs the re-analysis now.
            waiting.get_or_insert_with(|| self.request_waiting());
            let iter_start = Instant::now();
            let observed_in_flight = self.in_flight_changes.load(Ordering::Acquire) > 0;
            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            let cancelled = tokio::select! {
                _ = notified => false,
                _ = cancel_token.cancelled() => true,
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                    false
                }
            };
            let elapsed = iter_start.elapsed();
            if observed_in_flight {
                in_flight += elapsed;
            } else {
                blocked += elapsed;
            }
            if cancelled {
                return (
                    Freshness::Cancelled,
                    FreshWaitBreakdown {
                        total: started_at.elapsed(),
                        in_flight,
                        blocked,
                    },
                );
            }
        }
    }

    /// Whether a request aimed at `uri` can be answered against entries that
    /// match its text.
    ///
    /// Workspace-wide dirtiness is the wrong question for a request positioned
    /// inside one document: an edit to some other file leaves this one's entries
    /// matching its own text, so refusing it buys nothing and blanks the answer
    /// for every open document whenever anything is typed anywhere.
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

    /// Wait until the given file is no longer pending reindex and the
    /// re-analysis in progress has no work left in it, which it then settles
    /// first. Returns `LoopDead` without diagnosing when the debounce loop is
    /// dead (freshness can never arrive); callers must abort rather than
    /// diagnose stale data.
    pub async fn wait_for_reindex(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Freshness {
        loop {
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let is_pending = {
                let pending = self.pending_files.lock().await;
                let reindexing = self.reindexing_files.lock().await;
                pending.contains(&file_id) || reindexing.contains(&file_id)
            } || (self.settling.load(Ordering::Acquire)
                && self.file_id_is_settling(&*self.analysis.read().await, file_id));
            if !is_pending {
                return Freshness::Fresh;
            }
            if !self.debounce_loop_alive() {
                return Freshness::LoopDead;
            }
            tokio::select! {
                _ = notified => {}
                _ = cancel_token.cancelled() => return Freshness::Cancelled,
            }
        }
    }

    /// Arm an idle workspace diagnostic refresh so closed files hit by
    /// cross-file changes get re-diagnosed once typing pauses.
    ///
    /// Cancels the previously armed timer, if any. Clones the arcs it needs and
    /// takes no lock: the caller may still hold one.
    fn arm_idle_workspace_diagnostic(&self, token_slot: &mut Option<CancellationToken>) {
        if let Some(token) = token_slot.take() {
            token.cancel();
        }
        let cancel_token = CancellationToken::new();
        *token_slot = Some(cancel_token.clone());

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

    /// Takes the edited files into the re-analysis under the write lock, on
    /// a blocking thread so neither the wait for the lock nor the work stalls
    /// the Tokio workers; the re-analysis then settles in slices. Returns
    /// whether it took them.
    async fn take_edits(
        &self,
        file_ids: Vec<FileId>,
        timing: Option<&mut crate::util::ls_profile::WriteTimings>,
    ) -> bool {
        let profile = timing.is_some();
        let analysis = self.analysis.clone();
        let cache = self.shared_diagnostic_data_cache.clone();

        tokio::select! {
            _ = self.shutdown.cancelled() => false,
            result = tokio::task::spawn_blocking(move || {
                let wait_start = profile.then(Instant::now);
                let mut guard = analysis.blocking_write();
                let wait = wait_start.map(|start| start.elapsed()).unwrap_or_default();
                let hold_start = profile.then(Instant::now);
                guard.begin_reindex(file_ids);
                // Under the write lock, so no reader sees the new index next
                // to stale shared diagnostic data.
                cache.invalidate();
                let hold = hold_start.map(|start| start.elapsed()).unwrap_or_default();
                (wait, hold)
            }) => match result {
                Ok((wait, hold)) => {
                    if let Some(timing) = timing {
                        timing.wait = wait;
                        timing.hold = hold;
                    }
                    self.settling.store(true, Ordering::Release);
                    true
                }
                Err(err) => {
                    log::error!("reindex task failed: {}", err);
                    false
                }
            }
        }
    }

    /// Advances the re-analysis by one slice under the write lock, the
    /// prioritised files first. Returns where it stands and which of
    /// `edited` settled ahead of the rest; `None` on shutdown or failure.
    async fn advance_slice(&self, edited: Vec<FileId>) -> Option<(UpdateProgress, Vec<FileId>)> {
        let prioritised = self
            .prioritised
            .lock()
            .map(|mut prioritised| std::mem::take(&mut *prioritised))
            .unwrap_or_default();
        let analysis = self.analysis.clone();
        let cache = self.shared_diagnostic_data_cache.clone();

        tokio::select! {
            _ = self.shutdown.cancelled() => None,
            result = tokio::task::spawn_blocking(move || {
                let wait_start = crate::util::ls_profile::profile_instant();
                let mut guard = analysis.blocking_write();
                let hold_start = crate::util::ls_profile::profile_instant();
                guard.compilation.prioritise_update(&prioritised);
                let progress = guard.advance_reindex(Slice::Until(Instant::now() + SLICE));
                cache.invalidate();
                let settled_first = edited
                    .into_iter()
                    .filter(|file_id| !guard.compilation.settles_first(*file_id))
                    .collect::<Vec<_>>();
                if let (Some(wait_start), Some(hold_start)) = (wait_start, hold_start) {
                    crate::util::ls_profile::emit_ls_slice(
                        hold_start - wait_start,
                        hold_start.elapsed(),
                        &progress,
                        settled_first.len(),
                    );
                }
                (progress, settled_first)
            }) => match result {
                Ok(outcome) => Some(outcome),
                Err(err) => {
                    log::error!("reindex slice failed: {}", err);
                    None
                }
            }
        }
    }

    /// Takes in the files whose text changed without a schedule reaching the
    /// loop, so a re-analysis never waits on text it has not taken.
    async fn take_unscheduled_text(&self) -> Vec<FileId> {
        let file_ids = self
            .analysis
            .read()
            .await
            .compilation
            .get_db()
            .get_vfs()
            .files_changed_since_taken();
        if !file_ids.is_empty() && !self.take_edits(file_ids.clone(), None).await {
            return Vec::new();
        }
        file_ids
    }

    /// Lets requests aimed at `file_ids` through: their own entries match
    /// their text.
    async fn release_documents(&self, file_ids: &[FileId]) {
        if file_ids.is_empty() {
            return;
        }
        let mut reindexing = self.reindexing_files.lock().await;
        let mut blocked = self.blocked_documents.lock().await;
        for id in file_ids {
            reindexing.remove(id);
            blocked.remove(id);
        }
    }

    /// Re-diagnoses the files a re-analysis reported affected, the edited
    /// documents among them when what their diagnostics read moved after the
    /// client pulled them, and refreshes the client's derived views when the
    /// edit reached other files.
    async fn refresh_affected(
        &self,
        edited: &[FileId],
        affected: Vec<FileId>,
        idle_workspace_diagnostic_token: &mut Option<CancellationToken>,
    ) {
        if affected.is_empty() {
            return;
        }
        if affected.iter().any(|file_id| !edited.contains(file_id)) {
            if self.lsp_features.supports_semantic_tokens_refresh() {
                self.client.refresh_semantic_tokens();
            }
            if self.lsp_features.supports_inlay_hint_refresh() {
                self.client.refresh_inlay_hints();
            }
        }
        let interval = self
            .analysis
            .read()
            .await
            .get_emmyrc()
            .diagnostics
            .diagnostic_interval
            .unwrap_or(500);
        self.file_diagnostic
            .note_scoped_sweep_files(affected.iter().copied())
            .await;
        self.file_diagnostic
            .add_files_diagnostic_task(affected, interval, None)
            .await;
        self.arm_idle_workspace_diagnostic(idle_workspace_diagnostic_token);
    }

    /// Background loop: waits for edits, debounces them, takes them into the
    /// re-analysis, then settles it in slices that release the analysis lock,
    /// so requests run between them. New edits pre-empt the slices and are
    /// taken in, settling first. Spawn this once at server startup.
    pub async fn run(&self) {
        let mut idle_workspace_diagnostic_token: Option<CancellationToken> = None;
        // The edited files of the re-analysis in progress.
        let mut edited: Vec<FileId> = Vec::new();
        let mut cycle = None;
        let mut text_changed = false;
        loop {
            // Register before testing the condition: `notify_waiters()` stores
            // no permit, so a signal landing in between would be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let scheduled = !self.pending_files.lock().await.is_empty();
            let settling = self.settling.load(Ordering::Acquire);
            if !scheduled && (!settling || text_changed) {
                let needs_work = self.has_pending_changes.load(Ordering::Acquire) && !settling;
                if !needs_work {
                    tokio::select! {
                        _ = notified => {}
                        _ = self.shutdown.cancelled() => return,
                        // Text that changed without a schedule reaching the
                        // loop is taken in rather than left blocking slices.
                        _ = tokio::time::sleep(self.debounce_duration), if text_changed => {
                            let taken = self.take_unscheduled_text().await;
                            edited.extend(taken);
                        }
                    }
                    text_changed = false;
                    continue;
                }
            }

            if scheduled || !settling {
                // Debounce: keep resetting the timer while new events arrive.
                cycle = crate::util::ls_profile::ls_profile_enabled()
                    .then(crate::util::ls_profile::next_reindex_cycle);
                let debounce_start = crate::util::ls_profile::profile_instant();
                loop {
                    let notified = self.notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.waiting_requests.load(Ordering::Acquire) > 0
                        || self.refused_request.swap(false, Ordering::AcqRel)
                    {
                        #[cfg(test)]
                        self.expedited_debounces.fetch_add(1, Ordering::AcqRel);
                        break;
                    }
                    tokio::select! {
                        biased;
                        _ = self.shutdown.cancelled() => return,
                        _ = notified => {
                            #[cfg(test)]
                            self.debounce_wakes.fetch_add(1, Ordering::AcqRel);
                            continue;
                        }
                        _ = tokio::time::sleep(self.debounce_duration) => break,
                    }
                }
                let debounce_elapsed = debounce_start
                    .map(|start| start.elapsed())
                    .unwrap_or_default();

                let file_ids: Vec<FileId> = {
                    let mut pending = self.pending_files.lock().await;
                    let mut reindexing = self.reindexing_files.lock().await;
                    let mut ids: Vec<FileId> = pending.drain().collect();
                    ids.sort();
                    reindexing.extend(ids.iter().copied());
                    ids
                };

                if !file_ids.is_empty() {
                    log::info!(
                        "debounced reindex: {} file(s) after {}ms quiet",
                        file_ids.len(),
                        self.debounce_duration.as_millis()
                    );
                    let mut timings = crate::util::ls_profile::WriteTimings::default();
                    let taken = self
                        .take_edits(file_ids.clone(), cycle.map(|_| &mut timings))
                        .await;
                    if let Some(cycle) = cycle {
                        crate::util::ls_profile::emit_ls_reindex(
                            cycle,
                            debounce_elapsed,
                            timings.wait,
                            timings.hold,
                        );
                    }
                    if taken {
                        edited.extend(file_ids);
                    } else if self.shutdown.is_cancelled() {
                        return;
                    } else {
                        // Freshness waiters are released rather than parked on
                        // files that will not be re-analysed.
                        log::error!(
                            "LS_REINDEX_FAILED reindex of {} file(s) did not start; continuing so freshness waiters are released",
                            file_ids.len()
                        );
                        self.release_documents(&file_ids).await;
                    }
                }
                text_changed = false;
                self.refresh_dirty_state().await;
                self.reindex_notify.notify_waiters();
                continue;
            }

            let Some((progress, settled_first)) = self.advance_slice(edited.clone()).await else {
                if self.shutdown.is_cancelled() {
                    return;
                }
                log::error!(
                    "LS_REINDEX_FAILED a re-analysis slice did not complete; continuing so freshness waiters are released"
                );
                self.settling.store(false, Ordering::Release);
                self.release_documents(&std::mem::take(&mut edited)).await;
                self.refresh_dirty_state().await;
                self.reindex_notify.notify_waiters();
                continue;
            };
            self.release_documents(&settled_first).await;
            match progress {
                UpdateProgress::Settling => {}
                UpdateProgress::Settled(affected) => {
                    self.settling.store(false, Ordering::Release);
                    let settled = std::mem::take(&mut edited);
                    self.release_documents(&settled).await;
                    if let Some(cycle) = cycle.take() {
                        crate::util::ls_profile::emit_ls_settled(cycle, affected.len());
                    }
                    self.refresh_affected(&settled, affected, &mut idle_workspace_diagnostic_token)
                        .await;
                    self.refresh_dirty_state().await;
                }
                // A didChange applied its text; its schedule pre-empts.
                UpdateProgress::TextChanged => text_changed = true,
            }
            // Waiters re-check their condition after every slice: the edited
            // documents or a waited-on file may have settled.
            self.reindex_notify.notify_waiters();
            tokio::task::yield_now().await;
        }
    }

    async fn refresh_dirty_state(&self) {
        // Publish while holding both locks, or concurrent callers can
        // interleave and store a stale reading.
        let pending = self.pending_files.lock().await;
        let reindexing = self.reindexing_files.lock().await;

        let has_pending_file_work =
            !pending.is_empty() || !reindexing.is_empty() || self.settling.load(Ordering::Acquire);
        let has_in_flight_changes = self.in_flight_changes.load(Ordering::Acquire) > 0;

        self.has_pending_changes.store(
            has_pending_file_work || has_in_flight_changes,
            Ordering::Release,
        );

        // `in_flight_changes` is not covered by the locks above, so a
        // concurrent `begin_in_flight_change()` can land between the load and
        // the store and have its `true` overwritten. Re-reading narrows that
        // window rather than closing it; what is guaranteed is only that the
        // flag ends up `true` for any change whose `fetch_add` is visible by
        // the time this second load runs. A change that arrives later still
        // sets the flag itself, and `finish_in_flight_changes` calls back here.
        if self.in_flight_changes.load(Ordering::Acquire) > 0 {
            self.has_pending_changes.store(true, Ordering::Release);
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
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::{Duration, Instant};

    use super::Freshness;

    use glua_code_analysis::{DiagnosticCode, EmmyLuaAnalysis, FileId, file_path_to_uri};
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::Uri;
    use lsp_types::{ClientCapabilities, Diagnostic, NumberOrString, PublishDiagnosticsParams};
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
        let file_diagnostic = Arc::new(FileDiagnostic::new(
            analysis.clone(),
            status_bar,
            client.clone(),
            false,
        ));
        let shared_diagnostic_data_cache = file_diagnostic.shared_diagnostic_data_cache();
        Arc::new(DebouncedAnalysis::new(
            analysis,
            0,
            CancellationToken::new(),
            client,
            file_diagnostic,
            shared_diagnostic_data_cache,
            Arc::new(AtomicU8::new(0)),
            test_lsp_features(),
        ))
    }

    /// Takes `file_ids` in and settles the re-analysis slice by slice, as the
    /// loop does. Returns the files it reported affected.
    async fn settle(debounced_analysis: &DebouncedAnalysis, file_ids: Vec<FileId>) -> Vec<FileId> {
        assert!(debounced_analysis.take_edits(file_ids, None).await);
        loop {
            let (progress, _) = debounced_analysis
                .advance_slice(Vec::new())
                .await
                .expect("a slice completes");
            if let glua_code_analysis::UpdateProgress::Settled(affected) = progress {
                return affected;
            }
        }
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

    /// Once the supervisor has given up on the loop, freshness can never
    /// arrive: waiters must fail fast instead of parking until cancelled.
    #[gtest]
    fn dead_loop_fails_freshness_wait_fast() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            debounced_analysis
                .has_pending_changes
                .store(true, Ordering::Release);
            debounced_analysis.note_debounce_loop_dead();
            let token = CancellationToken::new();
            let fresh = tokio::time::timeout(
                Duration::from_secs(10),
                debounced_analysis.wait_until_fresh_for(&token, "test"),
            )
            .await
            .expect("freshness wait must fail fast once the loop is dead");
            verify_that!(fresh, eq(Freshness::LoopDead))?;
            Ok(())
        })
    }

    /// A waiter already parked when the loop dies must also be released: past
    /// the stuck warning it has no active timer left, so only a wakeup lets
    /// it re-check the alive flag.
    #[gtest]
    fn dead_loop_releases_already_parked_waiter() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            debounced_analysis
                .has_pending_changes
                .store(true, Ordering::Release);
            let token = CancellationToken::new();
            let waiter = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move {
                    debounced_analysis
                        .wait_until_fresh_for(&token, "test")
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                while debounced_analysis.freshness_wait_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("waiter should reach the freshness wait promptly");
            // Let the waiter complete a full park iteration first, so this
            // exercises release-from-parked rather than fail-fast-on-entry
            // (without the wakeup it would sit out the 5s stuck timer here).
            let started = Instant::now();
            tokio::time::sleep(Duration::from_millis(200)).await;
            debounced_analysis.note_debounce_loop_dead();
            let fresh = tokio::time::timeout(Duration::from_secs(10), waiter)
                .await
                .expect("parked waiter must be released promptly")
                .expect("waiter task must not panic");
            verify_that!(fresh, eq(Freshness::LoopDead))?;
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "release took {:?}, expected prompt wakeup not the stuck timer",
                started.elapsed()
            );
            Ok(())
        })
    }

    /// `wait_for_reindex` parks diagnostic tasks, not requests, so it has no
    /// cancel token of its own to rely on: without the alive check a dead
    /// loop parks it forever on a file that will never reindex.
    #[gtest]
    fn dead_loop_releases_reindex_waiter() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            debounced_analysis
                .pending_files
                .lock()
                .await
                .insert(FileId::new(7));
            let waiter = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move {
                    debounced_analysis
                        .wait_for_reindex(FileId::new(7), CancellationToken::new())
                        .await
                }
            });
            let started = Instant::now();
            tokio::time::sleep(Duration::from_millis(200)).await;
            debounced_analysis.note_debounce_loop_dead();
            let reindexed = tokio::time::timeout(Duration::from_secs(10), waiter)
                .await
                .expect("parked reindex waiter must be released promptly")
                .expect("waiter task must not panic");
            verify_that!(reindexed, eq(Freshness::LoopDead))?;
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "release took {:?}, expected prompt wakeup not the stuck timer",
                started.elapsed()
            );
            Ok(())
        })
    }

    /// The point of the per-file gate: an edit to one document must not park
    /// requests aimed at a different one, and must park requests aimed at
    /// itself until its own entries have been rebuilt.
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
            verify_that!(
                untouched_answered.unwrap_or(Freshness::Cancelled),
                eq(Freshness::Fresh)
            )?;

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
            analysis
                .update_file_by_uri(
                    &api_uri,
                    Some("function NeedsUse() return true end".to_string()),
                )
                .map(|(id, _)| id);

            let user_uri = file_path_to_uri(&workspace.join("lua/autorun/shared/user.lua"))
                .expect("user URI should parse");
            analysis
                .update_file_by_uri(&user_uri, Some("NeedsUse()".to_string()))
                .map(|(id, _)| id);

            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let analysis = Arc::new(RwLock::new(analysis));
            let file_diagnostic = Arc::new(FileDiagnostic::new(
                analysis.clone(),
                status_bar.clone(),
                client.clone(),
                false,
            ));

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

            let shared_diagnostic_data_cache = file_diagnostic.shared_diagnostic_data_cache();
            let debounced_analysis = DebouncedAnalysis::new(
                analysis.clone(),
                0,
                CancellationToken::new(),
                client,
                file_diagnostic.clone(),
                shared_diagnostic_data_cache,
                Arc::new(AtomicU8::new(0)),
                test_lsp_features(),
            );
            settle(&debounced_analysis, vec![api_file_id]).await;

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

    fn publish_params(message: &lsp_server::Message) -> Option<PublishDiagnosticsParams> {
        let lsp_server::Message::Notification(notification) = message else {
            return None;
        };
        if notification.method != "textDocument/publishDiagnostics" {
            return None;
        }
        serde_json::from_value(notification.params.clone()).ok()
    }

    /// A test bed wired so the debounce loop's diagnostic scheduling can be
    /// observed on the client channel: `peer` receives everything
    /// `FileDiagnostic` publishes.
    fn loop_test_debounced_analysis(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    ) -> (Arc<DebouncedAnalysis>, lsp_server::Connection) {
        let (connection, peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone(), true));
        let file_diagnostic = Arc::new(FileDiagnostic::new(
            analysis.clone(),
            status_bar,
            client.clone(),
            false,
        ));
        let shared_diagnostic_data_cache = file_diagnostic.shared_diagnostic_data_cache();
        let debounced_analysis = Arc::new(DebouncedAnalysis::new(
            analysis,
            0,
            CancellationToken::new(),
            client,
            file_diagnostic,
            shared_diagnostic_data_cache,
            Arc::new(AtomicU8::new(0)),
            test_lsp_features(),
        ));
        (debounced_analysis, peer)
    }

    async fn reference_revision(analysis: &RwLock<EmmyLuaAnalysis>, file_id: FileId) -> u64 {
        analysis
            .read()
            .await
            .compilation
            .get_db()
            .get_reference_index()
            .file_reference_revision(file_id)
    }

    /// A rename must reach its readers on the editor path: the consumer's
    /// diagnostics read the renamed global, so the debounced loop re-diagnoses
    /// it, without rebuilding the consumer's own source layer.
    #[gtest]
    fn a_rename_edit_refreshes_its_referencers_without_reindexing() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_rename_refresh");
            analysis.add_main_workspace(workspace.clone());
            analysis
                .diagnostic
                .enable_only(DiagnosticCode::UndefinedGlobal);

            let provider_uri = file_path_to_uri(&workspace.join("lua/rename/provider.lua"))
                .expect("provider URI should parse");
            let consumer_uri = file_path_to_uri(&workspace.join("lua/rename/consumer.lua"))
                .expect("consumer URI should parse");
            analysis
                .update_file_by_uri(&provider_uri, Some("function OldName() end\n".to_string()))
                .map(|(id, _)| id);
            analysis
                .update_file_by_uri(&consumer_uri, Some("OldName()\n".to_string()))
                .map(|(id, _)| id);
            let consumer_id = analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_id(&consumer_uri)
                .expect("consumer file id");

            analysis.diagnose_file(consumer_id, CancellationToken::new());

            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, peer) = loop_test_debounced_analysis(analysis.clone());
            let revision_before = reference_revision(&analysis, consumer_id).await;

            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });

            // didChange equivalent: stage the text, then schedule the file.
            let provider_id = {
                let mut guard = analysis.write().await;
                guard
                    .update_file_text_only(&provider_uri, "function NewName() end\n".to_string())
                    .expect("provider file should still exist")
            };
            debounced_analysis
                .schedule(provider_id, provider_uri.clone())
                .await;

            // Nothing else publishes for the consumer in this test, so its
            // first publish can only come from the loop's affected-file refresh.
            let published = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::task::spawn_blocking(move || {
                    loop {
                        let message = peer.receiver.recv().expect("peer channel should stay open");
                        if let Some(params) = publish_params(&message)
                            && params.uri == consumer_uri
                        {
                            return params;
                        }
                    }
                })
                .await
                .expect("the publish reader task must not panic")
            })
            .await
            .expect("the consumer should be re-published after the rename");
            let undefined_global = DiagnosticCode::UndefinedGlobal.get_name().to_string();
            verify_that!(
                published
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code
                        == Some(NumberOrString::String(undefined_global.clone()))),
                eq(true)
            )?;

            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");

            // The rename leaves the consumer's text alone: its source layer
            // must not have been rebuilt on the way to its refresh.
            let revision_after = reference_revision(&analysis, consumer_id).await;
            verify_that!(revision_after, eq(revision_before))?;
            Ok(())
        })
    }

    /// A request waiting for an edited document ends the debounce at once
    /// instead of waiting it out.
    #[gtest]
    fn a_request_waiting_on_an_edited_document_skips_the_debounce() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_expedite");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/autorun/shared/edited.lua"))
                .expect("URI should parse");
            analysis
                .update_file_by_uri(&uri, Some("Value = 1\n".to_string()))
                .map(|(id, _)| id);
            let analysis = Arc::new(RwLock::new(analysis));
            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let file_diagnostic = Arc::new(FileDiagnostic::new(
                analysis.clone(),
                status_bar,
                client.clone(),
                false,
            ));
            let shared_diagnostic_data_cache = file_diagnostic.shared_diagnostic_data_cache();
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
                shared_diagnostic_data_cache,
                Arc::new(AtomicU8::new(0)),
                test_lsp_features(),
            ));
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });

            let file_id = analysis
                .write()
                .await
                .update_file_text_only(&uri, "Value = 2\n".to_string())
                .expect("the file should still exist");
            debounced_analysis.schedule(file_id, uri.clone()).await;
            let fresh = tokio::time::timeout(
                Duration::from_secs(10),
                debounced_analysis.wait_until_file_fresh_for(
                    &CancellationToken::new(),
                    "textDocument/hover",
                    &uri,
                ),
            )
            .await
            .expect("the request is answered well before the debounce elapses");
            verify_that!(fresh, eq(Freshness::Fresh))?;
            verify_that!(
                debounced_analysis
                    .expedited_debounces
                    .load(Ordering::Acquire),
                eq(1)
            )?;

            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");
            Ok(())
        })
    }

    /// A request to a document with no pending edit is answered without
    /// touching the debounce of another document's edit.
    #[gtest]
    fn a_request_to_an_untouched_document_leaves_the_debounce_alone() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_untouched");
            analysis.add_main_workspace(workspace.clone());
            let edited = file_path_to_uri(&workspace.join("lua/autorun/shared/edited.lua"))
                .expect("URI should parse");
            let untouched = file_path_to_uri(&workspace.join("lua/autorun/shared/untouched.lua"))
                .expect("URI should parse");
            analysis
                .update_file_by_uri(
                    &edited,
                    Some(
                        "Value = 1
"
                        .to_string(),
                    ),
                )
                .map(|(id, _)| id);
            analysis
                .update_file_by_uri(
                    &untouched,
                    Some(
                        "Other = 1
"
                        .to_string(),
                    ),
                )
                .map(|(id, _)| id);
            let analysis = Arc::new(RwLock::new(analysis));
            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let file_diagnostic = Arc::new(FileDiagnostic::new(
                analysis.clone(),
                status_bar,
                client.clone(),
                false,
            ));
            let shared_diagnostic_data_cache = file_diagnostic.shared_diagnostic_data_cache();
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
                shared_diagnostic_data_cache,
                Arc::new(AtomicU8::new(0)),
                test_lsp_features(),
            ));
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });

            let file_id = analysis
                .write()
                .await
                .update_file_text_only(
                    &edited,
                    "Value = 2
"
                    .to_string(),
                )
                .expect("the file should still exist");
            debounced_analysis.schedule(file_id, edited.clone()).await;
            // The loop takes the schedule and sits in the debounce.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let wakes_before = debounced_analysis.debounce_wakes.load(Ordering::Acquire);
            for _ in 0..10 {
                let fresh = debounced_analysis
                    .wait_until_file_fresh_for(
                        &CancellationToken::new(),
                        "textDocument/hover",
                        &untouched,
                    )
                    .await;
                verify_that!(fresh, eq(Freshness::Fresh))?;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            verify_that!(
                debounced_analysis.debounce_wakes.load(Ordering::Acquire),
                eq(wakes_before)
            )?;
            verify_that!(
                debounced_analysis
                    .expedited_debounces
                    .load(Ordering::Acquire),
                eq(0)
            )?;

            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");
            Ok(())
        })
    }

    /// A request refused as stale, which the client re-sends, ends the
    /// debounce of the pending edits at once.
    #[gtest]
    fn a_request_refused_as_stale_skips_the_debounce() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_refused");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/autorun/shared/edited.lua"))
                .expect("URI should parse");
            analysis
                .update_file_by_uri(&uri, Some("Value = 1\n".to_string()))
                .map(|(id, _)| id);
            let analysis = Arc::new(RwLock::new(analysis));
            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let file_diagnostic = Arc::new(FileDiagnostic::new(
                analysis.clone(),
                status_bar,
                client.clone(),
                false,
            ));
            let shared_diagnostic_data_cache = file_diagnostic.shared_diagnostic_data_cache();
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
                shared_diagnostic_data_cache,
                Arc::new(AtomicU8::new(0)),
                test_lsp_features(),
            ));
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });

            let file_id = analysis
                .write()
                .await
                .update_file_text_only(&uri, "Value = 2\n".to_string())
                .expect("the file should still exist");
            debounced_analysis.schedule(file_id, uri.clone()).await;
            debounced_analysis.request_refused().await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !debounced_analysis.file_is_answerable(&uri).await {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the document is answerable well before the debounce elapses");
            verify_that!(
                debounced_analysis
                    .expedited_debounces
                    .load(Ordering::Acquire),
                eq(1)
            )?;

            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");
            Ok(())
        })
    }

    /// A change that moves an inference two files away reaches the file that
    /// only reads the inferred value, without rebuilding its source layer.
    #[gtest]
    fn a_moved_inference_reaches_its_transitive_reader() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_ripple_sideband");
            analysis.add_main_workspace(workspace.clone());

            let provider_uri = file_path_to_uri(&workspace.join("lua/chain/provider.lua"))
                .expect("provider URI should parse");
            let middle_uri = file_path_to_uri(&workspace.join("lua/chain/middle.lua"))
                .expect("middle URI should parse");
            let consumer_uri = file_path_to_uri(&workspace.join("lua/chain/consumer.lua"))
                .expect("consumer URI should parse");
            analysis
                .update_file_by_uri(&provider_uri, Some("Chain = { Value = 1 }\n".to_string()))
                .map(|(id, _)| id);
            analysis
                .update_file_by_uri(&middle_uri, Some("Binferred = Chain.Value\n".to_string()))
                .map(|(id, _)| id);
            analysis
                .update_file_by_uri(&consumer_uri, Some("local got = Binferred\n".to_string()))
                .map(|(id, _)| id);
            let consumer_id = analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_id(&consumer_uri)
                .expect("consumer file id");

            analysis.diagnose_file(consumer_id, CancellationToken::new());

            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, peer) = loop_test_debounced_analysis(analysis.clone());
            let revision_before = reference_revision(&analysis, consumer_id).await;

            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });

            let provider_id = {
                let mut guard = analysis.write().await;
                guard
                    .update_file_text_only(
                        &provider_uri,
                        "Chain = { Value = \"text\" }\n".to_string(),
                    )
                    .expect("provider file should still exist")
            };
            debounced_analysis
                .schedule(provider_id, provider_uri.clone())
                .await;

            // The consumer only names `Binferred`, inferred in the middleman:
            // its publish can only come from its recorded read of it.
            tokio::time::timeout(Duration::from_secs(15), async {
                tokio::task::spawn_blocking(move || {
                    loop {
                        let message = peer.receiver.recv().expect("peer channel should stay open");
                        if let Some(params) = publish_params(&message)
                            && params.uri == consumer_uri
                        {
                            return;
                        }
                    }
                })
                .await
                .expect("the publish reader task must not panic")
            })
            .await
            .expect("the consumer should be re-published after the inference moved");

            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");

            let revision_after = reference_revision(&analysis, consumer_id).await;
            verify_that!(revision_after, eq(revision_before))?;
            Ok(())
        })
    }

    /// A diagnostics pull for a file the re-analysis still has work in waits
    /// for that work, which the next slice settles first, then answers.
    #[gtest]
    fn a_diagnostics_wait_for_a_file_still_settling_puts_it_first() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_settled_wait");
            analysis.add_main_workspace(workspace.clone());
            let provider_uri = file_path_to_uri(&workspace.join("lua/settle/provider.lua"))
                .expect("provider URI should parse");
            let consumer_uri = file_path_to_uri(&workspace.join("lua/settle/consumer.lua"))
                .expect("consumer URI should parse");
            analysis
                .update_file_by_uri(&provider_uri, Some("Shared = { Value = 1 }\n".to_string()));
            analysis.update_file_by_uri(
                &consumer_uri,
                Some("local got = Shared.Value\n".to_string()),
            );
            let consumer_id = analysis
                .get_file_id(&consumer_uri)
                .expect("consumer file id");
            let provider_id = analysis
                .update_file_text_only(&provider_uri, "Shared = { Value = \"text\" }\n".to_string())
                .expect("provider file id");
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, _peer) = loop_test_debounced_analysis(analysis.clone());
            assert!(debounced_analysis.take_edits(vec![provider_id], None).await);
            verify_that!(
                analysis.read().await.compilation.is_settling(consumer_id),
                eq(true)
            )?;

            let waiter = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                let consumer_uri = consumer_uri.clone();
                async move {
                    debounced_analysis
                        .wait_until_file_settled_for(
                            &CancellationToken::new(),
                            "textDocument/diagnostic",
                            &consumer_uri,
                        )
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !debounced_analysis
                    .prioritised
                    .lock()
                    .expect("prioritised files")
                    .contains(&consumer_id)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the pull should put its file first");
            verify_that!(waiter.is_finished(), eq(false))?;

            while analysis.read().await.compilation.is_settling(consumer_id) {
                debounced_analysis.reindex_notify.notify_waiters();
                debounced_analysis
                    .advance_slice(Vec::new())
                    .await
                    .expect("a slice completes");
                debounced_analysis.reindex_notify.notify_waiters();
            }
            debounced_analysis.reindex_notify.notify_waiters();
            let freshness = tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("the pull should be released once its file settled")
                .expect("waiter task must not panic");
            verify_that!(freshness, eq(Freshness::Fresh))?;
            Ok(())
        })
    }

    /// An edit scheduled while the previous one is still settling in slices
    /// is taken into the same re-analysis, and the workspace settles to the
    /// last text.
    #[gtest]
    fn an_edit_made_while_the_last_one_settles_is_taken_in() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_debounced_preempt");
            analysis.add_main_workspace(workspace.clone());
            analysis
                .diagnostic
                .enable_only(DiagnosticCode::UndefinedGlobal);
            let provider_uri = file_path_to_uri(&workspace.join("lua/preempt/provider.lua"))
                .expect("provider URI should parse");
            let consumer_uri = file_path_to_uri(&workspace.join("lua/preempt/consumer.lua"))
                .expect("consumer URI should parse");
            analysis
                .update_file_by_uri(&provider_uri, Some("function OldName() end\n".to_string()));
            analysis.update_file_by_uri(&consumer_uri, Some("OldName()\n".to_string()));
            let consumer_id = analysis
                .get_file_id(&consumer_uri)
                .expect("consumer file id");
            let provider_id = analysis
                .update_file_text_only(&provider_uri, "function NewName() end\n".to_string())
                .expect("provider file id");
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, _peer) = loop_test_debounced_analysis(analysis.clone());
            assert!(debounced_analysis.take_edits(vec![provider_id], None).await);

            // The second edit lands before the first settled.
            analysis
                .write()
                .await
                .update_file_text_only(&provider_uri, "function OldName() end\n".to_string());
            let (progress, _) = debounced_analysis
                .advance_slice(Vec::new())
                .await
                .expect("a slice completes");
            verify_that!(
                progress,
                eq(&glua_code_analysis::UpdateProgress::TextChanged)
            )?;
            settle(&debounced_analysis, vec![provider_id]).await;

            let diagnostics = analysis
                .read()
                .await
                .diagnose_file(consumer_id, CancellationToken::new())
                .unwrap_or_default();
            verify_that!(
                has_diagnostic_code(&diagnostics, DiagnosticCode::UndefinedGlobal),
                eq(false)
            )?;
            Ok(())
        })
    }
}
