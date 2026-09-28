//! Frames: the reads behind one memoised value, so a later evaluation can
//! reuse the value by recording the reads that made it.
//!
//! A frame opens where a memo misses and closes once the value is computed.
//! Its reads are the log from where it opened: the log takes a slot again in
//! each frame that opened after the slot was last logged, so nothing an inner
//! frame or an earlier frame read is missing from it.
//!
//! A value is reusable wherever it would be computed the same: its reads hold
//! their values, and every context it consulted — a gate of the cache's view,
//! the inherited query realm — reads the same. A frame that saw something its
//! reads cannot vouch for (a memo hit without reads, a recursion cut, a
//! caller-supplied seed) is tainted, and its value stays with the evaluation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{CURRENT, Evaluation, KEYS, KeyIdx, KeyTable, latest_stamp, next_stamp};
use crate::GmodRealm;

/// Bits of a cache's view that inference can consult.
pub type Gates = u8;

#[derive(Debug, Clone, Copy)]
pub(super) struct Frame {
    start: u32,
    pub(super) stamp: u64,
    /// The outermost frame index a taint reaches, if any.
    taint: Option<u32>,
    gates: Gates,
    /// The inherited realm consulted (`None`: none was inherited), and the
    /// frame index its scope began at.
    realm: Option<(Option<GmodRealm>, u32)>,
    /// A context this frame's memo is keyed by, so a read of it stops here;
    /// see [`consult_absorbed`].
    absorbs: u64,
}

/// An open frame, to hand back to [`close_frame`].
#[derive(Debug)]
#[must_use]
pub struct OpenFrame(u32);

/// An evaluation's read log, kept once the evaluation ends for the memos
/// that point into it.
#[derive(Debug, Default)]
pub(super) struct LogHandle(
    pub(super) std::sync::OnceLock<Box<[KeyIdx]>>,
    /// The derivation whose evaluation this log is.
    pub(super) Option<super::DerivationId>,
);

impl LogHandle {
    pub(super) fn freeze(&self, log: &[KeyIdx]) {
        let _ = self.0.set(log.into());
    }
}

/// What a memoised value was computed from: a range of the log of the
/// evaluation that computed it. Its clones share it, so a walk stored at
/// every flow node it visited is one value.
#[derive(Debug, Clone)]
pub struct Provenance(Arc<Computed>);

#[derive(Debug)]
struct Computed {
    log: Arc<LogHandle>,
    start: u32,
    end: u32,
    since: u64,
    gates: Gates,
    view: Gates,
    /// The inherited realm consulted, if any was.
    realm: Option<Option<GmodRealm>>,
    /// The stamp its reads were last found unwritten at: they still are
    /// while the key table has not been written since.
    checked: AtomicU64,
    /// The stamp of the frame its reads were last replayed into: a frame
    /// holds them from then on.
    replayed_into: AtomicU64,
}

impl Provenance {
    /// Whether none of `reads`, this value's, was written after it was
    /// computed.
    fn holds(&self, table: &KeyTable, reads: &[KeyIdx]) -> bool {
        if !table.written_after(self.0.checked.load(Ordering::Relaxed)) {
            return true;
        }
        if !table.unwritten_since(reads, self.0.since) {
            return false;
        }
        self.0.checked.store(latest_stamp(), Ordering::Relaxed);
        true
    }

    /// Whether its reads still need replaying into the frame stamped
    /// `frame`, marking them replayed there.
    fn replays_into(&self, frame: u64) -> bool {
        self.0.replayed_into.swap(frame, Ordering::Relaxed) != frame
    }
}

fn with_evaluation<T>(f: impl FnOnce(&mut Evaluation, &mut KeyTable) -> T) -> Option<T> {
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let evaluation = current.as_mut()?;
        KEYS.with(|keys| keys.borrow_mut().as_mut().map(|table| f(evaluation, table)))
    })
}

fn with_innermost(f: impl FnOnce(u32, &mut Frame)) {
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            let depth = evaluation.frames.len();
            if let Some(frame) = evaluation.frames.last_mut() {
                f(depth as u32 - 1, frame);
            }
        }
    });
}

/// Opens a frame, or `None` when no evaluation records into a lent table.
pub fn open_frame() -> Option<OpenFrame> {
    open_frame_absorbing(0)
}

/// Opens a frame whose memo is keyed by the context `absorbs` names: a read
/// of that context inside it taints only the frames nested in it.
pub fn open_frame_absorbing(absorbs: u64) -> Option<OpenFrame> {
    with_evaluation(|evaluation, _| {
        let stamp = next_stamp();
        let depth = evaluation.frames.len() as u32;
        evaluation.frames.push(Frame {
            start: u32::try_from(evaluation.log.len()).expect("fewer than 2^32 reads"),
            stamp,
            taint: None,
            gates: 0,
            realm: None,
            absorbs,
        });
        OpenFrame(depth)
    })
}

/// Closes `frame` and returns what its value was computed from under `view`,
/// or `None` when the frame was tainted.
#[track_caller]
pub fn close_frame(frame: OpenFrame, view: Gates) -> Option<Provenance> {
    let caller = std::panic::Location::caller();
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let evaluation = current.as_mut()?;
        let depth = frame.0;
        debug_assert_eq!(
            evaluation.frames.len() as u32,
            depth + 1,
            "frames close in order"
        );
        let closed = evaluation.frames.pop()?;
        if let Some(parent) = evaluation.frames.last_mut() {
            let parent_depth = depth - 1;
            if let Some(reach) = closed.taint
                && reach <= parent_depth
            {
                parent.taint = Some(parent.taint.map_or(reach, |taint| taint.min(reach)));
            }
            parent.gates |= closed.gates;
            if let Some((realm, base)) = closed.realm
                && base <= parent_depth
            {
                parent.realm = Some((realm, base));
            }
        }
        if closed.taint.is_some() {
            count(|stats| {
                stats.tainted += 1;
                stats.sites.entry(caller).or_default()[0] += 1;
            });
            return None;
        }
        count(|stats| {
            stats.shared += 1;
            stats.sites.entry(caller).or_default()[1] += 1;
        });
        Some(Provenance(Arc::new(Computed {
            log: Arc::clone(&evaluation.log_handle),
            start: closed.start,
            end: u32::try_from(evaluation.log.len()).expect("fewer than 2^32 reads"),
            since: closed.stamp,
            gates: closed.gates,
            view: view & closed.gates,
            realm: closed.realm.map(|(realm, _)| realm),
            checked: AtomicU64::new(0),
            replayed_into: AtomicU64::new(0),
        })))
    })
}

/// The number of open frames: where a scope set now begins.
pub fn frame_depth() -> u32 {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(0, |evaluation| evaluation.frames.len() as u32)
    })
}

/// Taints every open frame: a value was used whose reads were not recorded
/// into them.
#[track_caller]
pub fn taint() {
    if profiling() {
        let caller = std::panic::Location::caller();
        STATS.with(|stats| *stats.borrow_mut().taints.entry(caller).or_default() += 1);
    }
    taint_scoped(0);
}

/// Taints the open frames from index `base` on: they consulted a context set
/// when `base` frames were open, which the frames outside it do not see.
pub fn taint_scoped(base: u32) {
    with_innermost(|depth, frame| {
        if base <= depth {
            frame.taint = Some(frame.taint.map_or(base, |taint| taint.min(base)));
        }
    });
}

/// Where a context was set: the open frames then, and the stamp of the
/// innermost (or of the evaluation), to tell whether that scope is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeMark {
    depth: u32,
    stamp: u64,
}

/// Marks where a context set now begins.
pub fn scope_mark() -> ScopeMark {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(ScopeMark::default(), |evaluation| ScopeMark {
                depth: evaluation.frames.len() as u32,
                stamp: evaluation.frame_stamp(),
            })
    })
}

/// Notes a read of a context set at `mark`: while its scope is open only the
/// frames inside it depend on it; once closed, every open frame does.
#[track_caller]
pub fn consult_scope(mark: ScopeMark) {
    let open = CURRENT.with(|current| {
        current.borrow().as_ref().is_some_and(|evaluation| {
            let stamp = match mark.depth {
                0 => evaluation.stamp,
                depth => match evaluation.frames.get(depth as usize - 1) {
                    Some(frame) => frame.stamp,
                    None => return false,
                },
            };
            stamp == mark.stamp && evaluation.frames.len() as u32 >= mark.depth
        })
    });
    if open {
        taint_scoped(mark.depth);
    } else {
        taint();
    }
}

/// Whether the scope `mark` began is still open on this thread.
fn scope_open(mark: ScopeMark) -> bool {
    CURRENT.with(|current| {
        current.borrow().as_ref().is_some_and(|evaluation| {
            let stamp = match mark.depth {
                0 => evaluation.stamp,
                depth => match evaluation.frames.get(depth as usize - 1) {
                    Some(frame) => frame.stamp,
                    None => return false,
                },
            };
            stamp == mark.stamp && evaluation.frames.len() as u32 >= mark.depth
        })
    })
}

/// Notes a read of the context `key`, set at `mark`. Frames keyed by it (see
/// [`open_frame_absorbing`]) already answer for it; every other frame in the
/// scope is tainted, since its value flows from theirs. A read after the scope
/// closed taints every frame.
#[track_caller]
pub fn consult_absorbed(key: u64, mark: ScopeMark) {
    if !scope_open(mark) {
        taint();
        return;
    }
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            for (index, frame) in evaluation.frames.iter_mut().enumerate().rev() {
                let index = index as u32;
                if index < mark.depth {
                    break;
                }
                if frame.absorbs != key {
                    frame.taint = Some(frame.taint.map_or(index, |taint| taint.min(index)));
                }
            }
        }
    });
}

/// Notes a recursion cut back to a computation marked at `mark` from inside
/// its frame: that frame and every frame inside it saw the cut, so their
/// values depend on where the computation started.
#[track_caller]
pub fn consult_cycle(mark: ScopeMark) {
    consult_scope(ScopeMark {
        depth: mark.depth.saturating_sub(1),
        stamp: CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .map_or(0, |evaluation| match mark.depth.checked_sub(2) {
                    None => evaluation.stamp,
                    Some(at) => evaluation
                        .frames
                        .get(at as usize)
                        .map_or(0, |frame| frame.stamp),
                })
        }),
    });
}

/// Notes that the open frames consulted `gates` of the view.
pub fn consult_gates(gates: Gates) {
    with_innermost(|_, frame| frame.gates |= gates);
}

/// Notes that the open frames from index `base` on consulted the inherited
/// query realm `realm` (`None`: none inherited), set when `base` frames were
/// open.
pub fn consult_realm(realm: Option<GmodRealm>, base: u32) {
    with_innermost(|depth, frame| {
        if base <= depth {
            frame.realm = Some((realm, base));
        }
    });
}

/// Whether a value computed as `provenance` records still holds under `view`
/// and the inherited `realm` (with the frame index its scope began at); if so
/// records its reads and consulted context into the open frames.
#[track_caller]
pub fn reuse(provenance: &Provenance, view: Gates, realm: Option<(GmodRealm, u32)>) -> bool {
    let caller = std::panic::Location::caller();
    let inherited = realm.map(|(realm, _)| realm);
    if view & provenance.0.gates != provenance.0.view
        || provenance
            .0
            .realm
            .is_some_and(|consulted| consulted != inherited)
    {
        count(|stats| stats.other_context += 1);
        return false;
    }
    with_evaluation(|evaluation, table| {
        let range = provenance.0.start as usize..provenance.0.end as usize;
        if Arc::ptr_eq(&provenance.0.log, &evaluation.log_handle) {
            // Computed by this evaluation: its reads are in the log already,
            // within the innermost frame if it opened before them.
            if provenance.0.since <= super::last_write()
                && !provenance.holds(table, &evaluation.log[range.clone()])
            {
                count(|stats| stats.stale += 1);
                return false;
            }
            if provenance.0.since < evaluation.frame_stamp()
                && provenance.replays_into(evaluation.frame_stamp())
            {
                count(|stats| {
                    stats.replayed += range.len() as u64;
                });
                for at in range {
                    let slot = evaluation.log[at];
                    evaluation.log_slot(table, slot);
                }
            }
            count(|stats| {
                stats.reused_here += 1;
                stats.sites.entry(caller).or_default()[2] += 1;
            });
        } else {
            let Some(reads) = provenance.0.log.0.get().and_then(|log| log.get(range)) else {
                return false;
            };
            if !provenance.holds(table, reads) {
                count(|stats| stats.stale += 1);
                return false;
            }
            REUSED_ACROSS.with(|flag| flag.set(true));
            let replays = provenance.replays_into(evaluation.frame_stamp());
            count(|stats| {
                stats.reused += 1;
                let site = stats.sites.entry(caller).or_default();
                site[3] += 1;
                if replays {
                    stats.replayed += reads.len() as u64;
                    site[4] += reads.len() as u64;
                }
            });
            if replays {
                for slot in reads {
                    evaluation.log_slot(table, *slot);
                }
            }
        }
        let depth = evaluation.frames.len() as u32;
        if let Some(frame) = evaluation.frames.last_mut() {
            frame.gates |= provenance.0.gates;
            let base = realm.map_or(0, |(_, base)| base);
            if provenance.0.realm.is_some() && base < depth {
                frame.realm = Some((inherited, base));
            }
        }
        true
    })
    .unwrap_or(false)
}

/// For `GLUALS_CHECK_TSC`: the keys `provenance` recorded, each with whether
/// it was written after the value was computed.
pub fn describe_reads(provenance: &Provenance) -> Vec<(String, bool)> {
    with_evaluation(|evaluation, table| {
        let range = provenance.0.start as usize..provenance.0.end as usize;
        let slots: Vec<KeyIdx> = if Arc::ptr_eq(&provenance.0.log, &evaluation.log_handle) {
            evaluation.log[range].to_vec()
        } else {
            provenance
                .0
                .log
                .0
                .get()
                .and_then(|log| log.get(range))
                .map(<[KeyIdx]>::to_vec)
                .unwrap_or_default()
        };
        slots
            .into_iter()
            .map(|slot| {
                let (key, written) = table.describe(slot);
                (
                    format!(
                        "{key:?} written={written} since={} origin={:?}",
                        provenance.0.since, provenance.0.log.1
                    ),
                    written > provenance.0.since,
                )
            })
            .collect()
    })
    .unwrap_or_default()
}

/// For `GLUALS_CHECK_TSC`: the derivation that computed `provenance`, and
/// each key it recorded.
pub fn provenance_keys(
    provenance: &Provenance,
) -> (Option<super::DerivationId>, Vec<(KeyIdx, super::HashedKey)>) {
    let keys = with_evaluation(|evaluation, table| {
        let range = provenance.0.start as usize..provenance.0.end as usize;
        let slots: Vec<KeyIdx> = if Arc::ptr_eq(&provenance.0.log, &evaluation.log_handle) {
            evaluation.log[range].to_vec()
        } else {
            provenance
                .0
                .log
                .0
                .get()
                .and_then(|log| log.get(range))
                .map(<[KeyIdx]>::to_vec)
                .unwrap_or_default()
        };
        slots
            .into_iter()
            .filter_map(|slot| Some((slot, table.describe(slot).0?)))
            .collect()
    })
    .unwrap_or_default();
    (provenance.0.log.1, keys)
}

/// A stamp newer than every value memoised so far.
pub fn new_stamp() -> u64 {
    next_stamp()
}

/// Marks keys written before the table was lent, now that it is.
pub fn note_written(keys: impl IntoIterator<Item = super::HashedKey>) {
    let now = next_stamp();
    KEYS.with(|table| {
        if let Some(table) = table.borrow_mut().as_mut() {
            for key in keys {
                table.mark_written(&key, now);
            }
        }
    });
}

/// Under `GLUALS_PROFILE`: how frames closed and memos were reused.
#[derive(Debug, Default)]
struct FrameStats {
    shared: u64,
    tainted: u64,
    reused: u64,
    reused_here: u64,
    replayed: u64,
    stale: u64,
    other_context: u64,
    taints: rustc_hash::FxHashMap<&'static std::panic::Location<'static>, u64>,
    /// Per call site: closes tainted, closes shared, reuses here, reuses
    /// across evaluations, slots those replayed.
    sites: rustc_hash::FxHashMap<&'static std::panic::Location<'static>, [u64; 5]>,
}

thread_local! {
    static STATS: std::cell::RefCell<FrameStats> = std::cell::RefCell::default();
    static REUSED_ACROSS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a reuse since the last call took a value another evaluation
/// computed; clears the flag. For `GLUALS_CHECK_TSC`.
pub fn take_reused_across() -> bool {
    REUSED_ACROSS.with(|flag| flag.replace(false))
}

/// Under `GLUALS_CHECK_TSC`, values reused across evaluations are recomputed
/// fresh and compared.
pub fn check_tsc() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_TSC").is_some())
}

fn profiling() -> bool {
    crate::profile::profiling()
}

fn count(f: impl FnOnce(&mut FrameStats)) {
    if profiling() {
        STATS.with(|stats| f(&mut stats.borrow_mut()));
    }
}

/// Prints and resets this thread's frame counts under `GLUALS_PROFILE`.
pub fn report_frames() {
    if !profiling() {
        return;
    }
    let stats = STATS.with(|stats| std::mem::take(&mut *stats.borrow_mut()));
    eprintln!(
        "[frames] closed shared={} tainted={} reused={} reused_here={} replayed={} stale={} other_context={}",
        stats.shared,
        stats.tainted,
        stats.reused,
        stats.reused_here,
        stats.replayed,
        stats.stale,
        stats.other_context
    );
    let mut taints = stats.taints.into_iter().collect::<Vec<_>>();
    taints.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (caller, count) in taints.into_iter().take(25) {
        eprintln!("[frames] taint {count} {caller}");
    }
    let mut sites = stats.sites.into_iter().collect::<Vec<_>>();
    sites.sort_by_key(|(caller, _)| caller.to_string());
    for (caller, [tainted, shared, here, across, replayed]) in sites {
        eprintln!(
            "[frames] site {caller} tainted={tainted} shared={shared} here={here} across={across} replayed={replayed}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileId;
    use crate::db_index::derivation::{self, DerivationId, DerivationKind, FactKey};

    /// A value reused inside a frame nested in one it was already replayed
    /// into is recorded by the nested frame too: that frame's value is stale
    /// once what the reused value read is written.
    #[test]
    fn a_value_reused_again_in_a_nested_frame_is_recorded_by_it() {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        let frame = open_frame().expect("a table is lent");
        derivation::note_read(read);
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        derivation::end();

        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let outer = open_frame().expect("a table is lent");
        let reused_outside = reuse(&value, 0, None);
        let inner = open_frame().expect("a table is lent");
        let reused_inside = reuse(&value, 0, None);
        let nested = close_frame(inner, 0).expect("the frame saw no taint");
        let _ = close_frame(outer, 0);
        derivation::end();

        derivation::begin(DerivationId::workspace(DerivationKind::NetHelperNames));
        derivation::note_write(read);
        let nested_holds = reuse(&nested, 0, None);
        derivation::end();
        derivation::take_keys();

        assert_eq!(
            (reused_outside, reused_inside, nested_holds),
            (true, true, false)
        );
    }
}
