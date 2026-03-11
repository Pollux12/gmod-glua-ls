use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use glua_code_analysis::{EmmyLuaAnalysis, FileId, Profile};
use log::{debug, info, warn};
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{ClientProxy, ProgressTask, StatusBar};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SuppressedDiagnosticLines {
    start_line: u32,
    end_line: u32,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct FileDiagnostic {
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    client: Arc<ClientProxy>,
    status_bar: Arc<StatusBar>,
    diagnostic_tokens: Arc<Mutex<HashMap<FileId, CancellationToken>>>,
    workspace_diagnostic_token: Arc<Mutex<Option<CancellationToken>>>,
    cached_file_diagnostics: Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>>,
    recently_edited_lines: Arc<Mutex<HashMap<Uri, SuppressedDiagnosticLines>>>,
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
            diagnostic_tokens: Arc::new(Mutex::new(HashMap::new())),
            workspace_diagnostic_token: Arc::new(Mutex::new(None)),
            cached_file_diagnostics: Arc::new(Mutex::new(HashMap::new())),
            recently_edited_lines: Arc::new(Mutex::new(HashMap::new())),
            status_bar,
        }
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
                    match tokio::task::spawn_blocking(move || {
                        if blocking_token.is_cancelled() {
                            return None;
                        }

                        // Diagnose under a blocking read lock so CPU work does not run on Tokio workers.
                        let guard = blocking_analysis.blocking_read();
                        let uri = guard.get_uri(file_id_clone)?;
                        let diagnostics = guard.diagnose_file(file_id_clone, blocking_token.clone())?;
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
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {
                    push_workspace_diagnostic(analysis, client_proxy, status_bar, silent, cancel_token).await
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

            guard.diagnose_file(file_id, cancel_token)
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
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.replace(cancel_token.clone());
        drop(token);

        let mut result = Vec::new();
        let analysis = self.analysis.read().await;
        let main_workspace_file_ids = analysis
            .compilation
            .get_db()
            .get_module_index()
            .get_main_workspace_file_ids();
        drop(analysis);
        let profile_text = format!(
            "workspace diagnostic pull slow: {} files",
            main_workspace_file_ids.len()
        );
        let _p = Profile::new(profile_text.as_str());
        info!(
            "workspace diagnostic pull slow started: files={}",
            main_workspace_file_ids.len()
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<(Vec<Diagnostic>, Uri)>>(100);
        let valid_file_count = main_workspace_file_ids.len();
        let semaphore = Arc::new(Semaphore::new(workspace_diagnostic_parallelism()));

        for file_id in main_workspace_file_ids {
            let analysis = self.analysis.clone();
            let token = cancel_token.clone();
            let tx = tx.clone();
            let semaphore = semaphore.clone();
            tokio::spawn(async move {
                let result =
                    diagnose_workspace_file_off_thread(analysis, semaphore, file_id, token).await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        let mut count = 0;
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
                }
            }
        }
        if count < valid_file_count && !cancel_token.is_cancelled() {
            warn!(
                "workspace diagnostic pull slow ended early: completed={} expected={}",
                count, valid_file_count
            );
        }

        result
    }

    pub async fn pull_workspace_diagnostics_fast(
        &self,
        cancel_token: CancellationToken,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        let mut token = self.workspace_diagnostic_token.lock().await;
        if let Some(token) = token.as_ref() {
            token.cancel();
            debug!("cancel workspace diagnostic");
        }
        token.replace(cancel_token.clone());
        drop(token);

        let mut result = Vec::new();
        let analysis = self.analysis.read().await;
        let main_workspace_file_ids = analysis
            .compilation
            .get_db()
            .get_module_index()
            .get_main_workspace_file_ids();
        drop(analysis);

        let status_bar = self.status_bar.clone();
        status_bar
            .create_progress_task(ProgressTask::DiagnoseWorkspace)
            .await;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<(Vec<Diagnostic>, Uri)>>(100);
        let valid_file_count = main_workspace_file_ids.len();
        let profile_text = format!("workspace diagnostic pull fast: {} files", valid_file_count);
        let _p = Profile::new(profile_text.as_str());
        info!(
            "workspace diagnostic pull fast started: files={}",
            valid_file_count
        );

        let analysis = self.analysis.clone();
        let semaphore = Arc::new(Semaphore::new(workspace_diagnostic_parallelism()));
        for file_id in main_workspace_file_ids {
            let analysis = analysis.clone();
            let token = cancel_token.clone();
            let tx = tx.clone();
            let semaphore = semaphore.clone();
            tokio::spawn(async move {
                let result =
                    diagnose_workspace_file_off_thread(analysis, semaphore, file_id, token).await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        let mut count = 0;
        if valid_file_count != 0 {
            let text = format!("diagnose {} files", valid_file_count);
            let _p = Profile::new(text.as_str());
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
                            let message = format!("diagnostic {}%", percentage_done);
                            status_bar.update_progress_task(
                                ProgressTask::DiagnoseWorkspace,
                                Some(percentage_done),
                                Some(message),
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
            Some("Diagnosis complete".to_string()),
        );

        result
    }
}

fn workspace_diagnostic_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(8)
}

async fn diagnose_workspace_file_off_thread(
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    semaphore: Arc<Semaphore>,
    file_id: FileId,
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
    let blocking_token = cancel_token.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if blocking_token.is_cancelled() {
            return None;
        }

        // Diagnose under a blocking read lock to avoid starving Tokio worker threads.
        let guard = blocking_analysis.blocking_read();
        let diagnostics = guard.diagnose_file(file_id, blocking_token.clone())?;
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
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    client_proxy: Arc<ClientProxy>,
    status_bar: Arc<StatusBar>,
    silent: bool,
    cancel_token: CancellationToken,
) {
    let read_analysis = analysis.read().await;
    let main_workspace_file_ids = read_analysis
        .compilation
        .get_db()
        .get_module_index()
        .get_main_workspace_file_ids();
    drop(read_analysis);
    // diagnostic files
    let (tx, mut rx) = tokio::sync::mpsc::channel::<FileId>(100);
    let valid_file_count = main_workspace_file_ids.len();
    let profile_text = format!(
        "workspace diagnostic push (silent={}): {} files",
        silent, valid_file_count
    );
    let _p = Profile::new(profile_text.as_str());
    info!(
        "workspace diagnostic push started: files={}, silent={}",
        valid_file_count, silent
    );
    if !silent {
        status_bar
            .create_progress_task(ProgressTask::DiagnoseWorkspace)
            .await;
    }

    let semaphore = Arc::new(Semaphore::new(workspace_diagnostic_parallelism()));
    for file_id in main_workspace_file_ids {
        let analysis = analysis.clone();
        let token = cancel_token.clone();
        let client = client_proxy.clone();
        let semaphore = semaphore.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result =
                diagnose_workspace_file_off_thread(analysis, semaphore, file_id, token).await;
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
        if silent {
            while (rx.recv().await).is_some() {
                count += 1;
                if count == valid_file_count {
                    break;
                }
            }
        } else {
            let mut last_percentage = 0;
            while (rx.recv().await).is_some() {
                count += 1;
                let percentage_done = ((count as f32 / valid_file_count as f32) * 100.0) as u32;
                if last_percentage != percentage_done {
                    last_percentage = percentage_done;
                    let message = format!("diagnostic {}%", percentage_done);
                    status_bar.update_progress_task(
                        ProgressTask::DiagnoseWorkspace,
                        Some(percentage_done),
                        Some(message),
                    );
                }
                if count == valid_file_count {
                    break;
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
            Some("Diagnosis complete".to_string()),
        );
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
    use googletest::prelude::*;
    use lsp_types::{Position, Range};

    use super::{changed_line_span, filter_diagnostics_by_line_span};

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
}
