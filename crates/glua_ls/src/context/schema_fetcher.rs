use std::collections::HashSet;
use std::sync::Arc;

use glua_code_analysis::{EmmyLuaAnalysis, fetch_schema_urls};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

pub(crate) struct SchemaFetcher {
    wake: Arc<Notify>,
}

impl SchemaFetcher {
    pub fn spawn(analysis: Arc<RwLock<EmmyLuaAnalysis>>, shutdown: CancellationToken) -> Arc<Self> {
        let wake = Arc::new(Notify::new());
        tokio::spawn(worker(analysis, shutdown, wake.clone()));
        Arc::new(Self { wake })
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

async fn worker(
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shutdown: CancellationToken,
    wake: Arc<Notify>,
) {
    let mut tried: HashSet<_> = HashSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = wake.notified() => {}
        }
        let urls = {
            let read_analysis = analysis.read().await;
            if !read_analysis.check_schema_update() {
                continue;
            }
            read_analysis
                .get_schemas_to_fetch()
                .into_iter()
                .filter(|url| !tried.contains(url))
                .collect::<Vec<_>>()
        };
        if urls.is_empty() {
            continue;
        }
        tried.extend(urls.iter().cloned());
        let url_contents = fetch_schema_urls(urls).await;
        if url_contents.is_empty() {
            continue;
        }
        for url in url_contents.keys() {
            tried.remove(url);
        }
        let mut write_analysis = analysis.write().await;
        write_analysis.apply_fetched_schemas(url_contents);
    }
}
