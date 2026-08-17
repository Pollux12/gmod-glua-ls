mod client;
mod client_id;
mod debounced_analysis;
mod did_change_coalescer;
mod file_diagnostic;
mod lsp_features;
mod snapshot;
mod status_bar;
mod workspace_manager;

pub use client::ClientProxy;
pub use client_id::{ClientId, get_client_id};
pub use debounced_analysis::{DebouncedAnalysis, InFlightChangeGuard};
pub use did_change_coalescer::DidChangeCoalescer;
pub use file_diagnostic::FileDiagnostic;
use glua_code_analysis::EmmyLuaAnalysis;
pub use lsp_features::LspFeatures;
use lsp_server::{Connection, ErrorCode, Message, RequestId, Response};
use lsp_types::{ClientCapabilities, Uri};
pub use snapshot::ServerContextSnapshot;
pub use status_bar::ProgressTask;
pub use status_bar::StatusBar;
use std::{collections::HashMap, future::Future, sync::Arc};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
pub use workspace_manager::*;

use crate::context::snapshot::ServerContextInner;

// LOCK ORDER (acquire low → high; never a lower lock while holding a higher):
// 1. diagnostic_tokens  2. workspace_diagnostic_token  3. cached_file_diagnostics
// 4. update_token  5. analysis(read)  6. workspace_manager(read)
// 7. workspace_manager(write)  8. analysis(write)
// Leaf: document_versions — statement-scoped only; never hold across an
// `.await` that takes another lock. Never upgrade read→write in place; avoid
// holding any lock across `.await`. Atomics are exempt.

#[derive(Clone)]
pub struct RequestTaskMetadata {
    pub method: String,
    pub uri: Option<Uri>,
}

impl RequestTaskMetadata {
    pub fn new(method: impl Into<String>, uri: Option<Uri>) -> Self {
        Self {
            method: method.into(),
            uri,
        }
    }
}

struct InFlightRequest {
    cancel_token: CancellationToken,
    metadata: RequestTaskMetadata,
}

// Methods answered with their computed result on cancel instead of an error,
// so the client keeps its current UI state.
// - semantic tokens excluded: relative offsets make a stale set wrong.
// - workspace/diagnostic included: vscode-languageclient permanently stops
//   workspace pulls after 6 non-cancellation errors.
// - textDocument/diagnostic excluded: the client rewrites a cancelled pull's
//   result to an empty full report; an error reschedules instead.
fn keep_stale_editor_data_on_cancel(method: &str) -> bool {
    matches!(
        method,
        "textDocument/codeLens"
            | "textDocument/inlayHint"
            | "gluals/annotator"
            | "workspace/diagnostic"
    )
}

/// The error code — and any `data` payload — for a cancelled request.
fn cancel_error(features: &LspFeatures, method: &str) -> (ErrorCode, Option<serde_json::Value>) {
    // The client only retriggers when `data` is present; it ignores the
    // spec's default-when-absent.
    if matches!(method, "textDocument/diagnostic" | "workspace/diagnostic") {
        return (
            ErrorCode::ServerCancelled,
            Some(serde_json::json!({ "retriggerRequest": true })),
        );
    }

    // ContentModified only for methods the client declares it re-sends;
    // others read it as "no result" and clear the feature's UI.
    if features.retries_on_content_modified(method) {
        (ErrorCode::ContentModified, None)
    } else {
        (ErrorCode::RequestCanceled, None)
    }
}

fn should_send_stale_response_on_cancel(method: &str, response: &Response) -> bool {
    let Some(result) = response.result.as_ref() else {
        return false;
    };

    if result.is_null() {
        return false;
    }

    if matches!(method, "textDocument/codeLens" | "textDocument/inlayHint") {
        // A stale-but-empty result would clear rendered UI while typing.
        return result.as_array().is_some_and(|hints| !hints.is_empty());
    }

    true
}

pub struct ServerContext {
    #[allow(unused)]
    conn: Connection,
    requests: Arc<Mutex<HashMap<RequestId, InFlightRequest>>>,
    debounced_shutdown: CancellationToken,
    inner: Arc<ServerContextInner>,
    did_change_coalescer: DidChangeCoalescer,
}

impl ServerContext {
    pub fn new(conn: Connection, client_capabilities: ClientCapabilities) -> Self {
        let client = Arc::new(ClientProxy::new(Connection {
            sender: conn.sender.clone(),
            receiver: conn.receiver.clone(),
        }));

        let analysis = Arc::new(RwLock::new(EmmyLuaAnalysis::new()));
        let lsp_features = Arc::new(LspFeatures::new(client_capabilities));
        let status_bar = Arc::new(StatusBar::new(
            client.clone(),
            lsp_features.supports_work_done_progress(),
        ));
        let file_diagnostic = Arc::new(FileDiagnostic::new(
            analysis.clone(),
            status_bar.clone(),
            client.clone(),
        ));
        let workspace_manager_inner = WorkspaceManager::new(
            analysis.clone(),
            client.clone(),
            status_bar.clone(),
            file_diagnostic.clone(),
            lsp_features.clone(),
        );
        let workspace_diagnostic_level = workspace_manager_inner.workspace_diagnostic_level_arc();
        let workspace_manager = Arc::new(RwLock::new(workspace_manager_inner));
        let debounced_shutdown = CancellationToken::new();
        let debounced_analysis = Arc::new(DebouncedAnalysis::new(
            analysis.clone(),
            200,
            debounced_shutdown.clone(),
            client.clone(),
            file_diagnostic.shared_diagnostic_data_cache(),
            workspace_diagnostic_level,
            lsp_features.clone(),
        ));

        // Supervise the debounce loop: freshness waiters park on it with no
        // deadline, so if it dies the whole server silently goes quiet.
        {
            let da = debounced_analysis.clone();
            let shutdown = debounced_shutdown.clone();
            tokio::spawn(async move {
                while !shutdown.is_cancelled() {
                    let task = tokio::spawn({
                        let da = da.clone();
                        async move { da.run().await }
                    });
                    match task.await {
                        // `run` only returns on shutdown.
                        Ok(()) => return,
                        Err(err) => {
                            log::error!(
                                "LS_DEBOUNCE_LOOP_PANIC debounced analysis loop died, restarting: {}",
                                err
                            );
                        }
                    }
                }
            });
        }

        let inner = Arc::new(ServerContextInner {
            analysis,
            client,
            file_diagnostic,
            workspace_manager,
            status_bar,
            lsp_features,
            debounced_analysis,
            document_versions: Arc::new(Mutex::new(HashMap::new())),
            document_version_notify: Arc::new(Notify::new()),
        });

        // Create the didChange coalescer with a snapshot of the inner state
        let did_change_coalescer =
            DidChangeCoalescer::new(ServerContextSnapshot::new(inner.clone()));

        ServerContext {
            conn,
            requests: Arc::new(Mutex::new(HashMap::new())),
            debounced_shutdown,
            inner,
            did_change_coalescer,
        }
    }

    pub fn snapshot(&self) -> ServerContextSnapshot {
        ServerContextSnapshot::new(self.inner.clone())
    }

    pub fn did_change_coalescer(&self) -> &DidChangeCoalescer {
        &self.did_change_coalescer
    }

    pub fn send(&self, response: Response) {
        let _ = self.conn.sender.send(Message::Response(response));
    }

    pub async fn task<F, Fut>(&self, req_id: RequestId, metadata: RequestTaskMetadata, exec: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Option<Response>> + Send + 'static,
    {
        let sender = self.conn.sender.clone();
        let cancel_token = CancellationToken::new();
        let lsp_features = self.inner.lsp_features.clone();
        let request_method = metadata.method.to_string();

        let mut requests = self.requests.lock().await;
        requests.insert(
            req_id.clone(),
            InFlightRequest {
                metadata,
                cancel_token: cancel_token.clone(),
            },
        );
        drop(requests);

        let requests = self.requests.clone();

        tokio::spawn(async move {
            // Own task per handler: a panic must not skip the response or the
            // `requests` cleanup below.
            let handler_token = cancel_token.clone();
            let res = match tokio::spawn(exec(handler_token)).await {
                Ok(res) => res,
                Err(err) => {
                    log::error!(
                        "LS_REQUEST_PANIC method={} request failed: {}",
                        request_method,
                        err
                    );
                    None
                }
            };
            if cancel_token.is_cancelled() {
                if keep_stale_editor_data_on_cancel(&request_method)
                    && let Some(response) = res
                    && should_send_stale_response_on_cancel(&request_method, &response)
                {
                    let _ = sender.send(Message::Response(response.clone()));
                } else {
                    let (code, data) = cancel_error(&lsp_features, &request_method);
                    let response = Response {
                        id: req_id.clone(),
                        result: None,
                        error: Some(lsp_server::ResponseError {
                            code: code as i32,
                            message: "cancel".to_string(),
                            data,
                        }),
                    };
                    let _ = sender.send(Message::Response(response));
                }
            } else if res.is_none() {
                let response = Response::new_err(
                    req_id.clone(),
                    ErrorCode::InternalError as i32,
                    "internal error".to_string(),
                );
                let _ = sender.send(Message::Response(response));
            } else if let Some(it) = res {
                let _ = sender.send(Message::Response(it));
            }

            let mut requests = requests.lock().await;
            requests.remove(&req_id);
        });
    }

    pub async fn cancel(&self, req_id: RequestId) {
        let requests = self.requests.lock().await;
        if let Some(request) = requests.get(&req_id) {
            request.cancel_token.cancel();
        }
    }

    pub async fn cancel_all_requests_except(&self, excluded_methods: &[&str]) {
        let requests = self.requests.lock().await;
        for request in requests.values() {
            if excluded_methods
                .iter()
                .any(|method| request.metadata.method == *method)
            {
                continue;
            }

            request.cancel_token.cancel();
        }
    }

    pub async fn cancel_text_requests_for_uri(&self, uri: &Uri) {
        let requests = self.requests.lock().await;
        for request in requests.values() {
            if request
                .metadata
                .uri
                .as_ref()
                .is_some_and(|request_uri| request_uri == uri)
            {
                request.cancel_token.cancel();
            }
        }
    }

    pub async fn close(&self) {
        self.debounced_shutdown.cancel();
        let mut workspace_manager = self.inner.workspace_manager.write().await;
        workspace_manager.watcher = None;
    }

    pub async fn send_response(&self, response: Response) {
        self.inner.client.on_response(response).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LspFeatures, RequestTaskMetadata, ServerContext, WorkspaceDiagnosticLevel, cancel_error,
        keep_stale_editor_data_on_cancel, should_send_stale_response_on_cancel,
    };
    use googletest::prelude::*;
    use lsp_server::{Connection, ErrorCode, RequestId, Response};
    use lsp_types::ClientCapabilities;
    use serde_json::json;
    use std::time::Duration;

    #[gtest]
    fn stale_inlay_and_code_lens_response_requires_non_empty_array() -> Result<()> {
        let empty = Response::new_ok(1.into(), json!([]));
        let non_empty = Response::new_ok(2.into(), json!([{"label": ": number"}]));

        verify_that!(
            should_send_stale_response_on_cancel("textDocument/inlayHint", &empty),
            eq(false)
        )?;
        verify_that!(
            should_send_stale_response_on_cancel("textDocument/inlayHint", &non_empty),
            eq(true)
        )?;
        verify_that!(
            should_send_stale_response_on_cancel("textDocument/codeLens", &empty),
            eq(false)
        )?;
        verify_that!(
            should_send_stale_response_on_cancel("textDocument/codeLens", &non_empty),
            eq(true)
        )?;
        Ok(())
    }

    #[gtest]
    fn cancelled_semantic_tokens_report_content_modified_without_stale_data() -> Result<()> {
        // Token offsets are relative and carry no version, so a set built
        // against superseded text is wrong rather than merely old.
        verify_that!(
            keep_stale_editor_data_on_cancel("textDocument/semanticTokens/full"),
            eq(false)
        )?;

        // ContentModified only for what the client says it re-sends. Inlay
        // hints are absent from VS Code's list, and answering them with it
        // clears the hints instead of preserving them.
        let features = LspFeatures::new(
            serde_json::from_value(json!({
                "general": {
                    "staleRequestSupport": {
                        "cancel": true,
                        "retryOnContentModified": ["textDocument/semanticTokens/full"],
                    }
                }
            }))
            .expect("capabilities should deserialize"),
        );
        verify_that!(
            cancel_error(&features, "textDocument/semanticTokens/full").0 as i32,
            eq(ErrorCode::ContentModified as i32)
        )?;
        verify_that!(
            cancel_error(&features, "textDocument/inlayHint").0 as i32,
            eq(ErrorCode::RequestCanceled as i32)
        )?;
        verify_that!(
            cancel_error(
                &LspFeatures::new(ClientCapabilities::default()),
                "textDocument/semanticTokens/full"
            )
            .0 as i32,
            eq(ErrorCode::RequestCanceled as i32)
        )?;
        Ok(())
    }

    #[gtest]
    fn stale_response_rejects_null_payloads() -> Result<()> {
        let null_result = Response::new_ok(1.into(), serde_json::Value::Null);

        verify_that!(
            should_send_stale_response_on_cancel("textDocument/inlayHint", &null_result),
            eq(false)
        )?;
        Ok(())
    }

    #[gtest]
    fn a_cancelled_workspace_sweep_restores_the_level_it_claimed() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = Connection::memory();

        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let workspace = context.snapshot().workspace_manager_arc();
            let workspace = workspace.read().await;

            workspace.update_workspace_version(WorkspaceDiagnosticLevel::Slow, false);
            workspace.update_workspace_version(WorkspaceDiagnosticLevel::Fast, false);
            verify_that!(
                workspace.claim_workspace_diagnostic_level(),
                eq(WorkspaceDiagnosticLevel::Slow)
            )?;

            workspace.update_workspace_version(WorkspaceDiagnosticLevel::Slow, false);

            // Claiming empties it, so a second pull finds nothing to do.
            verify_that!(
                workspace.claim_workspace_diagnostic_level(),
                eq(WorkspaceDiagnosticLevel::Slow)
            )?;
            verify_that!(
                workspace.claim_workspace_diagnostic_level(),
                eq(WorkspaceDiagnosticLevel::None)
            )?;

            // A `Fast` request arriving mid-sweep must not survive as the
            // restored value in place of the interrupted `Slow`.
            workspace.update_workspace_version(WorkspaceDiagnosticLevel::Fast, false);
            workspace.restore_workspace_diagnostic_level(WorkspaceDiagnosticLevel::Slow);
            verify_that!(
                workspace.claim_workspace_diagnostic_level(),
                eq(WorkspaceDiagnosticLevel::Slow)
            )?;

            // And restoring never lowers an already-higher pending level.
            workspace.update_workspace_version(WorkspaceDiagnosticLevel::Slow, false);
            workspace.restore_workspace_diagnostic_level(WorkspaceDiagnosticLevel::Fast);
            verify_that!(
                workspace.claim_workspace_diagnostic_level(),
                eq(WorkspaceDiagnosticLevel::Slow)
            )?;
            Ok(())
        })
    }

    /// See `keep_stale_editor_data_on_cancel`: cancelled document pulls must
    /// answer with an error, workspace pulls with a success.
    #[gtest]
    fn cancelled_document_diagnostics_answer_with_an_error() -> Result<()> {
        verify_that!(
            keep_stale_editor_data_on_cancel("textDocument/diagnostic"),
            eq(false)
        )?;
        verify_that!(
            keep_stale_editor_data_on_cancel("workspace/diagnostic"),
            eq(true)
        )?;

        let features = LspFeatures::new(ClientCapabilities::default());
        for method in ["textDocument/diagnostic", "workspace/diagnostic"] {
            let (code, data) = cancel_error(&features, method);
            verify_that!(code as i32, eq(ErrorCode::ServerCancelled as i32))?;
            verify_that!(data, eq(&Some(json!({ "retriggerRequest": true }))))?;
        }
        Ok(())
    }

    #[gtest]
    fn cancel_all_requests_except_preserves_inlay_and_code_lens_requests() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (conn, _peer) = lsp_server::Connection::memory();
            let context = ServerContext::new(conn, ClientCapabilities::default());

            let inlay_id: RequestId = 1.into();
            context
                .task(
                    inlay_id.clone(),
                    RequestTaskMetadata::new("textDocument/inlayHint", None),
                    |_cancel_token| async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Some(Response::new_ok(inlay_id, json!([{"label": ": number"}])))
                    },
                )
                .await;

            let hover_id: RequestId = 2.into();
            context
                .task(
                    hover_id.clone(),
                    RequestTaskMetadata::new("textDocument/hover", None),
                    |_cancel_token| async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Some(Response::new_ok(hover_id, serde_json::Value::Null))
                    },
                )
                .await;

            let code_lens_id: RequestId = 3.into();
            context
                .task(
                    code_lens_id.clone(),
                    RequestTaskMetadata::new("textDocument/codeLens", None),
                    |_cancel_token| async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Some(Response::new_ok(code_lens_id, json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "command": {"title": "test", "command": "test"}}])))
                    },
                )
                .await;

            let diag_id: RequestId = 4.into();
            context
                .task(
                    diag_id.clone(),
                    RequestTaskMetadata::new("textDocument/diagnostic", None),
                    |_cancel_token| async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Some(Response::new_ok(diag_id, json!({"kind": "unChanged", "resultId": "abc"})))
                    },
                )
                .await;

            let (inlay_token, code_lens_token, hover_token, diag_token) = {
                let requests = context.requests.lock().await;
                let inlay = requests
                    .get(&RequestId::from(1))
                    .expect("inlay request should exist")
                    .cancel_token
                    .clone();
                let code_lens = requests
                    .get(&RequestId::from(3))
                    .expect("code lens request should exist")
                    .cancel_token
                    .clone();
                let hover = requests
                    .get(&RequestId::from(2))
                    .expect("hover request should exist")
                    .cancel_token
                    .clone();
                let diag = requests
                    .get(&RequestId::from(4))
                    .expect("diagnostic request should exist")
                    .cancel_token
                    .clone();
                (inlay, code_lens, hover, diag)
            };

            context
                .cancel_all_requests_except(&[
                    "textDocument/inlayHint",
                    "textDocument/codeLens",
                ])
                .await;

            verify_that!(inlay_token.is_cancelled(), eq(false))?;
            verify_that!(code_lens_token.is_cancelled(), eq(false))?;
            verify_that!(diag_token.is_cancelled(), eq(true))?;
            verify_that!(hover_token.is_cancelled(), eq(true))?;
            Ok(())
        })
    }
}
