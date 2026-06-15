use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(5);
const FIRST_LOG_AFTER: Duration = Duration::from_secs(15);
const SECOND_LOG_AFTER: Duration = Duration::from_secs(30);
const REPEATED_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct LongRunningWatchdogSnapshot {
    phase: String,
    completed: Option<usize>,
    total: Option<usize>,
}

impl LongRunningWatchdogSnapshot {
    fn new(phase: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            completed: None,
            total: None,
        }
    }

    fn describe(&self) -> String {
        match (self.completed, self.total) {
            (Some(completed), Some(total)) if total > 0 => {
                let percent = completed.saturating_mul(100) / total;
                format!(
                    "{} ({} / {} complete, {}%)",
                    self.phase, completed, total, percent
                )
            }
            (Some(completed), Some(total)) => {
                format!("{} ({} / {} complete)", self.phase, completed, total)
            }
            _ => self.phase.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LongRunningWatchdogStatus {
    snapshot: Arc<Mutex<LongRunningWatchdogSnapshot>>,
}

impl LongRunningWatchdogStatus {
    pub fn new(phase: impl Into<String>) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(LongRunningWatchdogSnapshot::new(phase))),
        }
    }

    pub fn set_phase(&self, phase: impl Into<String>) {
        self.update(|snapshot| {
            snapshot.phase = phase.into();
            snapshot.completed = None;
            snapshot.total = None;
        });
    }

    pub fn set_progress(&self, phase: impl Into<String>, completed: usize, total: usize) {
        self.update(|snapshot| {
            snapshot.phase = phase.into();
            snapshot.completed = Some(completed);
            snapshot.total = Some(total);
        });
    }

    pub fn describe(&self) -> String {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.describe())
            .unwrap_or_else(|_| "status unavailable".to_string())
    }

    fn update(&self, update: impl FnOnce(&mut LongRunningWatchdogSnapshot)) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            update(&mut snapshot);
        }
    }
}

#[derive(Debug)]
pub struct LongRunningLogTicker {
    next_log_after: Duration,
}

impl Default for LongRunningLogTicker {
    fn default() -> Self {
        Self {
            next_log_after: FIRST_LOG_AFTER,
        }
    }
}

impl LongRunningLogTicker {
    pub fn should_log(&mut self, elapsed: Duration) -> bool {
        if elapsed < self.next_log_after {
            return false;
        }

        self.next_log_after = match self.next_log_after {
            FIRST_LOG_AFTER => SECOND_LOG_AFTER,
            SECOND_LOG_AFTER => SECOND_LOG_AFTER + REPEATED_LOG_INTERVAL / 2,
            next => next + REPEATED_LOG_INTERVAL,
        };

        true
    }
}

#[derive(Debug)]
pub struct LongRunningWatchdogGuard {
    cancel_token: CancellationToken,
    _handle: JoinHandle<()>,
}

impl Drop for LongRunningWatchdogGuard {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

pub fn spawn_long_running_watchdog(
    task_name: &'static str,
    status: LongRunningWatchdogStatus,
) -> LongRunningWatchdogGuard {
    let cancel_token = CancellationToken::new();
    let task_cancel_token = cancel_token.clone();
    let started_at = Instant::now();

    let handle = tokio::spawn(async move {
        let mut ticker = LongRunningLogTicker::default();

        loop {
            tokio::select! {
                _ = task_cancel_token.cancelled() => break,
                _ = tokio::time::sleep(WATCHDOG_POLL_INTERVAL) => {
                    let elapsed = started_at.elapsed();
                    if ticker.should_log(elapsed) {
                        log::warn!(
                            "{} still running after {}s: {}",
                            task_name,
                            elapsed.as_secs(),
                            status.describe()
                        );
                    }
                }
            }
        }
    });

    LongRunningWatchdogGuard {
        cancel_token,
        _handle: handle,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;

    use super::LongRunningLogTicker;

    #[gtest]
    fn long_running_log_ticker_uses_escalating_startup_cadence() {
        let mut ticker = LongRunningLogTicker::default();

        expect_that!(ticker.should_log(Duration::from_secs(14)), eq(false));
        expect_that!(ticker.should_log(Duration::from_secs(15)), eq(true));
        expect_that!(ticker.should_log(Duration::from_secs(20)), eq(false));
        expect_that!(ticker.should_log(Duration::from_secs(30)), eq(true));
        expect_that!(ticker.should_log(Duration::from_secs(59)), eq(false));
        expect_that!(ticker.should_log(Duration::from_secs(60)), eq(true));
        expect_that!(ticker.should_log(Duration::from_secs(119)), eq(false));
        expect_that!(ticker.should_log(Duration::from_secs(120)), eq(true));
    }
}
