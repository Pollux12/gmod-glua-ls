use std::collections::HashSet;

use lsp_types::Uri;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct EditorDisplayCache {
    /// Tracks URIs that have been seen, so cleanup operations (e.g. on
    /// document close) remain cheap even though we no longer cache
    /// display results — we used to cache inlay hints / semantic tokens
    /// here but that caused stale-position bugs.
    uris: Mutex<HashSet<UriKey>>,
}

/// Wrapper to provide Hash for Uri.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UriKey(Uri);

impl std::hash::Hash for UriKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl EditorDisplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn remove_uri(&self, _uri: &Uri) {
        // No-op: display results are no longer cached (see handlers for
        // inlay hints, semantic tokens, and annotators). Kept as a
        // call-site-compatible stub so callers do not need changes.
    }

    pub async fn clear(&self) {
        self.uris.lock().await.clear();
    }
}
