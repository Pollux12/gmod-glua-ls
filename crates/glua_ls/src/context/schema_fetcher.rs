use std::collections::HashSet;
use std::sync::Arc;

use glua_code_analysis::{EmmyLuaAnalysis, fetch_schema_urls};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

/// Fetches the JSON schemas an analysis round registered, in its own task.
///
/// The debounced analysis loop wakes the worker when a round settles - the
/// moment a `---@schema` tag an edit registered becomes visible - so the
/// tag's type resolves without waiting for the client to save anything. A
/// URL the worker already tried is left to the saves, which keep the retry
/// role: a URL that cannot be fetched must not be retried on every settled
/// round, while a `file://` schema that failed because its file did not
/// exist yet stays retryable for the save that follows the user creating
/// it.
pub(crate) struct SchemaFetcher {
    wake: Arc<Notify>,
}

impl SchemaFetcher {
    /// Starts the worker and returns the handle the debounce loop wakes.
    ///
    /// One fetch round runs at a time: a wake arriving mid-round is held by
    /// the `Notify` permit, so the next round starts right after it and a
    /// burst of settled rounds costs no more than one round each.
    pub fn spawn(analysis: Arc<RwLock<EmmyLuaAnalysis>>, shutdown: CancellationToken) -> Arc<Self> {
        let wake = Arc::new(Notify::new());
        tokio::spawn(worker(analysis, shutdown, wake.clone()));
        Arc::new(Self { wake })
    }

    /// Wakes the worker: schemas a settled round registered, if any, are
    /// fetched in the background.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// One fetch round: snapshot the pending URLs the worker has not tried
/// under a short read lock, fetch without holding any lock, then apply
/// under the write lock the analysis already uses.
///
/// The URL list is snapshotted before the fetch, so a URL registered while
/// a round is in flight stays `NeedResolve` and is fetched by the next
/// one.
async fn worker(
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shutdown: CancellationToken,
    wake: Arc<Notify>,
) {
    // URLs this worker already tried and left for saves to retry.
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
            // Nothing resolved: the URLs stay pending for a save to retry,
            // and applying would only take the write lock to do nothing.
            continue;
        }
        for url in url_contents.keys() {
            tried.remove(url);
        }
        let mut write_analysis = analysis.write().await;
        write_analysis.apply_fetched_schemas(url_contents);
    }
}
