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

/// The phase last entered. Only one phase is in flight at a time, so the
/// per-file loops inside it can report counts against this without naming it —
/// but those loops run on several worker threads, so reads and writes here are
/// concurrent and a count may be reported against a phase that has just ended.
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
