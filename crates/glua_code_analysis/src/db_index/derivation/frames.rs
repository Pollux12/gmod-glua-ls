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
//! the inherited query realm, whether a reference walks from a keyed origin —
//! reads the same. A frame that saw something its reads cannot vouch for (a
//! memo hit without reads, a recursion cut, a caller-supplied seed) is
//! tainted, and its value stays with the evaluation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::graph::Freshness;
use super::{CURRENT, Evaluation, KEYS, KeyIdx, KeyTable, latest_stamp, next_stamp, with_viewed};
use crate::GmodRealm;

/// Bits of a cache's view that inference can consult.
pub type Gates = u8;

pub const DYNAMIC_FIELDS_GATE: Gates = 1;
pub const CALL_SITE_PARAMS_GATE: Gates = 2;
/// One gate per analysis phase that inference answers differently under.
pub const ORDERED_PHASE_GATE: Gates = 4;
pub const FORCE_PHASE_GATE: Gates = 8;
pub const DIAGNOSTICS_PHASE_GATE: Gates = 16;

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
    /// Where the frame's references read as unkeyed begin in the
    /// evaluation's log of them; see [`consult_unkeyed`].
    unkeyed: u32,
    pub(super) has_query: bool,
}

/// An open frame, to hand back to [`close_frame`].
#[derive(Debug)]
#[must_use]
pub struct OpenFrame(u32);

/// An evaluation's read log, kept once the evaluation ends for the memos
/// that point into it: only the window of it their ranges cover.
///
/// Until frozen, `start` and `end` bound the ranges of the memos computed so
/// far; once frozen, `start` is where the kept window begins in the log and
/// `end` is the length the log had.
#[derive(Debug)]
pub(super) struct LogHandle {
    start: AtomicU32,
    end: AtomicU32,
    window: std::sync::OnceLock<Box<[KeyIdx]>>,
}

impl Default for LogHandle {
    fn default() -> Self {
        Self {
            start: AtomicU32::new(u32::MAX),
            end: AtomicU32::new(0),
            window: std::sync::OnceLock::new(),
        }
    }
}

impl LogHandle {
    /// Widens the window to cover a memo computed from `start..end`. Only the
    /// thread recording the evaluation calls it, before the log is frozen.
    fn cover(&self, start: u32, end: u32) {
        if self.window.get().is_some() {
            return;
        }
        if start < self.start.load(Ordering::Relaxed) {
            self.start.store(start, Ordering::Relaxed);
        }
        if end > self.end.load(Ordering::Relaxed) {
            self.end.store(end, Ordering::Relaxed);
        }
    }

    pub(super) fn freeze(&self, log: &[KeyIdx]) {
        if self.window.get().is_some() {
            return;
        }
        let start = self.start.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        let (start, end) = if start <= end { (start, end) } else { (0, 0) };
        self.start.store(start, Ordering::Relaxed);
        self.end.store(
            u32::try_from(log.len()).expect("fewer than 2^32 reads"),
            Ordering::Relaxed,
        );
        let _ = self.window.set(log[start as usize..end as usize].into());
    }

    /// The reads `start..end` of the frozen log, as slicing the whole log
    /// would give them.
    fn reads(&self, start: u32, end: u32) -> Option<&[KeyIdx]> {
        let window = self.window.get()?;
        if end > self.end.load(Ordering::Relaxed) {
            return None;
        }
        if start == end {
            return Some(&[]);
        }
        let offset = self.start.load(Ordering::Relaxed);
        window.get((start.checked_sub(offset)? as usize)..((end - offset) as usize))
    }

    #[cfg(test)]
    fn is_frozen(&self) -> bool {
        self.window.get().is_some()
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
    /// The references it read as walking from no keyed origin: it holds
    /// only while none of them does.
    unkeyed: Option<Box<[u64]>>,
    has_query: bool,
    /// Computed without a lent table or a recording worker, by an evaluation
    /// viewing the reader graph: it holds only within that evaluation, whose
    /// reads hold its own.
    viewed: bool,
}

#[cfg(test)]
mod query_presence_tests {
    use super::*;
    use crate::db_index::derivation::{
        self as derivation, DerivationId, DerivationKind, FactKey, FactRead, HashedKey,
        ReaderGraph, RecordingSnapshot,
    };
    use crate::{FileId, LuaSignatureId};
    use rowan::TextSize;
    use rustc_hash::FxHashMap;

    fn id() -> DerivationId {
        DerivationId::workspace(DerivationKind::ValidGuards)
    }

    fn query() -> FactKey {
        FactKey::CallbackParams(LuaSignatureId::new(FileId::new(1), TextSize::new(5)))
    }

    #[test]
    fn parallel_transport_query_presence_keeps_proof_and_frame_widths() {
        assert_eq!(std::mem::size_of::<Computed>(), 64);
        assert_eq!(std::mem::size_of::<Frame>(), 48);
        assert_eq!(std::mem::size_of::<LogHandle>(), 32);
    }

    #[test]
    fn parallel_transport_query_presence_covers_direct_hint_identity_and_named_hits() {
        derivation::lend_keys(KeyTable::default());
        derivation::begin(id());
        let outer = open_frame().unwrap();
        derivation::note_read(query);
        let inner = open_frame().unwrap();
        let handle = FactRead::default();
        handle.note(query, |key| key == &query());
        assert!(close_frame(inner, 0).unwrap().0.has_query);
        derivation::note_read_by((9, 9), query);
        let inner = open_frame().unwrap();
        derivation::note_read_by((9, 9), || panic!("identity hit must not rebuild its key"));
        assert!(close_frame(inner, 0).unwrap().0.has_query);
        derivation::note_named_read(9, 9, Some("query"), query);
        let inner = open_frame().unwrap();
        derivation::note_named_read(9, 9, Some("query"), || {
            panic!("named hit must not rebuild its key")
        });
        assert!(close_frame(inner, 0).unwrap().0.has_query);
        assert!(close_frame(outer, 0).unwrap().0.has_query);
        let empty = open_frame().unwrap();
        assert!(!close_frame(empty, 0).unwrap().0.has_query);
        let ordinary = open_frame().unwrap();
        derivation::note_read(|| FactKey::FileText(FileId::new(1)));
        assert!(!close_frame(ordinary, 0).unwrap().0.has_query);
        derivation::end();
        derivation::take_keys();
    }

    #[test]
    fn parallel_transport_query_presence_propagates_tainted_children() {
        derivation::lend_keys(KeyTable::default());
        derivation::begin(id());
        let outer = open_frame().unwrap();
        let inner = open_frame().unwrap();
        derivation::note_read(query);
        taint();
        assert!(close_frame(inner, 0).is_none());
        CURRENT.with(|current| {
            assert!(
                current
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .frames
                    .last()
                    .unwrap()
                    .has_query
            )
        });
        assert!(close_frame(outer, 0).is_none());
        derivation::end();
        derivation::take_keys();
    }

    #[test]
    fn parallel_transport_query_presence_frozen_worker_replay_and_remap_require_readiness() {
        let mut graph = ReaderGraph::default();
        graph.intern_read(HashedKey::new(FactKey::FileText(FileId::new(99))));
        derivation::lend_keys(graph.lend_keys());
        derivation::begin(id());
        let frame = open_frame().unwrap();
        derivation::note_read(query);
        let proof = close_frame(frame, 0).unwrap();
        assert!(proof.0.has_query);
        derivation::end();
        graph.restore_keys(derivation::take_keys());
        let guard = graph.quarantine_releases();
        let graph = Arc::new(graph);
        let unready =
            RecordingSnapshot::new(Arc::clone(&graph), guard.started(), FxHashMap::default());
        let rejected = derivation::compute_recorded(
            &unready,
            id(),
            || {
                assert_eq!(reuse(&proof, 0, &[], None), Reuse::Elsewhere);
            },
            |_| 17,
        );
        drop(rejected);
        drop(unready);
        let ready = RecordingSnapshot::new(
            Arc::clone(&graph),
            guard.started(),
            [(HashedKey::new(query()), 17)].into_iter().collect(),
        );
        let recorded = derivation::compute_recorded(
            &ready,
            id(),
            || {
                let outer = open_frame().unwrap();
                let inner = open_frame().unwrap();
                assert_eq!(reuse(&proof, 0, &[], None), Reuse::Held);
                assert_eq!(reuse(&proof, 0, &[], None), Reuse::Held);
                let inner_proof = close_frame(inner, 0).unwrap();
                assert!(inner_proof.0.has_query);
                let local = open_frame().unwrap();
                assert_eq!(reuse(&inner_proof, 0, &[], None), Reuse::Held);
                assert_eq!(reuse(&inner_proof, 0, &[], None), Reuse::Held);
                assert!(close_frame(local, 0).unwrap().0.has_query);
                let outer_proof = close_frame(outer, 0).unwrap();
                assert!(outer_proof.0.has_query);
                let empty = open_frame().unwrap();
                assert!(!close_frame(empty, 0).unwrap().0.has_query);
                outer_proof
            },
            |_| panic!("query witness must come from ready output"),
        );
        assert!(recorded.value.0.has_query);
        assert!(recorded.evaluation.validate(&graph, |_, _| Some(17)));
        drop(ready);
        let mut graph = Arc::try_unwrap(graph).ok().unwrap();
        let mut table = graph.lend_keys();
        let evaluation = recorded.evaluation.canonicalise(&mut table).unwrap();
        derivation::lend_keys(table);
        derivation::resume(evaluation);
        derivation::end();
        derivation::begin(id());
        let frame = open_frame().unwrap();
        assert_eq!(reuse(&recorded.value, 0, &[], None), Reuse::Held);
        assert!(close_frame(frame, 0).unwrap().0.has_query);
        derivation::end();
        let mut table = derivation::take_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_query_presence_new_local_memo_without_winner_cannot_reuse() {
        let mut graph = ReaderGraph::default();
        let guard = graph.quarantine_releases();
        let graph = Arc::new(graph);
        let snapshot = RecordingSnapshot::new(
            Arc::clone(&graph),
            guard.started(),
            [(HashedKey::new(query()), 17)].into_iter().collect(),
        );
        let recorded = derivation::compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = open_frame().unwrap();
                derivation::note_read(query);
                let proof = close_frame(frame, 0).unwrap();
                assert!(proof.0.has_query);
                assert_eq!(reuse(&proof, 0, &[], None), Reuse::Elsewhere);
                proof
            },
            |_| panic!("unobserved query must not be fingerprinted lazily"),
        );
        assert!(!recorded.evaluation.validate(&graph, |_, _| Some(17)));
        drop(recorded);
        drop(snapshot);
        let mut graph = Arc::try_unwrap(graph).ok().unwrap();
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn viewed_frames_hold_within_their_evaluation_and_stay_ordinary_on_workers() {
        let graph = Arc::new(ReaderGraph::default());
        derivation::begin(DerivationId::diagnostics(FileId::new(1)));
        let view = derivation::view_graph(Arc::clone(&graph));
        assert!(open_frame().is_none());
        let frame = open_frame_viewed().unwrap();
        let proof = close_frame(frame, 0).unwrap();
        assert!(proof.0.viewed);
        assert_eq!(reuse(&proof, 0, &[], None), Reuse::Held);
        drop(view);
        derivation::end();
        derivation::begin(DerivationId::diagnostics(FileId::new(1)));
        let view = derivation::view_graph(Arc::clone(&graph));
        assert_eq!(reuse(&proof, 0, &[], None), Reuse::Stale);
        drop(view);
        derivation::end();

        let mut graph = ReaderGraph::default();
        let guard = graph.quarantine_releases();
        let graph = Arc::new(graph);
        let snapshot =
            RecordingSnapshot::new(Arc::clone(&graph), guard.started(), FxHashMap::default());
        let recorded = derivation::compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = open_frame_viewed().unwrap();
                derivation::note_read(|| FactKey::FileText(FileId::new(1)));
                let proof = close_frame(frame, 0).unwrap();
                assert!(!proof.0.viewed);
                proof
            },
            |_| 1,
        );
        assert!(!recorded.value.0.viewed);
        drop(recorded);
        drop(snapshot);
        let mut graph = Arc::try_unwrap(graph).ok().unwrap();
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }
}

/// What a lookup of a memoised value found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reuse {
    /// It holds; its reads and context are recorded into the open frames.
    Held,
    /// Something it read was written since: it holds nowhere.
    Stale,
    /// It was computed under other context, where it may still hold.
    Elsewhere,
}

impl Provenance {
    /// Whether any of `reads`, this value's, was written after it was
    /// computed.
    fn freshness(&self, table: &KeyTable, reads: &[KeyIdx]) -> Freshness {
        if !table.written_after(self.0.checked.load(Ordering::Relaxed)) {
            return Freshness::Unwritten;
        }
        let freshness = table.written_since(reads, self.0.since);
        if freshness == Freshness::Unwritten {
            self.0.checked.store(latest_stamp(), Ordering::Relaxed);
        }
        freshness
    }

    /// Under `GLUALS_PROFILE`: its identity and bytes, and its log's.
    pub(crate) fn footprint(&self) -> (usize, usize, usize, usize) {
        let computed = std::mem::size_of::<Computed>()
            + 16
            + self
                .0
                .unkeyed
                .as_ref()
                .map_or(0, |unkeyed| unkeyed.len() * 8);
        let log = std::mem::size_of::<LogHandle>()
            + 16
            + self.0.log.window.get().map_or(0, |window| window.len() * 4);
        (
            self.identity(),
            computed,
            Arc::as_ptr(&self.0.log) as usize,
            log,
        )
    }

    pub(super) fn identity(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
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
    if !super::transport::active() && super::KEYS.with(|keys| keys.borrow().is_none()) {
        return None;
    }
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        Some(push_frame(current.as_mut()?, absorbs))
    })
}

/// Opens a frame as [`open_frame`] does, and also in an evaluation viewing the
/// reader graph without a lent table (see [`super::view_graph`]). A value such
/// a frame vouches for holds only within that evaluation.
pub fn open_frame_viewed() -> Option<OpenFrame> {
    open_frame().or_else(|| with_viewed(|evaluation, _| push_frame(evaluation, 0)))
}

fn push_frame(evaluation: &mut Evaluation, absorbs: u64) -> OpenFrame {
    let stamp = next_stamp();
    let depth = evaluation.frames.len() as u32;
    evaluation.frames.push(Frame {
        start: u32::try_from(evaluation.log.len()).expect("fewer than 2^32 reads"),
        stamp,
        taint: None,
        gates: 0,
        realm: None,
        absorbs,
        unkeyed: u32::try_from(evaluation.unkeyed.len()).expect("fewer than 2^32 context reads"),
        has_query: false,
    });
    OpenFrame(depth)
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
        let unkeyed = closed
            .taint
            .is_none()
            .then(|| unkeyed_since(&evaluation.unkeyed, closed.unkeyed))
            .flatten();
        if evaluation.frames.is_empty() {
            evaluation.unkeyed.clear();
        }
        if let Some(parent) = evaluation.frames.last_mut() {
            let parent_depth = depth - 1;
            if let Some(reach) = closed.taint
                && reach <= parent_depth
            {
                parent.taint = Some(parent.taint.map_or(reach, |taint| taint.min(reach)));
            }
            parent.gates |= closed.gates;
            parent.has_query |= closed.has_query;
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
        let end = u32::try_from(evaluation.log.len()).expect("fewer than 2^32 reads");
        evaluation.log_handle.cover(closed.start, end);
        Some(Provenance(Arc::new(Computed {
            log: Arc::clone(&evaluation.log_handle),
            start: closed.start,
            end,
            since: closed.stamp,
            gates: closed.gates,
            view: view & closed.gates,
            realm: closed.realm.map(|(realm, _)| realm),
            checked: AtomicU64::new(0),
            replayed_into: AtomicU64::new(0),
            unkeyed,
            has_query: closed.has_query,
            viewed: !super::transport::active() && KEYS.with(|keys| keys.borrow().is_none()),
        })))
    })
}

/// The distinct references `log` holds from `start` on.
fn unkeyed_since(log: &[u64], start: u32) -> Option<Box<[u64]>> {
    let read = log.get(start as usize..).filter(|read| !read.is_empty())?;
    let mut read = read.to_vec();
    read.sort_unstable();
    read.dedup();
    Some(read.into())
}

/// Notes that the open frames read the reference `key` as walking from no
/// keyed origin: their values hold only while it does not.
pub fn consult_unkeyed(key: u64) {
    CURRENT.with(|current| {
        if let Some(evaluation) = current.borrow_mut().as_mut() {
            note_unkeyed(evaluation, key);
        }
    });
}

fn note_unkeyed(evaluation: &mut Evaluation, key: u64) {
    if let Some(frame) = evaluation.frames.last()
        && !evaluation.unkeyed[frame.unkeyed as usize..].contains(&key)
    {
        evaluation.unkeyed.push(key);
    }
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

/// Whether a value computed as `provenance` records still holds under `view`,
/// the references `keyed` that walk from a keyed origin now, and the
/// inherited `realm` (with the frame index its scope began at); if so records
/// its reads and consulted context into the open frames.
#[track_caller]
pub fn reuse(
    provenance: &Provenance,
    view: Gates,
    keyed: &[u64],
    realm: Option<(GmodRealm, u32)>,
) -> Reuse {
    let caller = std::panic::Location::caller();
    let inherited = realm.map(|(realm, _)| realm);
    if view & provenance.0.gates != provenance.0.view
        || provenance
            .0
            .unkeyed
            .as_ref()
            .is_some_and(|unkeyed| unkeyed.iter().any(|key| keyed.contains(key)))
        || provenance
            .0
            .realm
            .is_some_and(|consulted| consulted != inherited)
    {
        count(|stats| stats.other_context += 1);
        return Reuse::Elsewhere;
    }
    if provenance.0.viewed {
        return reuse_in_evaluation(provenance, realm);
    }
    reuse_worker(provenance, view, keyed, realm, caller)
        .or_else(|| {
            with_evaluation(|evaluation, table| {
                let range = provenance.0.start as usize..provenance.0.end as usize;
                if Arc::ptr_eq(&provenance.0.log, &evaluation.log_handle) {
                    // Computed by this evaluation: its reads are in the log already,
                    // within the innermost frame if it opened before them.
                    if provenance.0.since <= super::last_write() {
                        match provenance.freshness(table, &evaluation.log[range.clone()]) {
                            Freshness::Unwritten => {}
                            Freshness::Recomputing => return Reuse::Elsewhere,
                            Freshness::Written => {
                                count(|stats| stats.stale += 1);
                                return Reuse::Stale;
                            }
                        }
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
                        replay_unkeyed(evaluation, provenance);
                    }
                    count(|stats| {
                        stats.reused_here += 1;
                        stats.sites.entry(caller).or_default()[2] += 1;
                    });
                } else {
                    let Some(reads) = provenance.0.log.reads(provenance.0.start, provenance.0.end)
                    else {
                        return Reuse::Elsewhere;
                    };
                    match provenance.freshness(table, reads) {
                        Freshness::Unwritten => {}
                        Freshness::Recomputing => return Reuse::Elsewhere,
                        Freshness::Written => {
                            count(|stats| stats.stale += 1);
                            return Reuse::Stale;
                        }
                    }
                    if check_cache()
                        && evaluation
                            .checks
                            .insert(check_identity(provenance, table, view, keyed, realm))
                    {
                        REUSED_ACROSS.with(|flag| flag.set(true));
                    }
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
                        replay_unkeyed(evaluation, provenance);
                    }
                }
                record_context(evaluation, provenance, realm);
                Reuse::Held
            })
        })
        .or_else(|| reuse_viewed(provenance, view, keyed, realm, caller))
        .unwrap_or(Reuse::Elsewhere)
}

/// Records into the innermost open frame the context `provenance` consulted,
/// as reused under the inherited `realm`.
fn record_context(
    evaluation: &mut Evaluation,
    provenance: &Provenance,
    realm: Option<(GmodRealm, u32)>,
) {
    let depth = evaluation.frames.len() as u32;
    if let Some(frame) = evaluation.frames.last_mut() {
        frame.gates |= provenance.0.gates;
        frame.has_query |= provenance.0.has_query;
        let base = realm.map_or(0, |(_, base)| base);
        if provenance.0.realm.is_some() && base < depth {
            frame.realm = Some((realm.map(|(realm, _)| realm), base));
        }
    }
}

/// [`reuse`] of a value computed by an evaluation viewing the reader graph:
/// it holds within that evaluation, whose reads already hold its own, and
/// nowhere else.
fn reuse_in_evaluation(provenance: &Provenance, realm: Option<(GmodRealm, u32)>) -> Reuse {
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        match current.as_mut() {
            Some(evaluation) if Arc::ptr_eq(&provenance.0.log, &evaluation.log_handle) => {
                replay_unkeyed(evaluation, provenance);
                record_context(evaluation, provenance, realm);
                count(|stats| stats.reused_here += 1);
                Reuse::Held
            }
            _ => Reuse::Stale,
        }
    })
}

fn reuse_worker(
    provenance: &Provenance,
    view: Gates,
    keyed: &[u64],
    realm: Option<(GmodRealm, u32)>,
    caller: &'static std::panic::Location<'static>,
) -> Option<Reuse> {
    super::transport::with_keys(|worker| {
        CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            let evaluation = current.as_mut()?;
            Some(reuse_on_worker(
                worker, evaluation, provenance, view, keyed, realm, caller,
            ))
        })
    })
    .flatten()
}

/// [`reuse_worker`] once the task's recorder and evaluation are in hand.
fn reuse_on_worker(
    worker: &mut super::transport::WorkerContext,
    evaluation: &mut Evaluation,
    provenance: &Provenance,
    view: Gates,
    keyed: &[u64],
    realm: Option<(GmodRealm, u32)>,
    caller: &'static std::panic::Location<'static>,
) -> Reuse {
    let range = provenance.0.start as usize..provenance.0.end as usize;
    if Arc::ptr_eq(&provenance.0.log, &evaluation.log_handle) {
        if provenance.0.has_query && !worker.local_queries_hold(&evaluation.log[range.clone()]) {
            return Reuse::Elsewhere;
        }
        match worker.local_freshness(&evaluation.log[range.clone()], provenance.0.since) {
            Freshness::Unwritten => {}
            Freshness::Recomputing => return Reuse::Elsewhere,
            Freshness::Written => {
                count(|stats| stats.stale += 1);
                return Reuse::Stale;
            }
        }
        if provenance.0.since < evaluation.frame_stamp()
            && worker.replays_into(provenance, evaluation.frame_stamp(), range.len())
        {
            for at in range {
                let local = evaluation.log[at];
                worker.log(evaluation, local);
                if provenance.0.has_query {
                    worker.observe_ready(local);
                }
            }
            replay_unkeyed(evaluation, provenance);
        }
        count(|stats| {
            stats.reused_here += 1;
            stats.sites.entry(caller).or_default()[2] += 1;
        });
    } else if let Some(log) = worker.task_log(Arc::as_ptr(&provenance.0.log) as usize) {
        let Some(reads) = log.get(range) else {
            return Reuse::Elsewhere;
        };
        let reads = reads.to_vec();
        if provenance.0.has_query && !worker.local_queries_hold(&reads) {
            return Reuse::Elsewhere;
        }
        match worker.local_freshness(&reads, provenance.0.since) {
            Freshness::Unwritten => {}
            Freshness::Recomputing => return Reuse::Elsewhere,
            Freshness::Written => {
                count(|stats| stats.stale += 1);
                return Reuse::Stale;
            }
        }
        if worker.replays_into(provenance, evaluation.frame_stamp(), reads.len()) {
            for local in reads {
                worker.log(evaluation, local);
                if provenance.0.has_query {
                    worker.observe_ready(local);
                }
            }
            replay_unkeyed(evaluation, provenance);
        }
        count(|stats| {
            stats.reused += 1;
            stats.sites.entry(caller).or_default()[3] += 1;
        });
    } else {
        let Some(reads) = provenance.0.log.reads(provenance.0.start, provenance.0.end) else {
            return Reuse::Elsewhere;
        };
        if provenance.0.has_query && !worker.canonical_queries_hold(reads) {
            return Reuse::Elsewhere;
        }
        match provenance
            .freshness(worker.graph().keys(), reads)
            .and(worker.snapshot_freshness(reads, provenance.0.since))
        {
            Freshness::Unwritten => {}
            Freshness::Recomputing => return Reuse::Elsewhere,
            Freshness::Written => return Reuse::Stale,
        }
        if check_cache()
            && evaluation.checks.insert(check_identity(
                provenance,
                worker.graph().keys(),
                view,
                keyed,
                realm,
            ))
        {
            REUSED_ACROSS.with(|flag| flag.set(true));
        }
        if worker.replays_into(provenance, evaluation.frame_stamp(), reads.len()) {
            for slot in reads {
                let local = worker
                    .record_known(evaluation, *slot)
                    .expect("qualified snapshot read is live");
                if provenance.0.has_query {
                    worker.observe_ready(local);
                }
            }
            replay_unkeyed(evaluation, provenance);
        }
        count(|stats| {
            stats.reused += 1;
            stats.sites.entry(caller).or_default()[3] += 1;
        });
    }
    record_context(evaluation, provenance, realm);
    Reuse::Held
}

fn replay_unkeyed(evaluation: &mut Evaluation, provenance: &Provenance) {
    for key in provenance
        .0
        .unkeyed
        .iter()
        .flat_map(|unkeyed| unkeyed.iter())
    {
        note_unkeyed(evaluation, *key);
    }
}

/// A stamp newer than every value memoised so far.
pub fn new_stamp() -> u64 {
    next_stamp()
}

/// Marks keys written before the table was lent, now that it is.
pub fn note_written(keys: impl IntoIterator<Item = super::HashedKey>) {
    let mut keys = Some(keys);
    if super::transport::with_keys(|worker| worker.note_written(keys.take().into_iter().flatten()))
        .is_some()
    {
        return;
    }
    let keys = keys.into_iter().flatten();
    let now = next_stamp();
    KEYS.with(|table| {
        if let Some(table) = table.borrow_mut().as_mut() {
            for key in keys {
                table.mark_written(&key, now);
            }
        }
    });
}

/// [`reuse`] for an evaluation that views the reader graph without a lent
/// table: a value that holds adds the slots it was computed from to the
/// evaluation's reads.
fn reuse_viewed(
    provenance: &Provenance,
    view: Gates,
    keyed: &[u64],
    realm: Option<(GmodRealm, u32)>,
    caller: &'static std::panic::Location<'static>,
) -> Option<Reuse> {
    with_viewed(|evaluation, graph| {
        let table = graph.keys();
        let Some(reads) = provenance.0.log.reads(provenance.0.start, provenance.0.end) else {
            return Reuse::Elsewhere;
        };
        match provenance.freshness(table, reads) {
            Freshness::Unwritten => {}
            Freshness::Recomputing => return Reuse::Elsewhere,
            Freshness::Written => {
                count(|stats| stats.stale += 1);
                return Reuse::Stale;
            }
        }
        if check_cache()
            && evaluation
                .checks
                .insert(check_identity(provenance, table, view, keyed, realm))
        {
            REUSED_ACROSS.with(|flag| flag.set(true));
        }
        if provenance.replays_into(evaluation.stamp) {
            evaluation.viewed.extend(reads.iter().copied());
        }
        replay_unkeyed(evaluation, provenance);
        record_context(evaluation, provenance, realm);
        count(|stats| {
            stats.reused += 1;
            stats.sites.entry(caller).or_default()[3] += 1;
        });
        Reuse::Held
    })
}

/// Marks keys an evaluation is about to compute again as written, now that
/// the table is lent; see [`KeyTable::open_window`].
pub fn note_recomputing(keys: impl IntoIterator<Item = super::HashedKey>) {
    let mut keys = Some(keys);
    if super::transport::with_keys(|worker| worker.open_windows(keys.take().into_iter().flatten()))
        .is_some()
    {
        return;
    }
    let keys = keys.into_iter().flatten();
    let now = next_stamp();
    KEYS.with(|table| {
        if let Some(table) = table.borrow_mut().as_mut() {
            for key in keys {
                table.open_window(&key, now);
            }
        }
    });
}

/// Ends [`note_recomputing`]: a key `unchanged` accepts reads as written
/// when it was before.
pub fn note_recomputed(unchanged: impl Fn(&super::HashedKey) -> bool) {
    if super::transport::with_keys(|worker| worker.close_windows(&unchanged)).is_some() {
        return;
    }
    KEYS.with(|table| {
        if let Some(table) = table.borrow_mut().as_mut() {
            table.close_windows(unchanged);
        }
    });
}

/// Taints the open frames of `evaluation`: it read a value being computed
/// again, which only this evaluation sees.
pub(super) fn taint_evaluation(evaluation: &mut Evaluation) {
    if let Some(frame) = evaluation.frames.last_mut() {
        frame.taint = Some(0);
    }
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
/// computed; clears the flag. For `GLUALS_CHECK_CACHE`.
pub fn take_reused_across() -> bool {
    REUSED_ACROSS.with(|flag| flag.replace(false))
}

/// What a check of reusing `provenance` depends on: the value, the state of
/// the key table and the context of the lookup. The same ones give the same
/// fresh answer, so each is checked once per evaluation.
fn check_identity(
    provenance: &Provenance,
    table: &KeyTable,
    view: Gates,
    keyed: &[u64],
    realm: Option<(GmodRealm, u32)>,
) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one((
        Arc::as_ptr(&provenance.0),
        table.last_written(),
        view,
        keyed,
        realm.map(|(realm, _)| realm),
    ))
}

/// Under `GLUALS_CHECK_CACHE`, values reused across evaluations are recomputed
/// fresh and compared.
pub fn check_cache() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_CACHE").is_some())
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
    super::transport::report_worker_replays();
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
    use crate::db_index::derivation::{self, DerivationId, DerivationKind, FactKey, HashedKey};

    #[test]
    fn parallel_transport_suspend_defers_freezing_and_restores_nested_parent() {
        derivation::lend_keys(KeyTable::default());
        let parent = DerivationId::workspace(DerivationKind::GmodLoad);
        derivation::begin(parent);
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(1)));
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let frame = open_frame().unwrap();
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(2)));
        let proof = close_frame(frame, 0).unwrap();
        let suspended = derivation::suspend();
        assert!(!proof.0.log.is_frozen());
        assert_eq!(derivation::current(), Some(parent));
        derivation::end();
        derivation::resume(suspended);
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(3)));
        derivation::end();
        assert_eq!(
            proof.0.log.reads(proof.0.start, proof.0.end).unwrap().len(),
            1
        );
        assert_eq!(proof.0.log.end.load(Ordering::Relaxed), 2);
        assert_eq!(proof.0.end, 1);
        derivation::take_keys();
    }

    #[test]
    fn parallel_transport_suspend_preserves_open_frame_context_in_place() {
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        let frame = open_frame().unwrap();
        consult_gates(1);
        consult_unkeyed(7);
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(1)));
        let mark = scope_mark();
        let suspended = derivation::suspend();
        assert_eq!(suspended.frames.len(), 1);
        derivation::resume(suspended);
        assert!(scope_open(mark));
        let proof = close_frame(frame, 1).unwrap();
        assert!(!proof.0.log.is_frozen());
        derivation::end();
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        assert_eq!(reuse(&proof, 1, &[], None), Reuse::Held);
        assert_eq!(reuse(&proof, 1, &[7], None), Reuse::Elsewhere);
        derivation::end();
        derivation::take_keys();
    }

    #[test]
    fn stored_fact_handle_leaves_unlent_reads_to_the_identity_recorder() {
        let handle = derivation::FactRead::default();
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        assert!(!handle.note(|| panic!("the legacy path owns this read"), |_| false));
        let (_, reads, _) = derivation::end().into_parts(|_| 1);
        assert!(reads.is_empty());
    }

    #[test]
    fn stored_fact_handle_records_new_evaluations_without_rebuilding_the_key() {
        let handle = derivation::FactRead::default();
        let key = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        assert!(handle.note(key, |fact| fact == &key()));
        let first = derivation::end().into_recorded().1;
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        assert!(handle.note(
            || panic!("a cached handle must not build a key"),
            |fact| fact == &key()
        ));
        let second = derivation::end().into_recorded().1;
        derivation::take_keys();
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
    }

    #[test]
    fn stored_fact_handle_records_nested_frames_and_detects_writes() {
        let handle = derivation::FactRead::default();
        let key = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        assert!(handle.note(key, |fact| fact == &key()));
        let frame = open_frame().expect("a table is lent");
        assert!(handle.note(
            || panic!("a nested read must use the handle"),
            |fact| fact == &key()
        ));
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        derivation::end();
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        derivation::note_write(key);
        let result = reuse(&value, 0, &[], None);
        derivation::end();
        derivation::take_keys();
        assert_eq!(result, Reuse::Stale);
    }

    #[test]
    fn stored_fact_handle_records_diagnostic_reads_in_a_viewed_graph() {
        let handle = derivation::FactRead::default();
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        assert!(handle.note(
            || FactKey::GmodRealm(FileId::new(1)),
            |fact| matches!(fact, FactKey::GmodRealm(file) if *file == FileId::new(1))
        ));
        derivation::end();
        let table = derivation::take_keys();
        let mut graph = super::super::ReaderGraph::default();
        graph.restore_keys(table);
        let graph = Arc::new(graph);
        derivation::begin(DerivationId::diagnostics(FileId::new(1)));
        let viewed = derivation::view_graph(Arc::clone(&graph));
        assert!(handle.note(
            || panic!("viewed reads must use the cached handle"),
            |fact| matches!(fact, FactKey::GmodRealm(file) if *file == FileId::new(1))
        ));
        drop(viewed);
        let reads = derivation::end().into_viewed_reads(&graph, |_| 1);
        assert_eq!(reads.keys(&graph).count(), 1);
    }

    /// A key read before the graph is viewed and again while it is, as its
    /// slot, is one read of the viewed reads.
    #[test]
    fn a_key_read_before_and_while_viewing_the_graph_is_read_once() {
        let read = || FactKey::FilePath(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        derivation::note_read(read);
        derivation::end();
        let mut graph = super::super::ReaderGraph::default();
        graph.restore_keys(derivation::take_keys());
        let graph = Arc::new(graph);

        derivation::begin(DerivationId::diagnostics(FileId::new(1)));
        derivation::note_read(read);
        let viewed = derivation::view_graph(Arc::clone(&graph));
        derivation::note_read(read);
        drop(viewed);
        let reads = derivation::end().into_viewed_reads(&graph, |_| 1);

        assert_eq!(reads.keys(&graph).count(), 1);
    }

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
        let reused_outside = reuse(&value, 0, &[], None);
        let inner = open_frame().expect("a table is lent");
        let reused_inside = reuse(&value, 0, &[], None);
        let nested = close_frame(inner, 0).expect("the frame saw no taint");
        let _ = close_frame(outer, 0);
        derivation::end();

        derivation::begin(DerivationId::workspace(DerivationKind::NetHelperNames));
        derivation::note_write(read);
        let nested_holds = reuse(&nested, 0, &[], None);
        derivation::end();
        derivation::take_keys();

        assert_eq!(
            (reused_outside, reused_inside, nested_holds),
            (Reuse::Held, Reuse::Held, Reuse::Stale)
        );
    }

    /// Computes a value that reads `read` in its own evaluation.
    fn value_reading(read: fn() -> FactKey) -> Provenance {
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        let frame = open_frame().expect("a table is lent");
        derivation::note_read(read);
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        derivation::end();
        value
    }

    /// Whether `value` holds in a new evaluation.
    fn reused(value: &Provenance) -> Reuse {
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let _frame = open_frame().expect("a table is lent");
        let reuse = reuse(value, 0, &[], None);
        derivation::end();
        reuse
    }

    /// Writes `key` in an evaluation of its own.
    fn write(key: fn() -> FactKey) {
        derivation::begin(DerivationId::workspace(DerivationKind::NetHelperNames));
        derivation::note_write(key);
        derivation::end();
    }

    /// A frozen log keeps only the reads its memos cover, and a memo reads
    /// them back where it read them: a write of a read outside its frame
    /// leaves it held, one of a read inside makes it stale.
    #[test]
    fn a_frozen_log_keeps_the_reads_its_memos_cover() {
        let before = || FactKey::GmodRealm(FileId::new(1));
        let inside = || FactKey::GmodRealm(FileId::new(3));
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        derivation::note_read(before);
        let frame = open_frame().expect("a table is lent");
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(2)));
        derivation::note_read(inside);
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(4)));
        derivation::end();
        let log = &value.0.log;
        let window = (
            log.start.load(Ordering::Relaxed),
            log.end.load(Ordering::Relaxed),
            log.window.get().map(|window| window.len()),
        );
        write(before);
        let after_outside = reused(&value);
        write(inside);
        let after_inside = reused(&value);
        derivation::take_keys();

        assert_eq!(
            (
                value.0.start,
                value.0.end,
                window,
                after_outside,
                after_inside
            ),
            (1, 3, (1, 4, Some(2)), Reuse::Held, Reuse::Stale)
        );
    }

    /// A value that read a fact an evaluation computes again waits while it
    /// runs, holds again if the fact comes back as it was, and is stale if it
    /// changed.
    #[test]
    fn a_value_that_read_a_recomputed_fact_holds_again_only_if_it_came_back_unchanged() {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        let value = value_reading(read);
        note_recomputing([HashedKey::new(read())]);
        let while_recomputing = reused(&value);
        note_recomputed(|_| true);
        let after_unchanged = reused(&value);
        note_recomputing([HashedKey::new(read())]);
        note_recomputed(|_| false);
        let after_changed = reused(&value);
        derivation::take_keys();

        assert_eq!(
            (while_recomputing, after_unchanged, after_changed),
            (Reuse::Elsewhere, Reuse::Held, Reuse::Stale)
        );
    }

    /// A value computed from a fact while an evaluation computes it again saw
    /// what only that evaluation sees, so it is not shared.
    #[test]
    fn a_value_computed_from_a_fact_being_recomputed_is_not_shared() {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        let _ = value_reading(read);
        note_recomputing([HashedKey::new(read())]);
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let frame = open_frame().expect("a table is lent");
        derivation::note_read(read);
        let shared = close_frame(frame, 0).is_some();
        derivation::end();
        note_recomputed(|_| true);
        derivation::take_keys();

        assert!(!shared);
    }

    /// Holds `key` in the lent table as a cleared loop does.
    fn hold(key: FactKey) {
        KEYS.with(|keys| {
            keys.borrow_mut()
                .as_mut()
                .expect("a table is lent")
                .hold_window(&HashedKey::new(key), new_stamp());
        });
    }

    /// Releases every held key, `unchanged` deciding which came back.
    fn release(unchanged: bool) {
        KEYS.with(|keys| {
            keys.borrow_mut()
                .as_mut()
                .expect("a table is lent")
                .release_held(|_| unchanged);
        });
    }

    /// A value that read a fact a cleared loop holds waits until the loop
    /// settles, whatever the loop's own evaluations compute again meanwhile,
    /// then holds again if the fact settled back as it was, and is stale if
    /// it changed.
    #[test]
    fn a_value_that_read_a_held_fact_holds_again_only_if_the_loop_settles_it_back() {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        let value = value_reading(read);
        hold(read());
        let while_held = reused(&value);
        note_recomputing([HashedKey::new(read())]);
        note_recomputed(|_| true);
        let after_an_evaluation_in_the_loop = reused(&value);
        release(true);
        let after_unchanged = reused(&value);
        hold(read());
        release(false);
        let after_changed = reused(&value);
        derivation::take_keys();

        assert_eq!(
            (
                while_held,
                after_an_evaluation_in_the_loop,
                after_unchanged,
                after_changed
            ),
            (
                Reuse::Elsewhere,
                Reuse::Elsewhere,
                Reuse::Held,
                Reuse::Stale
            )
        );
    }

    /// A value computed from a fact a cleared loop holds saw a value the loop
    /// may still move and come back from, so it is not shared.
    #[test]
    fn a_value_computed_from_a_held_fact_is_not_shared() {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        let _ = value_reading(read);
        hold(read());
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let frame = open_frame().expect("a table is lent");
        derivation::note_read(read);
        let shared = close_frame(frame, 0).is_some();
        derivation::end();
        release(true);
        derivation::take_keys();

        assert!(!shared);
    }

    /// A value that read a reference as unkeyed holds wherever that reference
    /// is still unkeyed, whichever others are keyed, and nowhere it is.
    #[test]
    fn a_value_that_read_a_reference_as_unkeyed_holds_while_it_stays_unkeyed() {
        const READ: u64 = 3;
        const OTHER: u64 = 5;
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        let frame = open_frame().expect("a table is lent");
        consult_unkeyed(READ);
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        let under_other = reuse(&value, 0, &[OTHER], None);
        let under_read = reuse(&value, 0, &[OTHER, READ], None);
        derivation::end();
        derivation::take_keys();

        assert_eq!((under_other, under_read), (Reuse::Held, Reuse::Elsewhere));
    }

    /// Notes a read of the realm of file 1 directly, then by its identity.
    fn note_realm_read_both_ways() {
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(1)));
        derivation::note_file_fact_read(derivation::FileFact::GmodRealm, FileId::new(1));
    }

    /// Notes a read of the realm of file 1 by its identity, then directly.
    fn note_realm_read_by_identity_first() {
        derivation::note_file_fact_read(derivation::FileFact::GmodRealm, FileId::new(1));
        derivation::note_read(|| FactKey::GmodRealm(FileId::new(1)));
    }

    /// Whether a value computed in a frame that `note` reads in, after the
    /// evaluation already read the same way, holds once the read is written.
    fn holds_after_write_of_a_read_made_before_its_frame(note: fn()) -> Reuse {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        note();
        let frame = open_frame().expect("a table is lent");
        note();
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        derivation::end();
        derivation::begin(DerivationId::workspace(DerivationKind::NetHelperNames));
        derivation::note_write(read);
        let holds = reuse(&value, 0, &[], None);
        derivation::end();
        derivation::take_keys();
        holds
    }

    /// A key read before a frame opened and read again inside it is one of
    /// the frame's reads, however it is noted.
    #[test]
    fn a_key_read_before_a_frame_opened_is_recorded_by_the_frame_reading_it_again() {
        assert_eq!(
            (
                holds_after_write_of_a_read_made_before_its_frame(note_realm_read_both_ways),
                holds_after_write_of_a_read_made_before_its_frame(
                    note_realm_read_by_identity_first
                ),
            ),
            (Reuse::Stale, Reuse::Stale)
        );
    }

    /// Whether a frame that `note` reads in, after the evaluation already read
    /// the same way, is shared while the read is being computed again.
    fn shared_while_a_read_made_before_its_frame_is_recomputed(note: fn()) -> bool {
        let read = || FactKey::GmodRealm(FileId::new(1));
        derivation::lend_keys(KeyTable::default());
        let _ = value_reading(read);
        note_recomputing([HashedKey::new(read())]);
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        note();
        let frame = open_frame().expect("a table is lent");
        note();
        let shared = close_frame(frame, 0).is_some();
        derivation::end();
        note_recomputed(|_| true);
        derivation::take_keys();
        shared
    }

    /// A key being computed again taints every frame that reads it, also one
    /// that opened after the evaluation first read it.
    #[test]
    fn a_key_being_recomputed_taints_a_frame_that_reads_it_again() {
        assert_eq!(
            (
                shared_while_a_read_made_before_its_frame_is_recomputed(note_realm_read_both_ways),
                shared_while_a_read_made_before_its_frame_is_recomputed(
                    note_realm_read_by_identity_first
                ),
            ),
            (false, false)
        );
    }

    fn realm() -> FactKey {
        FactKey::GmodRealm(FileId::new(1))
    }

    fn member_slot() -> FactKey {
        FactKey::MemberSlot(
            crate::LuaMemberOwner::GlobalPath(crate::GlobalId::new("resume_test")),
            crate::LuaMemberKey::Name("field".into()),
        )
    }

    /// Whether a value computed in a frame opened once the evaluation resumed
    /// with a table, reading as `note` reads `read` as it did before the table
    /// was lent, holds once `read` is written.
    fn holds_after_write_of_a_read_made_before_resuming(
        note: fn(),
        read: fn() -> FactKey,
    ) -> Reuse {
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        note();
        let evaluation = derivation::end();
        derivation::lend_keys(KeyTable::default());
        derivation::resume(evaluation);
        let frame = open_frame().expect("a table is lent");
        note();
        let value = close_frame(frame, 0).expect("the frame saw no taint");
        derivation::note_write(read);
        let holds = reuse(&value, 0, &[], None);
        derivation::end();
        derivation::take_keys();
        holds
    }

    /// A read noted while no table was lent is logged once the evaluation
    /// resumes with one, however it is noted, so a frame that reads it again
    /// depends on it.
    #[test]
    fn a_key_read_before_the_table_was_lent_is_recorded_by_a_frame_after_resuming() {
        assert_eq!(
            (
                holds_after_write_of_a_read_made_before_resuming(
                    || derivation::note_read(realm),
                    realm
                ),
                holds_after_write_of_a_read_made_before_resuming(
                    || {
                        derivation::note_file_fact_read(
                            derivation::FileFact::GmodRealm,
                            FileId::new(1),
                        );
                    },
                    realm
                ),
                holds_after_write_of_a_read_made_before_resuming(
                    || {
                        let FactKey::MemberSlot(owner, key) = member_slot() else {
                            unreachable!("a member slot");
                        };
                        derivation::note_member_slot_read(&owner, &key);
                    },
                    member_slot
                ),
            ),
            (Reuse::Stale, Reuse::Stale, Reuse::Stale)
        );
    }

    /// A write of a key the evaluation read only through a reused value
    /// advances the write generation memoised inference checks; a write of a
    /// key it never read does not.
    #[test]
    fn a_write_of_a_key_read_through_a_reused_value_advances_the_write_generation() {
        let read = || FactKey::GmodRealm(FileId::new(1));
        let unread = || FactKey::GmodRealm(FileId::new(2));
        derivation::lend_keys(KeyTable::default());
        let value = value_reading(read);
        let _ = value_reading(unread);
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let reused = reuse(&value, 0, &[], None);
        let before = derivation::write_generation();
        derivation::note_write(unread);
        let after_unread = derivation::write_generation();
        derivation::note_write(read);
        let after_read = derivation::write_generation();
        derivation::end();
        derivation::take_keys();

        assert_eq!(
            (reused, after_unread - before, after_read - after_unread),
            (Reuse::Held, 0, 1)
        );
    }

    /// Reads noted by identity stay apart for distinct keys: two table
    /// literals that start together are two reads, as are two global paths,
    /// while one path read twice is one.
    #[test]
    fn reads_noted_by_identity_keep_distinct_keys_apart() {
        use crate::{FileId, GlobalId, InFiled};
        let file = FileId::new(3);
        let site = |end: u32| InFiled::new(file, rowan::TextRange::new(4.into(), end.into()));
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        derivation::note_definition_site_read(&site(9));
        derivation::note_definition_site_read(&site(12));
        derivation::note_global_path_read(&GlobalId::new("identity_test.a"));
        derivation::note_global_path_read(&GlobalId::new("identity_test.b"));
        derivation::note_global_path_read(&GlobalId::new("identity_test.a"));
        let (_, reads, _) = derivation::end().into_parts(|_| 0);

        assert_eq!(reads.len(), 4);
    }

    /// Reads noted by owner identity and name stay apart for distinct keys:
    /// one owner's two names and two owners' one name are two reads each,
    /// while a name read twice is one.
    #[test]
    fn reads_noted_by_owner_and_name_keep_distinct_keys_apart() {
        use crate::{GlobalId, LuaMemberKey, LuaMemberOwner};
        let owner = |path: &str| LuaMemberOwner::GlobalPath(GlobalId::new(path));
        let name = |name: &str| LuaMemberKey::Name(name.into());
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        derivation::note_member_slot_read(&owner("named_test.a"), &name("x"));
        derivation::note_member_slot_read(&owner("named_test.a"), &name("y"));
        derivation::note_member_slot_read(&owner("named_test.b"), &name("x"));
        derivation::note_member_slot_read(&owner("named_test.a"), &name("x"));
        let (_, reads, _) = derivation::end().into_parts(|_| 0);

        assert_eq!(reads.len(), 3);
    }
}
