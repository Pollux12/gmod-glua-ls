use log::info;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Allocation counters, incremented by the process's global allocator when it
/// opts in (see the `determinism` tool). Zero everywhere else, so `Profile`
/// simply omits the allocation column when nobody is counting.
///
/// Sampling profilers attribute time to the allocator, not to the code that
/// asked for the memory. These counters answer the complementary question —
/// *how many* allocations a phase performs — deterministically, which makes
/// "is this phase allocation-bound?" a measurement rather than a guess.
pub static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
pub static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

/// True while a `Profile` whose name matches `GLUALS_PROFILE_SAMPLE` is alive.
/// An allocation sampler can gate on this to profile one phase instead of the
/// whole process — which is what makes sampling affordable, since the phase
/// worth sampling (`lua analyze`) is single-threaded and the parallel phases
/// would otherwise swamp the sample set and contend on the sampler's lock.
pub static SAMPLE_PHASE_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn sampled_phase() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| std::env::var("GLUALS_PROFILE_SAMPLE").ok())
        .as_deref()
}

/// Bump the allocation counters. Call from a `GlobalAlloc` implementation.
#[inline]
pub fn record_alloc(size: usize) {
    ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
}

fn alloc_snapshot() -> (u64, u64) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

/// Named sub-phase accumulator, gated on `GLUALS_PROFILE_PHASE`.
fn phase_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE_PHASE").is_some())
}

static PHASES: Mutex<Vec<(&'static str, Duration, u64, u64)>> = Mutex::new(Vec::new());

/// Run `f`, accumulating its elapsed time and allocation count under `name` when
/// phase profiling is on.
pub fn phase<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    if !phase_enabled() {
        return f();
    }
    let start = Instant::now();
    let allocs_before = ALLOC_COUNT.load(Ordering::Relaxed);
    let out = f();
    let elapsed = start.elapsed();
    let allocs = ALLOC_COUNT
        .load(Ordering::Relaxed)
        .saturating_sub(allocs_before);
    let mut phases = PHASES.lock().unwrap_or_else(|poison| poison.into_inner());
    match phases.iter_mut().find(|(phase, _, _, _)| *phase == name) {
        Some((_, total, count, total_allocs)) => {
            *total += elapsed;
            *count += 1;
            *total_allocs += allocs;
        }
        None => phases.push((name, elapsed, 1, allocs)),
    }
    out
}

/// Scoped form of [`phase`], for regions that span too many locals to wrap in a
/// closure. Accumulates from construction until drop.
pub struct PhaseGuard {
    name: &'static str,
    /// Timer and allocation baseline, or `None` when phase profiling is off.
    start: Option<(Instant, u64)>,
}

impl PhaseGuard {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: phase_enabled().then(|| (Instant::now(), ALLOC_COUNT.load(Ordering::Relaxed))),
        }
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let Some((start, allocs_before)) = self.start else {
            return;
        };
        let elapsed = start.elapsed();
        let allocs = ALLOC_COUNT
            .load(Ordering::Relaxed)
            .saturating_sub(allocs_before);
        let mut phases = PHASES.lock().unwrap_or_else(|poison| poison.into_inner());
        match phases.iter_mut().find(|(phase, _, _, _)| *phase == self.name) {
            Some((_, total, count, total_allocs)) => {
                *total += elapsed;
                *count += 1;
                *total_allocs += allocs;
            }
            None => phases.push((self.name, elapsed, 1, allocs)),
        }
    }
}

/// Print and clear the accumulated sub-phase table.
pub fn phase_report(label: &str) {
    if !phase_enabled() {
        return;
    }
    let mut phases =
        std::mem::take(&mut *PHASES.lock().unwrap_or_else(|poison| poison.into_inner()));
    phases.sort_unstable_by_key(|(_, total, _, _)| std::cmp::Reverse(*total));
    for (name, total, count, allocs) in phases {
        eprintln!(
            "  [phase] {label:<22} {name:<44} {:>8.3}s ({count} calls, {allocs} allocs)",
            total.as_secs_f64()
        );
    }
}

pub struct Profile<'a> {
    name: &'a str,
    start: Instant,
    allocs: (u64, u64),
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
        if sampled_phase() == Some(name) {
            SAMPLE_PHASE_ACTIVE.store(true, Ordering::Relaxed);
        }
        Self {
            name,
            start: Instant::now(),
            allocs: alloc_snapshot(),
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
        if sampled_phase() == Some(self.name) {
            SAMPLE_PHASE_ACTIVE.store(false, Ordering::Relaxed);
        }
        let duration = self.start.elapsed();
        if log::log_enabled!(log::Level::Info) {
            info!("{}: cost {:?}", self.name, duration);
        }
        if phase_profile_enabled() {
            let (count, bytes) = alloc_snapshot();
            let allocs = count.saturating_sub(self.allocs.0);
            if allocs == 0 {
                eprintln!("[profile] {}: cost {:?}", self.name, duration);
            } else {
                eprintln!(
                    "[profile] {}: cost {:?} ({} allocs, {:.1} MiB)",
                    self.name,
                    duration,
                    allocs,
                    (bytes.saturating_sub(self.allocs.1)) as f64 / (1024.0 * 1024.0),
                );
            }
        }
    }
}
