//! The server's diagnostics for every workspace file, each computed again
//! only once what it was computed from changed: the file's text, the config,
//! or a fact its diagnostics read, which a settled re-analysis reports.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};

use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, DebouncedAnalysis, file_diagnostic::SharedDiagnosticDataCache};

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
    /// Where changed diagnostics are published.
    client: Arc<ClientProxy>,
    /// `GLUALS_DIAG_VERIFY`: every stored result is checked against a fresh
    /// computation, and a difference logged as `LS_DIAG_DRIFT`.
    verify: bool,
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
            client,
            verify: std::env::var_os("GLUALS_DIAG_VERIFY").is_some(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Starts `workers` tasks computing dirty files once no edit is pending,
    /// the edited ones as soon as their own text has settled, and the
    /// verifier under `GLUALS_DIAG_VERIFY`.
    pub fn start(
        self: &Arc<Self>,
        workers: usize,
        debounced: Arc<DebouncedAnalysis>,
        shutdown: CancellationToken,
    ) {
        for _ in 0..workers.max(1) {
            let store = self.clone();
            let debounced = debounced.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let job = tokio::select! {
                        job = store.next_job(&debounced) => job,
                        _ = shutdown.cancelled() => return,
                    };
                    store.run(job).await;
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
                None if cancel.is_cancelled() => break,
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
                    let token = CancellationToken::new();
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
        let computed = compute(
            self.analysis.clone(),
            self.shared.clone(),
            job.file_id,
            job.token.clone(),
        )
        .await;
        // Held until the result is published, so no edit lands between the
        // revision check and the publish.
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
            match computed {
                Computed::Cancelled => {
                    let priority = state.priority_of(job.file_id, false);
                    state.invalidate(job.file_id, priority);
                    None
                }
                Computed::Gone => {
                    state.entries.remove(&job.file_id);
                    None
                }
                Computed::Diagnostics { revision, .. }
                    if analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_revision(&job.file_id)
                        != Some(revision) =>
                {
                    // Computed on text that has changed since.
                    let priority = state.priority_of(job.file_id, false);
                    state.invalidate(job.file_id, priority);
                    None
                }
                Computed::Diagnostics {
                    uri,
                    diagnostics,
                    version,
                    ..
                } => {
                    let version = version.filter(|_| state.open.contains(&job.file_id));
                    let result_id = result_id(&diagnostics);
                    let changed = state
                        .entries
                        .get(&job.file_id)
                        .is_none_or(|entry| entry.stored.result_id != result_id);
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
                        },
                    );
                    changed.then_some((stored, version))
                }
            }
        };
        if let Some((stored, version)) = published {
            self.client.publish_diagnostics(PublishDiagnosticsParams {
                uri: stored.uri,
                diagnostics: stored.diagnostics.to_vec(),
                version,
            });
        }
        drop(analysis);
        self.computed.notify_waiters();
        self.dirtied.notify_waiters();
    }

    /// Under `GLUALS_DIAG_VERIFY`, computes every file whose stored
    /// diagnostics hold again and logs `LS_DIAG_DRIFT` for each that differs.
    /// Only a quiet index is compared: the pass stops once an edit is pending,
    /// and the next settle starts another.
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
            if debounced.is_dirty() {
                break;
            }
            let Computed::Diagnostics {
                uri, diagnostics, ..
            } = compute(
                self.analysis.clone(),
                self.shared.clone(),
                file_id,
                CancellationToken::new(),
            )
            .await
            else {
                continue;
            };
            if debounced.is_dirty() {
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
}

enum Computed {
    Diagnostics {
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        /// The text revision they were computed on.
        revision: u64,
        version: Option<i32>,
    },
    /// The file left the workspace.
    Gone,
    Cancelled,
}

async fn compute(
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shared: SharedDiagnosticDataCache,
    file_id: FileId,
    token: CancellationToken,
) -> Computed {
    let computed = tokio::task::spawn_blocking(move || {
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
        let vfs = analysis.compilation.get_db().get_vfs();
        let (Some(revision), version) = (
            vfs.get_file_revision(&file_id),
            vfs.get_file_version(&file_id),
        ) else {
            return Computed::Gone;
        };
        let shared = shared.get_or_precompute(&analysis);
        match analysis.diagnose_file_with_shared(file_id, token.clone(), shared) {
            Some(diagnostics) => Computed::Diagnostics {
                uri,
                diagnostics,
                revision,
                version,
            },
            None if token.is_cancelled() => Computed::Cancelled,
            None => Computed::Gone,
        }
    })
    .await;
    computed.unwrap_or_else(|error| {
        log::warn!("diagnostic computation for {file_id:?} failed: {error}");
        Computed::Cancelled
    })
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
    use crate::context::ServerContext;

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

    /// A file computed again to the same diagnostics is not published again,
    /// and a removed one is cleared once.
    #[gtest]
    fn only_changed_diagnostics_are_published() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_diagnostic_store");
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

    /// An open file's diagnostics are published with its document version, a
    /// closed file's without one.
    #[gtest]
    fn open_files_publish_with_their_document_version() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_diagnostic_store_version");
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
}
