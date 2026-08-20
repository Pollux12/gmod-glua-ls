//! Forwards analysis phase reports to the status bar, the watchdog and the log.
//!
//! Indexing a workspace is a single blocking call into the analysis crate, so
//! the client would otherwise see one message for its whole duration. The
//! analysis passes report the phase they enter; this turns those into progress
//! updates a user can watch, and into a log line per phase plus a summary at
//! the end, so a report from a slow workspace says which pass was slow.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glua_code_analysis::progress;

use crate::context::{ProgressTask, StatusBar};
use crate::util::LongRunningWatchdogStatus;

/// A phase has to run this long before its count updates are forwarded. Phase
/// changes always go through; this only rate-limits the counter inside one.
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// How many phases the closing summary names.
const SUMMARY_PHASE_COUNT: usize = 5;

/// A phase is logged on its own only if it ran at least this long.
const NOTABLE_PHASE: Duration = Duration::from_millis(250);

/// Installs a progress sink for as long as it is alive, and logs a summary of
/// where the time went when it is dropped.
pub struct AnalysisProgressReporter {
    state: Arc<Mutex<ReporterState>>,
    started: Instant,
}

struct ReporterState {
    phase: String,
    phase_started: Instant,
    last_update: Instant,
    /// Total time per phase. Phases repeat, once per workspace group, so a
    /// per-phase total is what says where the run actually went.
    totals: HashMap<String, Duration>,
}

impl ReporterState {
    /// Close off the running phase, adding its time to the totals.
    fn finish_phase(&mut self, now: Instant) {
        if self.phase.is_empty() {
            return;
        }
        let elapsed = now.duration_since(self.phase_started);
        // Only the slow ones. A phase runs once per workspace group, so
        // logging every one buries the interesting lines under a hundred
        // that took a millisecond.
        if elapsed >= NOTABLE_PHASE {
            log::info!("analysis phase '{}' took {:?}", self.phase, elapsed);
        }
        *self
            .totals
            .entry(std::mem::take(&mut self.phase))
            .or_default() += elapsed;
    }
}

impl AnalysisProgressReporter {
    pub fn install(status_bar: StatusBar, watchdog_status: LongRunningWatchdogStatus) -> Self {
        let now = Instant::now();
        let state = Arc::new(Mutex::new(ReporterState {
            phase: String::new(),
            phase_started: now,
            last_update: now - MIN_UPDATE_INTERVAL,
            totals: HashMap::new(),
        }));

        let sink_state = state.clone();
        progress::set_sink(Arc::new(move |progress: progress::PhaseProgress<'_>| {
            let progress::PhaseProgress {
                phase,
                done,
                total,
                unit,
            } = progress;
            let Ok(mut state) = sink_state.lock() else {
                return;
            };
            let now = Instant::now();
            if state.phase != phase {
                state.finish_phase(now);
                state.phase.push_str(phase);
                state.phase_started = now;
            } else if now.duration_since(state.last_update) < MIN_UPDATE_INTERVAL {
                return;
            }
            state.last_update = now;
            drop(state);

            // A pass counts its own batch, which for a workspace loaded in
            // groups is not the whole file set, so the count is shown as what
            // it is rather than dressed up as workspace progress.
            let message = if total > 1 {
                format!("{phase} ({done}/{total} {unit})")
            } else {
                phase.to_string()
            };
            watchdog_status.set_phase(message.clone());
            status_bar.update_startup_phase(ProgressTask::LoadWorkspace, None, message);
        }));

        Self {
            state,
            started: now,
        }
    }
}

impl Drop for AnalysisProgressReporter {
    fn drop(&mut self) {
        progress::clear_sink();

        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        state.finish_phase(now);
        let mut totals = state.totals.drain().collect::<Vec<_>>();
        drop(state);

        // Ties broken on the name so the same run always reports the same
        // order.
        totals.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let slowest = totals
            .iter()
            .take(SUMMARY_PHASE_COUNT)
            .map(|(phase, elapsed)| format!("{phase} {:.2}s", elapsed.as_secs_f64()))
            .collect::<Vec<_>>();

        if slowest.is_empty() {
            return;
        }
        log::info!(
            "workspace analysis finished in {:.2}s; slowest phases: {}",
            now.duration_since(self.started).as_secs_f64(),
            slowest.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_the_sink_stops_reports() {
        // The reporter owns the global sink, so dropping it must leave the
        // analysis crate reporting to nobody.
        progress::clear_sink();
        assert!(!progress::is_active());

        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = counter.clone();
        progress::set_sink(Arc::new(move |_: progress::PhaseProgress<'_>| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        assert!(progress::is_active());
        progress::enter_phase("phase", 0, "files");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);

        progress::clear_sink();
        progress::enter_phase("phase", 0, "files");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn phase_totals_accumulate_across_repeats() {
        // Phases repeat once per workspace group, so the summary has to add
        // the repeats together rather than report only the last one.
        let now = Instant::now();
        let mut state = ReporterState {
            phase: String::new(),
            phase_started: now,
            last_update: now,
            totals: HashMap::new(),
        };

        state.phase.push_str("Inferring types");
        state.phase_started = now - Duration::from_secs(2);
        state.finish_phase(now);

        state.phase.push_str("Inferring types");
        state.phase_started = now - Duration::from_secs(3);
        state.finish_phase(now);

        assert_eq!(state.totals.len(), 1);
        assert!(state.totals["Inferring types"] >= Duration::from_secs(5));
        assert!(state.phase.is_empty());
    }
}
