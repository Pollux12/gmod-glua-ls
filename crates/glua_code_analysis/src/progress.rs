//! Reporting analysis progress back to whoever asked for the analysis.
//!
//! The sink is process-global for the same reason [`crate::profile`]'s is: the
//! passes that report are threaded through `&mut DbIndex`, not through any
//! object the caller owns.

use std::sync::{Arc, RwLock};

/// One progress report from an analysis pass.
pub struct PhaseProgress<'a> {
    /// What the pass is doing, already worded for a user.
    pub phase: &'a str,
    /// How much of `total` is done. Meaningless when `total` is 0.
    pub done: usize,
    /// How much there is to do, or 0 when the phase has nothing to count.
    pub total: usize,
    /// What `done` and `total` count, for the message: "files", "deferred
    /// types", and so on.
    pub unit: &'a str,
}

pub type ProgressSink = Arc<dyn Fn(PhaseProgress<'_>) + Send + Sync>;

static SINK: RwLock<Option<ProgressSink>> = RwLock::new(None);

/// The phase last entered. One workspace analyses on one thread at a time, so
/// the per-file loops inside a phase can report counts against it.
static CURRENT_PHASE: RwLock<Option<String>> = RwLock::new(None);

/// Install `sink` for the duration of an analysis run. Replaces any previous
/// sink; [`clear_sink`] removes it.
pub fn set_sink(sink: ProgressSink) {
    if let Ok(mut slot) = SINK.write() {
        *slot = Some(sink);
    }
}

pub fn clear_sink() {
    if let Ok(mut slot) = SINK.write() {
        *slot = None;
    }
    if let Ok(mut current) = CURRENT_PHASE.write() {
        *current = None;
    }
}

/// Whether anything is listening.
pub fn is_active() -> bool {
    SINK.read().is_ok_and(|slot| slot.is_some())
}

/// Enter `phase`, and report it.
pub fn enter_phase(phase: &str, total: usize, unit: &str) {
    if !is_active() {
        return;
    }
    if let Ok(mut current) = CURRENT_PHASE.write() {
        *current = Some(phase.to_string());
    }
    emit(PhaseProgress {
        phase,
        done: 0,
        total,
        unit,
    });
}

/// Report `done`/`total` under whichever phase is currently running.
pub fn advance_current_phase(done: usize, total: usize, unit: &str) {
    if !is_active() {
        return;
    }
    let phase = match CURRENT_PHASE.read() {
        Ok(current) => match current.as_ref() {
            Some(phase) => phase.clone(),
            None => return,
        },
        Err(_) => return,
    };
    emit(PhaseProgress {
        phase: &phase,
        done,
        total,
        unit,
    });
}

fn emit(progress: PhaseProgress<'_>) {
    let sink = match SINK.read() {
        Ok(slot) => slot.clone(),
        Err(_) => return,
    };
    if let Some(sink) = sink {
        sink(progress);
    }
}

/// A phase name to show a user, given the pipeline's Rust type name. An
/// unmapped pipeline falls back to its own name.
pub fn phase_label(pipeline_type_name: &str) -> &str {
    match pipeline_type_name {
        "DeclAnalysisPipeline" => "Collecting declarations",
        "DocAnalysisPipeline" => "Reading annotations",
        "FlowAnalysisPipeline" => "Building control flow",
        "GmodPreAnalysisPipeline" => "Resolving GMod metadata",
        "LuaAnalysisPipeline" => "Inferring types",
        "EarlyDynamicFieldAnalysisPipeline" | "DynamicFieldAnalysisPipeline" => {
            "Resolving dynamic fields"
        }
        "PreDynamicUnResolveAnalysisPipeline" | "UnResolveAnalysisPipeline" => {
            "Resolving deferred types"
        }
        "CallSiteParamAnalysisPipeline" => "Inferring parameters from call sites",
        "GmodNetworkAnalysisPipeline" => "Analysing net messages",
        "GmodPostAnalysisPipeline" => "Finishing GMod analysis",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The sink is process-global, so these must not run concurrently.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn phase_label_maps_known_pipelines_and_passes_through_others() {
        assert_eq!(phase_label("LuaAnalysisPipeline"), "Inferring types");
        assert_eq!(phase_label("SomeNewPipeline"), "SomeNewPipeline");
    }

    #[test]
    fn report_is_a_noop_without_a_sink() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_sink();
        assert!(!is_active());
        enter_phase("anything", 2, "files");
        advance_current_phase(1, 2, "files");
    }

    #[test]
    fn advance_reports_under_the_phase_last_entered() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen_phase = Arc::new(Mutex::new(String::new()));

        let counter = calls.clone();
        let phase_slot = seen_phase.clone();
        set_sink(Arc::new(move |progress: PhaseProgress<'_>| {
            counter.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut slot) = phase_slot.lock() {
                *slot = progress.phase.to_string();
            }
        }));

        enter_phase("Inferring types", 10, "files");
        advance_current_phase(5, 10, "files");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(seen_phase.lock().unwrap().as_str(), "Inferring types");

        clear_sink();
        advance_current_phase(6, 10, "files");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
