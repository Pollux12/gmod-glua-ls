use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicI32, Ordering},
    },
};

use lsp_server::{Connection, Message, Notification, RequestId, Response};
use lsp_types::{
    ApplyWorkspaceEditParams, ApplyWorkspaceEditResponse, ConfigurationParams, MessageActionItem,
    PublishDiagnosticsParams, RegistrationParams, ShowMessageParams, ShowMessageRequestParams,
    UnregistrationParams,
};
use serde::de::DeserializeOwned;
use tokio::{
    select,
    sync::{Mutex, oneshot},
};
use tokio_util::sync::CancellationToken;

pub struct ClientProxy {
    conn: Connection,
    id_counter: AtomicI32,
    response_manager: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Response>>>>,
    refresh_request_gates: StdMutex<RefreshRequestGates>,
}

#[derive(Default)]
struct RefreshRequestGates {
    workspace_diagnostics: Option<RequestId>,
    semantic_tokens: Option<RequestId>,
    inlay_hints: Option<RequestId>,
}

#[derive(Clone, Copy)]
enum RefreshRequestKind {
    WorkspaceDiagnostics,
    SemanticTokens,
    InlayHints,
}

impl RefreshRequestKind {
    const fn method(self) -> &'static str {
        match self {
            Self::WorkspaceDiagnostics => "workspace/diagnostic/refresh",
            Self::SemanticTokens => "workspace/semanticTokens/refresh",
            Self::InlayHints => "workspace/inlayHint/refresh",
        }
    }

    fn gate(self, gates: &mut RefreshRequestGates) -> &mut Option<RequestId> {
        match self {
            Self::WorkspaceDiagnostics => &mut gates.workspace_diagnostics,
            Self::SemanticTokens => &mut gates.semantic_tokens,
            Self::InlayHints => &mut gates.inlay_hints,
        }
    }
}

#[allow(unused)]
impl ClientProxy {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            id_counter: AtomicI32::new(0),
            response_manager: Arc::new(Mutex::new(HashMap::new())),
            refresh_request_gates: StdMutex::new(RefreshRequestGates::default()),
        }
    }

    pub fn send_notification(&self, method: &str, params: impl serde::Serialize) {
        let params = match serde_json::to_value(params) {
            Ok(value) => value,
            Err(e) => {
                log::error!("Failed to serialize notification params: {}", e);
                return;
            }
        };

        let _ = self.conn.sender.send(Message::Notification(Notification {
            method: method.to_string(),
            params,
        }));
    }

    pub async fn send_request(
        &self,
        id: RequestId,
        method: &str,
        params: impl serde::Serialize,
        cancel_token: CancellationToken,
    ) -> Option<Response> {
        let (sender, receiver) = oneshot::channel();
        self.response_manager
            .lock()
            .await
            .insert(id.clone(), sender);

        let params = match serde_json::to_value(params) {
            Ok(value) => value,
            Err(e) => {
                log::error!(
                    "Failed to serialize request params for method {}: {}",
                    method,
                    e
                );
                self.response_manager.lock().await.remove(&id);
                return None;
            }
        };

        let _ = self.conn.sender.send(Message::Request(lsp_server::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        }));
        let response = select! {
            response = receiver => response.ok(),
            _ = cancel_token.cancelled() => None,
        };
        self.response_manager.lock().await.remove(&id);
        response
    }

    fn send_request_no_wait(&self, id: RequestId, method: &str, params: impl serde::Serialize) {
        let params = match serde_json::to_value(params) {
            Ok(value) => value,
            Err(e) => {
                log::error!(
                    "Failed to serialize request params for method {}: {}",
                    method,
                    e
                );
                return;
            }
        };

        let _ = self.conn.sender.send(Message::Request(lsp_server::Request {
            id,
            method: method.to_string(),
            params,
        }));
    }

    fn send_deduped_refresh_request(&self, kind: RefreshRequestKind) {
        let Ok(mut gates) = self.refresh_request_gates.lock() else {
            log::error!("Failed to lock refresh request gates for {}", kind.method());
            self.send_request_no_response(kind.method(), ());
            return;
        };

        let gate = kind.gate(&mut gates);
        if gate.is_some() {
            return;
        }

        let request_id = self.next_id();
        *gate = Some(request_id.clone());
        drop(gates);

        let params = match serde_json::to_value(()) {
            Ok(value) => value,
            Err(error) => {
                log::error!(
                    "Failed to serialize request params for method {}: {}",
                    kind.method(),
                    error
                );
                self.clear_refresh_request_if_matches(kind, &request_id);
                return;
            }
        };

        if self
            .conn
            .sender
            .send(Message::Request(lsp_server::Request {
                id: request_id.clone(),
                method: kind.method().to_string(),
                params,
            }))
            .is_err()
        {
            self.clear_refresh_request_if_matches(kind, &request_id);
        }
    }

    fn clear_refresh_request_if_matches(&self, kind: RefreshRequestKind, request_id: &RequestId) {
        let Ok(mut gates) = self.refresh_request_gates.lock() else {
            log::error!("Failed to lock refresh request gates for {}", kind.method());
            return;
        };

        let gate = kind.gate(&mut gates);
        if gate
            .as_ref()
            .is_some_and(|in_flight| in_flight == request_id)
        {
            *gate = None;
        }
    }

    fn clear_completed_refresh_request(&self, request_id: &RequestId) {
        let Ok(mut gates) = self.refresh_request_gates.lock() else {
            log::error!("Failed to lock refresh request gates for response cleanup");
            return;
        };

        if gates
            .workspace_diagnostics
            .as_ref()
            .is_some_and(|in_flight| in_flight == request_id)
        {
            gates.workspace_diagnostics = None;
        }

        if gates
            .semantic_tokens
            .as_ref()
            .is_some_and(|in_flight| in_flight == request_id)
        {
            gates.semantic_tokens = None;
        }

        if gates
            .inlay_hints
            .as_ref()
            .is_some_and(|in_flight| in_flight == request_id)
        {
            gates.inlay_hints = None;
        }
    }

    pub async fn on_response(&self, response: Response) -> Option<()> {
        self.clear_completed_refresh_request(&response.id);
        let sender = self.response_manager.lock().await.remove(&response.id)?;
        let _ = sender.send(response);
        Some(())
    }

    pub fn next_id(&self) -> RequestId {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);

        id.into()
    }

    pub async fn get_configuration<C>(
        &self,
        params: ConfigurationParams,
        cancel_token: CancellationToken,
    ) -> Option<Vec<C>>
    where
        C: DeserializeOwned,
    {
        let request_id = self.next_id();
        let response = self
            .send_request(request_id, "workspace/configuration", params, cancel_token)
            .await?;
        serde_json::from_value(response.result?).ok()
    }

    pub fn dynamic_register_capability(&self, registration_param: RegistrationParams) {
        let request_id = self.next_id();
        self.send_request_no_wait(request_id, "client/registerCapability", registration_param);
    }

    pub fn dynamic_unregister_capability(&self, registration_param: UnregistrationParams) {
        let request_id = self.next_id();
        self.send_request_no_wait(
            request_id,
            "client/unregisterCapability",
            registration_param,
        );
    }

    pub fn show_message(&self, message: ShowMessageParams) {
        self.send_notification("window/showMessage", message);
    }

    pub async fn show_message_request(
        &self,
        params: ShowMessageRequestParams,
        cancel_token: CancellationToken,
    ) -> Option<MessageActionItem> {
        let request_id = self.next_id();
        let response = self
            .send_request(
                request_id,
                "window/showMessageRequest",
                params,
                cancel_token,
            )
            .await?;
        serde_json::from_value(response.result?).ok()
    }

    pub fn publish_diagnostics(&self, params: PublishDiagnosticsParams) {
        self.send_notification("textDocument/publishDiagnostics", params);
    }

    pub async fn apply_edit(
        &self,
        params: ApplyWorkspaceEditParams,
        cancel_token: CancellationToken,
    ) -> Option<ApplyWorkspaceEditResponse> {
        let request_id = self.next_id();
        let r = self
            .send_request(request_id, "workspace/applyEdit", params, cancel_token)
            .await?;
        serde_json::from_value(r.result?).ok()
    }

    pub fn send_request_no_response(&self, method: &str, params: impl serde::Serialize) {
        let request_id = self.next_id();
        self.send_request_no_wait(request_id, method, params);
    }

    pub fn refresh_workspace_diagnostics(&self) {
        self.send_deduped_refresh_request(RefreshRequestKind::WorkspaceDiagnostics);
    }

    pub fn refresh_semantic_tokens(&self) {
        self.send_deduped_refresh_request(RefreshRequestKind::SemanticTokens);
    }

    pub fn refresh_inlay_hints(&self) {
        self.send_deduped_refresh_request(RefreshRequestKind::InlayHints);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;
    use lsp_server::{Message, Request, Response};

    use super::ClientProxy;

    fn create_client_proxy() -> (ClientProxy, lsp_server::Connection) {
        let (proxy_connection, peer_connection) = lsp_server::Connection::memory();
        (ClientProxy::new(proxy_connection), peer_connection)
    }

    fn recv_request(connection: &lsp_server::Connection) -> Request {
        let message = connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("expected request");
        let Message::Request(request) = message else {
            panic!("expected request message, got {message:?}");
        };

        request
    }

    #[gtest]
    fn refresh_workspace_diagnostics_dedupes_while_request_is_in_flight() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (client, peer) = create_client_proxy();

        client.refresh_workspace_diagnostics();
        client.refresh_workspace_diagnostics();

        let request = recv_request(&peer);
        verify_that!(request.method.as_str(), eq("workspace/diagnostic/refresh"))?;
        verify_that!(peer.receiver.try_recv().is_err(), eq(true))?;

        runtime.block_on(client.on_response(Response {
            id: request.id.clone(),
            result: Some(serde_json::Value::Null),
            error: None,
        }));

        client.refresh_workspace_diagnostics();

        let second_request = recv_request(&peer);
        verify_that!(
            second_request.method.as_str(),
            eq("workspace/diagnostic/refresh")
        )?;
        assert_ne!(second_request.id, request.id);
        Ok(())
    }

    #[gtest]
    fn refresh_inlay_hints_dedupes_while_request_is_in_flight() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (client, peer) = create_client_proxy();

        client.refresh_inlay_hints();
        client.refresh_inlay_hints();

        let request = recv_request(&peer);
        verify_that!(request.method.as_str(), eq("workspace/inlayHint/refresh"))?;
        verify_that!(peer.receiver.try_recv().is_err(), eq(true))?;

        runtime.block_on(client.on_response(Response {
            id: request.id.clone(),
            result: Some(serde_json::Value::Null),
            error: None,
        }));

        client.refresh_inlay_hints();

        let second_request = recv_request(&peer);
        verify_that!(
            second_request.method.as_str(),
            eq("workspace/inlayHint/refresh")
        )?;
        assert_ne!(second_request.id, request.id);
        Ok(())
    }

    #[gtest]
    fn refresh_semantic_tokens_dedupes_while_request_is_in_flight() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (client, peer) = create_client_proxy();

        client.refresh_semantic_tokens();
        client.refresh_semantic_tokens();

        let request = recv_request(&peer);
        verify_that!(
            request.method.as_str(),
            eq("workspace/semanticTokens/refresh")
        )?;
        verify_that!(peer.receiver.try_recv().is_err(), eq(true))?;

        runtime.block_on(client.on_response(Response {
            id: request.id.clone(),
            result: Some(serde_json::Value::Null),
            error: None,
        }));

        client.refresh_semantic_tokens();

        let second_request = recv_request(&peer);
        verify_that!(
            second_request.method.as_str(),
            eq("workspace/semanticTokens/refresh")
        )?;
        assert_ne!(second_request.id, request.id);
        Ok(())
    }

    #[gtest]
    fn different_refresh_requests_keep_separate_gates() -> Result<()> {
        let (client, peer) = create_client_proxy();

        client.refresh_workspace_diagnostics();
        client.refresh_semantic_tokens();
        client.refresh_inlay_hints();

        let first_request = recv_request(&peer);
        let second_request = recv_request(&peer);
        let third_request = recv_request(&peer);
        let mut methods = [
            first_request.method,
            second_request.method,
            third_request.method,
        ];
        methods.sort();

        assert_eq!(
            methods,
            [
                "workspace/diagnostic/refresh".to_string(),
                "workspace/inlayHint/refresh".to_string(),
                "workspace/semanticTokens/refresh".to_string()
            ]
        );
        Ok(())
    }
}
