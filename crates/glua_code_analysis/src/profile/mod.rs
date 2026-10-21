pub mod cpu;
pub mod memory;
pub mod work;

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

/// Estimated heap bytes of a hash table with `capacity` entries of
/// `entry_size` bytes: its buckets and their control bytes.
pub(crate) fn hash_table_bytes(capacity: usize, entry_size: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = (capacity * 8 / 7).next_power_of_two();
    buckets * (entry_size + 1)
}

pub(crate) fn megabytes(bytes: usize) -> usize {
    bytes / 1_048_576
}

/// True while a `Profile` whose name matches `GLUALS_PROFILE_SAMPLE` is alive.
/// An allocation sampler can gate on this to profile one phase instead of the
/// whole process — which is what makes sampling affordable, since the phase
/// worth sampling (`lua analyze`) is single-threaded and the parallel phases
/// would otherwise swamp the sample set and contend on the sampler's lock.
/// A count rather than a flag: the same phase name can be entered more than
/// once at a time, and a plain flag would let the first exit turn sampling off
/// underneath the others.
static SAMPLE_PHASE_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether the phase named by `GLUALS_PROFILE_SAMPLE` is currently running.
#[inline]
pub fn sample_phase_active() -> bool {
    SAMPLE_PHASE_DEPTH.load(Ordering::Relaxed) > 0
}

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

/// Named sub-phase accumulator, gated on `GLUALS_PROFILE_PHASE`. Per-file
/// detail prints are gated on it too, so `GLUALS_PROFILE` stays per-run.
pub(crate) fn phase_enabled() -> bool {
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
        let allocs = ALLOC_COUNT
            .load(Ordering::Relaxed)
            .saturating_sub(allocs_before);
        add_phase(self.name, start.elapsed(), allocs);
    }
}

/// Accumulates `elapsed` and `allocs` under `name`, as one call.
pub fn add_phase(name: &'static str, elapsed: Duration, allocs: u64) {
    if !phase_enabled() {
        return;
    }
    let mut phases = PHASES.lock().unwrap_or_else(|poison| poison.into_inner());
    match phases.iter_mut().find(|(phase, _, _, _)| *phase == name) {
        Some((_, total, count, total_allocs)) => {
            *total += elapsed;
            *count += 1;
            *total_allocs += allocs;
        }
        None => phases.push((name, elapsed, 1, allocs)),
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
pub fn profiling() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE").is_some())
}

/// When `GLUALS_TRACE_EVALUATIONS` is set, the engine prints every derivation
/// it evaluates with the key that queued it and the reads that moved.
pub fn tracing_evaluations() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_TRACE_EVALUATIONS").is_some())
}

#[allow(unused)]
impl<'a> Profile<'a> {
    pub fn new(name: &'a str) -> Self {
        if sampled_phase() == Some(name) {
            SAMPLE_PHASE_DEPTH.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            name,
            start: Instant::now(),
            allocs: alloc_snapshot(),
        }
    }

    pub fn cond_new(name: &'a str, cond: bool) -> Option<Self> {
        if cond && (log::log_enabled!(log::Level::Info) || profiling()) {
            Some(Self::new(name))
        } else {
            None
        }
    }
}

impl<'a> Drop for Profile<'a> {
    fn drop(&mut self) {
        if sampled_phase() == Some(self.name) {
            SAMPLE_PHASE_DEPTH.fetch_sub(1, Ordering::Relaxed);
        }
        let duration = self.start.elapsed();
        if log::log_enabled!(log::Level::Info) {
            info!("{}: cost {:?}", self.name, duration);
        }
        if profiling() {
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
