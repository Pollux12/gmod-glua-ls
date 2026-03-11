use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;

use glua_code_analysis::EmmyLuaAnalysis;
use lsp_types::Uri;

use crate::context::lsp_features::LspFeatures;

use super::{
    client::ClientProxy, debounced_analysis::DebouncedAnalysis,
    editor_display_cache::EditorDisplayCache, file_diagnostic::FileDiagnostic,
    status_bar::StatusBar, workspace_manager::WorkspaceManager,
};

#[derive(Clone, Copy)]
pub enum DocumentVersionState {
    Open(i32),
    Closed,
}

fn is_stale_document_version(state: Option<DocumentVersionState>, version: i32) -> bool {
    match state {
        Some(DocumentVersionState::Open(seen_version)) => seen_version > version,
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

    pub fn editor_display_cache(&self) -> &EditorDisplayCache {
        &self.inner.editor_display_cache
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
        self.inner
            .document_versions
            .lock()
            .await
            .insert(uri.clone(), DocumentVersionState::Open(version));
    }

    pub async fn has_newer_seen_document_version(&self, uri: &Uri, version: i32) -> bool {
        is_stale_document_version(
            self.inner.document_versions.lock().await.get(uri).copied(),
            version,
        )
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
    pub editor_display_cache: Arc<EditorDisplayCache>,
    pub workspace_manager: Arc<RwLock<WorkspaceManager>>,
    pub status_bar: Arc<StatusBar>,
    pub lsp_features: Arc<LspFeatures>,
    pub debounced_analysis: Arc<DebouncedAnalysis>,
    pub document_versions: Arc<Mutex<HashMap<Uri, DocumentVersionState>>>,
}

#[cfg(test)]
mod tests {
    use super::{DocumentVersionState, is_stale_document_version};

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
            Some(DocumentVersionState::Open(3)),
            2,
        ));
        assert!(!is_stale_document_version(
            Some(DocumentVersionState::Open(3)),
            3,
        ));
    }
}
