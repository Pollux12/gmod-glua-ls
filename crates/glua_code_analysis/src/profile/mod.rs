use log::info;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Named sub-phase accumulator, gated on `GLUALS_PROFILE_PHASE`.
pub fn phase_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE_PHASE").is_some())
}

static PHASES: Mutex<Vec<(&'static str, Duration, u64)>> = Mutex::new(Vec::new());

/// Run `f`, accumulating its elapsed time under `name` when phase profiling is on.
pub fn phase<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    if !phase_enabled() {
        return f();
    }
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    let mut phases = PHASES.lock().unwrap_or_else(|poison| poison.into_inner());
    match phases.iter_mut().find(|(phase, _, _)| *phase == name) {
        Some((_, total, count)) => {
            *total += elapsed;
            *count += 1;
        }
        None => phases.push((name, elapsed, 1)),
    }
    out
}

/// Print and clear the accumulated sub-phase table.
pub fn phase_report(label: &str) {
    if !phase_enabled() {
        return;
    }
    let mut phases =
        std::mem::take(&mut *PHASES.lock().unwrap_or_else(|poison| poison.into_inner()));
    phases.sort_unstable_by_key(|(_, total, _)| std::cmp::Reverse(*total));
    for (name, total, count) in phases {
        eprintln!(
            "  [phase] {label:<22} {name:<44} {:>8.3}s ({count} calls)",
            total.as_secs_f64()
        );
    }
}

pub struct Profile<'a> {
    name: &'a str,
    start: Instant,
}

/// When `GLUALS_PROFILE` is set, phase-level `Profile` timers print to stderr
/// even without Info-level logging. This gives clean per-phase numbers without
/// the per-node instrumentation overhead that distorts Info-level runs.
fn phase_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE").is_some())
}

#[allow(unused)]
impl<'a> Profile<'a> {
    pub fn new(name: &'a str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }

    pub fn cond_new(name: &'a str, cond: bool) -> Option<Self> {
        if (cond && log::log_enabled!(log::Level::Info)) || phase_profile_enabled() {
            Some(Self::new(name))
        } else {
            None
        }
    }
}

impl<'a> Drop for Profile<'a> {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        if log::log_enabled!(log::Level::Info) {
            info!("{}: cost {:?}", self.name, duration);
        }
        if phase_profile_enabled() {
            eprintln!("[profile] {}: cost {:?}", self.name, duration);
        }
    }
}
