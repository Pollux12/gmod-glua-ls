use std::collections::HashMap;
use std::sync::Arc;

use glua_code_analysis::{EmmyLuaAnalysis, FileId, Url, fetch_schema_urls};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

/// How many wakes a url whose fetch produced nothing waits before it is
/// fetched again.
const FETCH_RETRY_WAKES: u32 = 3;

pub(crate) struct SchemaFetcher {
    wake: Arc<Notify>,
}

impl SchemaFetcher {
    pub fn spawn(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        shutdown: CancellationToken,
        rediagnose: UnboundedSender<Vec<FileId>>,
    ) -> Arc<Self> {
        let wake = Arc::new(Notify::new());
        tokio::spawn(worker(analysis, shutdown, wake.clone(), rediagnose));
        Arc::new(Self { wake })
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Which schema urls a wake fetches, so one whose fetch failed or whose content
/// did not convert waits a few wakes before it is tried again instead of being
/// retried on every settle or given up on.
#[derive(Default)]
struct FetchAttempts {
    /// Wakes left before each url is fetched again.
    retry_in: HashMap<Url, u32>,
}

impl FetchAttempts {
    /// The urls of `candidates` to fetch on this wake, in their order, counting
    /// the wake against those still waiting.
    fn due(&mut self, candidates: Vec<Url>) -> Vec<Url> {
        self.retry_in.retain(|url, _| candidates.contains(url));
        let mut due = Vec::new();
        for url in candidates {
            match self.retry_in.get_mut(&url) {
                Some(remaining) if *remaining > 0 => *remaining -= 1,
                _ => due.push(url),
            }
        }
        due
    }

    /// Holds `urls` back from the next wakes, whatever their fetch returns.
    fn attempted(&mut self, urls: &[Url]) {
        for url in urls {
            self.retry_in.insert(url.clone(), FETCH_RETRY_WAKES);
        }
    }

    /// Clears the wait of urls that resolved, so one that is needed again is
    /// fetched at once.
    fn resolved(&mut self, urls: &[Url]) {
        for url in urls {
            self.retry_in.remove(url);
        }
    }
}

async fn worker(
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    shutdown: CancellationToken,
    wake: Arc<Notify>,
    rediagnose: UnboundedSender<Vec<FileId>>,
) {
    let mut attempts = FetchAttempts::default();
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
            attempts.due(read_analysis.get_schemas_to_fetch())
        };
        if urls.is_empty() {
            continue;
        }
        attempts.attempted(&urls);
        let url_contents = fetch_schema_urls(urls).await;
        if url_contents.is_empty() {
            continue;
        }
        let applied = {
            let mut write_analysis = analysis.write().await;
            write_analysis.apply_fetched_schemas(url_contents)
        };
        attempts.resolved(&applied.resolved);
        if !applied.affected.is_empty() {
            let _ = rediagnose.send(applied.affected);
        }
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{FETCH_RETRY_WAKES, FetchAttempts};

    fn config_url() -> Url {
        Url::parse("https://example.com/config.json").expect("a URL")
    }

    /// A url whose content did not convert is not fetched again on the next
    /// settles, but is retried rather than given up on.
    #[test]
    fn a_url_that_did_not_resolve_waits_before_it_is_fetched_again() {
        let mut attempts = FetchAttempts::default();
        let candidates = || vec![config_url()];

        assert_eq!(attempts.due(candidates()), candidates());
        attempts.attempted(&candidates());

        for wake in 0..FETCH_RETRY_WAKES {
            assert!(
                attempts.due(candidates()).is_empty(),
                "retried after {} wake(s)",
                wake
            );
        }
        assert_eq!(attempts.due(candidates()), candidates());
    }

    /// A url that resolved carries no wait into being needed again.
    #[test]
    fn a_url_that_resolved_is_fetched_at_once_when_it_is_needed_again() {
        let mut attempts = FetchAttempts::default();
        attempts.attempted(&[config_url()]);

        attempts.resolved(&[config_url()]);

        assert_eq!(attempts.due(vec![config_url()]), vec![config_url()]);
    }
}
