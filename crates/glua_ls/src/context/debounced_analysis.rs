use glua_code_analysis::{EmmyLuaAnalysis, FileId, Slice, UpdateProgress, uri_to_file_path};
use lsp_types::{MessageType, ShowMessageParams, Uri};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, FileDiagnostic};
use crate::handlers::refresh_open_document_features;

const FRESHNESS_STUCK_WARN_AFTER: Duration = Duration::from_secs(5);

/// How long one slice of a re-analysis holds the analysis write lock. Only an
/// edit stops a slice early: a request waits for at most about one, and the
/// re-analysis is not handed back to it between every two evaluations.
const SLICE: Duration = Duration::from_millis(200);

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
/// means the request's token fired first. `LoopDead` means the debounce loop
/// is gone, given up on by its supervisor
/// ([`DebouncedAnalysis::note_debounce_loop_dead`]) or stopped by shutdown, so
/// freshness can never arrive and the caller must not diagnose stale data.
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
    /// Holds a URI of the document, so a request can be tested against its own
    /// params without taking the analysis lock — which the re-index holds for
    /// its whole duration, so resolving a file id first would wait out exactly
    /// what this exists to avoid. The client and the VFS need not spell one
    /// document's URI the same way, so a request matches by the path both name;
    /// entries are keyed by file id and cleared by identity.
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
    /// The analysis's slice interrupt, taken by the first slice: set when an
    /// edit arrives so the slice in progress stops for it.
    interrupt: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>>,
    /// When the newest change arrived: while changes keep arriving, the work
    /// a settled focus released waits; see [`DebouncedAnalysis::hold_while_editing`].
    last_change: std::sync::Mutex<Option<Instant>>,
    /// False once the supervisor has given up restarting the debounce loop
    /// (see `LS_DEBOUNCE_LOOP_DEAD`). Freshness can then never arrive, so
    /// waiters fail fast instead of parking until their request is cancelled.
    /// Set once, never re-armed: a dead loop stays dead for process lifetime.
    debounce_loop_alive: AtomicBool,
    in_flight_changes: AtomicUsize,
    /// The in-flight changes confined to one document, by the URI the client
    /// sent, so a request aimed at another document is not held back by them.
    in_flight_documents: std::sync::Mutex<HashMap<Uri, usize>>,
    /// In-flight changes that may touch any document, such as files changed
    /// on disk.
    in_flight_unscoped: AtomicUsize,
    /// Requests of an [`expedites_debounce`] method waiting for an edited
    /// document to be answerable. While any waits, a debounce ends at once:
    /// the request needs the re-analysis now, and only background propagation
    /// is worth coalescing.
    waiting_requests: AtomicUsize,
    /// A request was refused as stale while edits were pending: the client
    /// re-sends it, so the next debounce ends at once, as for a waiting one.
    refused_request: AtomicBool,
    /// A change no keystroke follows was scheduled: the next debounce ends
    /// at once; see [`DebouncedAnalysis::schedule_at_once`].
    at_once: AtomicBool,
    /// Whether the user was told a re-analysis failed: once is enough.
    failure_reported: AtomicBool,
    /// A change the index was derived under asks for every file to be
    /// analysed again; see [`DebouncedAnalysis::request_rebuild`].
    rebuild_requested: AtomicBool,
    notify: Notify,
    reindex_notify: Notify,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    /// Re-diagnoses the files an edit's re-analysis reports as affected.
    file_diagnostic: Arc<FileDiagnostic>,
    debounce_duration: Duration,
    shutdown: CancellationToken,
    client: Arc<ClientProxy>,
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
    /// Makes the next slice that leaves work to run outside the write lock
    /// panic while running it, where an engine bug was seen to.
    #[cfg(test)]
    panic_next_slice: Arc<AtomicBool>,
    /// Slices started, so a test can tell the loop is not retrying one.
    #[cfg(test)]
    slices_started: AtomicUsize,
}

/// Counts a request among [`DebouncedAnalysis::waiting_requests`] until
/// dropped.
struct WaitingRequest<'a>(&'a DebouncedAnalysis);

impl Drop for WaitingRequest<'_> {
    fn drop(&mut self) {
        self.0.waiting_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether a `method` request waiting for an edited document ends the edit's
/// debounce at once: what the user waits on while typing. Other requests wait
/// the debounce out.
pub(crate) fn expedites_debounce(method: &str) -> bool {
    matches!(
        method,
        "textDocument/completion"
            | "completionItem/resolve"
            | "textDocument/hover"
            | "gluals/hoverExpand"
            | "textDocument/signatureHelp"
            | "textDocument/definition"
            | "textDocument/semanticTokens/full"
            | "textDocument/selectionRange"
            | "textDocument/formatting"
            | "textDocument/rangeFormatting"
            | "textDocument/onTypeFormatting"
    )
}

/// What a freshness wait waits for.
#[derive(Clone, Copy)]
enum Wait<'a> {
    /// Every pending change reindexed.
    Settled,
    /// The document at the URI, for what it needs.
    Document(&'a Uri, DocumentNeed),
}

/// What a request aimed at one document waits for.
#[derive(Clone, Copy)]
enum DocumentNeed {
    /// Its own index entries match its text.
    Answerable,
    /// Its newest text is the one the analysis reads.
    TakenText,
}

/// Where the loop stands in rebuilding the analysis after a step of a
/// re-analysis panicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rebuild {
    None,
    /// A re-analysis of every file from its text settles in slices.
    Running,
    /// The rebuild failed too: the next change starts another, so a failure
    /// that repeats is not retried in a loop.
    Owed,
}

/// Takes `count` from `counter`, stopping at zero and logging an underflow.
fn release(counter: &AtomicUsize, count: usize) {
    let previous = counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            Some(current.saturating_sub(count))
        })
        .unwrap_or_else(|current| current);
    if previous < count {
        log::error!(
            "LS_INFLIGHT_UNDERFLOW attempted to finish {} in-flight changes with only {} pending",
            count,
            previous
        );
    }
}

impl DebouncedAnalysis {
    pub(crate) fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        debounce_ms: u64,
        shutdown: CancellationToken,
        client: Arc<ClientProxy>,
        file_diagnostic: Arc<FileDiagnostic>,
        lsp_features: Arc<crate::context::LspFeatures>,
    ) -> Self {
        Self {
            pending_files: Mutex::new(HashSet::new()),
            reindexing_files: Mutex::new(HashSet::new()),
            blocked_documents: Mutex::new(HashMap::new()),
            has_pending_changes: AtomicBool::new(false),
            settling: AtomicBool::new(false),
            prioritised: std::sync::Mutex::default(),
            interrupt: std::sync::OnceLock::new(),
            last_change: std::sync::Mutex::new(None),
            debounce_loop_alive: AtomicBool::new(true),
            in_flight_changes: AtomicUsize::new(0),
            in_flight_documents: std::sync::Mutex::default(),
            in_flight_unscoped: AtomicUsize::new(0),
            waiting_requests: AtomicUsize::new(0),
            refused_request: AtomicBool::new(false),
            at_once: AtomicBool::new(false),
            failure_reported: AtomicBool::new(false),
            rebuild_requested: AtomicBool::new(false),
            notify: Notify::new(),
            reindex_notify: Notify::new(),
            analysis,
            file_diagnostic,
            debounce_duration: Duration::from_millis(debounce_ms),
            shutdown,
            client,
            lsp_features,
            #[cfg(test)]
            freshness_waits: AtomicUsize::new(0),
            #[cfg(test)]
            expedited_debounces: AtomicUsize::new(0),
            #[cfg(test)]
            debounce_wakes: AtomicUsize::new(0),
            #[cfg(test)]
            panic_next_slice: Arc::default(),
            #[cfg(test)]
            slices_started: AtomicUsize::new(0),
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
    /// Its diagnostics go first, as soon as its own text settles: whatever
    /// changed its text, it is the file being edited.
    pub async fn schedule(&self, file_id: FileId, uri: Uri) {
        self.file_diagnostic
            .store()
            .invalidate([file_id], &[file_id]);
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

    /// [`Self::schedule`] for a change typing does not follow, such as files
    /// changed on disk or a document opened or closed on other text: nothing
    /// is coming to coalesce with it, so the debounce ends at once. A change
    /// that follows anyway stops the re-analysis and is taken in.
    pub async fn schedule_at_once(&self, file_id: FileId, uri: Uri) {
        self.at_once.store(true, Ordering::Release);
        self.schedule(file_id, uri).await;
    }

    /// [`Self::schedule_at_once`] for files one change brought together, such
    /// as files changed on disk at once: one re-analysis takes them all in,
    /// never some of them ahead of the rest.
    pub async fn schedule_all_at_once(&self, files: &[(FileId, Uri)]) {
        if files.is_empty() {
            return;
        }
        let file_ids = files
            .iter()
            .map(|(file_id, _)| *file_id)
            .collect::<Vec<_>>();
        self.file_diagnostic
            .store()
            .invalidate(file_ids.iter().copied(), &file_ids);
        self.pending_files
            .lock()
            .await
            .extend(file_ids.iter().copied());
        self.blocked_documents
            .lock()
            .await
            .extend(files.iter().cloned());
        self.has_pending_changes.store(true, Ordering::Release);
        self.at_once.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Signal that document changes are in-flight but not yet scheduled; they
    /// may touch any document, so no document is answerable until they settle.
    ///
    /// Called **synchronously** from the notification handler (inline, before
    /// spawning the didChange task) so that request handlers dispatched
    /// immediately afterward see the dirty flag and wait for reindex instead
    /// of computing on stale analysis data.
    pub fn begin_in_flight_change(self: &Arc<Self>) -> InFlightChangeGuard {
        self.in_flight_unscoped.fetch_add(1, Ordering::AcqRel);
        self.begin_change(None)
    }

    /// [`Self::begin_in_flight_change`] for a change confined to the document
    /// at `uri`: requests aimed at other documents stay answerable.
    pub fn begin_document_change(self: &Arc<Self>, uri: Uri) -> InFlightChangeGuard {
        self.begin_documents_change(vec![uri])
    }

    /// [`Self::begin_in_flight_change`] for a change confined to the documents
    /// at `uris`: requests aimed at other documents stay answerable.
    pub fn begin_documents_change(self: &Arc<Self>, uris: Vec<Uri>) -> InFlightChangeGuard {
        {
            let mut documents = self.in_flight_documents();
            for uri in &uris {
                *documents.entry(uri.clone()).or_default() += 1;
            }
        }
        self.begin_change(Some(uris))
    }

    fn begin_change(self: &Arc<Self>, documents: Option<Vec<Uri>>) -> InFlightChangeGuard {
        self.in_flight_changes.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut last_change) = self.last_change.lock() {
            *last_change = Some(Instant::now());
        }
        if let Some(interrupt) = self.interrupt.get() {
            interrupt.store(true, Ordering::Release);
        }
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
        InFlightChangeGuard::new(self.clone(), documents, 1)
    }

    /// Has the loop rebuild the analysis of every file from its text, settling
    /// in slices as an edit does: what a config change that reaches the index
    /// needs, without holding the analysis for the whole build.
    pub fn request_rebuild(&self) {
        self.rebuild_requested.store(true, Ordering::Release);
        if let Some(interrupt) = self.interrupt.get() {
            interrupt.store(true, Ordering::Release);
        }
        self.has_pending_changes.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn in_flight_documents(&self) -> std::sync::MutexGuard<'_, HashMap<Uri, usize>> {
        self.in_flight_documents
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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

    /// Settles `count` in-flight changes begun for `documents`, or for no one
    /// document when `None`.
    pub async fn finish_in_flight_changes(&self, documents: Option<&[Uri]>, count: usize) {
        if count == 0 {
            return;
        }

        match documents {
            Some(uris) => {
                let mut documents = self.in_flight_documents();
                for uri in uris {
                    if let Some(pending) = documents.get_mut(uri) {
                        *pending = pending.saturating_sub(count);
                        if *pending == 0 {
                            documents.remove(uri);
                        }
                    }
                }
            }
            None => release(&self.in_flight_unscoped, count),
        }
        release(&self.in_flight_changes, count);

        self.refresh_dirty_state().await;
        self.reindex_notify.notify_waiters();
        self.notify.notify_waiters();
    }

    /// Cancelled when the server shuts down.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Check whether document changes are pending reindex.
    ///
    /// Handlers that need consistent tree + index data (e.g. semantic tokens)
    /// can use this to decide whether to serve stale results or return `None`
    /// so the client keeps its previous state.
    pub fn is_dirty(&self) -> bool {
        self.has_pending_changes.load(Ordering::Acquire)
    }

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
    pub async fn wait_until_fresh_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
    ) -> Freshness {
        self.wait(cancel_token, request_method, Wait::Settled, None)
            .await
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
        let what = Wait::Document(uri, DocumentNeed::Answerable);
        self.wait(cancel_token, request_method, what, None).await
    }

    /// Wait until the re-analysis has taken the text of the document at
    /// `uri`, which is the text the analysis then reads for it. A request that
    /// reads only the document's syntax needs nothing more.
    pub async fn wait_until_text_taken_for(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        uri: &Uri,
    ) -> Freshness {
        let what = Wait::Document(uri, DocumentNeed::TakenText);
        self.wait(cancel_token, request_method, what, None).await
    }

    /// Profiled form of [`Self::wait_until_fresh_for`]: the same wait, plus
    /// time accumulated by observed state.
    pub async fn wait_until_fresh_for_profiled(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
    ) -> (Freshness, FreshWaitBreakdown) {
        let mut breakdown = FreshWaitBreakdown::default();
        let freshness = self
            .wait(
                cancel_token,
                request_method,
                Wait::Settled,
                Some(&mut breakdown),
            )
            .await;
        (freshness, breakdown)
    }

    /// Profiled form of [`Self::wait_until_file_fresh_for`]: the same wait,
    /// plus time accumulated by observed state.
    pub async fn wait_until_file_fresh_for_profiled(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        uri: &Uri,
    ) -> (Freshness, FreshWaitBreakdown) {
        let mut breakdown = FreshWaitBreakdown::default();
        let what = Wait::Document(uri, DocumentNeed::Answerable);
        let freshness = self
            .wait(cancel_token, request_method, what, Some(&mut breakdown))
            .await;
        (freshness, breakdown)
    }

    /// Waits until `what` holds, `cancel_token` fires or the debounce loop is
    /// dead; `breakdown` accumulates where the time went.
    async fn wait(
        &self,
        cancel_token: &CancellationToken,
        request_method: &'static str,
        what: Wait<'_>,
        mut breakdown: Option<&mut FreshWaitBreakdown>,
    ) -> Freshness {
        #[cfg(test)]
        self.freshness_waits.fetch_add(1, Ordering::AcqRel);

        let started_at = Instant::now();
        let mut warned_stuck = false;
        let mut waiting = None;

        let freshness = loop {
            // Register (`enable()`) before testing the condition: unpolled
            // Notified futures are invisible to `notify_waiters()`.
            let notified = self.reindex_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let met = match what {
                Wait::Settled => !self.has_pending_changes.load(Ordering::Acquire),
                Wait::Document(uri, DocumentNeed::Answerable) => self.file_is_answerable(uri).await,
                Wait::Document(uri, DocumentNeed::TakenText) => self.text_is_taken(uri).await,
            };
            if met {
                break Freshness::Fresh;
            }

            if !self.debounce_loop_alive() {
                break Freshness::LoopDead;
            }

            // Only a request that has to wait on its document needs the
            // re-analysis now.
            if matches!(what, Wait::Document(..)) && expedites_debounce(request_method) {
                waiting.get_or_insert_with(|| self.request_waiting());
            }
            let parked_at = Instant::now();
            let observed_in_flight = self.in_flight_change_count() > 0;
            let remaining = FRESHNESS_STUCK_WARN_AFTER.saturating_sub(started_at.elapsed());

            let ended = tokio::select! {
                _ = notified => None,
                _ = cancel_token.cancelled() => Some(Freshness::Cancelled),
                _ = self.shutdown.cancelled() => Some(Freshness::LoopDead),
                _ = tokio::time::sleep(remaining), if !warned_stuck => {
                    self.log_freshness_stuck(request_method, started_at).await;
                    warned_stuck = true;
                    None
                }
            };
            if let Some(breakdown) = breakdown.as_deref_mut() {
                let parked = parked_at.elapsed();
                if observed_in_flight {
                    breakdown.in_flight += parked;
                } else {
                    breakdown.blocked += parked;
                }
            }
            if let Some(freshness) = ended {
                break freshness;
            }
        };
        if let Some(breakdown) = breakdown {
            breakdown.total = started_at.elapsed();
        }
        freshness
    }

    /// Whether the analysis reads the newest text of the document at `uri`:
    /// none is in flight and none is staged for a take.
    async fn text_is_taken(&self, uri: &Uri) -> bool {
        if self.has_in_flight_change_for(uri) {
            return false;
        }
        let analysis = self.analysis.read().await;
        analysis
            .get_file_id(uri)
            .is_none_or(|file_id| !analysis.has_untaken_text(file_id))
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
        if self.has_in_flight_change_for(uri) {
            return false;
        }
        if !self.has_pending_changes.load(Ordering::Acquire) {
            return true;
        }
        // Only ever a handful of documents are mid-edit at once.
        let path = uri_to_file_path(uri);
        !self
            .blocked_documents
            .lock()
            .await
            .values()
            .any(|blocked| blocked == uri || path.is_some() && uri_to_file_path(blocked) == path)
    }

    /// Whether a change to the document at `uri`, or one that may touch any
    /// document, has text no task has applied yet.
    fn has_in_flight_change_for(&self, uri: &Uri) -> bool {
        self.in_flight_unscoped.load(Ordering::Acquire) > 0
            || self.in_flight_documents().contains_key(uri)
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

    /// Resolves once a slice, settle or schedule may have let more files'
    /// diagnostics be computed.
    pub fn reindex_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.reindex_notify.notified()
    }

    /// The first of `candidates` whose diagnostics can be computed now: its
    /// text was taken and the re-analysis in progress has no work left in it.
    /// Those it still has work in go first in the next slice.
    pub async fn first_ready_for_diagnostics(&self, candidates: &[FileId]) -> Option<FileId> {
        if !self.has_pending_changes.load(Ordering::Acquire) {
            let analysis = self.analysis.read().await;
            return candidates
                .iter()
                .copied()
                .find(|file_id| analysis.compilation.has_taken_text(*file_id));
        }
        let scheduled = {
            let pending = self.pending_files.lock().await;
            let reindexing = self.reindexing_files.lock().await;
            candidates
                .iter()
                .copied()
                .filter(|file_id| !pending.contains(file_id) && !reindexing.contains(file_id))
                .collect::<Vec<_>>()
        };
        let settling = self.settling.load(Ordering::Acquire);
        let analysis = self.analysis.read().await;
        scheduled.into_iter().find(|file_id| {
            analysis.compilation.has_taken_text(*file_id)
                && !(settling && self.file_id_is_settling(&analysis, *file_id))
        })
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

        tokio::select! {
            _ = self.shutdown.cancelled() => false,
            result = tokio::task::spawn_blocking(move || {
                let wait_start = profile.then(Instant::now);
                let mut guard = analysis.blocking_write();
                let wait = wait_start.map(|start| start.elapsed()).unwrap_or_default();
                let hold_start = profile.then(Instant::now);
                guard.begin_reindex(file_ids);
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
                    log::error!("LS_REINDEX_FAILED taking edits into the re-analysis failed: {err}");
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
        #[cfg(test)]
        self.slices_started.fetch_add(1, Ordering::AcqRel);
        #[cfg(test)]
        let panics = self.panic_next_slice.clone();

        tokio::select! {
            _ = self.shutdown.cancelled() => None,
            result = tokio::task::spawn_blocking(move || {
                let wait_start = crate::util::ls_profile::profile_instant();
                let mut guard = analysis.blocking_write();
                let hold_start = crate::util::ls_profile::profile_instant();
                guard.compilation.prioritise_update(&prioritised);
                let progress = guard.advance_reindex(Slice::Until(Instant::now() + SLICE));
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
                // A superstep's tasks run on forks of the index, which
                // requests read meanwhile; the next slice installs them.
                if guard.compilation.has_detached_tasks() {
                    let shared = guard.downgrade();
                    #[cfg(test)]
                    if panics.swap(false, Ordering::AcqRel) {
                        panic!("injected re-analysis slice panic");
                    }
                    shared.compilation.run_detached_tasks();
                }
                (progress, settled_first)
            }) => match result {
                Ok(outcome) => Some(outcome),
                Err(err) => {
                    log::error!("LS_REINDEX_FAILED a re-analysis slice failed: {err}");
                    None
                }
            }
        }
    }

    /// Clears the slice interrupt for the next slice, unless a change is on
    /// its way: one that arrived before the slice starts holds it back until
    /// its text is applied, and one that arrives later stops it. Returns
    /// whether the slice can start.
    async fn slice_can_start(&self) -> bool {
        if self.interrupt.get().is_none() {
            let interrupt = self.analysis.read().await.compilation.interrupt_handle();
            let _ = self.interrupt.set(interrupt);
        }
        if let Some(interrupt) = self.interrupt.get() {
            interrupt.store(false, Ordering::Release);
        }
        self.in_flight_change_count() == 0
    }

    /// Once the edited files settled, holds back the work their settled focus
    /// released while their diagnostics are computed, so the next slice does
    /// not take the write lock between two of them, and while changes are still
    /// arriving: that work would otherwise carry a passing state of the text
    /// into the rest of the workspace, only for the next change to carry it back
    /// out. Ends once neither holds, at the next change, or for a request that
    /// has to wait.
    async fn hold_while_editing(&self) {
        let store = self.file_diagnostic.store();
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (computed, dirtied) = store.progress();
            tokio::pin!(computed);
            tokio::pin!(dirtied);
            computed.as_mut().enable();
            dirtied.as_mut().enable();
            if self.waiting_requests.load(Ordering::Acquire) > 0
                || self.refused_request.load(Ordering::Acquire)
                || self.in_flight_change_count() > 0
                || !self.pending_files.lock().await.is_empty()
            {
                return;
            }
            let quiet_at = self
                .last_change
                .lock()
                .ok()
                .and_then(|last_change| *last_change)
                .map(|at| at + self.debounce_duration);
            let typing = quiet_at.is_some_and(|quiet_at| Instant::now() < quiet_at);
            if !typing && !store.edited_work_pending(self).await {
                return;
            }
            tokio::select! {
                _ = notified => {}
                _ = computed => {}
                _ = dirtied => {}
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep_until(quiet_at.unwrap_or_else(Instant::now).into()), if typing => {}
            }
        }
    }

    /// Takes in the files whose text changed without a schedule reaching the
    /// loop, so a re-analysis never waits on text it has not taken.
    async fn take_unscheduled_text(&self, rebuild: &mut Rebuild, edited: &mut Vec<FileId>) {
        let file_ids = self
            .analysis
            .read()
            .await
            .compilation
            .get_db()
            .get_vfs()
            .files_changed_since_taken();
        if !file_ids.is_empty() {
            self.take_in(file_ids, None, rebuild, edited).await;
        }
    }

    /// Takes `file_ids` into the re-analysis as edited files, or, when that
    /// fails or a rebuild is owed, recovers; see [`Self::recover`].
    async fn take_in(
        &self,
        file_ids: Vec<FileId>,
        timing: Option<&mut crate::util::ls_profile::WriteTimings>,
        rebuild: &mut Rebuild,
        edited: &mut Vec<FileId>,
    ) {
        let taken = *rebuild != Rebuild::Owed && self.take_edits(file_ids.clone(), timing).await;
        edited.extend(file_ids);
        if !taken && !self.shutdown.is_cancelled() {
            self.recover(rebuild, edited).await;
        }
    }

    /// Recovers from a step of the re-analysis that panicked, which may have
    /// left the re-analysis and the index half-written: drops both and
    /// rebuilds every file from its text, settling in slices as an edit does,
    /// unless that rebuild is what failed. The `edited` files settle with the
    /// rebuild, or are released when it cannot start.
    async fn recover(&self, rebuild: &mut Rebuild, edited: &mut Vec<FileId>) {
        if !self.failure_reported.swap(true, Ordering::AcqRel) {
            self.client.show_message(ShowMessageParams {
                typ: MessageType::WARNING,
                message: "GLuaLS hit an internal error while analysing a change and rebuilt \
                          its analysis of the workspace. Please report it with the language \
                          server log."
                    .to_string(),
            });
        }
        if *rebuild != Rebuild::Running && self.begin_rebuild().await {
            log::error!("LS_REINDEX_REBUILD rebuilding the analysis of every file");
            *rebuild = Rebuild::Running;
        } else {
            log::error!(
                "LS_REINDEX_FAILED the analysis could not be rebuilt; the next change starts another rebuild"
            );
            *rebuild = Rebuild::Owed;
            self.settling.store(false, Ordering::Release);
            self.release_documents(&std::mem::take(edited)).await;
        }
        self.refresh_dirty_state().await;
        self.reindex_notify.notify_waiters();
    }

    /// Starts re-analysing every file from its text on a cleared index, and
    /// marks every file's diagnostics dirty. Returns whether it started.
    async fn begin_rebuild(&self) -> bool {
        let analysis = self.analysis.clone();
        let rebuild = tokio::task::spawn_blocking(move || {
            let mut guard = analysis.blocking_write();
            guard.begin_full_reindex();
            guard.get_main_workspace_file_ids_for_diagnostics()
        });
        let file_ids = tokio::select! {
            _ = self.shutdown.cancelled() => return false,
            result = rebuild => match result {
                Ok(file_ids) => file_ids,
                Err(err) => {
                    log::error!("LS_REINDEX_FAILED rebuilding the analysis failed: {err}");
                    return false;
                }
            }
        };
        self.file_diagnostic.store().invalidate_all(file_ids);
        self.settling.store(true, Ordering::Release);
        true
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

    /// Re-diagnoses the files a re-analysis reported affected, and refreshes
    /// the client's derived views when the edit reached another open file:
    /// the client re-requests the edited documents' own views itself.
    async fn refresh_affected(&self, edited: &[FileId], affected: Vec<FileId>) {
        self.file_diagnostic.store().note_settled();
        if affected.is_empty() {
            return;
        }
        if self.file_diagnostic.store().any_open(&affected, edited) {
            self.refresh_views();
        }
        let affected = self.analysis.read().await.order_for_diagnostics(affected);
        self.file_diagnostic.store().invalidate(affected, edited);
    }

    /// Has the client request its derived views of open documents again.
    fn refresh_views(&self) {
        refresh_open_document_features(&self.client, &self.lsp_features);
    }

    /// Background loop: waits for edits, debounces them, takes them into the
    /// re-analysis, then settles it in slices that release the analysis lock,
    /// so requests run between them. New edits pre-empt the slices and are
    /// taken in, settling first. Spawn this once at server startup.
    pub async fn run(&self) {
        // The edited files of the re-analysis in progress.
        let mut edited: Vec<FileId> = Vec::new();
        let mut cycle = None;
        let mut text_changed = false;
        let mut rebuild = Rebuild::None;
        loop {
            // Register before testing the condition: `notify_waiters()` stores
            // no permit, so a signal landing in between would be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.rebuild_requested.swap(false, Ordering::AcqRel) {
                if self.begin_rebuild().await {
                    rebuild = Rebuild::Running;
                } else if self.shutdown.is_cancelled() {
                    return;
                } else {
                    rebuild = Rebuild::Owed;
                }
                self.refresh_dirty_state().await;
                self.reindex_notify.notify_waiters();
                continue;
            }

            let scheduled = !self.pending_files.lock().await.is_empty();
            let settling = self.settling.load(Ordering::Acquire);
            if !scheduled && (!settling || text_changed) {
                tokio::select! {
                    _ = notified => {}
                    _ = self.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(self.debounce_duration), if text_changed => {
                        self.take_unscheduled_text(&mut rebuild, &mut edited).await;
                    }
                }
                text_changed = false;
                continue;
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
                        || self.at_once.swap(false, Ordering::AcqRel)
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
                    self.take_in(
                        file_ids,
                        cycle.map(|_| &mut timings),
                        &mut rebuild,
                        &mut edited,
                    )
                    .await;
                    if self.shutdown.is_cancelled() {
                        return;
                    }
                    if let Some(cycle) = cycle {
                        crate::util::ls_profile::emit_ls_reindex(
                            cycle,
                            debounce_elapsed,
                            timings.wait,
                            timings.hold,
                        );
                    }
                }
                text_changed = false;
                self.refresh_dirty_state().await;
                self.reindex_notify.notify_waiters();
                continue;
            }

            if !self.slice_can_start().await {
                tokio::select! {
                    _ = notified => {}
                    _ = self.shutdown.cancelled() => return,
                }
                continue;
            }
            let Some((progress, settled_first)) = self.advance_slice(edited.clone()).await else {
                if self.shutdown.is_cancelled() {
                    return;
                }
                self.recover(&mut rebuild, &mut edited).await;
                continue;
            };
            self.release_documents(&settled_first).await;
            let focus_settled = !edited.is_empty() && settled_first.len() == edited.len();
            match progress {
                UpdateProgress::Settling if focus_settled => {
                    self.reindex_notify.notify_waiters();
                    self.hold_while_editing().await;
                }
                UpdateProgress::Settling => {}
                UpdateProgress::Settled(affected) => {
                    self.settling.store(false, Ordering::Release);
                    let settled = std::mem::take(&mut edited);
                    self.release_documents(&settled).await;
                    if let Some(cycle) = cycle.take() {
                        crate::util::ls_profile::emit_ls_settled(cycle, affected.len());
                    }
                    self.refresh_affected(&settled, affected).await;
                    if std::mem::replace(&mut rebuild, Rebuild::None) == Rebuild::Running {
                        self.refresh_views();
                    }
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

        let has_pending_file_work = !pending.is_empty()
            || !reindexing.is_empty()
            || self.settling.load(Ordering::Acquire)
            || self.rebuild_requested.load(Ordering::Acquire);
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
    /// The documents the change is confined to; `None` when it may touch any.
    documents: Option<Vec<Uri>>,
    count: usize,
}

impl InFlightChangeGuard {
    fn new(analysis: Arc<DebouncedAnalysis>, documents: Option<Vec<Uri>>, count: usize) -> Self {
        Self {
            analysis: Some(analysis),
            documents,
            count,
        }
    }

    pub async fn finish(mut self) {
        if let Some(analysis) = self.analysis.take() {
            analysis
                .finish_in_flight_changes(self.documents.as_deref(), self.count)
                .await;
        }
    }
}

impl Drop for InFlightChangeGuard {
    fn drop(&mut self) {
        let Some(analysis) = self.analysis.take() else {
            return;
        };
        let count = self.count;
        let documents = self.documents.take();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    analysis
                        .finish_in_flight_changes(documents.as_deref(), count)
                        .await;
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
    use std::sync::atomic::Ordering;
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
        test_debounced_analysis_with_debounce(0)
    }

    fn test_debounced_analysis_with_debounce(debounce_ms: u64) -> Arc<DebouncedAnalysis> {
        debounced_analysis_over(Arc::new(RwLock::new(EmmyLuaAnalysis::new())), debounce_ms)
    }

    /// A debounced analysis over `analysis` whose diagnostic workers are not
    /// started.
    fn debounced_analysis_over(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        debounce_ms: u64,
    ) -> Arc<DebouncedAnalysis> {
        let (connection, _peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone(), true));
        let file_diagnostic = Arc::new(FileDiagnostic::new(
            analysis.clone(),
            status_bar,
            client.clone(),
        ));
        Arc::new(DebouncedAnalysis::new(
            analysis,
            debounce_ms,
            CancellationToken::new(),
            client,
            file_diagnostic,
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
    fn idle_debounce_shutdown_with_unscheduled_change_and_waiting_request() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let in_flight = debounced_analysis.begin_in_flight_change();
            let waiting = debounced_analysis.request_waiting();
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });
            tokio::time::sleep(Duration::from_millis(25)).await;
            let expedited = debounced_analysis
                .expedited_debounces
                .load(Ordering::Acquire);
            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("idle loop must observe shutdown")
                .expect("idle loop must not panic");
            assert_eq!(expedited, 0);
            assert!(
                debounced_analysis
                    .has_pending_changes
                    .load(Ordering::Acquire)
            );
            drop(waiting);
            in_flight.finish().await;
            Ok(())
        })
    }

    #[gtest]
    fn idle_debounce_schedule_wakes_without_releasing_in_flight_freshness() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_idle_debounce_schedule");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/autorun/shared/edited.lua"))
                .expect("URI should parse");
            analysis.update_file_by_uri(
                &uri,
                Some(
                    "Value = 1
"
                    .to_string(),
                ),
            );
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, _peer) = loop_test_debounced_analysis(analysis.clone());
            let in_flight = debounced_analysis.begin_in_flight_change();
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });
            let wait_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                let uri = uri.clone();
                async move {
                    debounced_analysis
                        .wait_until_file_fresh_for(
                            &CancellationToken::new(),
                            "textDocument/hover",
                            &uri,
                        )
                        .await
                }
            });
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert!(!wait_task.is_finished());
            assert_eq!(
                debounced_analysis
                    .expedited_debounces
                    .load(Ordering::Acquire),
                0
            );
            let file_id = analysis
                .write()
                .await
                .update_file_text_only(
                    &uri,
                    "Value = 2
"
                    .to_string(),
                )
                .expect("file must exist");
            debounced_analysis.schedule(file_id, uri.clone()).await;
            in_flight.finish().await;
            let fresh = tokio::time::timeout(Duration::from_secs(10), wait_task)
                .await
                .expect("scheduled work must wake the parked loop")
                .expect("freshness waiter must not panic");
            assert_eq!(fresh, Freshness::Fresh);
            assert!(debounced_analysis.file_is_answerable(&uri).await);
            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("scheduled loop must observe shutdown")
                .expect("scheduled loop must not panic");
            Ok(())
        })
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

    /// A change on its way holds the next slice back until its text is
    /// applied: a slice started meanwhile would run its length before the
    /// change could stop it.
    #[gtest]
    fn a_change_on_its_way_holds_back_the_next_slice() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let in_flight = debounced_analysis.begin_in_flight_change();
            verify_that!(debounced_analysis.slice_can_start().await, eq(false))?;
            in_flight.finish().await;
            verify_that!(debounced_analysis.slice_can_start().await, eq(true))?;
            Ok(())
        })
    }

    /// The edited files' diagnostics are published before the re-analysis
    /// resumes: once the hold ends, the next slice's write lock finds them out.
    #[gtest]
    fn the_edited_files_diagnostics_are_published_before_the_re_analysis_resumes() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_edited_first");
            analysis.add_main_workspace(workspace.clone());
            analysis
                .diagnostic
                .enable_only(DiagnosticCode::UndefinedGlobal);
            let uri = file_path_to_uri(&workspace.join("lua/edited_first/edited.lua"))
                .expect("URI should parse");
            let (file_id, _) = analysis
                .update_file_by_uri(&uri, Some("Missing()\n".to_string()))
                .expect("file id");
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, peer) = loop_test_debounced_analysis(analysis.clone());
            debounced_analysis
                .file_diagnostic
                .store()
                .invalidate([file_id], &[file_id]);

            tokio::time::timeout(
                Duration::from_secs(10),
                debounced_analysis.hold_while_editing(),
            )
            .await
            .expect("the hold ends once the edited file is diagnosed");
            let next_slice = analysis.write().await;
            let published = peer
                .receiver
                .try_iter()
                .filter_map(|message| publish_params(&message))
                .filter(|params| params.uri == uri)
                .collect::<Vec<_>>();
            drop(next_slice);

            verify_that!(published.len(), eq(1))?;
            verify_that!(
                has_diagnostic_code(&published[0].diagnostics, DiagnosticCode::UndefinedGlobal),
                eq(true)
            )?;
            debounced_analysis.shutdown.cancel();
            Ok(())
        })
    }

    /// Edited files whose diagnostics wait hold the re-analysis back only until
    /// a change arrives, a request has to wait, or the server shuts down.
    #[gtest]
    fn waiting_for_the_edited_diagnostics_ends_for_a_change_a_request_or_shutdown() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_edited_escape");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/edited_escape/edited.lua"))
                .expect("URI should parse");
            let (file_id, _) = analysis
                .update_file_by_uri(&uri, Some("Value = 1\n".to_string()))
                .expect("file id");
            let debounced_analysis = debounced_analysis_over(Arc::new(RwLock::new(analysis)), 0);
            let store = debounced_analysis.file_diagnostic.store();
            store.mark_working();
            store.invalidate([file_id], &[file_id]);
            let held = || {
                tokio::spawn({
                    let debounced_analysis = debounced_analysis.clone();
                    async move { debounced_analysis.hold_while_editing().await }
                })
            };
            let ends = |hold: tokio::task::JoinHandle<()>| async move {
                tokio::time::timeout(Duration::from_secs(5), hold)
                    .await
                    .expect("the hold must end")
                    .expect("the hold must not panic");
            };

            let hold = held();
            tokio::time::sleep(Duration::from_millis(50)).await;
            verify_that!(hold.is_finished(), eq(false))?;
            let waiting = debounced_analysis.request_waiting();
            ends(hold).await;
            drop(waiting);

            let hold = held();
            tokio::time::sleep(Duration::from_millis(50)).await;
            verify_that!(hold.is_finished(), eq(false))?;
            let in_flight = debounced_analysis.begin_in_flight_change();
            ends(hold).await;
            in_flight.finish().await;

            let hold = held();
            tokio::time::sleep(Duration::from_millis(50)).await;
            verify_that!(hold.is_finished(), eq(false))?;
            debounced_analysis.schedule(file_id, uri).await;
            ends(hold).await;
            debounced_analysis.pending_files.lock().await.clear();

            let hold = held();
            tokio::time::sleep(Duration::from_millis(50)).await;
            verify_that!(hold.is_finished(), eq(false))?;
            debounced_analysis.shutdown.cancel();
            ends(hold).await;
            Ok(())
        })
    }

    /// Once the edited files settled, the work they released waits while
    /// changes keep arriving and goes on a debounce after the last one; a
    /// request that has to wait, or the next edit, ends the wait at once.
    #[gtest]
    fn released_work_waits_for_changes_to_stop() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounce = Duration::from_millis(300);
            let debounced_analysis = test_debounced_analysis_with_debounce(300);

            debounced_analysis.begin_in_flight_change().finish().await;
            let started = Instant::now();
            debounced_analysis.hold_while_editing().await;
            verify_that!(started.elapsed() >= debounce * 3 / 4, eq(true))?;

            debounced_analysis.begin_in_flight_change().finish().await;
            let waiting = debounced_analysis.request_waiting();
            let started = Instant::now();
            debounced_analysis.hold_while_editing().await;
            verify_that!(started.elapsed() < debounce * 3 / 4, eq(true))?;
            drop(waiting);

            debounced_analysis.begin_in_flight_change().finish().await;
            let edit = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let uri = Uri::from_str("file:///held.lua").expect("URI should parse");
                    debounced_analysis.schedule(FileId::new(1), uri).await;
                }
            });
            let started = Instant::now();
            debounced_analysis.hold_while_editing().await;
            verify_that!(started.elapsed() < debounce * 3 / 4, eq(true))?;
            edit.await.expect("the edit task must not panic");
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

    /// Shutdown stops the loop for good, so a waiter parked on it is released
    /// and its request answered.
    #[gtest]
    fn shutdown_releases_a_parked_freshness_waiter() -> Result<()> {
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
            while debounced_analysis.freshness_wait_count() == 0 {
                tokio::task::yield_now().await;
            }
            debounced_analysis.shutdown.cancel();
            let fresh = tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .expect("parked waiter must be released at shutdown")
                .expect("waiter task must not panic");
            verify_that!(fresh, eq(Freshness::LoopDead))
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

            debounced_analysis.finish_in_flight_changes(None, 1).await;

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
                crate::util::unique_temp_dir("gmod_glua_ls_debounced_shared_diagnostic_cache");
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
            let shutdown = CancellationToken::new();
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                0,
                shutdown.clone(),
                client,
                file_diagnostic.clone(),
                test_lsp_features(),
            ));
            file_diagnostic.store().start(1, debounced_analysis.clone());

            let initial_diagnostics = file_diagnostic
                .file_diagnostics(user_uri.clone(), CancellationToken::new())
                .await
                .map(|stored| stored.diagnostics.to_vec())
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

            let affected = settle(&debounced_analysis, vec![api_file_id]).await;
            debounced_analysis
                .refresh_affected(&[api_file_id], affected)
                .await;

            let updated_diagnostics = file_diagnostic
                .file_diagnostics(user_uri, CancellationToken::new())
                .await
                .map(|stored| stored.diagnostics.to_vec())
                .unwrap_or_default();
            verify_that!(
                has_diagnostic_code(&updated_diagnostics, DiagnosticCode::DiscardReturns),
                eq(true)
            )?;
            shutdown.cancel();
            file_diagnostic.store().stop();

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
        let shutdown = CancellationToken::new();
        let debounced_analysis = Arc::new(DebouncedAnalysis::new(
            analysis,
            0,
            shutdown.clone(),
            client,
            file_diagnostic.clone(),
            test_lsp_features(),
        ));
        file_diagnostic.store().start(1, debounced_analysis.clone());
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
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_rename_refresh");
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
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_expedite");
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
            ));
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
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

    /// A change no keystroke follows, such as a file changed on disk, ends
    /// the debounce at once without any request waiting on it.
    #[gtest]
    fn a_change_scheduled_at_once_skips_the_debounce() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_at_once");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/autorun/shared/on_disk.lua"))
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
            ));
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
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
            debounced_analysis
                .schedule_at_once(file_id, uri.clone())
                .await;
            let fresh = tokio::time::timeout(
                Duration::from_secs(10),
                debounced_analysis
                    .wait_until_fresh_for(&CancellationToken::new(), "workspace/symbol"),
            )
            .await
            .expect("the change settles well before the debounce elapses");
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
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_untouched");
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
            ));
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
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
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_refused");
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
            ));
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis.clone(),
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
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
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_ripple_sideband");
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

    /// Diagnostics waiting on a file the re-analysis still has work in put
    /// that work first, and the file is ready once it settled.
    #[gtest]
    fn diagnostics_waiting_on_a_file_still_settling_put_it_first() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_settled_wait");
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
            debounced_analysis.refresh_dirty_state().await;

            verify_that!(
                debounced_analysis
                    .first_ready_for_diagnostics(&[consumer_id])
                    .await,
                none()
            )?;
            verify_that!(
                debounced_analysis
                    .prioritised
                    .lock()
                    .expect("prioritised files")
                    .contains(&consumer_id),
                eq(true)
            )?;

            while analysis.read().await.compilation.is_settling(consumer_id) {
                debounced_analysis
                    .advance_slice(Vec::new())
                    .await
                    .expect("a slice completes");
            }
            verify_that!(
                debounced_analysis
                    .first_ready_for_diagnostics(&[consumer_id])
                    .await,
                some(eq(consumer_id))
            )?;
            Ok(())
        })
    }

    /// A file whose new text is stored but not yet taken in by a
    /// re-analysis is not ready for diagnostics, edited or not.
    #[gtest]
    fn text_not_yet_taken_in_is_not_ready_for_diagnostics() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_untaken");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/untaken/edited.lua"))
                .expect("URI should parse");
            analysis.update_file_by_uri(
                &uri,
                Some(
                    "Value = 1
"
                    .to_string(),
                ),
            );
            let file_id = analysis
                .update_file_text_only(
                    &uri,
                    "Value = 2
"
                    .to_string(),
                )
                .expect("the file should still exist");
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, _peer) = loop_test_debounced_analysis(analysis.clone());

            verify_that!(
                debounced_analysis
                    .first_ready_for_diagnostics(&[file_id])
                    .await,
                none()
            )?;
            let _in_flight = debounced_analysis.begin_in_flight_change();
            verify_that!(
                debounced_analysis
                    .first_ready_for_diagnostics(&[file_id])
                    .await,
                none()
            )?;

            analysis.write().await.reindex_files(vec![file_id]);
            verify_that!(
                debounced_analysis
                    .first_ready_for_diagnostics(&[file_id])
                    .await,
                some(eq(file_id))
            )?;
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
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_preempt");
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

    /// A request the user does not wait on while typing waits an edit's
    /// debounce out instead of ending it.
    #[gtest]
    fn a_passive_request_waiting_on_an_edited_document_leaves_the_debounce_alone() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_passive");
            analysis.add_main_workspace(workspace.clone());
            let uri = file_path_to_uri(&workspace.join("lua/autorun/shared/edited.lua"))
                .expect("URI should parse");
            analysis.update_file_by_uri(&uri, Some("Value = 1\n".to_string()));
            let file_id = analysis
                .update_file_text_only(&uri, "Value = 2\n".to_string())
                .expect("the file should still exist");
            let analysis = Arc::new(RwLock::new(analysis));
            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let file_diagnostic = Arc::new(FileDiagnostic::new(
                analysis.clone(),
                status_bar,
                client.clone(),
            ));
            let debounced_analysis = Arc::new(DebouncedAnalysis::new(
                analysis,
                60_000,
                CancellationToken::new(),
                client,
                file_diagnostic,
                test_lsp_features(),
            ));
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });
            debounced_analysis.schedule(file_id, uri.clone()).await;

            let cancel = CancellationToken::new();
            let waiter = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                let cancel = cancel.clone();
                async move {
                    debounced_analysis
                        .wait_until_file_fresh_for(&cancel, "textDocument/codeLens", &uri)
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while debounced_analysis.freshness_wait_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the request should reach the freshness wait");
            tokio::time::sleep(Duration::from_millis(100)).await;

            verify_that!(
                debounced_analysis
                    .expedited_debounces
                    .load(Ordering::Acquire),
                eq(0)
            )?;
            verify_that!(waiter.is_finished(), eq(false))?;

            cancel.cancel();
            let freshness = waiter.await.expect("the waiter must not panic");
            verify_that!(freshness, eq(Freshness::Cancelled))?;
            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");
            Ok(())
        })
    }

    /// An edit whose text no task has applied yet holds back only requests
    /// aimed at its own document; a change that may touch any document holds
    /// back every one.
    #[gtest]
    fn an_in_flight_edit_holds_back_only_its_own_document() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let edited = Uri::from_str("file:///workspace/edited.lua").expect("uri should parse");
            let other = Uri::from_str("file:///workspace/other.lua").expect("uri should parse");

            let edit = debounced_analysis.begin_document_change(edited.clone());
            verify_that!(debounced_analysis.is_dirty(), eq(true))?;
            verify_that!(
                debounced_analysis.file_is_answerable(&other).await,
                eq(true)
            )?;
            verify_that!(
                debounced_analysis.file_is_answerable(&edited).await,
                eq(false)
            )?;

            let disk_change = debounced_analysis.begin_in_flight_change();
            verify_that!(
                debounced_analysis.file_is_answerable(&other).await,
                eq(false)
            )?;

            disk_change.finish().await;
            edit.finish().await;
            verify_that!(
                debounced_analysis.file_is_answerable(&edited).await,
                eq(true)
            )?;
            verify_that!(debounced_analysis.is_dirty(), eq(false))
        })
    }

    /// Files changed on disk hold back the documents they are, not others.
    #[gtest]
    fn a_change_to_some_files_holds_back_only_those_documents() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let debounced_analysis = test_debounced_analysis();
            let changed = ["a", "b"].map(|name| {
                Uri::from_str(&format!("file:///workspace/{name}.lua")).expect("uri should parse")
            });
            let other = Uri::from_str("file:///workspace/other.lua").expect("uri should parse");

            let change = debounced_analysis.begin_documents_change(changed.to_vec());
            let answerable_during = (
                debounced_analysis.file_is_answerable(&changed[0]).await,
                debounced_analysis.file_is_answerable(&changed[1]).await,
                debounced_analysis.file_is_answerable(&other).await,
            );
            change.finish().await;

            verify_that!(answerable_during, eq((false, false, true)))?;
            verify_that!(
                debounced_analysis.file_is_answerable(&changed[0]).await,
                eq(true)
            )
        })
    }

    /// The client re-requests an edited document's own views, so only an edit
    /// that reached another open file refreshes them workspace-wide.
    #[gtest]
    fn only_an_edit_reaching_another_open_file_refreshes_views() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
            let (connection, peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let file_diagnostic = Arc::new(FileDiagnostic::new(
                analysis.clone(),
                status_bar,
                client.clone(),
            ));
            let features = LspFeatures::new(
                serde_json::from_value(serde_json::json!({
                    "workspace": {
                        "semanticTokens": { "refreshSupport": true },
                        "inlayHint": { "refreshSupport": true },
                        "codeLens": { "refreshSupport": true },
                    }
                }))
                .expect("capabilities should deserialize"),
            );
            let debounced_analysis = DebouncedAnalysis::new(
                analysis,
                0,
                CancellationToken::new(),
                client,
                file_diagnostic.clone(),
                Arc::new(features),
            );
            let (edited, closed, open) = (FileId::new(1), FileId::new(2), FileId::new(3));
            let store = file_diagnostic.store();
            store.set_open(edited, true);
            store.set_open(open, true);
            let refreshes = || {
                let mut methods = peer
                    .receiver
                    .try_iter()
                    .filter_map(|message| match message {
                        lsp_server::Message::Request(request) => Some(request.method),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                methods.sort();
                methods
            };

            debounced_analysis
                .refresh_affected(&[edited], vec![edited, closed])
                .await;
            verify_that!(refreshes(), is_empty())?;

            debounced_analysis
                .refresh_affected(&[edited], vec![edited, open])
                .await;
            verify_that!(
                refreshes(),
                eq(&vec![
                    "workspace/codeLens/refresh".to_string(),
                    "workspace/inlayHint/refresh".to_string(),
                    "workspace/semanticTokens/refresh".to_string(),
                ])
            )
        })
    }

    fn is_show_message(message: &lsp_server::Message) -> bool {
        matches!(message, lsp_server::Message::Notification(notification)
            if notification.method == "window/showMessage")
    }

    /// The diagnostics of `files`, each `(uri, text)`, in a cold build of
    /// that text.
    fn cold_diagnostics(
        workspace: &std::path::Path,
        files: &[(&Uri, &str)],
    ) -> Vec<Vec<Diagnostic>> {
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace.to_path_buf());
        analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedGlobal);
        let file_ids = files
            .iter()
            .map(|(uri, text)| {
                analysis
                    .update_file_by_uri(uri, Some(text.to_string()))
                    .expect("file id")
                    .0
            })
            .collect::<Vec<_>>();
        file_ids
            .into_iter()
            .map(|file_id| {
                analysis
                    .diagnose_file(file_id, CancellationToken::new())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// A document scheduled under one spelling of its URI is blocked for a
    /// request that spells it another way.
    #[gtest]
    fn a_scheduled_document_is_blocked_whatever_its_uri_spelling() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
            let (debounced_analysis, _peer) = loop_test_debounced_analysis(analysis);
            let scheduled = Uri::from_str("file:///work/A.lua").expect("URI should parse");
            let requested = Uri::from_str("file:///work/%41.lua").expect("URI should parse");
            debounced_analysis.schedule(FileId::new(1), scheduled).await;
            verify_that!(
                debounced_analysis.file_is_answerable(&requested).await,
                eq(false)
            )
        })
    }

    /// A requested rebuild analyses every file again on the loop, in slices,
    /// and publishes them once it settles; a freshness waiter waits for it.
    #[gtest]
    fn a_requested_rebuild_settles_on_the_loop_and_publishes_every_file() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            analysis.compilation.set_detached_tasks(true);
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_requested_rebuild");
            analysis.add_main_workspace(workspace.clone());
            analysis
                .diagnostic
                .enable_only(DiagnosticCode::UndefinedGlobal);
            let consumer_uri = file_path_to_uri(&workspace.join("lua/autorun/consumer.lua"))
                .expect("consumer URI should parse");
            analysis.update_file_by_uri(
                &consumer_uri,
                Some(
                    "Missing()
"
                    .to_string(),
                ),
            );
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, peer) = loop_test_debounced_analysis(analysis.clone());
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });

            debounced_analysis.request_rebuild();
            verify_that!(debounced_analysis.is_dirty(), eq(true))?;
            let freshness = tokio::time::timeout(
                Duration::from_secs(10),
                debounced_analysis.wait_until_fresh_for(&CancellationToken::new(), "test"),
            )
            .await
            .expect("the freshness waiter should return");
            verify_that!(freshness, eq(Freshness::Fresh))?;
            let receiver = peer.receiver.clone();
            let published = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    loop {
                        let message = receiver.recv().expect("peer channel should stay open");
                        if let Some(params) = publish_params(&message)
                            && params.uri == consumer_uri
                        {
                            return params.diagnostics;
                        }
                    }
                }),
            )
            .await
            .expect("the consumer should be published")
            .expect("the publish reader task must not panic");
            verify_that!(
                has_diagnostic_code(&published, DiagnosticCode::UndefinedGlobal),
                eq(true)
            )?;

            debounced_analysis.shutdown.cancel();
            let _ = loop_task.await;
            Ok(())
        })
    }

    /// A slice that panics leaves the loop serving: the analysis is rebuilt
    /// from the text, freshness waiters return, the user is warned once, a
    /// later edit is analysed and published, and nothing is retried in a loop.
    #[gtest]
    fn a_panicking_slice_rebuilds_the_analysis_and_keeps_serving() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut analysis = EmmyLuaAnalysis::new();
            analysis.compilation.set_detached_tasks(true);
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_debounced_slice_panic");
            analysis.add_main_workspace(workspace.clone());
            analysis
                .diagnostic
                .enable_only(DiagnosticCode::UndefinedGlobal);
            let provider_uri = file_path_to_uri(&workspace.join("lua/autorun/provider.lua"))
                .expect("provider URI should parse");
            let consumer_uri = file_path_to_uri(&workspace.join("lua/autorun/consumer.lua"))
                .expect("consumer URI should parse");
            analysis.update_file_by_uri(&provider_uri, Some("function Shared() end\n".to_string()));
            analysis.update_file_by_uri(&consumer_uri, Some("Shared()\n".to_string()));
            let analysis = Arc::new(RwLock::new(analysis));
            let (debounced_analysis, peer) = loop_test_debounced_analysis(analysis.clone());
            let loop_task = tokio::spawn({
                let debounced_analysis = debounced_analysis.clone();
                async move { debounced_analysis.run().await }
            });
            let warnings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let consumer_published = |undefined: bool| {
                let receiver = peer.receiver.clone();
                let consumer_uri = consumer_uri.clone();
                let warnings = warnings.clone();
                async move {
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        tokio::task::spawn_blocking(move || {
                            loop {
                                let message =
                                    receiver.recv().expect("peer channel should stay open");
                                if is_show_message(&message) {
                                    warnings.fetch_add(1, Ordering::AcqRel);
                                }
                                if let Some(params) = publish_params(&message)
                                    && params.uri == consumer_uri
                                    && has_diagnostic_code(
                                        &params.diagnostics,
                                        DiagnosticCode::UndefinedGlobal,
                                    ) == undefined
                                {
                                    return params.diagnostics;
                                }
                            }
                        }),
                    )
                    .await
                    .expect("the consumer should be published")
                    .expect("the publish reader task must not panic")
                }
            };
            let fresh = |uri: Option<Uri>| {
                let debounced_analysis = debounced_analysis.clone();
                tokio::spawn(async move {
                    let cancel = CancellationToken::new();
                    match uri {
                        Some(uri) => {
                            debounced_analysis
                                .wait_until_file_fresh_for(&cancel, "textDocument/hover", &uri)
                                .await
                        }
                        None => {
                            debounced_analysis
                                .wait_until_fresh_for(&cancel, "test")
                                .await
                        }
                    }
                })
            };
            let returned = |waiter: tokio::task::JoinHandle<Freshness>| async move {
                tokio::time::timeout(Duration::from_secs(10), waiter)
                    .await
                    .expect("the freshness waiter should return")
                    .expect("the freshness waiter must not panic")
            };

            debounced_analysis
                .panic_next_slice
                .store(true, Ordering::Release);
            let provider_id = analysis
                .write()
                .await
                .update_file_text_only(&provider_uri, "function Renamed() end\n".to_string())
                .expect("provider file should exist");
            debounced_analysis
                .schedule(provider_id, provider_uri.clone())
                .await;
            let workspace_fresh = fresh(None);
            let provider_fresh = fresh(Some(provider_uri.clone()));
            consumer_published(true).await;
            verify_that!(returned(workspace_fresh).await, eq(Freshness::Fresh))?;
            verify_that!(returned(provider_fresh).await, eq(Freshness::Fresh))?;
            verify_that!(
                debounced_analysis.panic_next_slice.load(Ordering::Acquire),
                eq(false)
            )?;

            let consumer_id = analysis
                .write()
                .await
                .update_file_text_only(&consumer_uri, "Renamed()\n".to_string())
                .expect("consumer file should exist");
            debounced_analysis
                .schedule(consumer_id, consumer_uri.clone())
                .await;
            let consumer_fresh = fresh(Some(consumer_uri.clone()));
            let published = consumer_published(false).await;
            verify_that!(returned(consumer_fresh).await, eq(Freshness::Fresh))?;
            verify_that!(returned(fresh(None)).await, eq(Freshness::Fresh))?;

            let slices = debounced_analysis.slices_started.load(Ordering::Acquire);
            tokio::time::sleep(Duration::from_millis(300)).await;
            verify_that!(
                debounced_analysis.slices_started.load(Ordering::Acquire),
                eq(slices)
            )?;
            verify_that!(loop_task.is_finished(), eq(false))?;

            let live = {
                let analysis = analysis.read().await;
                [provider_id, consumer_id]
                    .into_iter()
                    .map(|file_id| {
                        analysis
                            .diagnose_file(file_id, CancellationToken::new())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
            };
            let cold = cold_diagnostics(
                &workspace,
                &[
                    (&provider_uri, "function Renamed() end\n"),
                    (&consumer_uri, "Renamed()\n"),
                ],
            );
            verify_that!(live, eq(&cold))?;
            verify_that!(published, eq(&cold[1]))?;
            let warnings = warnings.load(Ordering::Acquire)
                + peer.receiver.try_iter().filter(is_show_message).count();
            verify_that!(warnings, eq(1))?;

            debounced_analysis.shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), loop_task)
                .await
                .expect("the loop should stop on shutdown")
                .expect("the loop task must not panic");
            Ok(())
        })
    }
}
