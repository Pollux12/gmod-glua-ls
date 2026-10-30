//! The server's diagnostics for every workspace file, each computed again
//! only once what it was computed from changed: the file's text, the config,
//! or a fact its diagnostics read, which a settled re-analysis reports.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};

use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};
use tokio::sync::futures::Notified;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, DebouncedAnalysis, file_diagnostic::SharedDiagnosticDataCache};

/// How many files' diagnostic reads one write lock files while idle.
const FILED_PER_LOCK: usize = 64;

/// Which dirty diagnostics are computed first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Edited,
    Open,
    Closed,
}

/// A file's diagnostics as last computed.
#[derive(Clone, Debug)]
pub struct StoredDiagnostics {
    pub uri: Uri,
    pub diagnostics: Arc<[Diagnostic]>,
    /// Equal exactly when the diagnostics are, whatever their order.
    pub result_id: u64,
}

struct Entry {
    stored: StoredDiagnostics,
    config_gen: u64,
    /// The job that stored it.
    job: u64,
}

struct Job {
    file_id: FileId,
    /// Tells this computation apart from a later one of the same file.
    id: u64,
    token: CancellationToken,
    config_gen: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<FileId, Entry>,
    /// Dirty files by priority, then in the order they were invalidated.
    queue: BTreeSet<(Priority, u64, FileId)>,
    dirty: HashMap<FileId, (Priority, u64)>,
    /// Files being computed, with the job's id and its cancellation.
    computing: HashMap<FileId, (u64, CancellationToken)>,
    open: HashSet<FileId>,
    sequence: u64,
    config_gen: u64,
}

impl State {
    fn priority_of(&self, file_id: FileId, edited: bool) -> Priority {
        if edited {
            Priority::Edited
        } else if self.open.contains(&file_id) {
            Priority::Open
        } else {
            Priority::Closed
        }
    }

    /// Marks `file_id` dirty at `priority` or higher, cancelling a
    /// computation of what it held before.
    fn invalidate(&mut self, file_id: FileId, priority: Priority) {
        if let Some((_, token)) = self.computing.remove(&file_id) {
            token.cancel();
        }
        let priority = match self.dirty.get(&file_id) {
            Some(&(queued, sequence)) => {
                if queued <= priority {
                    return;
                }
                self.queue.remove(&(queued, sequence, file_id));
                priority
            }
            None => priority,
        };
        self.sequence += 1;
        self.queue.insert((priority, self.sequence, file_id));
        self.dirty.insert(file_id, (priority, self.sequence));
    }

    fn is_valid(&self, file_id: FileId) -> bool {
        !self.dirty.contains_key(&file_id)
            && !self.computing.contains_key(&file_id)
            && self
                .entries
                .get(&file_id)
                .is_some_and(|entry| entry.config_gen == self.config_gen)
    }
}

pub struct DiagnosticStore {
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shared: SharedDiagnosticDataCache,
    state: Mutex<State>,
    /// Signalled when a file turns dirty, for the workers.
    dirtied: Notify,
    /// Signalled when a computation finishes, for the waiters.
    computed: Notify,
    /// Signalled when a re-analysis settles, for the verifier.
    settled: Notify,
    /// Signalled when no file is dirty or computing, for the filer.
    idle: Notify,
    /// Where changed diagnostics are published.
    client: Arc<ClientProxy>,
    /// Held from reading an entry to sending what it shows, so a file's
    /// publishes reach the client in the order its entries were stored.
    publishing: Mutex<()>,
    /// `GLUALS_DIAG_VERIFY`: every stored result is checked against a fresh
    /// computation, and a difference logged as `LS_DIAG_DRIFT`.
    verify: bool,
    /// Whether workers were started: without them no job is ever claimed.
    working: std::sync::atomic::AtomicBool,
    /// Cancelled at shutdown: the workers stop, every computation's token,
    /// a child of it, is cancelled, and nothing waits on a file they would
    /// have computed.
    stopped: CancellationToken,
    /// Makes every computation panic while set, as a diagnostic bug would.
    #[cfg(test)]
    panic_computes: std::sync::atomic::AtomicBool,
}

impl DiagnosticStore {
    pub(crate) fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        shared: SharedDiagnosticDataCache,
        client: Arc<ClientProxy>,
    ) -> Self {
        Self {
            analysis,
            shared,
            state: Mutex::default(),
            dirtied: Notify::new(),
            computed: Notify::new(),
            settled: Notify::new(),
            idle: Notify::new(),
            client,
            publishing: Mutex::default(),
            verify: std::env::var_os("GLUALS_DIAG_VERIFY").is_some(),
            working: std::sync::atomic::AtomicBool::new(false),
            stopped: CancellationToken::new(),
            #[cfg(test)]
            panic_computes: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Stops the workers, cancels what they compute and releases every
    /// waiter, as the server shuts down.
    pub fn stop(&self) {
        self.stopped.cancel();
        self.computed.notify_waiters();
    }

    /// Resolves once the store is stopped.
    pub async fn stopped(&self) {
        self.stopped.cancelled().await;
    }

    fn publishing(&self) -> MutexGuard<'_, ()> {
        self.publishing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Starts `workers` tasks computing dirty files once no edit is pending,
    /// the edited ones as soon as their own text has settled, one filing what
    /// they read once none is left, and the verifier under
    /// `GLUALS_DIAG_VERIFY`, all until [`Self::stop`].
    pub fn start(self: &Arc<Self>, workers: usize, debounced: Arc<DebouncedAnalysis>) {
        let shutdown = self.stopped.clone();
        self.working
            .store(true, std::sync::atomic::Ordering::Release);
        for _ in 0..workers.max(1) {
            let store = self.clone();
            let debounced = debounced.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let job = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => return,
                        job = store.next_job(&debounced) => job,
                    };
                    store.run(job).await;
                }
            });
        }
        {
            let store = self.clone();
            let debounced = debounced.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = store.idle.notified() => {}
                        _ = shutdown.cancelled() => return,
                    }
                    store.file_reads_while_idle(&debounced).await;
                }
            });
        }
        if self.verify {
            let store = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = store.settled.notified() => {}
                        _ = shutdown.cancelled() => return,
                    }
                    store.verify_stored(&debounced).await;
                }
            });
        }
    }

    /// Notes that a re-analysis settled: what the store holds is checked
    /// again under `GLUALS_DIAG_VERIFY`.
    pub fn note_settled(&self) {
        self.settled.notify_one();
    }

    pub fn set_open(&self, file_id: FileId, open: bool) {
        let mut state = self.state();
        if open {
            state.open.insert(file_id);
            if let Some(&(priority, _)) = state.dirty.get(&file_id)
                && priority > Priority::Open
            {
                state.invalidate(file_id, Priority::Open);
            }
        } else {
            state.open.remove(&file_id);
        }
    }

    /// Publishes the diagnostics held for `file_id` again with `version`, for
    /// a document opened on the text they were computed on.
    pub fn publish_opened(&self, file_id: FileId, version: i32) {
        let _publishing = self.publishing();
        let stored = {
            let state = self.state();
            state
                .is_valid(file_id)
                .then(|| {
                    state
                        .entries
                        .get(&file_id)
                        .map(|entry| entry.stored.clone())
                })
                .flatten()
        };
        if let Some(stored) = stored {
            self.client.publish_diagnostics(PublishDiagnosticsParams {
                uri: stored.uri,
                diagnostics: stored.diagnostics.to_vec(),
                version: Some(version),
            });
        }
    }

    /// Resolve when a computation finishes and when a file turns dirty, the
    /// two events that can end [`Self::edited_work_pending`].
    pub fn progress(&self) -> (Notified<'_>, Notified<'_>) {
        (self.computed.notified(), self.dirtied.notified())
    }

    /// Whether a file is being computed or a worker could claim a dirty Edited
    /// file now: its text was taken and the re-analysis has no work left in it.
    /// Counts the workers as started without spawning any, so queued jobs
    /// stay unclaimed.
    #[cfg(test)]
    pub(crate) fn mark_working(&self) {
        self.working
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub async fn edited_work_pending(&self, debounced: &DebouncedAnalysis) -> bool {
        if !self.working.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let edited = {
            let state = self.state();
            if !state.computing.is_empty() {
                return true;
            }
            state
                .queue
                .iter()
                .take_while(|&&(priority, _, _)| priority == Priority::Edited)
                .map(|&(_, _, file_id)| file_id)
                .collect::<Vec<_>>()
        };
        !edited.is_empty()
            && debounced
                .first_ready_for_diagnostics(&edited)
                .await
                .is_some()
    }

    /// The priority `file_id` waits at to be computed, if it is dirty.
    #[cfg(test)]
    pub(crate) fn dirty_priority(&self, file_id: FileId) -> Option<Priority> {
        self.state()
            .dirty
            .get(&file_id)
            .map(|&(priority, _)| priority)
    }

    /// Whether any of `file_ids` outside `except` is open in the client.
    pub fn any_open(&self, file_ids: &[FileId], except: &[FileId]) -> bool {
        let state = self.state();
        file_ids
            .iter()
            .any(|file_id| !except.contains(file_id) && state.open.contains(file_id))
    }

    /// Marks `file_ids` dirty; those in `edited` are computed first, then
    /// the open ones.
    pub fn invalidate(&self, file_ids: impl IntoIterator<Item = FileId>, edited: &[FileId]) {
        {
            let mut state = self.state();
            for file_id in file_ids {
                let priority = state.priority_of(file_id, edited.contains(&file_id));
                state.invalidate(file_id, priority);
            }
        }
        self.dirtied.notify_waiters();
    }

    /// Marks `file_ids`, in their order, then every other stored file dirty,
    /// as after a config change: nothing stored before holds.
    pub fn invalidate_all(&self, file_ids: impl IntoIterator<Item = FileId>) {
        {
            let mut state = self.state();
            state.config_gen += 1;
            let mut rest = state.entries.keys().copied().collect::<BTreeSet<_>>();
            for file_id in file_ids {
                rest.remove(&file_id);
                let priority = state.priority_of(file_id, false);
                state.invalidate(file_id, priority);
            }
            for file_id in rest {
                let priority = state.priority_of(file_id, false);
                state.invalidate(file_id, priority);
            }
        }
        self.dirtied.notify_waiters();
    }

    /// Forgets a file that left the workspace, clearing what the client
    /// shows for it.
    pub fn remove(&self, file_id: FileId, uri: &Uri) {
        let _publishing = self.publishing();
        let had = {
            let mut state = self.state();
            if let Some((_, token)) = state.computing.remove(&file_id) {
                token.cancel();
            }
            if let Some((priority, sequence)) = state.dirty.remove(&file_id) {
                state.queue.remove(&(priority, sequence, file_id));
            }
            state.open.remove(&file_id);
            state.entries.remove(&file_id).is_some()
        };
        if had {
            self.cleared(uri.clone());
        }
        self.computed.notify_waiters();
    }

    /// Forgets every file not in `live`, clearing what the client shows for
    /// those it had diagnostics for.
    pub fn forget_missing(&self, live: &HashSet<FileId>) {
        let _publishing = self.publishing();
        let gone = {
            let mut state = self.state();
            let missing = state
                .entries
                .keys()
                .chain(state.dirty.keys())
                .filter(|file_id| !live.contains(file_id))
                .copied()
                .collect::<BTreeSet<_>>();
            let mut gone = Vec::new();
            for file_id in missing {
                if let Some((_, token)) = state.computing.remove(&file_id) {
                    token.cancel();
                }
                if let Some((priority, sequence)) = state.dirty.remove(&file_id) {
                    state.queue.remove(&(priority, sequence, file_id));
                }
                if let Some(entry) = state.entries.remove(&file_id) {
                    gone.push(entry.stored.uri);
                }
            }
            gone
        };
        for uri in gone {
            self.cleared(uri);
        }
        self.computed.notify_waiters();
    }

    fn cleared(&self, uri: Uri) {
        self.client.publish_diagnostics(PublishDiagnosticsParams {
            uri,
            diagnostics: Vec::new(),
            version: None,
        });
    }

    /// A watchdog detail naming the files being computed.
    pub fn watchdog_detail(
        self: &Arc<Self>,
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    ) -> crate::util::WatchdogDetailSource {
        let store = self.clone();
        Arc::new(move || {
            let mut computing = store.state().computing.keys().copied().collect::<Vec<_>>();
            if computing.is_empty() {
                return None;
            }
            computing.sort_unstable();
            // `try_read`, because a computation stuck holding the lock must
            // still get a line out.
            let described = match analysis.try_read() {
                Ok(analysis) => {
                    let vfs = analysis.compilation.get_db().get_vfs();
                    computing
                        .iter()
                        .take(3)
                        .map(|file_id| {
                            vfs.get_file_path(file_id)
                                .map(|path| path.to_string_lossy().to_string())
                                .unwrap_or_else(|| format!("{file_id:?}"))
                        })
                        .collect::<Vec<_>>()
                }
                Err(_) => computing
                    .iter()
                    .take(3)
                    .map(|file_id| format!("{file_id:?}"))
                    .collect(),
            };
            Some(format!(
                "{} file(s) being diagnosed: {}",
                computing.len(),
                described.join(", ")
            ))
        })
    }

    /// `file_id`'s current diagnostics, computed at `priority` first if they
    /// do not hold. `None` when cancelled or the file has none to compute.
    pub async fn get(
        &self,
        file_id: FileId,
        priority: Priority,
        cancel: &CancellationToken,
    ) -> Option<StoredDiagnostics> {
        {
            let mut state = self.state();
            let queued = state.dirty.get(&file_id).map(|&(queued, _)| queued);
            if queued.is_some_and(|queued| queued > priority)
                || (queued.is_none()
                    && !state.computing.contains_key(&file_id)
                    && !state.is_valid(file_id))
            {
                state.invalidate(file_id, priority);
                drop(state);
                self.dirtied.notify_waiters();
            }
        }
        loop {
            let computed = self.computed.notified();
            tokio::pin!(computed);
            computed.as_mut().enable();
            {
                let state = self.state();
                if state.is_valid(file_id) {
                    return state
                        .entries
                        .get(&file_id)
                        .map(|entry| entry.stored.clone());
                }
                if !state.dirty.contains_key(&file_id) && !state.computing.contains_key(&file_id) {
                    // Computed to nothing: the file left the workspace.
                    return None;
                }
            }
            tokio::select! {
                _ = computed => {}
                _ = cancel.cancelled() => return None,
                _ = self.stopped.cancelled() => return None,
            }
        }
    }

    /// The current diagnostics of every file in `file_ids` that has any,
    /// computing those that do not hold; `progress` hears how many are done.
    pub async fn get_all(
        &self,
        file_ids: &[FileId],
        cancel: &CancellationToken,
        mut progress: impl FnMut(usize),
    ) -> Vec<StoredDiagnostics> {
        {
            let mut state = self.state();
            for &file_id in file_ids {
                if !state.is_valid(file_id)
                    && !state.dirty.contains_key(&file_id)
                    && !state.computing.contains_key(&file_id)
                {
                    let priority = state.priority_of(file_id, false);
                    state.invalidate(file_id, priority);
                }
            }
        }
        self.dirtied.notify_waiters();
        let mut stored = Vec::with_capacity(file_ids.len());
        for (done, &file_id) in file_ids.iter().enumerate() {
            progress(done);
            let priority = self.state().priority_of(file_id, false);
            match self.get(file_id, priority, cancel).await {
                Some(diagnostics) => stored.push(diagnostics),
                None if cancel.is_cancelled() || self.stopped.is_cancelled() => break,
                None => {}
            }
        }
        progress(file_ids.len());
        stored
    }

    async fn next_job(&self, debounced: &DebouncedAnalysis) -> Job {
        loop {
            let dirtied = self.dirtied.notified();
            tokio::pin!(dirtied);
            dirtied.as_mut().enable();
            let settled = debounced.reindex_notified();
            tokio::pin!(settled);
            settled.as_mut().enable();

            // While an edit settles, only the edited files go, each once its
            // own text has settled: the rest settle with the ripple.
            let quiet = !debounced.is_dirty();
            let candidates = self
                .state()
                .queue
                .iter()
                .filter(|&&(priority, _, _)| quiet || priority == Priority::Edited)
                .map(|&(_, _, file_id)| file_id)
                .collect::<Vec<_>>();
            if let Some(file_id) = debounced.first_ready_for_diagnostics(&candidates).await {
                let mut state = self.state();
                if let Some((priority, sequence)) = state.dirty.remove(&file_id) {
                    state.queue.remove(&(priority, sequence, file_id));
                    let token = self.stopped.child_token();
                    state.sequence += 1;
                    let id = state.sequence;
                    state.computing.insert(file_id, (id, token.clone()));
                    return Job {
                        file_id,
                        id,
                        token,
                        config_gen: state.config_gen,
                    };
                }
                // Claimed by another worker meanwhile: look again.
                continue;
            }

            tokio::select! {
                _ = dirtied => {}
                _ = settled => {}
            }
        }
    }

    async fn run(&self, job: Job) {
        let computed = self.compute(job.file_id, job.token.clone()).await;
        let analysis = self.analysis.read().await;
        let published = {
            let mut state = self.state();
            let current = state
                .computing
                .get(&job.file_id)
                .is_some_and(|(id, _)| *id == job.id);
            if !current {
                return;
            }
            state.computing.remove(&job.file_id);
            // A result finished as the store stopped is not published.
            let computed = if job.token.is_cancelled() {
                Computed::Cancelled
            } else {
                computed
            };
            match computed {
                Computed::Cancelled => {
                    let priority = state.priority_of(job.file_id, false);
                    state.invalidate(job.file_id, priority);
                    None
                }
                // What the client showed no longer holds, and nothing replaces
                // it until the file is invalidated again.
                Computed::Gone => state
                    .entries
                    .remove(&job.file_id)
                    .map(|entry| Published::Cleared(entry.stored.uri)),
                Computed::Diagnostics { revision, .. }
                    if !analysis.compilation.has_taken_text(job.file_id)
                        || analysis
                            .compilation
                            .get_db()
                            .get_vfs()
                            .get_file_revision(&job.file_id)
                            != Some(revision) =>
                {
                    // Computed on text that has changed since, or has newer
                    // text staged.
                    let priority = state.priority_of(job.file_id, false);
                    state.invalidate(job.file_id, priority);
                    None
                }
                Computed::Diagnostics {
                    uri, diagnostics, ..
                } => {
                    // The revision check holds them to the taken text, whose
                    // version a didOpen since they began may have set.
                    let version = analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_version(&job.file_id)
                        .filter(|_| state.open.contains(&job.file_id));
                    let result_id = result_id(&diagnostics);
                    // A file that moved shows its diagnostics at its new URI
                    // only.
                    let previous = state.entries.get(&job.file_id).map(|entry| &entry.stored);
                    let moved_from = previous
                        .filter(|previous| previous.uri != uri)
                        .map(|previous| previous.uri.clone());
                    let changed = moved_from.is_some()
                        || previous.is_none_or(|previous| previous.result_id != result_id);
                    let stored = StoredDiagnostics {
                        uri,
                        diagnostics: diagnostics.into(),
                        result_id,
                    };
                    state.entries.insert(
                        job.file_id,
                        Entry {
                            stored: stored.clone(),
                            config_gen: job.config_gen,
                            job: job.id,
                        },
                    );
                    changed.then_some(Published::Stored {
                        stored,
                        version,
                        moved_from,
                    })
                }
            }
        };
        drop(analysis);
        if let Some(published) = published {
            self.publish(&job, published);
        }
        if self.is_idle() {
            crate::util::ls_profile::emit_ls_diagnostics_idle();
            self.idle.notify_one();
        }
        self.computed.notify_waiters();
        self.dirtied.notify_waiters();
    }

    /// Sends what `job` left for its file as far as its entry still holds
    /// it: whatever replaced the entry since publishes that newer state.
    fn publish(&self, job: &Job, published: Published) {
        let _publishing = self.publishing();
        let (stored_by, shown_at) = {
            let state = self.state();
            let entry = state.entries.get(&job.file_id);
            (
                entry.map(|entry| entry.job),
                entry.map(|entry| entry.stored.uri.clone()),
            )
        };
        let clear = |uri: Uri| {
            if shown_at.as_ref() != Some(&uri) {
                self.cleared(uri);
            }
        };
        match published {
            Published::Stored {
                stored,
                version,
                moved_from,
            } => {
                if let Some(moved_from) = moved_from {
                    clear(moved_from);
                }
                if stored_by == Some(job.id) {
                    self.client.publish_diagnostics(PublishDiagnosticsParams {
                        uri: stored.uri,
                        diagnostics: stored.diagnostics.to_vec(),
                        version,
                    });
                }
            }
            Published::Cleared(uri) => clear(uri),
        }
    }

    fn is_idle(&self) -> bool {
        let state = self.state();
        state.dirty.is_empty() && state.computing.is_empty()
    }

    /// Files what the computed diagnostics read, a part per write lock, while
    /// no file or edit waits: the next re-analysis files whatever is left
    /// before it starts, holding up the edit it takes.
    async fn file_reads_while_idle(&self, debounced: &DebouncedAnalysis) {
        while !debounced.is_dirty() && self.is_idle() {
            let mut analysis = self.analysis.write().await;
            if debounced.is_dirty() || analysis.compilation.is_updating() {
                return;
            }
            if !analysis
                .compilation
                .file_some_diagnostic_reads(FILED_PER_LOCK)
            {
                return;
            }
            drop(analysis);
            tokio::task::yield_now().await;
        }
    }

    /// Under `GLUALS_DIAG_VERIFY`, computes every file whose stored
    /// diagnostics hold again and logs `LS_DIAG_DRIFT` for each that differs.
    /// Only a quiet index is compared: the pass stops once an edit is pending,
    /// and the next settle starts another, or once the store stops.
    async fn verify_stored(&self, debounced: &DebouncedAnalysis) {
        let held = {
            let state = self.state();
            let mut held = state
                .entries
                .iter()
                .filter(|(file_id, _)| state.is_valid(**file_id))
                .map(|(file_id, entry)| (*file_id, entry.stored.result_id))
                .collect::<Vec<_>>();
            held.sort_unstable();
            held
        };
        let (mut checked, mut drifted) = (0usize, 0usize);
        for (file_id, stored_id) in held {
            if debounced.is_dirty() || self.stopped.is_cancelled() {
                break;
            }
            let Computed::Diagnostics {
                uri, diagnostics, ..
            } = self.compute(file_id, self.stopped.child_token()).await
            else {
                continue;
            };
            if debounced.is_dirty() || self.stopped.is_cancelled() {
                break;
            }
            let state = self.state();
            let still_held = state.is_valid(file_id)
                && state
                    .entries
                    .get(&file_id)
                    .is_some_and(|entry| entry.stored.result_id == stored_id);
            if !still_held {
                continue;
            }
            checked += 1;
            if result_id(&diagnostics) != stored_id {
                drifted += 1;
                log::error!(
                    "LS_DIAG_DRIFT file={} stored diagnostics differ from a fresh computation",
                    uri.as_str()
                );
            }
        }
        log::info!("LS_DIAG_VERIFY checked={checked} drifted={drifted}");
    }

    async fn compute(&self, file_id: FileId, token: CancellationToken) -> Computed {
        let analysis = self.analysis.clone();
        let shared = self.shared.clone();
        #[cfg(test)]
        let panics = self
            .panic_computes
            .load(std::sync::atomic::Ordering::Acquire);
        let computed = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if panics {
                panic!("injected diagnostic computation panic");
            }
            if token.is_cancelled() {
                return Computed::Cancelled;
            }
            let analysis = analysis.blocking_read();
            let Some(uri) = analysis.get_uri(file_id) else {
                return Computed::Gone;
            };
            // Text the index has not taken in yet would be diagnosed against the
            // facts of the text before it.
            if !analysis.compilation.has_taken_text(file_id) {
                return Computed::Cancelled;
            }
            let Some(revision) = analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_revision(&file_id)
            else {
                return Computed::Gone;
            };
            let shared = shared.get_or_precompute(&analysis);
            match analysis.diagnose_file_with_shared(file_id, token.clone(), shared) {
                Some(diagnostics) => Computed::Diagnostics {
                    uri,
                    diagnostics,
                    revision,
                },
                None if token.is_cancelled() => Computed::Cancelled,
                // Diagnostics are off for the file, so it shows none, and what
                // the client showed before is cleared.
                None => Computed::Diagnostics {
                    uri,
                    diagnostics: Vec::new(),
                    revision,
                },
            }
        })
        .await;
        computed.unwrap_or_else(|error| {
            if !error.is_panic() {
                return Computed::Cancelled;
            }
            log::error!(
                "LS_DIAGNOSE_FAILED diagnostics of {file_id:?} panicked: {error}; computed again once the file is invalidated"
            );
            Computed::Gone
        })
    }
}

/// What one computation sends the client.
enum Published {
    /// The file's diagnostics, and the URI it showed them at before a move.
    Stored {
        stored: StoredDiagnostics,
        version: Option<i32>,
        moved_from: Option<Uri>,
    },
    /// The diagnostics the client shows at this URI no longer hold.
    Cleared(Uri),
}

enum Computed {
    Diagnostics {
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        /// The text revision they were computed on.
        revision: u64,
    },
    /// Nothing to hold: the file left the workspace, or computing its
    /// diagnostics failed, which is tried again only once they are invalidated.
    Gone,
    Cancelled,
}

/// An order-insensitive hash of `diagnostics`, from each one's serialized
/// form so every field counts.
pub fn result_id(diagnostics: &[Diagnostic]) -> u64 {
    let mut item_hashes = diagnostics
        .iter()
        .map(|diagnostic| {
            let mut hasher = DefaultHasher::new();
            if let Ok(serialized) = serde_json::to_vec(diagnostic) {
                serialized.hash(&mut hasher);
            }
            hasher.finish()
        })
        .collect::<Vec<_>>();
    item_hashes.sort_unstable();
    let mut hasher = DefaultHasher::new();
    item_hashes.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use glua_code_analysis::FileId;
    use googletest::prelude::*;
    use lsp_server::{Connection, Message};
    use lsp_types::{ClientCapabilities, PublishDiagnosticsParams};
    use tokio_util::sync::CancellationToken;

    use super::{Priority, State};
    use crate::context::{ServerContext, ServerContextSnapshot};

    /// The diagnostics published to the client so far, in order.
    fn published(peer: &Connection) -> Vec<PublishDiagnosticsParams> {
        peer.receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics" =>
                {
                    serde_json::from_value(notification.params).ok()
                }
                _ => None,
            })
            .collect()
    }

    #[gtest]
    fn the_first_dirty_file_is_the_most_urgent_one() -> Result<()> {
        let mut state = State::default();
        let (closed, open, edited) = (FileId::new(1), FileId::new(2), FileId::new(3));
        state.open.insert(open);
        for (file_id, is_edited) in [(closed, false), (open, false), (edited, true)] {
            let priority = state.priority_of(file_id, is_edited);
            state.invalidate(file_id, priority);
        }
        let order = state
            .queue
            .iter()
            .map(|&(priority, _, file_id)| (priority, file_id))
            .collect::<Vec<_>>();
        verify_that!(
            order,
            eq(&vec![
                (Priority::Edited, edited),
                (Priority::Open, open),
                (Priority::Closed, closed)
            ])
        )
    }

    #[gtest]
    fn invalidating_a_file_being_computed_cancels_the_computation() -> Result<()> {
        let mut state = State::default();
        let file_id = FileId::new(1);
        let token = CancellationToken::new();
        state.computing.insert(file_id, (1, token.clone()));

        state.invalidate(file_id, Priority::Closed);

        verify_that!(token.is_cancelled(), eq(true))?;
        verify_that!(state.computing.contains_key(&file_id), eq(false))?;
        verify_that!(state.dirty.contains_key(&file_id), eq(true))
    }

    /// A job a worker claims after the store stopped is cancelled before it
    /// computes, so nothing is computed or published past shutdown.
    #[test]
    fn a_job_claimed_after_stop_is_cancelled() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::TempDir::new("gmod_glua_ls_store_stop");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/stop_test.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace.to_path_buf());
                analysis
                    .update_file_by_uri(&uri, Some("local unused = 1".to_string()))
                    .map(|(file_id, _)| file_id)
                    .expect("file id")
            };
            let store = snapshot.file_diagnostic().store();
            store.stop();
            store.invalidate([file_id], &[]);

            let job = tokio::time::timeout(
                Duration::from_secs(10),
                store.next_job(snapshot.debounced_analysis()),
            )
            .await
            .expect("the dirty file is claimed");

            assert!(job.token.is_cancelled());
        });
    }

    /// A file computed again to the same diagnostics is not published again,
    /// and a removed one is cleared once.
    #[gtest]
    fn only_changed_diagnostics_are_published() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/store_test.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .map(|(file_id, _)| file_id)
                    .expect("file id")
            };
            let store = snapshot.file_diagnostic().store();
            let cancel = CancellationToken::new();

            let first = store
                .get(file_id, Priority::Edited, &cancel)
                .await
                .expect("diagnostics");
            store.invalidate([file_id], &[]);
            let second = store
                .get(file_id, Priority::Edited, &cancel)
                .await
                .expect("diagnostics");
            verify_that!(second.result_id, eq(first.result_id))?;

            store.remove(file_id, &uri);
            tokio::time::sleep(Duration::from_millis(50)).await;
            let published = published(&peer)
                .into_iter()
                .filter(|params| params.uri == uri)
                .map(|params| params.diagnostics.len())
                .collect::<Vec<_>>();
            verify_that!(published, eq(&vec![first.diagnostics.len(), 0]))
        })
    }

    /// A computation that panics is tried again only once the file is
    /// invalidated: trying again at once would spin a worker on it.
    /// Sending to a client that has not read the last message yet blocks, as
    /// stdio does; an edit's write lock must not wait for it.
    #[gtest]
    fn a_publish_waiting_on_the_client_does_not_hold_the_analysis() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (sender, peer_receiver) = crossbeam_channel::bounded(0);
        let (_peer_sender, receiver) = crossbeam_channel::bounded(0);
        let connection = Connection { sender, receiver };
        let result = runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/store_test.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .map(|(file_id, _)| file_id)
                    .expect("file id")
            };
            let store = snapshot.file_diagnostic().store();
            store.invalidate([file_id], &[file_id]);
            tokio::time::timeout(Duration::from_secs(10), async {
                while !store.state().entries.contains_key(&file_id) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the store records a computation for the file");

            let write = tokio::time::timeout(Duration::from_secs(2), snapshot.analysis().write())
                .await
                .map(drop);
            verify_that!(write.is_ok(), eq(true))
        });
        std::thread::spawn(move || while peer_receiver.recv().is_ok() {});
        result
    }

    #[gtest]
    fn a_panicking_computation_waits_for_the_next_invalidation() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_panic");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/panics.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .map(|(file_id, _)| file_id)
                    .expect("file id")
            };
            let store = snapshot.file_diagnostic().store();
            let cancel = CancellationToken::new();
            store
                .panic_computes
                .store(true, std::sync::atomic::Ordering::Release);

            let failed = tokio::time::timeout(
                Duration::from_secs(10),
                store.get(file_id, Priority::Edited, &cancel),
            )
            .await
            .expect("a failed computation ends the wait");
            verify_that!(failed.is_none(), eq(true))?;
            verify_that!(store.is_idle(), eq(true))?;

            store
                .panic_computes
                .store(false, std::sync::atomic::Ordering::Release);
            store.invalidate([file_id], &[]);
            let computed = store.get(file_id, Priority::Edited, &cancel).await;
            verify_that!(computed.is_some(), eq(true))
        })
    }

    /// Diagnostics shown for text a failed computation can no longer speak
    /// for are cleared rather than left in place.
    #[gtest]
    fn a_failed_computation_clears_what_the_client_shows() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_cleared");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/cleared.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .map(|(file_id, _)| file_id)
                    .expect("file id")
            };
            let store = snapshot.file_diagnostic().store();
            let cancel = CancellationToken::new();
            store.get(file_id, Priority::Edited, &cancel).await;

            store
                .panic_computes
                .store(true, std::sync::atomic::Ordering::Release);
            store.invalidate([file_id], &[]);
            store.get(file_id, Priority::Edited, &cancel).await;

            let published = published(&peer)
                .into_iter()
                .map(|params| params.diagnostics.is_empty())
                .collect::<Vec<_>>();
            verify_that!(published, eq(&vec![false, true]))
        })
    }

    /// An open file's diagnostics are published with its document version, a
    /// closed file's without one.
    #[gtest]
    fn open_files_publish_with_their_document_version() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_version");
            let open_uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/open.lua"))
                .expect("file URI should parse");
            let closed_uri =
                glua_code_analysis::file_path_to_uri(&workspace.join("lua/closed.lua"))
                    .expect("file URI should parse");
            let (open_id, closed_id) = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                let mut add = |uri| {
                    let (file_id, _) = analysis
                        .update_file_by_uri(uri, Some("local unused = UndefinedGlobal".to_string()))
                        .expect("file id");
                    analysis
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .update_file_version(&file_id, Some(7));
                    file_id
                };
                (add(&open_uri), add(&closed_uri))
            };
            let store = snapshot.file_diagnostic().store();
            let cancel = CancellationToken::new();
            store.set_open(open_id, true);

            store.get(open_id, Priority::Edited, &cancel).await;
            store.get(closed_id, Priority::Closed, &cancel).await;

            let versions = published(&peer)
                .into_iter()
                .map(|params| (params.uri, params.version))
                .collect::<Vec<_>>();
            verify_that!(versions, eq(&vec![(open_uri, Some(7)), (closed_uri, None)]))
        })
    }

    async fn set_diagnostics_enabled(snapshot: &ServerContextSnapshot, enable: bool) {
        let mut analysis = snapshot.analysis().write().await;
        let mut emmyrc = (*analysis.get_emmyrc()).clone();
        emmyrc.diagnostics.enable = enable;
        analysis.update_config(std::sync::Arc::new(emmyrc));
    }

    /// Turning diagnostics off clears what an open and a closed file showed,
    /// and turning them on again brings it back.
    #[gtest]
    fn turning_diagnostics_off_clears_what_files_showed() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_off");
            let uris = ["lua/open.lua", "lua/closed.lua"].map(|path| {
                glua_code_analysis::file_path_to_uri(&workspace.join(path))
                    .expect("file URI should parse")
            });
            let file_ids = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                uris.clone().map(|uri| {
                    analysis
                        .update_file_by_uri(
                            &uri,
                            Some("local unused = UndefinedGlobal".to_string()),
                        )
                        .expect("file id")
                        .0
                })
            };
            let store = snapshot.file_diagnostic().store();
            store.set_open(file_ids[0], true);
            let cancel = CancellationToken::new();
            let diagnose = || store.get_all(&file_ids, &cancel, |_| {});

            let shown = diagnose().await[0].diagnostics.len();
            verify_that!(shown, gt(0))?;
            set_diagnostics_enabled(&snapshot, false).await;
            store.invalidate_all([]);
            diagnose().await;
            set_diagnostics_enabled(&snapshot, true).await;
            store.invalidate_all([]);
            diagnose().await;

            let published = published(&peer);
            for uri in &uris {
                let counts = published
                    .iter()
                    .filter(|params| &params.uri == uri)
                    .map(|params| params.diagnostics.len())
                    .collect::<Vec<_>>();
                verify_that!(counts, eq(&vec![shown, 0, shown]))?;
            }
            Ok(())
        })
    }

    /// A wait for diagnostics no worker will compute ends when the server
    /// shuts down, whether it began before the shutdown or after it.
    #[gtest]
    fn shutting_down_releases_waits_for_diagnostics() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_stop");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/pending.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                let (file_id, _) = analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .expect("file id");
                // Text no re-analysis takes keeps the file from being computed.
                analysis.update_file_text_only(&uri, "local a = Missing".to_string());
                file_id
            };
            let store = snapshot.file_diagnostic().store().clone();
            let pending = tokio::spawn({
                let store = store.clone();
                async move {
                    store
                        .get_all(&[file_id], &CancellationToken::new(), |_| {})
                        .await
                }
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            verify_that!(pending.is_finished(), eq(false))?;

            context.close().await;
            let released = tokio::time::timeout(Duration::from_secs(5), pending).await;
            verify_that!(released.is_ok(), eq(true))?;

            store.invalidate([file_id], &[file_id]);
            let after = tokio::time::timeout(
                Duration::from_secs(5),
                store.get(file_id, Priority::Edited, &CancellationToken::new()),
            )
            .await;
            verify_that!(after, ok(none()))
        })
    }

    /// Diagnostics computed while a file was closed go out again with the
    /// document version once it opens on the same text.
    #[gtest]
    fn a_file_opened_on_diagnosed_text_republishes_with_its_version() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_reopen");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/opened.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .expect("file id")
                    .0
            };
            let store = snapshot.file_diagnostic().store();
            store
                .get(file_id, Priority::Closed, &CancellationToken::new())
                .await
                .expect("diagnostics");
            store.set_open(file_id, true);

            store.publish_opened(file_id, 3);

            let versions = published(&peer)
                .into_iter()
                .map(|params| params.version)
                .collect::<Vec<_>>();
            verify_that!(versions, eq(&vec![None, Some(3)]))
        })
    }

    /// Diagnostics are computed on the text a take installed and published
    /// with its version: none goes out while newer text is staged, and the
    /// take's publish carries the newer version.
    #[gtest]
    fn diagnostics_wait_for_staged_text_and_carry_its_version() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_diagnostic_store_staged");
            let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/staged.lua"))
                .expect("file URI should parse");
            let file_id = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace);
                let (file_id, _) = analysis
                    .update_file_by_uri(&uri, Some("local unused = UndefinedGlobal".to_string()))
                    .expect("file id");
                analysis.update_file_text_only(&uri, "local a = Missing".to_string());
                analysis
                    .compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, Some(2));
                file_id
            };
            let store = snapshot.file_diagnostic().store();
            store.set_open(file_id, true);
            store.invalidate([file_id], &[file_id]);
            tokio::time::sleep(Duration::from_millis(100)).await;
            verify_that!(published(&peer), is_empty())?;

            {
                let mut analysis = snapshot.analysis().write().await;
                analysis.begin_reindex(vec![file_id]);
                while !matches!(
                    analysis.advance_reindex(glua_code_analysis::Slice::Whole),
                    glua_code_analysis::UpdateProgress::Settled(_)
                ) {}
            }
            store.invalidate([file_id], &[file_id]);
            store
                .get(file_id, Priority::Edited, &CancellationToken::new())
                .await
                .expect("diagnostics");
            let versions = published(&peer)
                .into_iter()
                .map(|params| params.version)
                .collect::<Vec<_>>();
            verify_that!(versions, eq(&vec![Some(2)]))
        })
    }
}
