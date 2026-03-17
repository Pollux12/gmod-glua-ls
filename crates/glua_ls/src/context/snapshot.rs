use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Notify, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;

use glua_code_analysis::EmmyLuaAnalysis;
use lsp_types::Uri;

use crate::context::lsp_features::LspFeatures;

use super::{
    client::ClientProxy, debounced_analysis::DebouncedAnalysis, file_diagnostic::FileDiagnostic,
    status_bar::StatusBar, workspace_manager::WorkspaceManager,
};

#[derive(Clone, Copy)]
pub enum DocumentVersionState {
    Open {
        seen_version: i32,
        applied_version: Option<i32>,
    },
    Closed,
}

fn is_stale_document_version(state: Option<DocumentVersionState>, version: i32) -> bool {
    match state {
        Some(DocumentVersionState::Open { seen_version, .. }) => seen_version > version,
        Some(DocumentVersionState::Closed) => true,
        None => false,
    }
}

#[derive(Clone)]
pub struct ServerContextSnapshot {
    inner: Arc<ServerContextInner>,
}

impl ServerContextSnapshot {
    pub fn new(inner: Arc<ServerContextInner>) -> Self {
        Self { inner }
    }

    pub fn analysis(&self) -> &RwLock<EmmyLuaAnalysis> {
        &self.inner.analysis
    }

    pub fn client(&self) -> &ClientProxy {
        &self.inner.client
    }

    pub fn file_diagnostic(&self) -> &FileDiagnostic {
        &self.inner.file_diagnostic
    }

    pub fn workspace_manager(&self) -> &RwLock<WorkspaceManager> {
        &self.inner.workspace_manager
    }

    pub fn status_bar(&self) -> &StatusBar {
        &self.inner.status_bar
    }

    pub fn lsp_features(&self) -> &LspFeatures {
        &self.inner.lsp_features
    }

    pub fn debounced_analysis(&self) -> &DebouncedAnalysis {
        &self.inner.debounced_analysis
    }

    pub fn debounced_analysis_arc(&self) -> Arc<DebouncedAnalysis> {
        self.inner.debounced_analysis.clone()
    }

    pub async fn note_document_seen_version(&self, uri: &Uri, version: i32) {
        let mut versions = self.inner.document_versions.lock().await;
        let applied_version = match versions.get(uri).copied() {
            Some(DocumentVersionState::Open {
                applied_version, ..
            }) => applied_version,
            _ => None,
        };
        versions.insert(
            uri.clone(),
            DocumentVersionState::Open {
                seen_version: version,
                applied_version,
            },
        );
        drop(versions);
        self.inner.document_version_notify.notify_waiters();
    }

    pub async fn has_newer_seen_document_version(&self, uri: &Uri, version: i32) -> bool {
        is_stale_document_version(
            self.inner.document_versions.lock().await.get(uri).copied(),
            version,
        )
    }

    pub async fn note_document_applied_version(&self, uri: &Uri, version: i32) {
        let mut versions = self.inner.document_versions.lock().await;
        let next_state = match versions.get(uri).copied() {
            Some(DocumentVersionState::Open { seen_version, .. }) => DocumentVersionState::Open {
                seen_version,
                applied_version: Some(version),
            },
            Some(DocumentVersionState::Closed) => DocumentVersionState::Closed,
            None => DocumentVersionState::Open {
                seen_version: version,
                applied_version: Some(version),
            },
        };
        versions.insert(uri.clone(), next_state);
        drop(versions);
        self.inner.document_version_notify.notify_waiters();
    }

    pub async fn wait_until_latest_document_version_applied(
        &self,
        uri: &Uri,
        cancel_token: &CancellationToken,
    ) -> bool {
        loop {
            let notified = self.inner.document_version_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let is_fresh = match self.inner.document_versions.lock().await.get(uri).copied() {
                Some(DocumentVersionState::Open {
                    seen_version,
                    applied_version,
                }) => applied_version.is_some_and(|applied| applied >= seen_version),
                Some(DocumentVersionState::Closed) => return false,
                None => true,
            };

            if is_fresh {
                return true;
            }

            tokio::select! {
                _ = notified => {}
                _ = cancel_token.cancelled() => return false,
            }
        }
    }

    pub async fn is_document_closed(&self, uri: &Uri) -> bool {
        matches!(
            self.inner.document_versions.lock().await.get(uri).copied(),
            Some(DocumentVersionState::Closed)
        )
    }

    pub async fn mark_document_closed(&self, uri: &Uri) {
        self.inner
            .document_versions
            .lock()
            .await
            .insert(uri.clone(), DocumentVersionState::Closed);
        self.inner.document_version_notify.notify_waiters();
    }

    /// Acquire a read lock on the analysis, racing against a cancellation token.
    /// Returns `None` immediately if the token fires before the lock is acquired.
    /// This prevents handlers from blocking on a write-preferring RwLock when
    /// their request has already been superseded.
    pub async fn read_analysis(
        &self,
        cancel_token: &CancellationToken,
    ) -> Option<RwLockReadGuard<'_, EmmyLuaAnalysis>> {
        tokio::select! {
            guard = self.analysis().read() => Some(guard),
            _ = cancel_token.cancelled() => None,
        }
    }

    /// Acquire a read lock on the workspace manager, racing against a
    /// cancellation token.
    pub async fn read_workspace_manager(
        &self,
        cancel_token: &CancellationToken,
    ) -> Option<RwLockReadGuard<'_, WorkspaceManager>> {
        tokio::select! {
            guard = self.workspace_manager().read() => Some(guard),
            _ = cancel_token.cancelled() => None,
        }
    }
}

pub struct ServerContextInner {
    pub analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    pub client: Arc<ClientProxy>,
    pub file_diagnostic: Arc<FileDiagnostic>,
    pub workspace_manager: Arc<RwLock<WorkspaceManager>>,
    pub status_bar: Arc<StatusBar>,
    pub lsp_features: Arc<LspFeatures>,
    pub debounced_analysis: Arc<DebouncedAnalysis>,
    pub document_versions: Arc<Mutex<HashMap<Uri, DocumentVersionState>>>,
    pub document_version_notify: Arc<Notify>,
}

#[cfg(test)]
mod tests {
    use super::{DocumentVersionState, is_stale_document_version};
    use crate::context::ServerContext;
    use googletest::prelude::*;
    use lsp_types::{ClientCapabilities, Uri};
    use std::str::FromStr;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn treats_closed_documents_as_stale() {
        assert!(is_stale_document_version(
            Some(DocumentVersionState::Closed),
            1
        ));
    }

    #[test]
    fn treats_newer_versions_as_stale() {
        assert!(is_stale_document_version(
            Some(DocumentVersionState::Open {
                seen_version: 3,
                applied_version: Some(2),
            }),
            2,
        ));
        assert!(!is_stale_document_version(
            Some(DocumentVersionState::Open {
                seen_version: 3,
                applied_version: Some(3),
            }),
            3,
        ));
    }

    #[gtest]
    fn waits_until_latest_document_version_is_applied() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (conn, _peer) = lsp_server::Connection::memory();
            let context = ServerContext::new(conn, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///format.lua").expect("uri should parse");

            snapshot.note_document_seen_version(&uri, 2).await;
            snapshot.note_document_applied_version(&uri, 1).await;

            let waiter_snapshot = snapshot.clone();
            let waiter_uri = uri.clone();
            let waiter = tokio::spawn(async move {
                waiter_snapshot
                    .wait_until_latest_document_version_applied(
                        &waiter_uri,
                        &CancellationToken::new(),
                    )
                    .await
            });

            tokio::time::sleep(Duration::from_millis(10)).await;
            verify_that!(waiter.is_finished(), eq(false))?;

            snapshot.note_document_applied_version(&uri, 2).await;

            let completed = tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("waiter should complete")
                .expect("waiter should join successfully");
            verify_that!(completed, eq(true))?;
            Ok(())
        })
    }
}
