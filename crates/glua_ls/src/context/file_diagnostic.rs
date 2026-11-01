use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use glua_code_analysis::SharedDiagnosticComponentCache;
use glua_code_analysis::{EmmyLuaAnalysis, SharedDiagnosticData};
use log::{debug, info};
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
        // Checked before the lock, so files diagnosed together do not queue
        // on it while the shared data holds.
        if self.components.is_fresh(&analysis.compilation)
            && let Some(shared_data) = self.cached.lock().ok().and_then(|cached| cached.clone())
        {
            return shared_data;
        }
        let mut cached = self.cached.lock().ok();
        if let Some(cache) = cached.as_ref()
            && let Some(shared_data) = cache.as_ref()
            && self.components.is_fresh(&analysis.compilation)
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

    #[cfg(test)]
    fn cached_ptr(&self) -> Option<*const SharedDiagnosticData> {
        self.cached
            .lock()
            .expect("shared diagnostic cache mutex should not be poisoned")
            .as_ref()
            .map(Arc::as_ptr)
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
    /// Every workspace file's diagnostics, computed again only when they
    /// may have changed.
    store: Arc<super::DiagnosticStore>,
}

impl FileDiagnostic {
    pub fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        status_bar: Arc<StatusBar>,
        client: Arc<ClientProxy>,
    ) -> Self {
        let store = Arc::new(super::DiagnosticStore::new(
            analysis.clone(),
            SharedDiagnosticDataCache::default(),
            client.clone(),
        ));
        Self {
            analysis,
            client,
            workspace_loaded_notified: Arc::new(AtomicBool::new(false)),
            startup_complete_notified: Arc::new(AtomicBool::new(false)),
            workspace_diagnostic_token: Arc::new(Mutex::new(None)),
            store,
            status_bar,
        }
    }

    pub fn store(&self) -> &Arc<super::DiagnosticStore> {
        &self.store
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

    /// After `interval` ms, computes every workspace file's diagnostics again;
    /// the store publishes those that changed.
    pub async fn add_workspace_diagnostic_task(&self, interval: u64, silent: bool) {
        let cancel_token = {
            let mut token = self.workspace_diagnostic_token.lock().await;
            if let Some(token) = token.as_ref() {
                token.cancel();
                debug!("cancel workspace diagnostic");
            }
            let cancel_token = CancellationToken::new();
            token.replace(cancel_token.clone());
            cancel_token
        };
        let file_diagnostic = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {
                    file_diagnostic.refill(silent, cancel_token).await;
                }
                _ = cancel_token.cancelled() => {
                    log::info!("cancel workspace diagnostic");
                }
                _ = file_diagnostic.store.stopped() => {}
            }
        });
    }

    /// Forgets the stored diagnostics of files no longer in the workspace,
    /// clearing what the client shows for them.
    pub async fn forget_missing(&self) {
        let live = self
            .analysis
            .read()
            .await
            .get_main_workspace_file_ids_for_diagnostics()
            .into_iter()
            .collect::<HashSet<_>>();
        self.store.forget_missing(&live);
    }

    /// `uri`'s current diagnostics, computed ahead of every other file if
    /// they do not hold.
    #[cfg(test)]
    pub async fn file_diagnostics(
        &self,
        uri: lsp_types::Uri,
        cancel_token: CancellationToken,
    ) -> Option<super::diagnostic_store::StoredDiagnostics> {
        let file_id = self.analysis.read().await.get_file_id(&uri)?;
        self.store
            .get(
                file_id,
                super::diagnostic_store::Priority::Edited,
                &cancel_token,
            )
            .await
    }

    /// Computes every workspace file's diagnostics again, reporting progress.
    async fn refill(&self, silent: bool, cancel_token: CancellationToken) {
        let watchdog_status = LongRunningWatchdogStatus::new("Preparing workspace diagnostics");
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

        let file_ids = self
            .analysis
            .read()
            .await
            .get_main_workspace_file_ids_for_diagnostics();
        self.store.invalidate_all(file_ids.iter().copied());

        watchdog_status.set_detail_source(self.store.watchdog_detail(self.analysis.clone()));
        let total = file_ids.len();
        let progress_label = "Diagnosing workspace files";
        watchdog_status.set_progress(progress_label, 0, total);
        let mut last_percentage = None;
        self.store
            .get_all(&file_ids, &cancel_token, |done| {
                watchdog_status.set_progress(progress_label, done, total);
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
            info!("workspace diagnostics cancelled");
        } else {
            self.notify_startup_complete();
        }
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
    use crate::context::{ClientProxy, StatusBar};
    use glua_code_analysis::{DiagnosticCode, EmmyLuaAnalysis, Emmyrc, file_path_to_uri};
    use googletest::prelude::*;
    use lsp_server::{Connection, Message};
    use lsp_types::NumberOrString;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    use super::FileDiagnostic;

    #[gtest]
    fn concurrent_diagnostic_pulls_share_the_first_precompute() {
        let mut analysis = EmmyLuaAnalysis::new();
        let workspace = crate::util::unique_temp_dir("gmod_glua_ls_concurrent_shared_diagnostics");
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
        {
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
        }
    }

    #[gtest]
    fn workspace_loaded_notification_does_not_suppress_startup_complete() -> Result<()> {
        let (connection, peer) = Connection::memory();
        let client = Arc::new(ClientProxy::new(connection));
        let status_bar = Arc::new(StatusBar::new(client.clone(), true));
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
        let workspace = crate::util::unique_temp_dir("gmod_glua_ls_shared_single_file_diagnostics");
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

        Ok(())
    }
}
