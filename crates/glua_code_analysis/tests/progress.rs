//! The progress sink and current phase are process-global, so these tests run
//! in their own binary: in the unit-test binary every test that analyses a
//! workspace reports into them, which changes both the call count and the phase
//! name under test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use glua_code_analysis::progress::{
    PhaseProgress, advance_current_phase, clear_sink, enter_phase, is_active, phase_label, set_sink,
};

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
