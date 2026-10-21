mod client;
mod client_id;
mod debounced_analysis;
pub(crate) mod diagnostic_store;
mod did_change_coalescer;
mod editor_documents;
mod file_diagnostic;
mod lsp_features;
mod snapshot;
mod status_bar;
mod workspace_manager;

pub use client::ClientProxy;
pub use client_id::{ClientId, get_client_id};
#[cfg(test)]
pub(crate) use debounced_analysis::expedites_debounce;
pub use debounced_analysis::{DebouncedAnalysis, Freshness, InFlightChangeGuard};
pub use diagnostic_store::DiagnosticStore;
pub use did_change_coalescer::DidChangeCoalescer;
pub use editor_documents::{EditorDocuments, SyncError};
pub use file_diagnostic::FileDiagnostic;
use glua_code_analysis::EmmyLuaAnalysis;
pub use lsp_features::LspFeatures;
use lsp_server::{Connection, ErrorCode, Message, RequestId, Response};
use lsp_types::{ClientCapabilities, Uri};
pub use snapshot::ServerContextSnapshot;
pub use status_bar::ProgressTask;
pub use status_bar::StatusBar;
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
pub use workspace_manager::*;

use crate::context::snapshot::ServerContextInner;

// LOCK ORDER (acquire low → high; never a lower lock while holding a higher):
// 1. workspace_diagnostic_token  2. recheck_token  3. analysis(read)
// 4. workspace_manager(read)  5. workspace_manager(write)  6. analysis(write)
// Within `DebouncedAnalysis`: pending_files before reindexing_files, and both
// are released before anything above is taken.
// Leaves: document_versions, renamed_uris, the diagnostic store's publishing
// then state, and
// `DebouncedAnalysis::in_flight_documents` — synchronous
// locks, so they can never be held across an `.await`. Never upgrade read→write in place; avoid holding any lock across
// `.await`. Atomics are exempt.

/// Panics the debounce supervisor will restart before it stops trying.
const DEBOUNCE_RESTART_LIMIT: u32 = 5;

const DEBOUNCE_RESTART_BACKOFF_BASE: Duration = Duration::from_millis(200);
const DEBOUNCE_RESTART_BACKOFF_MAX: Duration = Duration::from_secs(5);

fn debounce_restart_backoff(restarts: u32) -> Duration {
    DEBOUNCE_RESTART_BACKOFF_BASE
        .saturating_mul(1_u32 << restarts.min(16).saturating_sub(1))
        .min(DEBOUNCE_RESTART_BACKOFF_MAX)
}

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
// Semantic tokens are excluded: relative offsets make a stale set wrong.
fn keep_stale_editor_data_on_cancel(method: &str) -> bool {
    matches!(
        method,
        "textDocument/codeLens" | "textDocument/inlayHint" | "gluals/annotator"
    )
}

/// The error code — and any `data` payload — for a cancelled request.
fn cancel_error(features: &LspFeatures, method: &str) -> (ErrorCode, Option<serde_json::Value>) {
    // ContentModified only for methods the client declares it re-sends;
    // others read it as "no result" and clear the feature's UI.
    if features.retries_on_content_modified(method) {
        (ErrorCode::ContentModified, None)
    } else {
        (ErrorCode::RequestCanceled, None)
    }
}

/// The answer to `id`, a `method` request the server gave up on, in the form
/// the client re-sends it on.
pub(crate) fn cancelled_response(
    features: &LspFeatures,
    id: RequestId,
    method: &str,
    message: &str,
) -> Response {
    let (code, data) = cancel_error(features, method);
    Response {
        id,
        result: None,
        error: Some(lsp_server::ResponseError {
            code: code as i32,
            message: message.to_string(),
            data,
        }),
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
    editor_documents: EditorDocuments,
}

impl ServerContext {
    pub fn new(conn: Connection, client_capabilities: ClientCapabilities) -> Self {
        let client = Arc::new(ClientProxy::new(Connection {
            sender: conn.sender.clone(),
            receiver: conn.receiver.clone(),
        }));

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.compilation.set_detached_tasks(true);
        let analysis = Arc::new(RwLock::new(analysis));
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
        let debounced_shutdown = CancellationToken::new();
        let debounced_analysis = Arc::new(DebouncedAnalysis::new(
            analysis.clone(),
            200,
            debounced_shutdown.clone(),
            client.clone(),
            file_diagnostic.clone(),
            lsp_features.clone(),
        ));
        let workspace_manager_inner = WorkspaceManager::new(
            analysis.clone(),
            client.clone(),
            status_bar.clone(),
            file_diagnostic.clone(),
            lsp_features.clone(),
            debounced_analysis.clone(),
        );
        let workspace_manager = Arc::new(RwLock::new(workspace_manager_inner));

        file_diagnostic.store().start(
            file_diagnostic::workspace_diagnostic_parallelism(),
            debounced_analysis.clone(),
        );

        // Supervise the debounce loop: freshness waiters park on it with no
        // deadline, so if it dies the waiters fail fast via the alive flag
        // instead of the whole server silently going quiet.
        {
            let da = debounced_analysis.clone();
            let shutdown = debounced_shutdown.clone();
            tokio::spawn(async move {
                let mut restarts = 0_u32;
                while !shutdown.is_cancelled() {
                    let task = tokio::spawn({
                        let da = da.clone();
                        async move { da.run().await }
                    });
                    match task.await {
                        // `run` only returns on shutdown.
                        Ok(()) => return,
                        Err(err) => {
                            restarts += 1;
                            if restarts > DEBOUNCE_RESTART_LIMIT {
                                log::error!(
                                    "LS_DEBOUNCE_LOOP_DEAD debounced analysis loop panicked {} times; giving up, so edits stop being re-indexed and freshness waits fail fast: {}",
                                    restarts,
                                    err
                                );
                                da.note_debounce_loop_dead();
                                return;
                            }
                            log::error!(
                                "LS_DEBOUNCE_LOOP_PANIC debounced analysis loop died, restarting: {}",
                                err
                            );
                        }
                    }

                    // A panic on entry would otherwise respawn at full CPU,
                    // and every restart writes a log line.
                    tokio::select! {
                        _ = tokio::time::sleep(debounce_restart_backoff(restarts)) => {}
                        _ = shutdown.cancelled() => return,
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
            document_versions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            document_version_notify: Arc::new(Notify::new()),
            renamed_uris: Default::default(),
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
            editor_documents: EditorDocuments::default(),
        }
    }

    /// The answer to a request refused while the workspace initializes.
    pub fn refused_during_init(&self, id: RequestId, method: &str) -> Response {
        cancelled_response(&self.inner.lsp_features, id, method, "server initializing")
    }

    pub fn snapshot(&self) -> ServerContextSnapshot {
        ServerContextSnapshot::new(self.inner.clone())
    }

    pub fn did_change_coalescer(&self) -> &DidChangeCoalescer {
        &self.did_change_coalescer
    }

    pub fn editor_documents(&mut self) -> &mut EditorDocuments {
        &mut self.editor_documents
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
                    let response = cancelled_response(
                        &lsp_features,
                        req_id.clone(),
                        &request_method,
                        "cancel",
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

    /// Cancels the in-flight requests aimed at `uri`, except those of
    /// `excluded_methods`.
    ///
    /// Request URIs and `uri` both come from the client's own params, so they
    /// are spelled alike and compare as they are.
    pub async fn cancel_requests_for_uri(&self, uri: &Uri, excluded_methods: &[&str]) {
        let requests = self.requests.lock().await;
        for request in requests.values() {
            if request.metadata.uri.as_ref() == Some(uri)
                && !excluded_methods.contains(&request.metadata.method.as_str())
            {
                request.cancel_token.cancel();
            }
        }
    }

    pub async fn close(&self) {
        self.debounced_shutdown.cancel();
        self.inner.file_diagnostic.store().stop();
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
        DEBOUNCE_RESTART_BACKOFF_MAX, LspFeatures, RequestTaskMetadata, ServerContext,
        cancel_error, debounce_restart_backoff, keep_stale_editor_data_on_cancel,
        should_send_stale_response_on_cancel,
    };
    use googletest::prelude::*;
    use lsp_server::{ErrorCode, RequestId, Response};
    use lsp_types::{ClientCapabilities, Uri};
    use serde_json::json;
    use std::str::FromStr;
    use std::time::Duration;

    #[test]
    fn debounce_restart_backoff_doubles_then_saturates() {
        assert_eq!(debounce_restart_backoff(1), Duration::from_millis(200));
        assert_eq!(debounce_restart_backoff(3), Duration::from_millis(800));
        assert_eq!(debounce_restart_backoff(60), DEBOUNCE_RESTART_BACKOFF_MAX);
    }

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

    /// A document change supersedes only that document's requests: other
    /// documents' requests, and those aimed at none, keep running, as do the
    /// exempted methods.
    #[gtest]
    fn cancelling_a_documents_requests_leaves_other_documents_alone() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (conn, _peer) = lsp_server::Connection::memory();
            let context = ServerContext::new(conn, ClientCapabilities::default());
            let edited = Uri::from_str("file:///workspace/edited.lua").expect("uri should parse");
            let other = Uri::from_str("file:///workspace/other.lua").expect("uri should parse");

            let requests = [
                ("textDocument/inlayHint", Some(edited.clone())),
                ("textDocument/codeLens", Some(edited.clone())),
                ("textDocument/hover", Some(edited.clone())),
                ("textDocument/hover", Some(other)),
                ("workspace/symbol", None),
            ];
            for (id, (method, uri)) in requests.into_iter().enumerate() {
                let request_id = RequestId::from(id as i32);
                context
                    .task(
                        request_id.clone(),
                        RequestTaskMetadata::new(method, uri),
                        |_cancel_token| async move {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            Some(Response::new_ok(request_id, serde_json::Value::Null))
                        },
                    )
                    .await;
            }
            let tokens = {
                let requests = context.requests.lock().await;
                (0..5)
                    .map(|id| {
                        requests
                            .get(&RequestId::from(id))
                            .expect("request should be in flight")
                            .cancel_token
                            .clone()
                    })
                    .collect::<Vec<_>>()
            };

            context
                .cancel_requests_for_uri(
                    &edited,
                    &["textDocument/inlayHint", "textDocument/codeLens"],
                )
                .await;

            let cancelled = tokens
                .iter()
                .map(|token| token.is_cancelled())
                .collect::<Vec<_>>();
            verify_that!(cancelled, eq(&vec![false, false, true, false, false]))
        })
    }
}
