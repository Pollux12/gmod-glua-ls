use lsp_types::Uri;

#[derive(Default)]
pub struct EditorDisplayCache;

impl EditorDisplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn remove_uri(&self, _uri: &Uri) {
        // Overlay display results are no longer cached here.
    }

    pub async fn clear(&self) {}
}
