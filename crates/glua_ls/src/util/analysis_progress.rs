//! Forwards analysis phase reports to the status bar, the watchdog and the log.

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
    /// Total time per phase. A phase repeats once per workspace group.
    totals: HashMap<String, Duration>,
}

impl ReporterState {
    /// Close off the running phase, adding its time to the totals.
    fn finish_phase(&mut self, now: Instant) {
        if self.phase.is_empty() {
            return;
        }
        let elapsed = now.duration_since(self.phase_started);
        // Only the slow ones: a phase repeats per workspace group.
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

            // A pass counts its own batch, not the whole workspace.
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

        // Ties broken on the name so the order is stable.
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
        // The reporter owns the global sink, so dropping it must clear it.
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
        // Phases repeat per workspace group, so the summary adds the repeats.
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
