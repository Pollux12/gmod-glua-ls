use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use glua_code_analysis::{EmmyLuaAnalysis, FileId, SharedDiagnosticData};
use log::{debug, info, warn};
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::util::{LongRunningWatchdogStatus, spawn_long_running_watchdog};

use super::{ClientProxy, ProgressTask, StatusBar};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SuppressedDiagnosticLines {
    start_line: u32,
    end_line: u32,
    expires_at: Instant,
}

#[derive(Clone, Default)]
pub(crate) struct SharedDiagnosticDataCache {
    cached: Arc<StdMutex<Option<Arc<SharedDiagnosticData>>>>,
}

impl SharedDiagnosticDataCache {
    fn get_or_precompute(&self, analysis: &EmmyLuaAnalysis) -> Arc<SharedDiagnosticData> {
        if let Ok(mut cache) = self.cached.lock() {
            if let Some(cached) = cache.as_ref() {
                return cached.clone();
            }

            let shared_data = analysis.precompute_diagnostic_shared_data();
            *cache = Some(shared_data.clone());
            return shared_data;
        }

        analysis.precompute_diagnostic_shared_data()
    }

    fn force_precompute(&self, analysis: &EmmyLuaAnalysis) -> Arc<SharedDiagnosticData> {
        let shared_data = analysis.precompute_diagnostic_shared_data();
        if let Ok(mut cache) = self.cached.lock() {
            *cache = Some(shared_data.clone());
        }
        shared_data
    }

    pub(crate) fn invalidate(&self) {
        if let Ok(mut cache) = self.cached.lock() {
            *cache = None;
        }
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
    diagnostic_tokens: Arc<Mutex<HashMap<FileId, CancellationToken>>>,
    workspace_diagnostic_token: Arc<Mutex<Option<CancellationToken>>>,
    cached_file_diagnostics: Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>>,
    recently_edited_lines: Arc<Mutex<HashMap<Uri, SuppressedDiagnosticLines>>>,
    shared_diagnostic_data_cache: SharedDiagnosticDataCache,
}

impl FileDiagnostic {
    pub fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        status_bar: Arc<StatusBar>,
        client: Arc<ClientProxy>,
    ) -> Self {
        Self {
            analysis,
            client,
            workspace_loaded_notified: Arc::new(AtomicBool::new(false)),
            startup_complete_notified: Arc::new(AtomicBool::new(false)),
            diagnostic_tokens: Arc::new(Mutex::new(HashMap::new())),
            workspace_diagnostic_token: Arc::new(Mutex::new(None)),
            cached_file_diagnostics: Arc::new(Mutex::new(HashMap::new())),
            recently_edited_lines: Arc::new(Mutex::new(HashMap::new())),
            shared_diagnostic_data_cache: SharedDiagnosticDataCache::default(),
            status_bar,
        }
    }

    pub(crate) fn shared_diagnostic_data_cache(&self) -> SharedDiagnosticDataCache {
        self.shared_diagnostic_data_cache.clone()
    }

    fn get_or_precompute_shared_diagnostic_data(
        &self,
        analysis: &EmmyLuaAnalysis,
    ) -> Arc<SharedDiagnosticData> {
        self.shared_diagnostic_data_cache
            .get_or_precompute(analysis)
    }

    fn force_precompute_shared_diagnostic_data(
        &self,
        analysis: &EmmyLuaAnalysis,
    ) -> Arc<SharedDiagnosticData> {
        self.shared_diagnostic_data_cache.force_precompute(analysis)
    }

    fn diagnose_file_with_shared_data(
        &self,
        analysis: &EmmyLuaAnalysis,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Option<Vec<Diagnostic>> {
        if cancel_token.is_cancelled() {
            return None;
        }

        let shared_data = self.get_or_precompute_shared_diagnostic_data(analysis);
        if cancel_token.is_cancelled() {
            return None;
        }

        analysis.diagnose_file_with_shared(file_id, cancel_token, shared_data)
    }

    pub fn invalidate_shared_diagnostic_data(&self) {
        self.shared_diagnostic_data_cache.invalidate();
    }

    pub async fn note_recent_edit(
        &self,
        uri: &Uri,
        previous_text: Option<&str>,
        new_text: &str,
        suppress_for: Duration,
    ) {
        let Some(previous_text) = previous_text else {
            return;
        };

        let Some((start_line, end_line)) = changed_line_span(previous_text, new_text) else {
            return;
        };

        self.recently_edited_lines.lock().await.insert(
            uri.clone(),
            SuppressedDiagnosticLines {
                start_line,
                end_line,
                expires_at: Instant::now() + suppress_for,
            },
        );
    }

    pub async fn clear_recent_edit(&self, uri: &Uri) {
        self.recently_edited_lines.lock().await.remove(uri);
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

    pub async fn cache_fresh_file_diagnostics(&self, uri: &Uri, diagnostics: &[Diagnostic]) {
        self.cached_file_diagnostics
            .lock()
            .await
            .insert(uri.clone(), diagnostics.to_vec());
    }

    pub async fn cached_display_diagnostics(&self, uri: &Uri) -> Option<Vec<Diagnostic>> {
        let diagnostics = {
            let cache = self.cached_file_diagnostics.lock().await;
            cache.get(uri).cloned()
        }?;

        Some(self.filter_display_diagnostics(uri, diagnostics).await)
    }

    pub async fn filter_display_diagnostics(
        &self,
        uri: &Uri,
        diagnostics: Vec<Diagnostic>,
    ) -> Vec<Diagnostic> {
        let suppressed_lines = {
            let mut recent_edits = self.recently_edited_lines.lock().await;
            let Some(suppressed_lines) = recent_edits.get(uri).copied() else {
                return diagnostics;
            };

            if Instant::now() >= suppressed_lines.expires_at {
                recent_edits.remove(uri);
                return diagnostics;
            }

            suppressed_lines
        };

        filter_diagnostics_by_line_span(
            diagnostics,
            suppressed_lines.start_line,
            suppressed_lines.end_line,
        )
    }

    pub async fn publish_fresh_file_diagnostics(
        &self,
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) {
        self.cache_fresh_file_diagnostics(&uri, &diagnostics).await;
        self.client.publish_diagnostics(PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        });
    }

    pub async fn add_diagnostic_task(
        &self,
        file_id: FileId,
        interval: u64,
        debounced_analysis: Option<Arc<crate::context::DebouncedAnalysis>>,
    ) {
        let mut tokens = self.diagnostic_tokens.lock().await;

        if let Some(token) = tokens.remove(&file_id) {
            token.cancel();
            debug!("cancel diagnostic: {:?}", file_id);
        }

        // create new token
        let cancel_token = CancellationToken::new();
        tokens.insert(file_id, cancel_token.clone());
        drop(tokens); // free the lock

        let analysis = self.analysis.clone();
        let file_diagnostic = self.clone();
        let diagnostic_tokens = self.diagnostic_tokens.clone();
        let file_id_clone = file_id;

        // Spawn a new task to perform diagnostic
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {
                    if let Some(da) = debounced_analysis {
                        da.wait_for_reindex(file_id_clone, cancel_token.clone()).await;
                    }
                    if cancel_token.is_cancelled() {
                        return;
                    }
                    let blocking_analysis = analysis.clone();
                    let blocking_token = cancel_token.clone();
                    let diagnostic_runner = file_diagnostic.clone();
                    match tokio::task::spawn_blocking(move || {
                        if blocking_token.is_cancelled() {
                            return None;
                        }

                        // Diagnose under a blocking read lock so CPU work does not run on Tokio workers.
                        let guard = blocking_analysis.blocking_read();
                        let uri = guard.get_uri(file_id_clone)?;
                        let diagnostics = diagnostic_runner.diagnose_file_with_shared_data(
                            &guard,
                            file_id_clone,
                            blocking_token.clone(),
                        )?;
                        Some((uri, diagnostics))
                    })
                    .await
                    {
                        Ok(Some((uri, diagnostics))) => {
                            file_diagnostic
                                .publish_fresh_file_diagnostics(uri, diagnostics, None)
                                .await;
                        }
                        Ok(None) => {
                            if !cancel_token.is_cancelled() {
                                info!("file not found: {:?}", file_id_clone);
                            }
                        }
                        Err(err) => {
                            warn!(
                                "single-file diagnostic worker failed for file {:?}: {}",
                                file_id_clone, err
                            );
                        }
                    }
                    // Remove our token only if this task was not cancelled.
                    // Keeping the check inside the lock avoids a check/remove race with replacements.
                    let mut tokens = diagnostic_tokens.lock().await;
                    if !cancel_token.is_cancelled() {
                        tokens.remove(&file_id_clone);
                    }
                }
                _ = cancel_token.cancelled() => {
                    debug!("cancel diagnostic: {:?}", file_id_clone);
                }
            }
        });
    }

    // todo add message show
    pub async fn add_files_diagnostic_task(
        &self,
        file_ids: Vec<FileId>,
        interval: u64,
        debounced_analysis: Option<Arc<crate::context::DebouncedAnalysis>>,
    ) {
        for file_id in file_ids {
            self.add_diagnostic_task(file_id, interval, debounced_analysis.clone())
                .await;
        }
    }

    /// 清除指定文件的诊断信息
    pub async fn clear_push_file_diagnostics(&self, uri: lsp_types::Uri) {
        self.cached_file_diagnostics.lock().await.remove(&uri);
        self.recently_edited_lines.lock().await.remove(&uri);

        let diagnostic_param = PublishDiagnosticsParams {
            uri,
            diagnostics: vec![],
            version: None,
        };
        self.client.publish_diagnostics(diagnostic_param);
    }

    pub async fn add_workspace_diagnostic_task(&self, interval: u64, silent: bool) {
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }

        let cancel_token = CancellationToken::new();
        token.replace(cancel_token.clone());
        drop(token);

        let analysis = self.analysis.clone();
        let client_proxy = self.client.clone();
        let status_bar = self.status_bar.clone();
        let file_diagnostic = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {
                    push_workspace_diagnostic(
                        file_diagnostic,
                        analysis,
                        client_proxy,
                        status_bar,
                        silent,
                        cancel_token,
                    ).await
                }
                _ = cancel_token.cancelled() => {
                    log::info!("cancel workspace diagnostic");
                }
            }
        });
    }

    #[allow(unused)]
    pub async fn cancel_all(&self) {
        let mut tokens = self.diagnostic_tokens.lock().await;
        for (_, token) in tokens.iter() {
            token.cancel();
        }
        tokens.clear();
    }

    pub async fn cancel_file_diagnostic(&self, file_id: FileId) {
        let mut tokens = self.diagnostic_tokens.lock().await;
        if let Some(token) = tokens.remove(&file_id) {
            token.cancel();
            debug!("cancel diagnostic: {:?}", file_id);
        }
    }

    pub async fn cancel_workspace_diagnostic(&self) {
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.take();
    }

    pub async fn pull_file_diagnostics(
        &self,
        uri: Uri,
        cancel_token: CancellationToken,
    ) -> Option<Vec<Diagnostic>> {
        if cancel_token.is_cancelled() {
            return None;
        }

        let analysis = self.analysis.clone();
        let file_diagnostic = self.clone();
        match tokio::task::spawn_blocking(move || {
            if cancel_token.is_cancelled() {
                return None;
            }

            // Pull diagnostics under a blocking read lock to avoid blocking async workers.
            let guard = analysis.blocking_read();
            let file_id = guard.get_file_id(&uri)?;

            if cancel_token.is_cancelled() {
                return None;
            }

            file_diagnostic.diagnose_file_with_shared_data(&guard, file_id, cancel_token)
        })
        .await
        {
            Ok(diagnostics) => diagnostics,
            Err(err) => {
                warn!("pull-file diagnostic worker failed: {}", err);
                None
            }
        }
    }

    pub async fn pull_workspace_diagnostics_slow(
        &self,
        cancel_token: CancellationToken,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        info!("workspace diagnostic pull slow started");
        let watchdog_status =
            LongRunningWatchdogStatus::new("Preparing workspace diagnostics (slow pull)");
        let _watchdog =
            spawn_long_running_watchdog("workspace diagnostics", watchdog_status.clone());
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.replace(cancel_token.clone());
        drop(token);

        let mut result = Vec::new();
        let status_bar = self.status_bar.clone();
        status_bar
            .create_progress_task(ProgressTask::DiagnoseWorkspace)
            .await;
        status_bar.update_progress_task(
            ProgressTask::DiagnoseWorkspace,
            None,
            Some(String::from("Preparing diagnostics")),
        );
        watchdog_status.set_phase("Preparing workspace diagnostics (slow pull)");

        let (main_workspace_file_ids, shared_data) = {
            let analysis = self.analysis.read().await;
            let file_ids = analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_main_workspace_file_ids();
            info!(
                "precomputing shared diagnostic data for {} workspace files",
                file_ids.len()
            );
            let shared_data = self.force_precompute_shared_diagnostic_data(&analysis);
            (file_ids, shared_data)
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<(Vec<Diagnostic>, Uri)>>(100);
        let valid_file_count = main_workspace_file_ids.len();
        if valid_file_count != 0 {
            status_bar.update_progress_task(
                ProgressTask::DiagnoseWorkspace,
                Some(0),
                Some(format!("Diagnosing 0/{}", valid_file_count)),
            );
            watchdog_status.set_progress(
                "Diagnosing workspace files (slow pull)",
                0,
                valid_file_count,
            );
        }
        let semaphore = Arc::new(Semaphore::new(workspace_diagnostic_parallelism()));

        for file_id in main_workspace_file_ids {
            let analysis = self.analysis.clone();
            let token = cancel_token.clone();
            let tx = tx.clone();
            let semaphore = semaphore.clone();
            let shared_data = shared_data.clone();
            tokio::spawn(async move {
                let result = diagnose_workspace_file_off_thread(
                    analysis,
                    semaphore,
                    file_id,
                    shared_data,
                    token,
                )
                .await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        let mut count = 0;
        let mut last_percentage = 0;
        while count < valid_file_count {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    break;
                }
                file_diagnostic_result = rx.recv() => {
                    let Some(file_diagnostic_result) = file_diagnostic_result else {
                        break;
                    };

                    if let Some((diagnostics, uri)) = file_diagnostic_result {
                        result.push((uri, diagnostics));
                    }
                    count += 1;
                    let percentage_done = ((count as f32 / valid_file_count as f32) * 100.0) as u32;
                    if last_percentage != percentage_done {
                        last_percentage = percentage_done;
                        let message = format!(
                            "Diagnosing {}/{} ({}%)",
                            count, valid_file_count, percentage_done
                        );
                        status_bar.update_progress_task(
                            ProgressTask::DiagnoseWorkspace,
                            Some(percentage_done),
                            Some(message),
                        );
                        watchdog_status.set_progress(
                            "Diagnosing workspace files (slow pull)",
                            count,
                            valid_file_count,
                        );
                    }
                }
            }
        }
        if count < valid_file_count && !cancel_token.is_cancelled() {
            warn!(
                "workspace diagnostic pull slow ended early: completed={} expected={}",
                count, valid_file_count
            );
        }

        status_bar.finish_progress_task(
            ProgressTask::DiagnoseWorkspace,
            Some("Diagnostics complete".to_string()),
        );
        if count == valid_file_count && !cancel_token.is_cancelled() {
            self.notify_startup_complete();
        } else {
            info!(
                "workspace diagnostic pull slow finished without startup completion: completed={} expected={} cancelled={}",
                count,
                valid_file_count,
                cancel_token.is_cancelled()
            );
        }

        result
    }

    pub async fn pull_workspace_diagnostics_fast(
        &self,
        cancel_token: CancellationToken,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        info!("workspace diagnostic pull fast started");
        let watchdog_status =
            LongRunningWatchdogStatus::new("Preparing workspace diagnostics (fast pull)");
        let _watchdog =
            spawn_long_running_watchdog("workspace diagnostics", watchdog_status.clone());
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.replace(cancel_token.clone());
        drop(token);

        let mut result = Vec::new();
        let status_bar = self.status_bar.clone();
        status_bar
            .create_progress_task(ProgressTask::DiagnoseWorkspace)
            .await;
        status_bar.update_progress_task(
            ProgressTask::DiagnoseWorkspace,
            None,
            Some(String::from("Preparing diagnostics")),
        );
        watchdog_status.set_phase("Preparing workspace diagnostics (fast pull)");

        let (main_workspace_file_ids, shared_data) = {
            let analysis = self.analysis.read().await;
            let file_ids = analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_main_workspace_file_ids();
            info!(
                "precomputing shared diagnostic data for {} workspace files",
                file_ids.len()
            );
            let shared_data = self.force_precompute_shared_diagnostic_data(&analysis);
            (file_ids, shared_data)
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<(Vec<Diagnostic>, Uri)>>(100);
        let valid_file_count = main_workspace_file_ids.len();
        if valid_file_count != 0 {
            status_bar.update_progress_task(
                ProgressTask::DiagnoseWorkspace,
                Some(0),
                Some(format!("Diagnosing 0/{}", valid_file_count)),
            );
            watchdog_status.set_progress(
                "Diagnosing workspace files (fast pull)",
                0,
                valid_file_count,
            );
        }

        let analysis = self.analysis.clone();
        let semaphore = Arc::new(Semaphore::new(workspace_diagnostic_parallelism()));
        for file_id in main_workspace_file_ids {
            let analysis = analysis.clone();
            let token = cancel_token.clone();
            let tx = tx.clone();
            let semaphore = semaphore.clone();
            let shared_data = shared_data.clone();
            tokio::spawn(async move {
                let result = diagnose_workspace_file_off_thread(
                    analysis,
                    semaphore,
                    file_id,
                    shared_data,
                    token,
                )
                .await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        let mut count = 0;
        if valid_file_count != 0 {
            let mut last_percentage = 0;

            while count < valid_file_count {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    file_diagnostic_result = rx.recv() => {
                        let Some(file_diagnostic_result) = file_diagnostic_result else {
                            break;
                        };

                        if let Some((diagnostics, uri)) = file_diagnostic_result {
                            result.push((uri, diagnostics));
                        }

                        count += 1;
                        let percentage_done = ((count as f32 / valid_file_count as f32) * 100.0) as u32;
                        if last_percentage != percentage_done {
                            last_percentage = percentage_done;
                            let message = format!(
                                "Diagnosing {}/{} ({}%)",
                                count, valid_file_count, percentage_done
                            );
                            status_bar.update_progress_task(
                                ProgressTask::DiagnoseWorkspace,
                                Some(percentage_done),
                                Some(message),
                            );
                            watchdog_status.set_progress(
                                "Diagnosing workspace files (fast pull)",
                                count,
                                valid_file_count,
                            );
                        }
                    }
                }
            }
        }
        if count < valid_file_count && !cancel_token.is_cancelled() {
            warn!(
                "workspace diagnostic pull fast ended early: completed={} expected={}",
                count, valid_file_count
            );
        }

        status_bar.finish_progress_task(
            ProgressTask::DiagnoseWorkspace,
            Some("Diagnostics complete".to_string()),
        );
        if count == valid_file_count && !cancel_token.is_cancelled() {
            self.notify_startup_complete();
        } else {
            info!(
                "workspace diagnostic pull fast finished without startup completion: completed={} expected={} cancelled={}",
                count,
                valid_file_count,
                cancel_token.is_cancelled()
            );
        }

        result
    }
}

fn workspace_diagnostic_parallelism() -> usize {
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

async fn diagnose_workspace_file_off_thread(
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    semaphore: Arc<Semaphore>,
    file_id: FileId,
    shared_data: Arc<glua_code_analysis::SharedDiagnosticData>,
    cancel_token: CancellationToken,
) -> Option<(Vec<Diagnostic>, Uri)> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let permit = tokio::select! {
        _ = cancel_token.cancelled() => {
            return None;
        }
        permit = semaphore.acquire_owned() => {
            match permit {
                Ok(permit) => permit,
                Err(_) => return None,
            }
        }
    };

    let blocking_analysis = analysis;
    let blocking_shared_data = shared_data;
    let blocking_token = cancel_token.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if blocking_token.is_cancelled() {
            return None;
        }

        // Diagnose under a blocking read lock to avoid starving Tokio worker threads.
        let guard = blocking_analysis.blocking_read();
        let diagnostics = guard.diagnose_file_with_shared(
            file_id,
            blocking_token.clone(),
            blocking_shared_data,
        )?;
        let uri = guard.get_uri(file_id)?;
        Some((diagnostics, uri))
    })
    .await
    {
        Ok(result) => result,
        Err(err) => {
            warn!(
                "workspace diagnostic worker failed for file {:?}: {}",
                file_id, err
            );
            None
        }
    }
}

async fn push_workspace_diagnostic(
    file_diagnostic: FileDiagnostic,
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    client_proxy: Arc<ClientProxy>,
    status_bar: Arc<StatusBar>,
    silent: bool,
    cancel_token: CancellationToken,
) {
    info!("workspace diagnostic push started; silent={}", silent);
    let watchdog_status = LongRunningWatchdogStatus::new("Preparing workspace diagnostics (push)");
    let _watchdog = spawn_long_running_watchdog("workspace diagnostics", watchdog_status.clone());
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
    watchdog_status.set_phase("Preparing workspace diagnostics (push)");

    let (main_workspace_file_ids, shared_data) = {
        let read_analysis = analysis.read().await;
        let file_ids = read_analysis
            .compilation
            .get_db()
            .get_module_index()
            .get_main_workspace_file_ids();
        info!(
            "precomputing shared diagnostic data for {} workspace files",
            file_ids.len()
        );
        let shared_data = file_diagnostic.force_precompute_shared_diagnostic_data(&read_analysis);
        (file_ids, shared_data)
    };
    // diagnostic files
    let (tx, mut rx) = tokio::sync::mpsc::channel::<FileId>(100);
    let valid_file_count = main_workspace_file_ids.len();
    if !silent && valid_file_count != 0 {
        status_bar.update_progress_task(
            ProgressTask::DiagnoseWorkspace,
            Some(0),
            Some(format!("Diagnosing 0/{}", valid_file_count)),
        );
    }
    if valid_file_count != 0 {
        watchdog_status.set_progress("Diagnosing workspace files (push)", 0, valid_file_count);
    }

    let semaphore = Arc::new(Semaphore::new(workspace_diagnostic_parallelism()));
    for file_id in main_workspace_file_ids {
        let analysis = analysis.clone();
        let token = cancel_token.clone();
        let client = client_proxy.clone();
        let semaphore = semaphore.clone();
        let tx = tx.clone();
        let shared_data = shared_data.clone();
        tokio::spawn(async move {
            let result = diagnose_workspace_file_off_thread(
                analysis,
                semaphore,
                file_id,
                shared_data,
                token,
            )
            .await;
            if let Some((diagnostics, uri)) = result {
                let diagnostic_param = lsp_types::PublishDiagnosticsParams {
                    uri,
                    diagnostics,
                    version: None,
                };
                client.publish_diagnostics(diagnostic_param);
            }
            let _ = tx.send(file_id).await;
        });
    }
    drop(tx);

    let mut count = 0;
    if valid_file_count != 0 {
        let mut last_percentage = 0;
        while count < valid_file_count {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    break;
                }
                maybe_file_id = rx.recv() => {
                    if maybe_file_id.is_none() {
                        break;
                    }
                    count += 1;
                    let percentage_done = ((count as f32 / valid_file_count as f32) * 100.0) as u32;
                    if last_percentage != percentage_done {
                        last_percentage = percentage_done;
                        if !silent {
                            let message = format!(
                                "Diagnosing {}/{} ({}%)",
                                count, valid_file_count, percentage_done
                            );
                            status_bar.update_progress_task(
                                ProgressTask::DiagnoseWorkspace,
                                Some(percentage_done),
                                Some(message),
                            );
                        }
                        watchdog_status.set_progress(
                            "Diagnosing workspace files (push)",
                            count,
                            valid_file_count,
                        );
                    }
                }
            }
        }
        if count < valid_file_count && !cancel_token.is_cancelled() {
            warn!(
                "workspace diagnostic push ended early: completed={} expected={} silent={}",
                count, valid_file_count, silent
            );
        }
    }

    if !silent {
        status_bar.finish_progress_task(
            ProgressTask::DiagnoseWorkspace,
            Some("Diagnostics complete".to_string()),
        );
        if count == valid_file_count && !cancel_token.is_cancelled() {
            file_diagnostic.notify_startup_complete();
        } else {
            info!(
                "workspace diagnostic push finished without startup completion: completed={} expected={} cancelled={} silent={}",
                count,
                valid_file_count,
                cancel_token.is_cancelled(),
                silent
            );
        }
    }
}

fn changed_line_span(previous_text: &str, new_text: &str) -> Option<(u32, u32)> {
    let previous_bytes = previous_text.as_bytes();
    let new_bytes = new_text.as_bytes();

    if previous_bytes == new_bytes {
        return None;
    }

    let mut prefix = 0usize;
    let prefix_limit = previous_bytes.len().min(new_bytes.len());
    while prefix < prefix_limit && previous_bytes[prefix] == new_bytes[prefix] {
        prefix += 1;
    }

    let mut previous_suffix = previous_bytes.len();
    let mut new_suffix = new_bytes.len();
    while previous_suffix > prefix
        && new_suffix > prefix
        && previous_bytes[previous_suffix - 1] == new_bytes[new_suffix - 1]
    {
        previous_suffix -= 1;
        new_suffix -= 1;
    }

    let start_line = count_newlines(&new_bytes[..prefix]);
    if count_newlines(previous_bytes) != count_newlines(new_bytes) {
        return Some((start_line as u32, u32::MAX));
    }

    let end_line = count_newlines(&new_bytes[..new_suffix]).max(start_line);
    Some((start_line as u32, end_line as u32))
}

fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
}

fn filter_diagnostics_by_line_span(
    diagnostics: Vec<Diagnostic>,
    start_line: u32,
    end_line: u32,
) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.range.end.line < start_line || diagnostic.range.start.line > end_line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::context::{ClientProxy, StatusBar};
    use glua_code_analysis::{DiagnosticCode, EmmyLuaAnalysis, Emmyrc, file_path_to_uri};
    use googletest::prelude::*;
    use lsp_server::{Connection, Message};
    use lsp_types::NumberOrString;
    use lsp_types::{Position, Range};
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    use super::{FileDiagnostic, changed_line_span, filter_diagnostics_by_line_span};

    fn diagnostic(start_line: u32, end_line: u32) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: Range {
                start: Position {
                    line: start_line,
                    character: 0,
                },
                end: Position {
                    line: end_line,
                    character: 0,
                },
            },
            ..Default::default()
        }
    }

    #[gtest]
    fn changed_line_span_tracks_single_line_edits() -> Result<()> {
        verify_that!(
            changed_line_span("print(1)\n", "print(12)\n"),
            some(eq((0, 0)))
        )?;
        Ok(())
    }

    #[gtest]
    fn changed_line_span_tracks_multiline_edits() -> Result<()> {
        verify_that!(
            changed_line_span("a\nb\nc\n", "a\nb\nx\ny\nc\n"),
            some(eq((2, u32::MAX)))
        )?;
        Ok(())
    }

    #[gtest]
    fn changed_line_span_suppresses_to_eof_when_line_numbers_shift() -> Result<()> {
        verify_that!(
            changed_line_span("bad\nfoo\nbar\n", "x\nbad\nfoo\nbar\n"),
            some(eq((0, u32::MAX)))
        )?;
        Ok(())
    }

    #[gtest]
    fn filter_diagnostics_by_line_span_removes_overlapping_ranges() -> Result<()> {
        let filtered = filter_diagnostics_by_line_span(
            vec![
                diagnostic(0, 0),
                diagnostic(1, 1),
                diagnostic(1, 2),
                diagnostic(3, 3),
            ],
            1,
            1,
        )
        .into_iter()
        .map(|diagnostic| (diagnostic.range.start.line, diagnostic.range.end.line))
        .collect::<Vec<_>>();

        verify_that!(filtered.as_slice(), eq(&[(0, 0), (3, 3)]))?;
        Ok(())
    }

    #[gtest]
    fn workspace_loaded_notification_does_not_suppress_startup_complete() -> Result<()> {
        let (connection, peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone()));
        let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
        let file_diagnostic = FileDiagnostic::new(analysis, status_bar, client);

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
        analysis.update_file_by_uri(
            &server_uri,
            Some("function ServerOnlyApi() return true end".to_string()),
        );

        let client_uri = file_path_to_uri(&workspace.join("lua/autorun/client/cl_user.lua"))
            .expect("client URI should parse");
        let client_file = analysis
            .update_file_by_uri(&client_uri, Some("ServerOnlyApi()".to_string()))
            .expect("client file should be indexed");

        let (connection, _peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone()));
        let analysis = Arc::new(RwLock::new(analysis));
        let file_diagnostic = FileDiagnostic::new(analysis.clone(), status_bar, client);

        let diagnostics = {
            let guard = analysis.blocking_read();
            file_diagnostic
                .diagnose_file_with_shared_data(&guard, client_file, CancellationToken::new())
                .unwrap_or_default()
        };

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

        let first_cached = file_diagnostic.shared_diagnostic_data_cache.cached_ptr();
        {
            let guard = analysis.blocking_read();
            let _ = file_diagnostic.diagnose_file_with_shared_data(
                &guard,
                client_file,
                CancellationToken::new(),
            );
        }
        let second_cached = file_diagnostic.shared_diagnostic_data_cache.cached_ptr();
        verify_that!(second_cached, eq(first_cached))?;

        file_diagnostic.invalidate_shared_diagnostic_data();
        verify_that!(
            file_diagnostic.shared_diagnostic_data_cache.is_empty(),
            eq(true)
        )?;

        Ok(())
    }
}
