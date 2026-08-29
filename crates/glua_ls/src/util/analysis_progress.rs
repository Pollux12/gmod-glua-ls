//! Forwards analysis phase reports to the status bar, the watchdog and the log.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
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

/// Status messages allowed to queue before new ones are dropped.
const STATUS_QUEUE_CAPACITY: usize = 64;

/// Which reporter owns the process-global sink. `progress` keeps one slot, so
/// two overlapping analyses share it and only the newest may clear it.
static SINK_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Installs a progress sink for as long as it is alive, and logs a summary of
/// where the time went when it is dropped.
pub struct AnalysisProgressReporter {
    state: Arc<Mutex<ReporterState>>,
    started: Instant,
    generation: u64,
    /// Cleared on drop, so a queued message cannot reach the client after
    /// whatever the caller reports next.
    forwarding: Arc<AtomicBool>,
    /// Dropping this ends the forwarding thread.
    status_sender: Option<SyncSender<String>>,
    /// Joined on drop, so a message already being delivered lands before the
    /// caller's next report rather than racing it.
    status_thread: Option<JoinHandle<()>>,
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

/// Forwards status-bar messages off the analysis threads.
///
/// The status bar reaches `lsp_server`'s rendezvous channel, so a send parks
/// the caller until the writer thread — itself parked on stdout — accepts it. A
/// client that stops reading stdout would otherwise stall every analysis worker
/// that reports progress. A progress update nobody sees costs nothing, so the
/// hop drops rather than blocks.
fn spawn_status_forwarder(
    status_bar: StatusBar,
    forwarding: Arc<AtomicBool>,
) -> Option<(SyncSender<String>, JoinHandle<()>)> {
    let (sender, receiver) = sync_channel::<String>(STATUS_QUEUE_CAPACITY);

    match std::thread::Builder::new()
        .name("gluals-progress".to_string())
        .spawn(move || {
            for message in receiver {
                if !forwarding.load(Ordering::Acquire) {
                    continue;
                }
                status_bar.update_startup_phase(ProgressTask::LoadWorkspace, None, message);
            }
        }) {
        Ok(handle) => Some((sender, handle)),
        Err(error) => {
            log::error!("could not start the progress forwarding thread: {error}");
            None
        }
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

        let generation = SINK_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        let forwarding = Arc::new(AtomicBool::new(true));
        let (status_sender, status_thread) =
            match spawn_status_forwarder(status_bar, forwarding.clone()) {
                Some((sender, handle)) => (Some(sender), Some(handle)),
                None => (None, None),
            };
        let sink_sender = status_sender.clone();
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
            if let Some(sender) = sink_sender.as_ref() {
                let _ = sender.try_send(message);
            }
        }));

        Self {
            state,
            started: now,
            generation,
            forwarding,
            status_sender,
            status_thread,
        }
    }
}

impl Drop for AnalysisProgressReporter {
    fn drop(&mut self) {
        self.forwarding.store(false, Ordering::Release);

        // A later reporter has taken the sink over; clearing would blind it.
        if SINK_GENERATION.load(Ordering::Acquire) == self.generation {
            progress::clear_sink();
        }

        // Closing the channel ends the loop; joining then waits out the one
        // message that may already be inside `update_startup_phase`, so it
        // cannot land after whatever the caller reports next. Everything still
        // queued is discarded by the flag above, so this waits on at most one
        // send — on the same channel the caller is about to use anyway.
        //
        // Safe to block here because the reporter is dropped after the analysis
        // it wraps has finished, on the thread that started it rather than on a
        // worker. A caller that installs a reporter around work whose threads
        // outlive it would be blocking one of them here.
        self.status_sender = None;
        if let Some(handle) = self.status_thread.take() {
            let _ = handle.join();
        }

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

    /// The sink is process-global, so these tests cannot run alongside each
    /// other.
    static SINK: Mutex<()> = Mutex::new(());

    #[test]
    fn clearing_the_sink_stops_reports() {
        let _guard = SINK.lock().unwrap_or_else(|error| error.into_inner());
        // The reporter owns the global sink, so dropping it must clear it.
        progress::clear_sink();
        assert!(!progress::is_active());

        // Tests that analyse a workspace report into the same global sink, so
        // count only the phase this test enters.
        const PHASE: &str = "clearing_the_sink_stops_reports";
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = counter.clone();
        progress::set_sink(Arc::new(move |progress: progress::PhaseProgress<'_>| {
            if progress.phase == PHASE {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
        assert!(progress::is_active());
        progress::enter_phase(PHASE, 0, "files");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);

        progress::clear_sink();
        progress::enter_phase(PHASE, 0, "files");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn an_overlapping_reporter_keeps_the_sink_until_the_newest_one_goes() {
        let _guard = SINK.lock().unwrap_or_else(|error| error.into_inner());
        progress::clear_sink();

        let (connection, _peer) = lsp_server::Connection::memory();
        let status_bar = StatusBar::new(
            Arc::new(crate::context::ClientProxy::new(connection)),
            false,
        );
        let watchdog = LongRunningWatchdogStatus::new("test");

        let older = AnalysisProgressReporter::install(status_bar.clone(), watchdog.clone());
        let newer = AnalysisProgressReporter::install(status_bar, watchdog);

        drop(older);
        assert!(progress::is_active());

        drop(newer);
        assert!(!progress::is_active());
    }

    /// Progress is forwarded off the analysis threads, so a report can still be
    /// in flight when the reporter goes. Whatever the caller says next has to be
    /// the last thing the client hears, or a finished workspace is left showing
    /// a phase name. A report still queued at that point is discarded outright;
    /// one already being delivered is waited out by the join in `Drop`.
    #[test]
    fn no_forwarded_report_arrives_after_the_caller_closes_the_task() {
        let _guard = SINK.lock().unwrap_or_else(|error| error.into_inner());
        progress::clear_sink();

        let (connection, peer) = lsp_server::Connection::memory();
        // The status bar drops every notification unless the client asked for
        // work-done progress.
        let status_bar =
            StatusBar::new(Arc::new(crate::context::ClientProxy::new(connection)), true);
        let watchdog = LongRunningWatchdogStatus::new("test");

        // The client side is a rendezvous channel, so someone has to be reading
        // it for a send to complete at all.
        let reader = std::thread::spawn(move || {
            let mut messages = Vec::new();
            for message in &peer.receiver {
                let lsp_server::Message::Notification(notification) = message else {
                    continue;
                };
                if let Some(text) = notification.params["value"]["message"].as_str() {
                    messages.push(text.to_string());
                }
            }
            messages
        });

        let reporter = AnalysisProgressReporter::install(status_bar.clone(), watchdog);
        progress::enter_phase("Indexing", 0, "files");
        drop(reporter);

        status_bar.update_startup_phase(ProgressTask::LoadWorkspace, Some(100), "done");
        drop(status_bar);

        let messages = reader.join().expect("reader thread");
        assert_eq!(
            messages.last().map(String::as_str),
            Some("done"),
            "the closing update must be the last thing the client hears, got {messages:?}"
        );
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
