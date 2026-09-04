use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn profile_gate() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE").is_some())
}

/// Whether `GLUALS_PROFILE` instrumentation should record and emit.
///
/// All `ls_*` records are gated on this. When unset there is no output and
/// callers must not pay for `Instant::now()` or extra locking.
#[inline]
pub fn ls_profile_enabled() -> bool {
    profile_gate()
}

static LS_SEQ: AtomicU64 = AtomicU64::new(1);
static REINDEX_CYCLE: AtomicU64 = AtomicU64::new(1);

/// Monotonic id shared by request and edit lines so they order against each
/// other.
pub fn next_ls_seq() -> u64 {
    LS_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Monotonic id for reindex cycles.
pub fn next_reindex_cycle() -> u64 {
    REINDEX_CYCLE.fetch_add(1, Ordering::Relaxed)
}

#[inline]
pub fn ms(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

/// `Some(Instant::now())` when profiling, `None` otherwise (zero cost path
/// avoids the clock read entirely).
#[inline]
pub fn profile_instant() -> Option<Instant> {
    ls_profile_enabled().then(Instant::now)
}

// ---------------------------------------------------------------- request ---

/// Accumulator for one fresh-index request, scoped in the dispatch arm so
/// `read_analysis` can contribute its lock wait without signature changes.
pub struct LsRequestAccum {
    read_wait: std::sync::Mutex<Duration>,
    handler_time: std::sync::Mutex<Option<Duration>>,
}

impl LsRequestAccum {
    pub fn new() -> Self {
        Self {
            read_wait: std::sync::Mutex::new(Duration::ZERO),
            handler_time: std::sync::Mutex::new(None),
        }
    }

    pub fn add_read_wait(&self, elapsed: Duration) {
        if let Ok(mut guard) = self.read_wait.lock() {
            *guard += elapsed;
        }
    }

    pub fn take_read_wait(&self) -> Duration {
        self.read_wait
            .lock()
            .map(|guard| *guard)
            .unwrap_or_default()
    }

    pub fn set_handler_time(&self, elapsed: Duration) {
        if let Ok(mut guard) = self.handler_time.lock() {
            *guard = Some(elapsed);
        }
    }

    pub fn take_handler_time(&self) -> Option<Duration> {
        self.handler_time.lock().ok().and_then(|guard| *guard)
    }
}

impl Default for LsRequestAccum {
    fn default() -> Self {
        Self::new()
    }
}

tokio::task_local! {
    pub static LS_REQUEST_ACCUM: std::sync::Arc<LsRequestAccum>;
}

/// Guard that records handler time into the scoped request accumulator on
/// drop, so every early return in the handler is covered.
pub struct HandlerTimer {
    start: Instant,
    accum: std::sync::Arc<LsRequestAccum>,
}

impl HandlerTimer {
    pub fn scoped() -> Option<Self> {
        if !ls_profile_enabled() {
            return None;
        }
        let start = Instant::now();
        let accum = LS_REQUEST_ACCUM.try_with(|accum| accum.clone()).ok()?;
        Some(Self { start, accum })
    }
}

impl Drop for HandlerTimer {
    fn drop(&mut self) {
        self.accum.set_handler_time(self.start.elapsed());
    }
}

#[allow(clippy::too_many_arguments)]
pub fn emit_ls_request(
    seq: u64,
    id: &str,
    method: &str,
    total: Duration,
    fresh_total: Duration,
    fresh_in_flight: Duration,
    fresh_blocked: Duration,
    read_wait: Duration,
    handler: Duration,
) {
    if !ls_profile_enabled() {
        return;
    }
    eprintln!(
        "[profile] ls_request seq={} id={} method={} total_ms={} fresh_wait_ms={} fresh_wait_in_flight_ms={} fresh_wait_blocked_ms={} analysis_read_wait_ms={} handler_ms={}",
        seq,
        id,
        method,
        ms(total),
        ms(fresh_total),
        ms(fresh_in_flight),
        ms(fresh_blocked),
        ms(read_wait),
        ms(handler),
    );
}

// -------------------------------------------------------------- didChange ---

/// Enqueue-time evidence captured synchronously in the notification handler,
/// so `total_ms` spans notification receipt through schedule.
#[derive(Clone, Copy)]
pub struct DidChangeEnqueue {
    pub seq: u64,
    pub enqueue_at: Instant,
    pub version: i32,
}

impl DidChangeEnqueue {
    pub fn capture(version: i32) -> Option<Self> {
        if !ls_profile_enabled() {
            return None;
        }
        Some(Self {
            seq: next_ls_seq(),
            enqueue_at: Instant::now(),
            version,
        })
    }
}

#[derive(Default)]
pub struct DidChangeTimings {
    pub preparse: Duration,
    pub write_wait: Duration,
    pub write_hold: Duration,
    pub notify_to_schedule: Duration,
}

pub struct DidChangeAccum {
    timings: std::sync::Mutex<DidChangeTimings>,
}

impl Default for DidChangeAccum {
    fn default() -> Self {
        Self::new()
    }
}

impl DidChangeAccum {
    pub fn new() -> Self {
        Self {
            timings: std::sync::Mutex::new(DidChangeTimings::default()),
        }
    }

    fn with_timings(&self, f: impl FnOnce(&mut DidChangeTimings)) {
        if let Ok(mut guard) = self.timings.lock() {
            f(&mut guard);
        }
    }

    pub fn add_preparse(&self, elapsed: Duration) {
        self.with_timings(|timings| timings.preparse += elapsed);
    }

    pub fn add_write_wait(&self, elapsed: Duration) {
        self.with_timings(|timings| timings.write_wait += elapsed);
    }

    pub fn add_write_hold(&self, elapsed: Duration) {
        self.with_timings(|timings| timings.write_hold += elapsed);
    }

    pub fn add_notify_to_schedule(&self, elapsed: Duration) {
        self.with_timings(|timings| timings.notify_to_schedule += elapsed);
    }

    pub fn snapshot(&self) -> DidChangeTimingsSnapshot {
        let (preparse, write_wait, write_hold, notify_to_schedule) = self
            .timings
            .lock()
            .map(|timings| {
                (
                    timings.preparse,
                    timings.write_wait,
                    timings.write_hold,
                    timings.notify_to_schedule,
                )
            })
            .unwrap_or_default();
        DidChangeTimingsSnapshot {
            preparse,
            write_wait,
            write_hold,
            notify_to_schedule,
        }
    }
}

pub struct DidChangeTimingsSnapshot {
    pub preparse: Duration,
    pub write_wait: Duration,
    pub write_hold: Duration,
    pub notify_to_schedule: Duration,
}

tokio::task_local! {
    pub static LS_DID_CHANGE_ACCUM: std::sync::Arc<DidChangeAccum>;
}

pub fn record_did_change_preparse(elapsed: Duration) {
    if !ls_profile_enabled() {
        return;
    }
    let _ = LS_DID_CHANGE_ACCUM.try_with(|accum| accum.add_preparse(elapsed));
}

pub fn record_did_change_write_wait(elapsed: Duration) {
    if !ls_profile_enabled() {
        return;
    }
    let _ = LS_DID_CHANGE_ACCUM.try_with(|accum| accum.add_write_wait(elapsed));
}

pub fn record_did_change_write_hold(elapsed: Duration) {
    if !ls_profile_enabled() {
        return;
    }
    let _ = LS_DID_CHANGE_ACCUM.try_with(|accum| accum.add_write_hold(elapsed));
}

pub fn record_did_change_notify_to_schedule(elapsed: Duration) {
    if !ls_profile_enabled() {
        return;
    }
    let _ = LS_DID_CHANGE_ACCUM.try_with(|accum| accum.add_notify_to_schedule(elapsed));
}

#[allow(clippy::too_many_arguments)]
pub fn emit_ls_did_change(
    seq: u64,
    version: i32,
    coalescer_wait: Duration,
    preparse: Duration,
    write_wait: Duration,
    write_hold: Duration,
    notify_to_schedule: Duration,
    total: Duration,
) {
    if !ls_profile_enabled() {
        return;
    }
    eprintln!(
        "[profile] ls_did_change seq={} version={} coalescer_wait_ms={} preparse_ms={} analysis_write_wait_ms={} analysis_write_hold_ms={} notify_to_schedule_ms={} total_ms={}",
        seq,
        version,
        ms(coalescer_wait),
        ms(preparse),
        ms(write_wait),
        ms(write_hold),
        ms(notify_to_schedule),
        ms(total),
    );
}

// ---------------------------------------------------------------- reindex ---

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HandoffOutcome {
    Idle,
    Released,
    Timeout,
}

impl HandoffOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Released => "released",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QuietOutcome {
    Edit,
    Timer,
    MaxDeferral,
}

impl QuietOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Timer => "timer",
            Self::MaxDeferral => "max_deferral",
        }
    }
}

#[derive(Default)]
pub struct SelfWriteTimings {
    pub wait: Duration,
    pub hold: Duration,
}

#[derive(Default)]
pub struct RippleWriteTimings {
    pub wait: Duration,
    pub hold: Duration,
}

#[derive(Default)]
pub struct HandoffTiming {
    pub wait: Duration,
    pub outcome: Option<HandoffOutcome>,
}

#[derive(Default)]
pub struct QuietTiming {
    pub wait: Duration,
    pub outcome: Option<QuietOutcome>,
}

#[allow(clippy::too_many_arguments)]
pub fn emit_ls_reindex(
    cycle: u64,
    debounce: Duration,
    self_wait: Duration,
    self_hold: Duration,
    handoff_wait: Duration,
    handoff_outcome: HandoffOutcome,
    quiet_wait: Duration,
    quiet_outcome: QuietOutcome,
    ripple_wait: Duration,
    ripple_hold: Duration,
) {
    if !ls_profile_enabled() {
        return;
    }
    eprintln!(
        "[profile] ls_reindex cycle={} debounce_ms={} self_write_wait_ms={} self_write_hold_ms={} handoff_wait_ms={} handoff_outcome={} quiet_wait_ms={} quiet_outcome={} ripple_write_wait_ms={} ripple_write_hold_ms={}",
        cycle,
        ms(debounce),
        ms(self_wait),
        ms(self_hold),
        ms(handoff_wait),
        handoff_outcome.as_str(),
        ms(quiet_wait),
        quiet_outcome.as_str(),
        ms(ripple_wait),
        ms(ripple_hold),
    );
}
