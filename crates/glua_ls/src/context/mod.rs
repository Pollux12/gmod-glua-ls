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

// ============================================================================
// LOCK ORDERING GUIDELINES (CRITICAL - Must Follow to Avoid Deadlocks)
// ============================================================================
//
// This module uses multiple locks (RwLock and Mutex) for concurrent access to shared state.
// To prevent deadlocks, **ALL code must acquire locks in the following order**:
//
// ## Global Lock Order (Low to High Priority):
// 1. **diagnostic_tokens** (Mutex) - File diagnostic task tokens
// 2. **workspace_diagnostic_token** (Mutex) - Workspace diagnostic task token
// 3. **cached_file_diagnostics / recently_edited_lines** (Mutex) - UI state
// 4. **update_token** (Mutex) - Reindex/config update token
// 5. **analysis** (RwLock - READ) - Read-only access to EmmyLuaAnalysis
// 6. **workspace_manager** (RwLock - READ) - Read-only access to WorkspaceManager
// 7. **workspace_manager** (RwLock - WRITE) - Exclusive access to WorkspaceManager
// 8. **analysis** (RwLock - WRITE) - Exclusive access to EmmyLuaAnalysis
//
// ## Lock Ordering Rules:
// - **NEVER acquire a lower-priority lock while holding a higher-priority lock**
// - **ALWAYS release locks in reverse order (LIFO) or use explicit scope blocks**
// - **NEVER upgrade a read lock to a write lock (release read, then acquire write)**
// - **Minimize lock scope**: only hold locks for the minimum necessary time
// - **Avoid holding locks across `.await` points when possible**
// - **NEVER call async methods that might acquire locks while holding a lock**
//
// ## Examples:
//
// ### ✅ CORRECT - Proper lock ordering:
// ```rust
// // Acquire workspace_manager read lock first, then release before analysis write
// let should_process = {
//     let workspace_manager = context.workspace_manager().read().await;
//     workspace_manager.is_workspace_file(&uri)
// };
// if should_process {
//     let mut analysis = context.analysis().write().await;
//     analysis.update_file(&uri, text);
// }
// ```
//
// ### ❌ WRONG - ABBA deadlock risk:
// ```rust
// let mut analysis = context.analysis().write().await;  // Lock A
// // ... operations ...
// let workspace = context.workspace_manager().write().await;  // Lock B (while holding A!)
// // DEADLOCK RISK: Another thread might hold B and wait for A
// ```
//
// ### ✅ CORRECT - Release before calling async methods:
// ```rust
// let data = {
//     let workspace = context.workspace_manager().read().await;
//     workspace.get_config().clone()  // Clone data
// }; // Lock released
// init_analysis(data).await;  // Safe to call async method
// ```
//
// ### ❌ WRONG - Holding lock while calling async method:
// ```rust
// let workspace = context.workspace_manager().write().await;
// workspace.reload_workspace().await;  // May acquire analysis lock internally!
// ```
//
// ## Atomic Operations (Lock-Free):
// The following atomics can be accessed without lock ordering concerns:
// - `workspace_initialized` (AtomicBool)
// - `workspace_diagnostic_level` (AtomicU8)
// - `workspace_version` (AtomicI64)
//
// ## Notes:
// - Use `drop(lock_guard)` explicitly to release locks early when needed
// - Use scope blocks `{ ... }` to limit lock lifetime
// - When in doubt, release all locks before performing complex operations
// - If you need to modify this ordering, update this documentation AND review all call sites
// ============================================================================

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

fn keep_stale_editor_data_on_cancel(method: &str) -> bool {
    // When these requests are cancelled (typically because a new didChange
    // arrived and cancel_all_requests() fired), prefer sending whatever
    // result was already computed rather than RequestCanceled. Per the LSP
    // spec, "the result even computed on an older state might still be
    // useful for the client". Sending RequestCanceled for these methods
    // causes brief visual flickering as the client clears its display.
    //
    // Semantic tokens are deliberately excluded: their result carries no
    // version and is encoded as offsets relative to the previous token, so a
    // set computed against superseded text does not degrade — every offset
    // past the edit lands on the wrong word. They get ContentModified
    // instead, via `cancel_error_code`.
    matches!(
        method,
        "textDocument/codeLens" | "textDocument/inlayHint" | "gluals/annotator"
    )
}

fn cancel_error_code(features: &LspFeatures, method: &str) -> ErrorCode {
    // LSP 3.17 implementation considerations: "Use ContentModified only when
    // the server's own internal state invalidates an in-flight result." A
    // cancelled request is exactly that — `didChange` fired
    // `cancel_all_requests_except`, so the text it describes is gone.
    //
    // Only worth saying to a client that re-sends the request afterwards.
    // `retryOnContentModified` is the client's own per-method declaration of
    // that; for anything absent from it, ContentModified reads as "no result"
    // and clears the feature's UI, so those keep RequestCanceled.
    if features.retries_on_content_modified(method) {
        ErrorCode::ContentModified
    } else {
        ErrorCode::RequestCanceled
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
        // Returning stale-but-empty results for inlay hints/code lens can clear
        // currently rendered UI while typing. Let RequestCanceled keep the
        // previous output visible until fresh results are ready.
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
        let status_bar = Arc::new(StatusBar::new(client.clone()));
        let file_diagnostic = Arc::new(FileDiagnostic::new(
            analysis.clone(),
            status_bar.clone(),
            client.clone(),
        ));
        let lsp_features = Arc::new(LspFeatures::new(client_capabilities));
        let workspace_manager = Arc::new(RwLock::new(WorkspaceManager::new(
            analysis.clone(),
            client.clone(),
            status_bar.clone(),
            file_diagnostic.clone(),
            lsp_features.clone(),
        )));
        let debounced_shutdown = CancellationToken::new();
        let debounced_analysis = Arc::new(DebouncedAnalysis::new(
            analysis.clone(),
            200,
            debounced_shutdown.clone(),
            client.clone(),
            file_diagnostic.shared_diagnostic_data_cache(),
        ));

        // Spawn the debounced analysis background loop
        {
            let da = debounced_analysis.clone();
            tokio::spawn(async move { da.run().await });
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
        let cancel_token = CancellationToken::new();
        let request_method = metadata.method.clone();

        {
            let mut requests = self.requests.lock().await;
            requests.insert(
                req_id.clone(),
                InFlightRequest {
                    cancel_token: cancel_token.clone(),
                    metadata,
                },
            );
        }

        let sender = self.conn.sender.clone();
        let requests = self.requests.clone();
        let lsp_features = self.inner.lsp_features.clone();

        tokio::spawn(async move {
            let res = exec(cancel_token.clone()).await;
            if cancel_token.is_cancelled() {
                if keep_stale_editor_data_on_cancel(&request_method)
                    && let Some(response) = res
                    && should_send_stale_response_on_cancel(&request_method, &response)
                {
                    // Handler completed with a non-null result before/during
                    // cancellation — send it. Per LSP spec, "the result even
                    // computed on an older state might still be useful for the
                    // client."
                    let _ = sender.send(Message::Response(response.clone()));
                } else {
                    let response = Response::new_err(
                        req_id.clone(),
                        cancel_error_code(&lsp_features, &request_method) as i32,
                        "cancel".to_string(),
                    );
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

    pub async fn cancel_requests_by_method(&self, method: &str) {
        let requests = self.requests.lock().await;
        for request in requests.values() {
            if request.metadata.method == method {
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
        LspFeatures, RequestTaskMetadata, ServerContext, cancel_error_code,
        keep_stale_editor_data_on_cancel, should_send_stale_response_on_cancel,
    };
    use googletest::prelude::*;
    use lsp_server::{ErrorCode, RequestId, Response};
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
            cancel_error_code(&features, "textDocument/semanticTokens/full") as i32,
            eq(ErrorCode::ContentModified as i32)
        )?;
        verify_that!(
            cancel_error_code(&features, "textDocument/inlayHint") as i32,
            eq(ErrorCode::RequestCanceled as i32)
        )?;
        verify_that!(
            cancel_error_code(
                &LspFeatures::new(ClientCapabilities::default()),
                "textDocument/semanticTokens/full"
            ) as i32,
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

            let (inlay_token, code_lens_token, hover_token) = {
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
                (inlay, code_lens, hover)
            };

            context
                .cancel_all_requests_except(&["textDocument/inlayHint", "textDocument/codeLens"])
                .await;

            verify_that!(inlay_token.is_cancelled(), eq(false))?;
            verify_that!(code_lens_token.is_cancelled(), eq(false))?;
            verify_that!(hover_token.is_cancelled(), eq(true))?;
            Ok(())
        })
    }
}
