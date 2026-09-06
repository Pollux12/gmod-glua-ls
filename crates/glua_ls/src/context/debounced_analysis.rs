use glua_code_analysis::{DirtySet, EmmyLuaAnalysis, FileId};
use lsp_types::Uri;
use smol_str::SmolStr;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, FileDiagnostic, file_diagnostic::SharedDiagnosticDataCache};

const FRESHNESS_STUCK_WARN_AFTER: Duration = Duration::from_secs(5);

/// How long the user must stay idle after a reindex before the whole workspace
/// is re-diagnosed.
const IDLE_WORKSPACE_DIAGNOSTIC_DELAY: Duration = Duration::from_millis(2000);

/// How long the user must stay idle before the dependency ripple runs.
///
/// The edited file's own re-index runs on the much shorter debounce, which is
/// all a request positioned inside that file needs. This timer governs only the
/// cross-file settle, which holds the write lock for seconds on a large
/// gamemode — and while it does, no keystroke can even be applied. Starting it
/// after every brief pause is what put a ripple in flight for nearly every
/// completion.
const RIPPLE_QUIET: Duration = Duration::from_millis(1000);

/// Cap on how long one typing burst can hold the ripple off, so diagnostics
/// still settle during sustained typing.
const MAX_RIPPLE_DEFERRAL: Duration = Duration::from_secs(5);

/// How long the ripple gives requests released by the self-index to take their
/// read lock before it takes the write lock back.
///
/// Bounded so a stream of requests cannot starve the ripple: past this, the
/// ripple proceeds and the stragglers wait it out as they did before.
const READER_HANDOFF_GRACE: Duration = Duration::from_millis(250);

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

/// Debounced analysis: accumulates file IDs from rapid edits and runs `reindex_files` once the user pauses typing.
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
    /// False once the supervisor has given up restarting the debounce loop
    /// (see `LS_DEBOUNCE_LOOP_DEAD`). Freshness can then never arrive, so
    /// waiters fail fast instead of parking until their request is cancelled.
    /// Set once, never re-armed: a dead loop stays dead for process lifetime.
    debounce_loop_alive: AtomicBool,
    in_flight_changes: AtomicUsize,
    /// Requests aimed at one document that are waiting for, or reading against,
    /// that document's own index entries.
    ///
    /// The self-index releases them and then immediately queues the ripple's
    /// write lock. A woken request still has to be polled before it can queue
    /// its read, and the lock is fair-FIFO, so without this the ripple wins the
    /// race every time and the request waits out the whole ripple it was just
    /// released from.
    pending_readers: AtomicUsize,
    readers_idle_notify: Notify,
    notify: Notify,
    reindex_notify: Notify,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shared_diagnostic_data_cache: SharedDiagnosticDataCache,
    /// Used to schedule diagnostic-only refreshes for textual refresh names:
    /// referencers of a name an edit moved that need no re-index of their own.
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
            debounce_loop_alive: AtomicBool::new(true),
            in_flight_changes: AtomicUsize::new(0),
            pending_readers: AtomicUsize::new(0),
            readers_idle_notify: Notify::new(),
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
        }
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

    /// Register that a request is waiting on, or reading against, one
    /// document's own index entries.
    ///
    /// Hold the guard until the request has finished reading. The ripple yields
    /// to outstanding guards — up to [`READER_HANDOFF_GRACE`] — before it takes
    /// the write lock back, so a request the self-index just released is not
    /// made to wait out the ripple anyway.
    pub fn begin_reader_handoff(self: &Arc<Self>) -> ReaderHandoff {
        self.pending_readers.fetch_add(1, Ordering::AcqRel);
        ReaderHandoff {
            analysis: self.clone(),
        }
    }

    /// Let outstanding [`ReaderHandoff`]s take their read lock before the
    /// caller takes the write lock.
    ///
    /// Returns as soon as none are outstanding, or after
    /// [`READER_HANDOFF_GRACE`] so a stream of requests cannot starve the
    /// ripple.
    async fn await_reader_handoff(
        &self,
        timing: Option<&mut crate::util::ls_profile::HandoffTiming>,
    ) {
        let profile = timing.is_some();
        let start = profile.then(Instant::now);
        // Whether any reader was ever observed: distinguishes `idle` (none at
        // entry) from `released` (drained within the grace).
        let mut saw_reader = false;
        let outcome = self.await_reader_handoff_inner(&mut saw_reader).await;
        if let (Some(timing), Some(start)) = (timing, start) {
            timing.wait = start.elapsed();
            timing.outcome = Some(outcome);
        }
    }

    async fn await_reader_handoff_inner(
        &self,
        saw_reader: &mut bool,
    ) -> crate::util::ls_profile::HandoffOutcome {
        use crate::util::ls_profile::HandoffOutcome;
        let deadline = Instant::now() + READER_HANDOFF_GRACE;

        loop {
            // Register before testing, or a drop landing in between is lost.
            let idle = self.readers_idle_notify.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();

            if self.pending_readers.load(Ordering::Acquire) == 0 {
                return if *saw_reader {
                    HandoffOutcome::Released
                } else {
                    HandoffOutcome::Idle
                };
            }
            *saw_reader = true;

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return HandoffOutcome::Timeout;
            }

            tokio::select! {
                _ = idle => {}
                _ = tokio::time::sleep(remaining) => return HandoffOutcome::Timeout,
                _ = self.shutdown.cancelled() => {
                    return if self.pending_readers.load(Ordering::Acquire) == 0 {
                        if *saw_reader { HandoffOutcome::Released } else { HandoffOutcome::Idle }
                    } else {
                        HandoffOutcome::Timeout
                    };
                }
            }
        }
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

    /// Wait until the given file is no longer pending reindex. Returns
    /// `LoopDead` without diagnosing when the debounce loop is dead (freshness
    /// can never arrive); callers must abort rather than diagnose stale data.
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
            };
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

    /// Re-index the edited files' own entries, and report which other files
    /// the edit invalidated.
    ///
    /// This takes the write lock and gives it back, which is the whole point: a
    /// freshness flag published while the lock is still held buys a waiting
    /// request nothing, since it cannot read the index until the lock is free.
    async fn self_index_without_queuing(
        &self,
        file_ids: Vec<FileId>,
        timing: Option<&mut crate::util::ls_profile::SelfWriteTimings>,
    ) -> Option<DirtySet> {
        let profile = timing.is_some();
        let analysis = self.analysis.clone();
        let cache = self.shared_diagnostic_data_cache.clone();

        tokio::select! {
            _ = self.shutdown.cancelled() => None,
            result = tokio::task::spawn_blocking(move || {
                let wait_start = profile.then(Instant::now);
                let mut guard = analysis.blocking_write();
                let wait = wait_start.map(|start| start.elapsed()).unwrap_or_default();
                let hold_start = profile.then(Instant::now);
                // Change-aware: the dirty set holds only the files that read a
                // fact this edit actually moved. Most keystrokes (typing inside
                // a function, trailing comment, local rename) export nothing
                // new and leave it empty.
                let dirty = guard.self_index_and_diff(file_ids);
                cache.invalidate();
                let hold = hold_start.map(|start| start.elapsed()).unwrap_or_default();
                (dirty, wait, hold)
            }) => match result {
                Ok((result, wait, hold)) => {
                    if let Some(timing) = timing {
                        timing.wait = wait;
                        timing.hold = hold;
                    }
                    Some(result)
                }
                Err(err) => {
                    log::error!("self-index task failed: {}", err);
                    None
                }
            }
        }
    }

    async fn ripple_without_queuing(
        &self,
        dirty: DirtySet,
        timing: Option<&mut crate::util::ls_profile::RippleWriteTimings>,
    ) -> (bool, HashSet<SmolStr>, Vec<FileId>) {
        let profile = timing.is_some();
        let analysis = self.analysis.clone();
        let cache = self.shared_diagnostic_data_cache.clone();

        // Re-index under a blocking write lock on a blocking thread: the wait
        // for the lock and the CPU work both stay off the Tokio workers.
        tokio::select! {
            _ = self.shutdown.cancelled() => (false, HashSet::default(), Vec::new()),
            result = tokio::task::spawn_blocking(move || {
                let wait_start = profile.then(Instant::now);
                let mut guard = analysis.blocking_write();
                let wait = wait_start.map(|start| start.elapsed()).unwrap_or_default();
                let hold_start = profile.then(Instant::now);
                let (rippled, refresh_names) = guard.ripple_with_refresh_names(dirty);
                // Invalidate under the write lock so no reader can observe the
                // fresh index next to the stale shared diagnostic data.
                cache.invalidate();
                let hold = hold_start.map(|start| start.elapsed()).unwrap_or_default();
                (rippled, refresh_names, wait, hold)
            }) => {
                match result {
                    Ok((rippled, refresh_names, wait, hold)) => {
                        if let Some(timing) = timing {
                            timing.wait = wait;
                            timing.hold = hold;
                        }
                        (true, refresh_names, rippled)
                    }
                    Err(err) => {
                        log::error!("reindex task failed: {}", err);
                        (false, HashSet::default(), Vec::new())
                    }
                }
            }
        }
    }

    /// Resolve owed textual refresh names to their main-workspace referencers
    /// and schedule diagnostic-only tasks for them.
    ///
    /// Never reindexes: the candidates go to
    /// [`FileDiagnostic::add_files_diagnostic_task`], the same path a
    /// watched-file change uses. `exclude` carries the files the edit path
    /// already settled (the edited batch, the files owed a ripple, and the
    /// files the ripple re-analysed), which the candidate filter drops.
    async fn schedule_textual_refresh(
        &self,
        names: HashSet<SmolStr>,
        exclude: &HashSet<FileId>,
    ) -> bool {
        if names.is_empty() {
            return false;
        }
        let analysis = self.analysis.clone();
        let exclude = exclude.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            let guard = analysis.blocking_read();
            let interval = guard
                .get_emmyrc()
                .diagnostics
                .diagnostic_interval
                .unwrap_or(500);
            let candidates = guard.textual_refresh_candidates(&names, &exclude);
            (candidates, interval)
        })
        .await;
        match resolved {
            Ok((candidates, interval)) => {
                if candidates.is_empty() {
                    return false;
                }
                self.file_diagnostic
                    .add_files_diagnostic_task(candidates, interval, None)
                    .await;
                true
            }
            Err(err) => {
                log::error!("textual refresh resolve task failed: {}", err);
                false
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

    /// Hold the owed ripple until typing has stopped for [`RIPPLE_QUIET`].
    ///
    /// Returns `true` when the caller should run the ripple now, `false` when
    /// another edit arrived and the loop should self-index that first — the
    /// ripple it owes then joins the one already outstanding.
    async fn ripple_quiet_elapsed(
        &self,
        burst_started_at: Instant,
        timing: Option<&mut crate::util::ls_profile::QuietTiming>,
    ) -> bool {
        use crate::util::ls_profile::QuietOutcome;
        let profile = timing.is_some();
        let start = profile.then(Instant::now);
        let extra = RIPPLE_QUIET.saturating_sub(self.debounce_duration);
        let deferral_left = MAX_RIPPLE_DEFERRAL.saturating_sub(burst_started_at.elapsed());
        // Capped by the deferral budget rather than by quiet: sustained typing
        // must not hold diagnostics off indefinitely.
        let capped_by_deferral = deferral_left <= extra;
        if extra.is_zero() || deferral_left.is_zero() {
            if let (Some(timing), Some(start)) = (timing, start) {
                timing.wait = start.elapsed();
                timing.outcome = Some(if deferral_left.is_zero() && !extra.is_zero() {
                    QuietOutcome::MaxDeferral
                } else if extra.is_zero() && deferral_left.is_zero() {
                    // Both exhausted: the deferral cap is what forces the run.
                    QuietOutcome::MaxDeferral
                } else {
                    QuietOutcome::Timer
                });
            }
            return true;
        }

        enum QuietWait {
            Shutdown,
            Edit,
            Timer,
        }
        let wait_outcome = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => QuietWait::Shutdown,
            _ = self.notify.notified() => QuietWait::Edit,
            _ = tokio::time::sleep(extra.min(deferral_left)) => QuietWait::Timer,
        };
        let run_now = match wait_outcome {
            QuietWait::Edit => false,
            QuietWait::Shutdown => true,
            QuietWait::Timer => {
                // A notify landing before the select registered would be lost, so the
                // timer expiring is not on its own proof that nothing arrived.
                self.pending_files.lock().await.is_empty()
            }
        };
        if let (Some(timing), Some(start)) = (timing, start) {
            timing.wait = start.elapsed();
            timing.outcome = Some(match wait_outcome {
                QuietWait::Edit => QuietOutcome::Edit,
                QuietWait::Shutdown => QuietOutcome::Timer,
                QuietWait::Timer => {
                    if run_now {
                        if capped_by_deferral {
                            QuietOutcome::MaxDeferral
                        } else {
                            QuietOutcome::Timer
                        }
                    } else {
                        // Timer fired but an edit landed just before the check.
                        QuietOutcome::Edit
                    }
                }
            });
        }
        run_now
    }

    /// Background loop: waits for events, debounces, then runs reindex.
    /// Spawn this once at server startup.
    pub async fn run(&self) {
        let mut idle_workspace_diagnostic_token: Option<CancellationToken> = None;
        // The ripple owed by the self-indexes run so far in this typing burst,
        // and the union of the dirty sets each of them reported.
        let mut owed_files: HashSet<FileId> = HashSet::new();
        let mut owed_ripple: Option<DirtySet> = None;
        // Textual refresh names owed a diagnostic-only refresh, merged from
        // every self-index diff and ripple sideband of this burst. They steer
        // no reindex: they are drained whenever no ripple is owed.
        let mut owed_refresh_names: HashSet<SmolStr> = HashSet::default();
        // Files the edit path already settled (edited batch, ripple-settled),
        // excluded from the textual refresh resolution.
        let mut refresh_exclude: HashSet<FileId> = HashSet::default();
        let mut burst_started_at: Option<Instant> = None;
        loop {
            // Register before testing the condition: `notify_waiters()` stores
            // no permit, so a signal landing in between would be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let needs_work = !self.pending_files.lock().await.is_empty()
                || self.has_pending_changes.load(Ordering::Acquire)
                || !owed_files.is_empty()
                || !owed_refresh_names.is_empty();
            if !needs_work {
                tokio::select! {
                    _ = notified => {}
                    _ = self.shutdown.cancelled() => return,
                }
            }

            // Debounce: keep resetting the timer while new events arrive.
            let profile_cycle = crate::util::ls_profile::ls_profile_enabled();
            let cycle_id = profile_cycle.then(crate::util::ls_profile::next_reindex_cycle);
            let debounce_start = crate::util::ls_profile::profile_instant();
            loop {
                tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => return,
                    _ = self.notify.notified() => continue,
                    _ = tokio::time::sleep(self.debounce_duration) => break,
                }
            }
            let debounce_elapsed = debounce_start
                .map(|start| start.elapsed())
                .unwrap_or_default();
            let mut self_timings = crate::util::ls_profile::SelfWriteTimings::default();
            let mut handoff_timing = crate::util::ls_profile::HandoffTiming::default();
            let mut quiet_timing = crate::util::ls_profile::QuietTiming::default();
            let mut ripple_timings = crate::util::ls_profile::RippleWriteTimings::default();

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

                // Re-index the edited files themselves first and release the
                // write lock, so a completion or hover positioned inside one of
                // them can be answered against entries that match its text
                // instead of waiting out the whole dependency ripple. The
                // ripple is by far the larger half — measured on a gamemode
                // workspace, 106ms against 5.1s.
                let self_timing = profile_cycle.then_some(&mut self_timings);
                let Some(mut dirty) = self
                    .self_index_without_queuing(file_ids.clone(), self_timing)
                    .await
                else {
                    if self.shutdown.is_cancelled() {
                        return;
                    }
                    // Release the batch, or every request aimed at these
                    // documents parks until some later edit happens to cover
                    // them.
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
                    if let Some(cycle) = cycle_id {
                        use crate::util::ls_profile::{HandoffOutcome, QuietOutcome};
                        crate::util::ls_profile::emit_ls_reindex(
                            cycle,
                            debounce_elapsed,
                            self_timings.wait,
                            self_timings.hold,
                            std::time::Duration::ZERO,
                            HandoffOutcome::Idle,
                            std::time::Duration::ZERO,
                            QuietOutcome::Timer,
                            std::time::Duration::ZERO,
                            std::time::Duration::ZERO,
                        );
                    }
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
                // they can queue their read. Taking the write lock back now
                // would put them behind the whole ripple.
                let handoff_opt = profile_cycle.then_some(&mut handoff_timing);
                self.await_reader_handoff(handoff_opt).await;

                // Refresh names ride along whichever path the dirty set takes:
                // they are taken before the emptiness check, which ignores
                // them by design.
                owed_refresh_names.extend(dirty.take_textual_refresh_names());
                refresh_exclude.extend(file_ids.iter().copied());

                if dirty.is_empty() {
                    // Nothing else reads a fact this edit moved - the
                    // self-index already makes these files answerable. Clear
                    // them from reindexing immediately so dirty state can
                    // settle without waiting for a ripple that will never come.
                    {
                        let mut reindexing = self.reindexing_files.lock().await;
                        for id in &file_ids {
                            reindexing.remove(id);
                        }
                    }
                    self.refresh_dirty_state().await;
                    self.reindex_notify.notify_waiters();
                    // Refresh names still owed: fall through so the gate below
                    // drains them without a ripple.
                    if owed_ripple.is_none() && owed_refresh_names.is_empty() {
                        if let Some(cycle) = cycle_id {
                            use crate::util::ls_profile::{HandoffOutcome, QuietOutcome};
                            crate::util::ls_profile::emit_ls_reindex(
                                cycle,
                                debounce_elapsed,
                                self_timings.wait,
                                self_timings.hold,
                                handoff_timing.wait,
                                handoff_timing.outcome.unwrap_or(HandoffOutcome::Idle),
                                std::time::Duration::ZERO,
                                QuietOutcome::Timer,
                                std::time::Duration::ZERO,
                                std::time::Duration::ZERO,
                            );
                        }
                        continue;
                    }
                } else {
                    owed_files.extend(file_ids.iter().copied());
                    match &mut owed_ripple {
                        Some(owed) => owed.extend(dirty),
                        none => *none = Some(dirty),
                    }
                    burst_started_at.get_or_insert_with(Instant::now);
                }
            }

            let Some(owed) = owed_ripple.take() else {
                if owed_refresh_names.is_empty() {
                    self.refresh_dirty_state().await;
                    self.reindex_notify.notify_waiters();
                    continue;
                }
                // No ripple is owed, but textual refresh names are: resolve
                // them and schedule diagnostics, then settle the burst.
                let names = std::mem::take(&mut owed_refresh_names);
                let exclude = std::mem::take(&mut refresh_exclude);
                let scheduled = self.schedule_textual_refresh(names, &exclude).await;
                self.refresh_dirty_state().await;
                self.reindex_notify.notify_waiters();
                if scheduled {
                    self.arm_idle_workspace_diagnostic(&mut idle_workspace_diagnostic_token);
                }
                continue;
            };

            // Hold the ripple until typing has genuinely stopped. Another edit
            // sends us back for its own self-index, and the ripple it owes
            // joins this one.
            let burst_started_at_instant = burst_started_at.unwrap_or_else(Instant::now);
            let quiet_opt = profile_cycle.then_some(&mut quiet_timing);
            if !self
                .ripple_quiet_elapsed(burst_started_at_instant, quiet_opt)
                .await
            {
                owed_ripple = Some(owed);
                if let Some(cycle) = cycle_id {
                    use crate::util::ls_profile::{HandoffOutcome, QuietOutcome};
                    crate::util::ls_profile::emit_ls_reindex(
                        cycle,
                        debounce_elapsed,
                        self_timings.wait,
                        self_timings.hold,
                        handoff_timing.wait,
                        handoff_timing.outcome.unwrap_or(HandoffOutcome::Idle),
                        quiet_timing.wait,
                        quiet_timing.outcome.unwrap_or(QuietOutcome::Edit),
                        std::time::Duration::ZERO,
                        std::time::Duration::ZERO,
                    );
                }
                continue;
            }

            {
                let ripple_files: Vec<FileId> = {
                    let mut ids: Vec<FileId> = owed_files.iter().copied().collect();
                    ids.sort();
                    ids
                };
                log::info!(
                    "ripple: {} edited file(s) over {} dirty file(s) after {}ms quiet",
                    ripple_files.len(),
                    owed.dirty_len(),
                    RIPPLE_QUIET.as_millis()
                );

                let ripple_opt = profile_cycle.then_some(&mut ripple_timings);
                let (reindex_completed, ripple_names, rippled) =
                    self.ripple_without_queuing(owed, ripple_opt).await;
                // The ripple's own sideband joins the names already owed; they
                // drain through the gate on the next iteration.
                owed_refresh_names.extend(ripple_names);
                refresh_exclude.extend(rippled.iter().copied());

                {
                    let mut reindexing = self.reindexing_files.lock().await;
                    for id in &ripple_files {
                        reindexing.remove(id);
                    }
                }
                owed_files.clear();
                burst_started_at = None;

                self.reindex_notify.notify_waiters();
                if let Some(cycle) = cycle_id {
                    use crate::util::ls_profile::{HandoffOutcome, QuietOutcome};
                    crate::util::ls_profile::emit_ls_reindex(
                        cycle,
                        debounce_elapsed,
                        self_timings.wait,
                        self_timings.hold,
                        handoff_timing.wait,
                        handoff_timing.outcome.unwrap_or(HandoffOutcome::Idle),
                        quiet_timing.wait,
                        quiet_timing.outcome.unwrap_or(QuietOutcome::Timer),
                        ripple_timings.wait,
                        ripple_timings.hold,
                    );
                }
                if !reindex_completed {
                    // Only shutdown stops the loop; a panicked reindex must
                    // fall through so `refresh_dirty_state()` releases waiters.
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
                self.arm_idle_workspace_diagnostic(&mut idle_workspace_diagnostic_token);
            }

            self.refresh_dirty_state().await;

            // Always notify waiters so they can re-check the condition.
            // Even if we didn't reindex (pending was empty), clearing the
            // dirty flag means waiters should proceed with available data.
            self.reindex_notify.notify_waiters();
        }
    }

    async fn refresh_dirty_state(&self) {
        // Publish while holding both locks, or concurrent callers can
        // interleave and store a stale reading.
        let pending = self.pending_files.lock().await;
        let reindexing = self.reindexing_files.lock().await;

        let has_pending_file_work = !pending.is_empty() || !reindexing.is_empty();
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

/// Keeps the ripple off the write lock while one request takes its read lock.
///
/// See [`DebouncedAnalysis::begin_reader_handoff`].
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
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::{Duration, Instant};

    use super::{Freshness, MAX_RIPPLE_DEFERRAL, READER_HANDOFF_GRACE};

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
                debounced_analysis.ripple_quiet_elapsed(Instant::now(), None),
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
                debounced_analysis.ripple_quiet_elapsed(started_at, None),
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
                debounced_analysis.await_reader_handoff(None),
            )
            .await
            .expect("no readers should let the ripple straight through");

            let handoff = debounced_analysis.begin_reader_handoff();
            let held = tokio::time::timeout(
                Duration::from_millis(50),
                debounced_analysis.await_reader_handoff(None),
            )
            .await;
            verify_that!(held.is_err(), eq(true))?;

            drop(handoff);
            tokio::time::timeout(
                Duration::from_millis(250),
                debounced_analysis.await_reader_handoff(None),
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
            debounced_analysis.await_reader_handoff(None).await;

            verify_that!(started_at.elapsed() >= READER_HANDOFF_GRACE, eq(true))?;
            verify_that!(started_at.elapsed() < READER_HANDOFF_GRACE * 4, eq(true))?;
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
            let dirty = debounced_analysis
                .self_index_without_queuing(vec![api_file_id], None)
                .await
                .expect("self-index should complete");
            let (completed, _refresh_names, _rippled) =
                debounced_analysis.ripple_without_queuing(dirty, None).await;
            verify_that!(completed, eq(true))?;

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

    /// A rename that dirties nothing must still reach its readers on the
    /// editor path: the debounced loop drains the self-index's textual refresh
    /// names, schedules diagnostic-only tasks for their referencers, and
    /// leaves those referencers un-reindexed (mirrors
    /// `global_function_rename_refreshes_old_and_new_referencers`).
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
            // first publish can only come from the names-only refresh task.
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
            .expect("the consumer should be re-published by the textual refresh");
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

            // The rename dirties nothing: the referencer must not have been
            // reindexed on the way to its refresh.
            let revision_after = reference_revision(&analysis, consumer_id).await;
            verify_that!(revision_after, eq(revision_before))?;
            Ok(())
        })
    }

    /// Names carried by a ripple's own export diffs must survive the ripple
    /// path: a file that only names a rippled inference comes back
    /// diagnostic-only, un-reindexed (mirrors
    /// `ripple_moved_inference_refreshes_its_lazy_reader`).
    #[gtest]
    fn a_ripple_and_its_sideband_names_both_reach_their_readers() -> Result<()> {
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

            // The consumer only names `Binferred`, an export of the rippled
            // middleman: its publish can only come from the sideband names the
            // ripple reported, drained after the ripple ran.
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
            .expect("the consumer should be re-published by the ripple sideband");

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
}
