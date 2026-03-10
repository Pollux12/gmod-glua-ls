use std::sync::Arc;
use tokio::sync::{RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;

use glua_code_analysis::EmmyLuaAnalysis;

use crate::context::lsp_features::LspFeatures;

use super::{
    client::ClientProxy, debounced_analysis::DebouncedAnalysis, file_diagnostic::FileDiagnostic,
    status_bar::StatusBar, workspace_manager::WorkspaceManager,
};

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
}
