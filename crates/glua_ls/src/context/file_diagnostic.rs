use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use glua_code_analysis::SharedDiagnosticComponentCache;
use glua_code_analysis::{EmmyLuaAnalysis, FileId, SharedDiagnosticData};
use log::{debug, info};
use lsp_types::{Diagnostic, Uri};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::util::{LongRunningWatchdogStatus, spawn_long_running_watchdog};

use super::{ClientProxy, ProgressTask, StatusBar};

#[derive(Clone, Default)]
pub(crate) struct SharedDiagnosticDataCache {
    cached: Arc<StdMutex<Option<Arc<SharedDiagnosticData>>>>,
    components: SharedDiagnosticComponentCache,
}

impl SharedDiagnosticDataCache {
    pub(crate) fn get_or_precompute(
        &self,
        analysis: &EmmyLuaAnalysis,
    ) -> Arc<SharedDiagnosticData> {
        let mut cached = self.cached.lock().ok();
        if let Some(cache) = cached.as_ref()
            && let Some(shared_data) = cache.as_ref()
            && self.components.is_fresh(analysis.compilation.get_db())
        {
            return shared_data.clone();
        }

        let shared_data = analysis
            .diagnostic
            .precompute_shared_data_with_cache(&analysis.compilation, &self.components);
        if let Some(cache) = cached.as_mut() {
            **cache = Some(shared_data.clone());
        }
        shared_data
    }

    pub(crate) fn invalidate(&self) {
        if let Ok(mut cache) = self.cached.lock() {
            *cache = None;
        }
        self.components.invalidate();
    }

    #[cfg(test)]
    fn cached_ptr(&self) -> Option<*const SharedDiagnosticData> {
        self.cached
            .lock()
            .expect("shared diagnostic cache mutex should not be poisoned")
            .as_ref()
            .map(Arc::as_ptr)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.cached
            .lock()
            .expect("shared diagnostic cache mutex should not be poisoned")
            .is_none()
    }
}

#[derive(Clone)]
pub struct FileDiagnostic {
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    client: Arc<ClientProxy>,
    status_bar: Arc<StatusBar>,
    workspace_loaded_notified: Arc<AtomicBool>,
    startup_complete_notified: Arc<AtomicBool>,
    workspace_diagnostic_token: Arc<Mutex<Option<CancellationToken>>>,
    /// Files an edit reported as diagnostically touched (re-analysed files,
    /// readers the export diff named, textual-refresh candidates). The next
    /// `Fast` workspace sweep diagnoses exactly this set instead of the whole
    /// workspace: nothing outside it can have a stale report, since a
    /// diagnostic only moves when a fact the file reads moved, and the dirty
    /// machinery names every such file. `None`-equivalent (empty) means the
    /// arm was not edit-scoped — open/reindex/config paths still sweep all.
    scoped_sweep_files: Arc<Mutex<rustc_hash::FxHashSet<FileId>>>,
    shared_diagnostic_data_cache: SharedDiagnosticDataCache,
    /// Every workspace file's diagnostics, computed again only when they
    /// may have changed.
    store: Arc<super::DiagnosticStore>,
}

impl FileDiagnostic {
    /// `fetches_diagnostics`: whether diagnostics reach this client by being
    /// fetched *and* the client accepts `workspace/diagnostic/refresh`. Such a
    /// client gets nothing published: VS Code keeps a pushed collection beside
    /// the pulled one and the Problems panel would show their union.
    pub fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        status_bar: Arc<StatusBar>,
        client: Arc<ClientProxy>,
        fetches_diagnostics: bool,
    ) -> Self {
        let shared_diagnostic_data_cache = SharedDiagnosticDataCache::default();
        let store = Arc::new(super::DiagnosticStore::new(
            analysis.clone(),
            shared_diagnostic_data_cache.clone(),
            (!fetches_diagnostics).then(|| client.clone()),
        ));
        Self {
            analysis,
            client,
            workspace_loaded_notified: Arc::new(AtomicBool::new(false)),
            startup_complete_notified: Arc::new(AtomicBool::new(false)),
            workspace_diagnostic_token: Arc::new(Mutex::new(None)),
            scoped_sweep_files: Arc::new(Mutex::new(rustc_hash::FxHashSet::default())),
            shared_diagnostic_data_cache,
            store,
            status_bar,
        }
    }

    pub fn store(&self) -> &Arc<super::DiagnosticStore> {
        &self.store
    }

    pub(crate) fn shared_diagnostic_data_cache(&self) -> SharedDiagnosticDataCache {
        self.shared_diagnostic_data_cache.clone()
    }

    pub fn invalidate_shared_diagnostic_data(&self) {
        self.shared_diagnostic_data_cache.invalidate();
    }

    pub fn is_workspace_loaded(&self) -> bool {
        self.workspace_loaded_notified.load(Ordering::Acquire)
    }

    pub fn notify_workspace_loaded(&self) {
        if self.workspace_loaded_notified.swap(true, Ordering::AcqRel) {
            return;
        }

        info!("workspace loaded; language server is ready while diagnostics may continue");
        self.send_server_status("workspaceLoaded");
    }

    fn notify_startup_complete(&self) {
        if self.startup_complete_notified.swap(true, Ordering::AcqRel) {
            return;
        }

        info!("workspace diagnostics complete; language server startup fully complete");
        self.send_server_status("startupComplete");
    }

    fn send_server_status(&self, state: &'static str) {
        self.client.send_notification(
            "gluals/serverStatus",
            serde_json::json!({
                "state": state,
            }),
        );
    }

    /// The last diagnostics computed for `uri`, whether or not they still
    /// hold.
    pub async fn cached_file_diagnostics(&self, uri: &Uri) -> Option<Vec<Diagnostic>> {
        let file_id = self.analysis.read().await.get_file_id(uri)?;
        self.store
            .last(file_id)
            .map(|stored| stored.diagnostics.to_vec())
    }

    /// Accumulate files whose diagnostics an edit may have moved. The debounced
    /// loop extends this across the whole burst (each ripple's widened set,
    /// each textual-refresh batch), so the armed idle sweep covers every file
    /// the burst touched rather than only the last settle's.
    pub async fn note_scoped_sweep_files(&self, file_ids: impl IntoIterator<Item = FileId>) {
        self.scoped_sweep_files.lock().await.extend(file_ids);
    }

    /// The files an edit-scoped sweep owes, sorted for deterministic worker
    /// dispatch. `None` means no scoped set accumulated — sweep the whole
    /// workspace (the open/reindex/config arms leave it empty).
    async fn take_scoped_sweep_files(&self) -> Option<Vec<FileId>> {
        let mut slot = self.scoped_sweep_files.lock().await;
        if slot.is_empty() {
            return None;
        }
        let mut files: Vec<FileId> = slot.drain().collect();
        files.sort_unstable();
        Some(files)
    }

    /// Put back the undiagnosed remainder when a scoped sweep is cut short, so
    /// the re-armed pull re-sweeps it instead of losing it to the cancel.
    async fn restore_scoped_sweep_files(&self, file_ids: impl IntoIterator<Item = FileId>) {
        self.scoped_sweep_files.lock().await.extend(file_ids);
    }

    /// Forget the pending scoped set because a sweep about to run covers the
    /// whole workspace anyway (config reloads, explicit reindexes): the noted
    /// files are swept as part of it, so nothing is lost.
    pub async fn clear_scoped_sweep_files(&self) {
        self.scoped_sweep_files.lock().await.clear();
    }

    /// After `interval` ms, computes every workspace file's diagnostics again;
    /// the store publishes those that changed to a client that does not fetch
    /// them.
    pub async fn add_workspace_diagnostic_task(&self, interval: u64, silent: bool) {
        let cancel_token = self.replace_workspace_diagnostic_token().await;
        let file_diagnostic = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {
                    file_diagnostic.sweep(None, silent, "push", cancel_token).await;
                }
                _ = cancel_token.cancelled() => {
                    log::info!("cancel workspace diagnostic");
                }
            }
        });
    }

    async fn replace_workspace_diagnostic_token(&self) -> CancellationToken {
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        let cancel_token = CancellationToken::new();
        token.replace(cancel_token.clone());
        cancel_token
    }

    pub async fn cancel_workspace_diagnostic(&self) {
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.take();
    }

    /// `uri`'s current diagnostics, computed ahead of every other file if
    /// they do not hold.
    pub async fn pull_file_diagnostics(
        &self,
        uri: Uri,
        cancel_token: CancellationToken,
    ) -> Option<Vec<Diagnostic>> {
        let file_id = self.analysis.read().await.get_file_id(&uri)?;
        self.store
            .get(file_id, super::Priority::Edited, &cancel_token)
            .await
            .map(|stored| stored.diagnostics.to_vec())
    }

    pub async fn pull_workspace_diagnostics_slow(
        &self,
        cancel_token: CancellationToken,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        info!("workspace diagnostic pull slow started");
        self.adopt_workspace_diagnostic_token(&cancel_token).await;
        self.sweep(None, false, "slow pull", cancel_token).await
    }

    pub async fn pull_workspace_diagnostics_fast(
        &self,
        cancel_token: CancellationToken,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        info!("workspace diagnostic pull fast started");
        let sweep_started = crate::util::ls_profile::profile_instant();
        self.adopt_workspace_diagnostic_token(&cancel_token).await;
        // An armed idle sweep after an edit is scoped to the files the burst
        // touched; the other Fast arms (open, reindex, config) leave the slot
        // empty and sweep everything.
        let scoped = self.take_scoped_sweep_files().await;
        let was_scoped = scoped.is_some();
        let result = self
            .sweep(scoped.clone(), false, "fast pull", cancel_token.clone())
            .await;
        if cancel_token.is_cancelled() {
            // Re-diagnosing the few files already completed is cheap and keeps
            // the next armed sweep's coverage exact.
            self.restore_scoped_sweep_files(scoped.unwrap_or_default())
                .await;
        }
        if let Some(started) = sweep_started {
            crate::util::ls_profile::emit_ls_diag_sweep(
                result.len(),
                started.elapsed(),
                was_scoped,
            );
        }
        result
    }

    async fn adopt_workspace_diagnostic_token(&self, cancel_token: &CancellationToken) {
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.replace(cancel_token.clone());
    }

    /// The current diagnostics of the workspace files in `scope`, or of every
    /// workspace file computed again when there is none.
    async fn sweep(
        &self,
        scope: Option<Vec<FileId>>,
        silent: bool,
        label: &'static str,
        cancel_token: CancellationToken,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        let watchdog_status =
            LongRunningWatchdogStatus::new(format!("Preparing workspace diagnostics ({label})"));
        let _watchdog =
            spawn_long_running_watchdog("workspace diagnostics", watchdog_status.clone());
        let status_bar = self.status_bar.clone();
        if !silent {
            status_bar
                .create_progress_task(ProgressTask::DiagnoseWorkspace)
                .await;
            status_bar.update_progress_task(
                ProgressTask::DiagnoseWorkspace,
                None,
                Some(String::from("Preparing diagnostics")),
            );
        }

        let mut file_ids = self
            .analysis
            .read()
            .await
            .get_main_workspace_file_ids_for_diagnostics();
        match &scope {
            Some(scoped_ids) => {
                let scoped_set = scoped_ids.iter().copied().collect::<HashSet<_>>();
                file_ids.retain(|file_id| scoped_set.contains(file_id));
                info!(
                    "edit-scoped workspace diagnostic sweep: {} file(s)",
                    file_ids.len()
                );
            }
            None => self.store.invalidate_all(file_ids.iter().copied()),
        }

        watchdog_status.set_detail_source(self.store.watchdog_detail(self.analysis.clone()));
        let total = file_ids.len();
        let progress_label = format!("Diagnosing workspace files ({label})");
        watchdog_status.set_progress(&progress_label, 0, total);
        let mut last_percentage = None;
        let stored = self
            .store
            .get_all(&file_ids, &cancel_token, |done| {
                watchdog_status.set_progress(&progress_label, done, total);
                let percentage = (done * 100 / total.max(1)) as u32;
                if !silent && last_percentage != Some(percentage) {
                    last_percentage = Some(percentage);
                    status_bar.update_progress_task(
                        ProgressTask::DiagnoseWorkspace,
                        Some(percentage),
                        Some(format!("Diagnosing {done}/{total} ({percentage}%)")),
                    );
                }
            })
            .await;

        if !silent {
            status_bar.finish_progress_task(
                ProgressTask::DiagnoseWorkspace,
                Some("Diagnostics complete".to_string()),
            );
        }
        if cancel_token.is_cancelled() {
            info!("workspace diagnostic {label} cancelled");
        } else {
            self.notify_startup_complete();
        }
        stored
            .into_iter()
            .map(|stored| (stored.uri, stored.diagnostics.to_vec()))
            .collect()
    }
}

pub(crate) fn workspace_diagnostic_parallelism() -> usize {
    if let Ok(raw) = std::env::var("GLUALS_WORKSPACE_DIAGNOSTIC_PARALLELISM")
        && let Ok(parsed) = raw.parse::<usize>()
        && parsed > 0
    {
        return parsed;
    }

    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .clamp(1, 16)
}

#[cfg(test)]
mod tests {
    use crate::context::{ClientProxy, ServerContext, StatusBar};
    use glua_code_analysis::{DiagnosticCode, EmmyLuaAnalysis, Emmyrc, FileId, file_path_to_uri};
    use googletest::prelude::*;
    use lsp_server::{Connection, Message};
    use lsp_types::{ClientCapabilities, NumberOrString};
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    use super::FileDiagnostic;

    #[gtest]
    fn concurrent_diagnostic_pulls_share_the_first_precompute() {
        let mut analysis = EmmyLuaAnalysis::new();
        let workspace = std::env::temp_dir().join("gmod_glua_ls_concurrent_shared_diagnostics");
        analysis.add_main_workspace(workspace.clone());
        for i in 0..8 {
            let uri = file_path_to_uri(&workspace.join(format!("lua/autorun/shared_{i}.lua")))
                .expect("workspace URI should parse");
            analysis.update_file_by_uri(
                &uri,
                Some(format!(
                    "function SharedApi{i}() return true end SharedApi{i}()"
                )),
            );
        }
        let analysis = RwLock::new(analysis);
        let cache = super::SharedDiagnosticDataCache::default();
        let barrier = std::sync::Barrier::new(8);
        for _ in 0..2 {
            let bundles = std::thread::scope(|scope| {
                let workers = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            let guard = analysis.blocking_read();
                            barrier.wait();
                            cache.get_or_precompute(&guard)
                        })
                    })
                    .collect::<Vec<_>>();
                workers
                    .into_iter()
                    .map(|worker| worker.join().expect("diagnostic worker should finish"))
                    .collect::<Vec<_>>()
            });
            assert!(
                bundles
                    .iter()
                    .all(|bundle| Arc::ptr_eq(bundle, &bundles[0]))
            );
            cache.invalidate();
        }
    }

    #[gtest]
    fn workspace_loaded_notification_does_not_suppress_startup_complete() -> Result<()> {
        let (connection, peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone(), true));
        let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
        let file_diagnostic = FileDiagnostic::new(analysis, status_bar, client, false);

        file_diagnostic.notify_workspace_loaded();
        file_diagnostic.notify_workspace_loaded();
        file_diagnostic.notify_startup_complete();

        let statuses = peer
            .receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "gluals/serverStatus" =>
                {
                    notification
                        .params
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        verify_that!(
            statuses.as_slice(),
            eq(&["workspaceLoaded".to_string(), "startupComplete".to_string()])
        )?;
        Ok(())
    }

    #[gtest]
    fn single_file_diagnostics_use_shared_workspace_data() -> Result<()> {
        let mut analysis = EmmyLuaAnalysis::new();
        let workspace = std::env::temp_dir().join("gmod_glua_ls_shared_single_file_diagnostics");
        analysis.add_main_workspace(workspace.clone());

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        analysis.update_config(Arc::new(emmyrc));
        analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);

        let server_uri = file_path_to_uri(&workspace.join("lua/autorun/server/sv_api.lua"))
            .expect("server URI should parse");
        analysis
            .update_file_by_uri(
                &server_uri,
                Some("function ServerOnlyApi() return true end".to_string()),
            )
            .map(|(id, _)| id);

        let client_uri = file_path_to_uri(&workspace.join("lua/autorun/client/cl_user.lua"))
            .expect("client URI should parse");
        let client_file = analysis
            .update_file_by_uri(&client_uri, Some("ServerOnlyApi()".to_string()))
            .map(|(id, _)| id)
            .expect("client file should be indexed");

        let cache = super::SharedDiagnosticDataCache::default();
        let diagnose = |analysis: &EmmyLuaAnalysis| {
            analysis
                .diagnose_file_with_shared(
                    client_file,
                    CancellationToken::new(),
                    cache.get_or_precompute(analysis),
                )
                .unwrap_or_default()
        };
        let diagnostics = diagnose(&analysis);

        let target_code = Some(NumberOrString::String(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        ));
        verify_that!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == target_code),
            eq(true)
        )?;

        let first_cached = cache.cached_ptr();
        diagnose(&analysis);
        verify_that!(cache.cached_ptr(), eq(first_cached))?;

        cache.invalidate();
        verify_that!(cache.is_empty(), eq(true))?;

        Ok(())
    }

    /// The scoped-sweep slot accumulates the touched set across a burst, hands
    /// it out sorted exactly once, then reports nothing until the next note.
    #[gtest]
    fn scoped_sweep_files_round_trip() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (connection, _peer) = Connection::memory();
            let client = Arc::new(ClientProxy::new(connection));
            let status_bar = Arc::new(StatusBar::new(client.clone(), true));
            let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
            let file_diagnostic = FileDiagnostic::new(analysis, status_bar, client, false);

            verify_that!(file_diagnostic.take_scoped_sweep_files().await, none())?;

            file_diagnostic
                .note_scoped_sweep_files([FileId::new(7), FileId::new(3), FileId::new(7)])
                .await;
            file_diagnostic
                .note_scoped_sweep_files([FileId::new(9)])
                .await;
            verify_that!(
                file_diagnostic.take_scoped_sweep_files().await.as_deref(),
                some(eq(&[FileId::new(3), FileId::new(7), FileId::new(9)]))
            )?;
            verify_that!(file_diagnostic.take_scoped_sweep_files().await, none())?;

            file_diagnostic
                .restore_scoped_sweep_files([FileId::new(5)])
                .await;
            verify_that!(
                file_diagnostic.take_scoped_sweep_files().await.as_deref(),
                some(eq(&[FileId::new(5)]))
            )?;
            Ok(())
        })
    }

    /// A noted touched set scopes the fast pull's sweep to the files the burst
    /// moved; without one the pull still sweeps every workspace file.
    #[gtest]
    fn fast_pull_scopes_to_noted_files() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (connection, _peer) = Connection::memory();
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let workspace = std::env::temp_dir().join("gmod_glua_ls_scoped_fast_pull_workspace");
            let uri_a = file_path_to_uri(&workspace.join("lua/autorun/sh_a.lua"))
                .expect("uri A should parse");
            let file_a = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace.clone());
                let mut emmyrc = Emmyrc::default();
                emmyrc.gmod.enabled = true;
                analysis.update_config(Arc::new(emmyrc));
                let file_a = analysis
                    .update_file_by_uri(&uri_a, Some("local a = 1".to_string()))
                    .map(|(id, _)| id)
                    .expect("file A should be indexed");
                let uri_b = file_path_to_uri(&workspace.join("lua/autorun/sh_b.lua"))
                    .expect("uri B should parse");
                analysis.update_file_by_uri(&uri_b, Some("local b = 1".to_string()));
                file_a
            };
            let file_diagnostic = snapshot.file_diagnostic();

            // Scoped: only the noted file is swept.
            file_diagnostic.note_scoped_sweep_files([file_a]).await;
            let report = file_diagnostic
                .pull_workspace_diagnostics_fast(CancellationToken::new())
                .await;
            verify_that!(report.len(), eq(1))?;
            verify_that!(&report[0].0, eq(&uri_a))?;

            // Slot drained: the next pull sweeps the whole workspace again.
            let report = file_diagnostic
                .pull_workspace_diagnostics_fast(CancellationToken::new())
                .await;
            verify_that!(report.len(), eq(2))?;
            Ok(())
        })
    }
}
