//! The progress sink and current phase are process-global, so these tests run
//! in their own binary: in the unit-test binary every test that analyses a
//! workspace reports into them, which changes both the call count and the phase
//! name under test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use glua_code_analysis::VirtualWorkspace;
use glua_code_analysis::progress::{
    PhaseProgress, advance_current_phase, clear_sink, enter_phase, is_active, set_sink,
};

/// The sink is process-global, so these must not run concurrently.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// A load reports each phase it passes through, counting the files of the
/// whole load rather than of the batch a worker happens to hold.
#[test]
fn a_load_reports_its_phases_against_every_file_it_loads() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink_reports = reports.clone();
    set_sink(Arc::new(move |progress: PhaseProgress<'_>| {
        if let Ok(mut reports) = sink_reports.lock() {
            reports.push((progress.phase.to_string(), progress.done, progress.total));
        }
    }));

    let mut ws = VirtualWorkspace::new();
    ws.def_files(vec![
        ("lua/autorun/sh_a.lua", "A = 1"),
        ("lua/autorun/sh_b.lua", "B = A"),
        ("lua/autorun/sh_c.lua", "C = B"),
    ]);
    clear_sink();

    let reports = reports.lock().unwrap().clone();
    let finished = |phase: &str| {
        reports
            .iter()
            .any(|(seen, done, total)| seen == phase && *total == 3 && *done == 3)
    };
    assert!(
        finished("Collecting declarations")
            && finished("Reading annotations")
            && reports
                .iter()
                .any(|(phase, _, _)| phase == "Inferring types"),
        "{reports:?}"
    );
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
