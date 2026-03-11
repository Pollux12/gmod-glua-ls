use std::{collections::HashMap, hash::Hash};

use lsp_types::Uri;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EditorDisplayCacheKind {
    EmmyAnnotator,
    InlayHints,
    SemanticTokens,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditorDisplayCacheKey {
    kind: EditorDisplayCacheKind,
    uri: Uri,
}

impl EditorDisplayCacheKey {
    fn new(kind: EditorDisplayCacheKind, uri: Uri) -> Self {
        Self { kind, uri }
    }
}

impl Hash for EditorDisplayCacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.uri.hash(state);
    }
}

#[derive(Default)]
pub struct EditorDisplayCache {
    values: Mutex<HashMap<EditorDisplayCacheKey, Value>>,
}

impl EditorDisplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get<T>(&self, kind: EditorDisplayCacheKind, uri: &Uri) -> Option<T>
    where
        T: DeserializeOwned,
    {
        let value = {
            let values = self.values.lock().await;
            values
                .get(&EditorDisplayCacheKey::new(kind, uri.clone()))
                .cloned()
        }?;

        serde_json::from_value(value).ok()
    }

    pub async fn insert<T>(&self, kind: EditorDisplayCacheKind, uri: &Uri, value: &T) -> Option<()>
    where
        T: Serialize,
    {
        let value = serde_json::to_value(value).ok()?;
        self.values
            .lock()
            .await
            .insert(EditorDisplayCacheKey::new(kind, uri.clone()), value);
        Some(())
    }

    pub async fn remove_kind(&self, kind: EditorDisplayCacheKind, uri: &Uri) {
        self.values
            .lock()
            .await
            .remove(&EditorDisplayCacheKey::new(kind, uri.clone()));
    }

    pub async fn remove_uri(&self, uri: &Uri) {
        self.values.lock().await.retain(|key, _| key.uri != *uri);
    }

    pub async fn clear(&self) {
        self.values.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use lsp_types::Uri;
    use std::str::FromStr;

    use super::{EditorDisplayCache, EditorDisplayCacheKind};

    #[gtest]
    fn round_trips_cached_values() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let cache = EditorDisplayCache::new();
        let uri = Uri::from_str("file:///cache.lua").expect("uri should parse");

        runtime.block_on(async {
            let _ = cache
                .insert(EditorDisplayCacheKind::InlayHints, &uri, &vec![1_u32, 2, 3])
                .await;
        });

        let cached = runtime.block_on(async {
            cache
                .get::<Vec<u32>>(EditorDisplayCacheKind::InlayHints, &uri)
                .await
        });

        verify_that!(cached.as_deref(), some(eq(&[1, 2, 3][..])))?;
        Ok(())
    }
}
