use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::{Notify, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;

use glua_code_analysis::EmmyLuaAnalysis;
use lsp_types::Uri;

use crate::context::lsp_features::LspFeatures;

use super::{
    client::ClientProxy, debounced_analysis::DebouncedAnalysis, file_diagnostic::FileDiagnostic,
    status_bar::StatusBar, workspace_manager::WorkspaceManager,
};

/// Every open document's version state. Sessions order the edits of one
/// `didOpen`..`didClose` span; a reopened document starts a new one.
#[derive(Default)]
pub struct DocumentVersions {
    next_session: u64,
    states: HashMap<Uri, DocumentVersionState>,
}

impl DocumentVersions {
    fn next_session(&mut self) -> u64 {
        self.next_session += 1;
        self.next_session
    }

    fn open(&mut self, uri: &Uri, version: i32) -> u64 {
        let session = self.next_session();
        self.states.insert(
            uri.clone(),
            DocumentVersionState::Open {
                session,
                seen_version: version,
                applied_version: None,
            },
        );
        session
    }
}

#[derive(Clone, Copy)]
pub enum DocumentVersionState {
    Open {
        session: u64,
        seen_version: i32,
        applied_version: Option<i32>,
    },
    Closed {
        session: u64,
    },
}

fn is_current_document_update(
    state: Option<DocumentVersionState>,
    session: u64,
    version: i32,
) -> bool {
    matches!(
        state,
        Some(DocumentVersionState::Open { session: current, seen_version, .. })
            if current == session && seen_version <= version
    )
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

    pub fn workspace_manager_arc(&self) -> Arc<RwLock<WorkspaceManager>> {
        self.inner.workspace_manager.clone()
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

    /// The document version map is a leaf: a synchronous lock, so it can never
    /// be held across an `.await` and every caller is free to take it while
    /// holding an analysis lock.
    fn document_versions(&self) -> MutexGuard<'_, DocumentVersions> {
        self.inner
            .document_versions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Starts a new editor session for `uri`, returning its id.
    pub fn note_document_opened(&self, uri: &Uri, version: i32) -> u64 {
        let session = self.document_versions().open(uri, version);
        self.inner.document_version_notify.notify_waiters();
        session
    }

    /// Records that `version` was seen for an open `uri`, returning the
    /// session it belongs to. A change after close is dropped rather than
    /// reopening the document.
    pub fn note_document_seen_version(&self, uri: &Uri, version: i32) -> Option<u64> {
        let session = {
            let mut versions = self.document_versions();
            match versions.states.get_mut(uri) {
                Some(DocumentVersionState::Open {
                    session,
                    seen_version,
                    ..
                }) => {
                    *seen_version = version;
                    *session
                }
                Some(DocumentVersionState::Closed { .. }) => return None,
                None => versions.open(uri, version),
            }
        };
        self.inner.document_version_notify.notify_waiters();
        Some(session)
    }

    /// Whether `version` of `session` is still the newest edit seen for `uri`.
    pub fn is_current_document_update(&self, uri: &Uri, session: u64, version: i32) -> bool {
        is_current_document_update(
            self.document_versions().states.get(uri).copied(),
            session,
            version,
        )
    }

    /// Whether `session` has applied any version of `uri` yet.
    pub fn has_applied_document_version(&self, uri: &Uri, session: u64) -> bool {
        matches!(
            self.document_versions().states.get(uri).copied(),
            Some(DocumentVersionState::Open {
                session: current,
                applied_version: Some(_),
                ..
            }) if current == session
        )
    }

    /// Records that `version` of `session` reached the analysis. An apply from
    /// a superseded session is ignored, so it cannot satisfy waiters for this
    /// one.
    pub fn note_document_applied_version(&self, uri: &Uri, session: u64, version: i32) -> bool {
        match self.document_versions().states.get_mut(uri) {
            Some(DocumentVersionState::Open {
                session: current,
                applied_version,
                ..
            }) if *current == session => *applied_version = Some(version),
            _ => return false,
        }
        self.inner.document_version_notify.notify_waiters();
        true
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

            let is_fresh = match self.document_versions().states.get(uri).copied() {
                Some(DocumentVersionState::Open {
                    seen_version,
                    applied_version,
                    ..
                }) => applied_version.is_some_and(|applied| applied >= seen_version),
                Some(DocumentVersionState::Closed { .. }) => return false,
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

    pub fn is_document_closed_for(&self, uri: &Uri, session: u64) -> bool {
        matches!(
            self.document_versions().states.get(uri).copied(),
            Some(DocumentVersionState::Closed { session: current }) if current == session
        )
    }

    /// Marks `uri` closed, returning the session that closed. A close handler
    /// from a superseded session can tell it no longer applies.
    pub fn mark_document_closed(&self, uri: &Uri) -> u64 {
        let session = {
            let mut versions = self.document_versions();
            let session = match versions.states.get(uri).copied() {
                Some(DocumentVersionState::Open { session, .. })
                | Some(DocumentVersionState::Closed { session }) => session,
                None => versions.next_session(),
            };
            versions
                .states
                .insert(uri.clone(), DocumentVersionState::Closed { session });
            session
        };
        self.inner.document_version_notify.notify_waiters();
        session
    }

    /// Closes `session` only if it is still current.
    pub fn mark_document_closed_if_current(&self, uri: &Uri, session: u64) {
        {
            let mut versions = self.document_versions();
            let Some(state) = versions.states.get_mut(uri) else {
                return;
            };
            if !matches!(*state, DocumentVersionState::Open { session: current, .. } if current == session)
            {
                return;
            }
            *state = DocumentVersionState::Closed { session };
        }
        self.inner.document_version_notify.notify_waiters();
    }

    /// Remembers the URIs of a rename `didRenameFiles` applied, for the
    /// watcher to recognise its own events for the same rename.
    pub fn note_renamed_uris(&self, uris: impl IntoIterator<Item = Uri>) {
        self.renamed_uris().extend(uris);
    }

    /// Takes the URIs among `uris` that an applied rename is waiting on.
    pub fn take_renamed_uris<'a>(&self, uris: impl IntoIterator<Item = &'a Uri>) -> HashSet<Uri> {
        let mut renamed = self.renamed_uris();
        uris.into_iter()
            .filter_map(|uri| renamed.take(uri))
            .collect()
    }

    /// A leaf lock, as [`Self::document_versions`] is.
    fn renamed_uris(&self) -> MutexGuard<'_, HashSet<Uri>> {
        self.inner
            .renamed_uris
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Acquire a read lock on the analysis, racing against a cancellation token.
    /// Returns `None` immediately if the token fires before the lock is acquired.
    /// This prevents handlers from blocking on a write-preferring RwLock when
    /// their request has already been superseded.
    pub async fn read_analysis(
        &self,
        cancel_token: &CancellationToken,
    ) -> Option<RwLockReadGuard<'_, EmmyLuaAnalysis>> {
        if let Ok(guard) = self.analysis().try_read() {
            return Some(guard);
        }
        if !crate::util::ls_profile::ls_profile_enabled() {
            return tokio::select! {
                guard = self.analysis().read() => Some(guard),
                _ = cancel_token.cancelled() => None,
            };
        }

        // Profiled path: time the lock wait and attribute it to the request
        // accumulator scoped in the fresh-index dispatch arm, so the per
        // request summary line can report `analysis_read_wait_ms` without
        // changing handler signatures. Callers outside that scope (other
        // dispatch arms) simply have nowhere to record and drop the sample.
        let start = std::time::Instant::now();
        let guard = tokio::select! {
            guard = self.analysis().read() => Some(guard),
            _ = cancel_token.cancelled() => None,
        };
        if guard.is_some() {
            let elapsed = start.elapsed();
            let _ = crate::util::ls_profile::LS_REQUEST_ACCUM
                .try_with(|accum| accum.add_read_wait(elapsed));
        }
        guard
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
    /// Leaf lock: see [`ServerContextSnapshot::document_versions`].
    pub document_versions: Arc<Mutex<DocumentVersions>>,
    pub document_version_notify: Arc<Notify>,
    /// Leaf lock: see [`ServerContextSnapshot::renamed_uris`].
    pub renamed_uris: Mutex<HashSet<Uri>>,
}

#[cfg(test)]
mod tests {
    use super::{DocumentVersionState, is_current_document_update};
    use crate::context::{ServerContext, ServerContextSnapshot};
    use googletest::prelude::*;
    use lsp_types::{ClientCapabilities, Uri};
    use std::str::FromStr;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn with_snapshot<T>(test: impl FnOnce(ServerContextSnapshot, Uri) -> T) -> T {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (conn, _peer) = lsp_server::Connection::memory();
            let context = ServerContext::new(conn, ClientCapabilities::default());
            let uri = Uri::from_str("file:///document.lua").expect("uri should parse");
            test(context.snapshot(), uri)
        })
    }

    #[test]
    fn treats_closed_documents_as_stale() {
        assert!(!is_current_document_update(
            Some(DocumentVersionState::Closed { session: 1 }),
            1,
            1
        ));
    }

    #[test]
    fn treats_newer_versions_as_stale() {
        let open = Some(DocumentVersionState::Open {
            session: 1,
            seen_version: 3,
            applied_version: Some(2),
        });
        assert!(!is_current_document_update(open, 1, 2));
        assert!(is_current_document_update(open, 1, 3));
    }

    #[gtest]
    fn a_reopen_starts_a_new_session_without_an_applied_version() -> Result<()> {
        with_snapshot(|snapshot, uri| {
            let first = snapshot.note_document_opened(&uri, 1);
            snapshot.note_document_applied_version(&uri, first, 1);
            let second = snapshot.note_document_opened(&uri, 1);

            verify_that!(
                snapshot.is_current_document_update(&uri, first, 1),
                eq(false)
            )?;
            verify_that!(
                snapshot.is_current_document_update(&uri, second, 1),
                eq(true)
            )?;
            verify_that!(
                snapshot.has_applied_document_version(&uri, second),
                eq(false)
            )
        })
    }

    #[gtest]
    fn a_change_from_an_older_session_is_not_current() -> Result<()> {
        with_snapshot(|snapshot, uri| {
            let old = snapshot.note_document_opened(&uri, 1);
            let reopened = snapshot.note_document_opened(&uri, 1);
            let seen = snapshot.note_document_seen_version(&uri, 2);

            verify_that!(seen, some(eq(reopened)))?;
            verify_that!(snapshot.is_current_document_update(&uri, old, 2), eq(false))?;
            verify_that!(
                snapshot.is_current_document_update(&uri, reopened, 2),
                eq(true)
            )
        })
    }

    #[gtest]
    fn a_change_after_close_does_not_reopen() -> Result<()> {
        with_snapshot(|snapshot, uri| {
            let session = snapshot.note_document_opened(&uri, 1);
            let closed = snapshot.mark_document_closed(&uri);
            let seen = snapshot.note_document_seen_version(&uri, 2);

            verify_that!(closed, eq(session))?;
            verify_that!(seen, none())?;
            verify_that!(snapshot.is_document_closed_for(&uri, session), eq(true))
        })
    }

    #[gtest]
    fn an_old_filtered_handler_cannot_close_a_reopened_document() -> Result<()> {
        with_snapshot(|snapshot, uri| {
            let old = snapshot.note_document_opened(&uri, 1);
            let current = snapshot.note_document_opened(&uri, 1);

            snapshot.mark_document_closed_if_current(&uri, old);
            verify_that!(
                snapshot.is_current_document_update(&uri, current, 1),
                eq(true)
            )
        })
    }

    #[gtest]
    fn waits_until_latest_document_version_is_applied() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (conn, _peer) = lsp_server::Connection::memory();
            let context = ServerContext::new(conn, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///format.lua").expect("uri should parse");

            let session = snapshot.note_document_opened(&uri, 2);
            snapshot.note_document_applied_version(&uri, session, 1);

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

            snapshot.note_document_applied_version(&uri, session, 2);

            let completed = tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("waiter should complete")
                .expect("waiter should join successfully");
            verify_that!(completed, eq(true))?;
            Ok(())
        })
    }
}
