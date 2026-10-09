use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

use super::super::graph::Freshness;
use super::super::{
    self as derivation, DerivationId, EndedEvaluation, Evaluation, FactKey, Fingerprint, HashedKey,
    KeyIdx, KeyTable, Provenance, ReaderGraph,
};
use super::WorkerKeys;

struct SnapshotData {
    graph: Arc<ReaderGraph>,
    started: u64,
    queries: FxHashMap<HashedKey, Fingerprint>,
}

pub(crate) struct RecordingSnapshot(Arc<SnapshotData>);

impl RecordingSnapshot {
    pub(crate) fn new(
        graph: Arc<ReaderGraph>,
        started: u64,
        queries: FxHashMap<HashedKey, Fingerprint>,
    ) -> Self {
        assert_eq!(graph.keys().quarantine_started(), Some(started));
        Self(Arc::new(SnapshotData {
            graph,
            started,
            queries,
        }))
    }
}

#[derive(Default)]
struct ReplayCounts {
    checks: u64,
    already: u64,
    performed: u64,
    avoided_reads: u64,
    performed_reads: u64,
}

static REPLAY_TOTALS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

impl ReplayCounts {
    fn publish(&self) {
        for (total, count) in REPLAY_TOTALS.iter().zip([
            self.checks,
            self.already,
            self.performed,
            self.avoided_reads,
            self.performed_reads,
        ]) {
            total.fetch_add(count, Ordering::Relaxed);
        }
    }
}

pub(in super::super) fn report_worker_replays() {
    let [checks, already, performed, avoided_reads, performed_reads] = REPLAY_TOTALS
        .each_ref()
        .map(|total| total.swap(0, Ordering::Relaxed));
    eprintln!(
        "[worker-replay] checks={checks} already={already} performed={performed} avoided_reads={avoided_reads} performed_reads={performed_reads}"
    );
}

#[derive(Clone, Copy)]
struct QueryWitness {
    seen: Fingerprint,
    mixed: bool,
}

pub(in super::super) struct WorkerContext {
    snapshot: Arc<SnapshotData>,
    keys: WorkerKeys,
    queries: FxHashMap<u32, QueryWitness>,
    replayed: FxHashMap<usize, (Provenance, u64)>,
    replay_counts: ReplayCounts,
    requires_canonical: Arc<AtomicBool>,
    /// Under a [`TaskRecorder`], the evaluations the task ended.
    task: Option<TaskLogs>,
}

/// The evaluations a task ended, each with whether it read a query the
/// snapshot had no answer for, and each found by its log's handle.
#[derive(Default)]
struct TaskLogs {
    evaluations: Vec<(EndedEvaluation, bool)>,
    by_log: FxHashMap<usize, usize>,
}

impl WorkerContext {
    pub(in super::super) fn replays_into(
        &mut self,
        provenance: &Provenance,
        frame: u64,
        reads: usize,
    ) -> bool {
        let replayed = self
            .replayed
            .entry(provenance.identity())
            .or_insert_with(|| (provenance.clone(), 0));
        let performs = replayed.1 != frame;
        replayed.1 = frame;
        if cfg!(test) || crate::profile::profiling() {
            self.replay_counts.checks += 1;
            if performs {
                self.replay_counts.performed += 1;
                self.replay_counts.performed_reads += reads as u64;
            } else {
                self.replay_counts.already += 1;
                self.replay_counts.avoided_reads += reads as u64;
            }
        }
        performs
    }

    pub(in super::super) fn graph(&self) -> &ReaderGraph {
        &self.snapshot.graph
    }

    pub(in super::super) fn record(&mut self, evaluation: &mut Evaluation, key: HashedKey) -> u32 {
        self.keys.record(evaluation, key, &self.snapshot.graph)
    }

    pub(in super::super) fn record_known(
        &mut self,
        evaluation: &mut Evaluation,
        slot: KeyIdx,
    ) -> Option<u32> {
        self.keys
            .record_known(evaluation, slot, &self.snapshot.graph)
    }

    pub(in super::super) fn log(&mut self, evaluation: &mut Evaluation, local: u32) {
        self.keys.log(evaluation, local, &self.snapshot.graph);
    }

    pub(in super::super) fn key(&self, local: u32) -> &HashedKey {
        self.keys.key(local, &self.snapshot.graph)
    }

    fn observe(&mut self, local: u32, seen: Fingerprint) {
        self.queries
            .entry(local)
            .and_modify(|witness| witness.mixed |= witness.seen != seen)
            .or_insert(QueryWitness { seen, mixed: false });
    }

    pub(in super::super) fn canonical_queries_hold(&self, reads: &[KeyIdx]) -> bool {
        reads.iter().all(|slot| {
            let Some(key) = self.graph().keys().live_key(*slot) else {
                return false;
            };
            !query_key(key.key())
                || (!derivation::mapping_back() && self.snapshot.queries.contains_key(key))
        })
    }

    pub(in super::super) fn local_queries_hold(&self, reads: &[u32]) -> bool {
        reads.iter().all(|local| {
            let key = self.key(*local);
            !query_key(key.key())
                || (!derivation::mapping_back()
                    && self.queries.get(local).is_some_and(|witness| {
                        !witness.mixed && self.snapshot.queries.get(key) == Some(&witness.seen)
                    }))
        })
    }

    /// Marks `key` written at `now` by an evaluation that began at `since`;
    /// whether it logged the key.
    pub(in super::super) fn note_write(&mut self, key: &HashedKey, since: u64, now: u64) -> bool {
        self.keys
            .mark_written_logged(key, since, now, &self.snapshot.graph)
    }

    pub(in super::super) fn note_written(&mut self, keys: impl IntoIterator<Item = HashedKey>) {
        let now = derivation::next_stamp();
        for key in keys {
            self.keys.mark_written(&key, now, &self.snapshot.graph);
        }
    }

    pub(in super::super) fn open_windows(&mut self, keys: impl IntoIterator<Item = HashedKey>) {
        let now = derivation::next_stamp();
        for key in keys {
            self.keys.open_window(&key, now, &self.snapshot.graph);
        }
    }

    pub(in super::super) fn close_windows(&mut self, unchanged: impl Fn(&HashedKey) -> bool) {
        self.keys.close_windows(&self.snapshot.graph, unchanged);
    }

    /// Whether `key` is being computed again, here or in the snapshot.
    pub(in super::super) fn windowed(&self, key: &HashedKey) -> bool {
        self.keys.windowed(key, &self.snapshot.graph)
            || self
                .graph()
                .find_slot(key)
                .is_some_and(|slot| self.graph().keys().windowed(slot))
    }

    /// Whether any of the entries `locals` was written here after `since`.
    pub(in super::super) fn local_freshness(&self, locals: &[u32], since: u64) -> Freshness {
        self.keys.freshness(locals, since)
    }

    /// Whether any of the snapshot's `slots` was written here after `since`.
    pub(in super::super) fn snapshot_freshness(&self, slots: &[KeyIdx], since: u64) -> Freshness {
        self.keys.snapshot_freshness(slots, since)
    }

    /// The log, in entries, of the ended evaluation of this task `log` is the
    /// handle of.
    pub(in super::super) fn task_log(&self, log: usize) -> Option<&[u32]> {
        let task = self.task.as_ref()?;
        let at = *task.by_log.get(&log)?;
        Some(&task.evaluations[at].0.log)
    }

    pub(in super::super) fn observe_ready(&mut self, local: u32) {
        if self.keys.is_query(local) {
            let key = self.key(local);
            let seen = self.snapshot.queries[key];
            self.observe(local, seen);
        }
    }
}

thread_local! {
    static WORKER: RefCell<Option<WorkerContext>> = const { RefCell::new(None) };
    static QUERY_VIEW: RefCell<Option<QueryView>> = const { RefCell::new(None) };
}

struct QueryView {
    snapshot: Arc<SnapshotData>,
    requires_canonical: Arc<AtomicBool>,
}

struct QueryScope(Option<QueryView>);

static RECORDING_SCOPES: AtomicUsize = AtomicUsize::new(0);

impl Drop for QueryScope {
    fn drop(&mut self) {
        QUERY_VIEW.with(|view| *view.borrow_mut() = self.0.take());
        RECORDING_SCOPES.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn worker_query_readiness(key: &FactKey) -> Option<Option<Fingerprint>> {
    if RECORDING_SCOPES.load(Ordering::Relaxed) == 0 {
        return None;
    }
    QUERY_VIEW.with(|view| {
        view.borrow().as_ref().map(|view| {
            if derivation::mapping_back() {
                view.requires_canonical.store(true, Ordering::Relaxed);
                None
            } else {
                view.snapshot
                    .queries
                    .get(&HashedKey::new(key.clone()))
                    .copied()
            }
        })
    })
}

pub(crate) fn require_canonical_query() {
    if RECORDING_SCOPES.load(Ordering::Relaxed) == 0 {
        return;
    }
    QUERY_VIEW.with(|view| {
        if let Some(view) = view.borrow().as_ref() {
            view.requires_canonical.store(true, Ordering::Relaxed);
        }
    });
    derivation::taint();
}

pub(crate) fn query_requires_canonical() -> bool {
    if RECORDING_SCOPES.load(Ordering::Relaxed) == 0 {
        return false;
    }
    QUERY_VIEW.with(|view| {
        view.borrow()
            .as_ref()
            .is_some_and(|view| view.requires_canonical.load(Ordering::Relaxed))
    })
}

pub(in super::super) fn with_keys<T>(f: impl FnOnce(&mut WorkerContext) -> T) -> Option<T> {
    if RECORDING_SCOPES.load(Ordering::Relaxed) == 0 {
        return None;
    }
    WORKER.with(|worker| worker.borrow_mut().as_mut().map(f))
}

pub(in super::super) fn active() -> bool {
    if RECORDING_SCOPES.load(Ordering::Relaxed) == 0 {
        return false;
    }
    WORKER.with(|worker| worker.borrow().is_some())
}

pub(crate) fn note_observed_query_read(key: FactKey, seen: Fingerprint) {
    if RECORDING_SCOPES.load(Ordering::Relaxed) == 0 {
        return;
    }
    if worker_query_readiness(&key).is_some_and(|ready| ready != Some(seen)) {
        require_canonical_query();
    }
    if derivation::UNTRACKED.with(|depth| depth.get() > 0) {
        return;
    }
    derivation::CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let Some(evaluation) = current.as_mut() else {
            return;
        };
        with_keys(|worker| {
            let local = worker.record(evaluation, HashedKey::new(key));
            worker.observe(local, seen);
        });
    });
}

#[derive(Debug, Clone, Copy)]
struct FlagInput {
    slot: KeyIdx,
    windowed: bool,
    held: bool,
}

pub(crate) struct ComputeInputs {
    reads: Vec<(u32, Fingerprint)>,
    flags: Vec<FlagInput>,
    new_flags: Vec<u32>,
    absent_base_flags: Vec<HashedKey>,
    complete: bool,
}

fn query_key(key: &FactKey) -> bool {
    matches!(
        key,
        FactKey::ReceiverAuthority(_) | FactKey::CallbackParams(_)
    )
}

impl ComputeInputs {
    fn capture(worker: &WorkerContext, fingerprint: impl Fn(&HashedKey) -> Fingerprint) -> Self {
        let mut inputs = Self {
            reads: Vec::with_capacity(worker.keys.entries.len()),
            flags: Vec::new(),
            new_flags: Vec::new(),
            absent_base_flags: Vec::new(),
            complete: true,
        };
        let mut flag_slots = Vec::new();
        let mut absent_bases = FxHashSet::default();
        for local in 0..worker.keys.entries.len() {
            let key = worker.key(local as u32);
            if query_key(key.key()) {
                if let Some(witness) = worker.queries.get(&(local as u32)) {
                    inputs.complete &= !witness.mixed;
                    inputs.reads.push((local as u32, witness.seen));
                } else {
                    inputs.complete = false;
                }
            } else {
                let seen = derivation::untracked(|| fingerprint(key));
                inputs.reads.push((local as u32, seen));
            }
            match worker.keys.entries[local] {
                super::WorkerKey::Known(slot) => flag_slots.push(slot),
                super::WorkerKey::New(_) => inputs.new_flags.push(local as u32),
            }
            let mut base = key.base();
            while let Some(key) = base {
                let key = HashedKey::new(key);
                if let Some(slot) = worker.graph().find_slot(&key) {
                    flag_slots.push(slot);
                } else {
                    absent_bases.insert(key.clone());
                }
                base = key.base();
            }
        }
        flag_slots.sort_unstable();
        flag_slots.dedup();
        for slot in flag_slots {
            let table = worker.graph().keys();
            inputs.flags.push(FlagInput {
                slot,
                windowed: table.windowed(slot),
                held: table.held(slot),
            });
        }
        inputs.absent_base_flags = absent_bases.into_iter().collect();
        inputs
    }

    fn validate(
        &self,
        keys: &WorkerKeys,
        table: &KeyTable,
        fingerprint: impl Fn(&HashedKey, Option<KeyIdx>) -> Option<Fingerprint>,
    ) -> bool {
        self.complete
            && self.flags.iter().all(|input| {
                table.live_key(input.slot).is_some()
                    && table.windowed(input.slot) == input.windowed
                    && table.held(input.slot) == input.held
            })
            && self.new_flags.iter().all(|local| {
                let super::WorkerKey::New(index) = keys.entries[*local as usize] else {
                    unreachable!()
                };
                table
                    .find(&keys.new_keys[index as usize])
                    .is_none_or(|slot| !table.taints(slot))
            })
            && self
                .absent_base_flags
                .iter()
                .all(|key| table.find(key).is_none_or(|slot| !table.taints(slot)))
            && self.reads.iter().all(|(local, seen)| {
                let (key, slot) = match keys.entries[*local as usize] {
                    super::WorkerKey::Known(slot) => (table.live_key(slot), Some(slot)),
                    super::WorkerKey::New(index) => (Some(&keys.new_keys[index as usize]), None),
                };
                key.is_some_and(|key| fingerprint(key, slot) == Some(*seen))
            })
    }
}

pub(crate) struct WorkerEvaluation {
    evaluation: Evaluation,
    keys: WorkerKeys,
    inputs: ComputeInputs,
    started: u64,
}

impl WorkerEvaluation {
    /// Whether the computation's reads still hold. `fingerprint` takes a read
    /// key, with its slot when the snapshot had one.
    pub(crate) fn validate(
        &self,
        graph: &ReaderGraph,
        fingerprint: impl Fn(&HashedKey, Option<KeyIdx>) -> Option<Fingerprint>,
    ) -> bool {
        self.validate_table(graph.keys(), fingerprint)
    }

    pub(crate) fn read_keys<'a>(
        &'a self,
        graph: &'a ReaderGraph,
    ) -> impl Iterator<Item = &'a HashedKey> {
        self.keys
            .entries
            .iter()
            .filter_map(move |entry| match *entry {
                super::WorkerKey::Known(slot) => graph.keys().live_key(slot),
                super::WorkerKey::New(index) => Some(&self.keys.new_keys[index as usize]),
            })
    }

    fn validate_table(
        &self,
        table: &KeyTable,
        fingerprint: impl Fn(&HashedKey, Option<KeyIdx>) -> Option<Fingerprint>,
    ) -> bool {
        table.quarantine_started() == Some(self.started)
            && self.inputs.validate(&self.keys, table, fingerprint)
    }

    pub(crate) fn canonicalise(self, table: &mut KeyTable) -> Option<Evaluation> {
        assert_eq!(table.quarantine_started(), Some(self.started));
        self.keys.canonicalise(self.evaluation, table)
    }
}

pub(crate) struct WorkerResult<T> {
    pub(crate) evaluation: WorkerEvaluation,
    pub(crate) value: T,
}

pub(in super::super) struct SavedRecording {
    current: Option<Evaluation>,
    keys: Option<KeyTable>,
    view: Option<Arc<ReaderGraph>>,
    worker: Option<WorkerContext>,
    suspended: usize,
    untracked: u32,
    rehoming: u32,
    last_write: Option<u64>,
    thread: PhantomData<Rc<()>>,
}

impl SavedRecording {
    pub(in super::super) fn take() -> Self {
        Self {
            current: derivation::CURRENT.with(|current| current.borrow_mut().take()),
            keys: derivation::KEYS.with(|keys| keys.borrow_mut().take()),
            view: derivation::VIEW.with(|view| view.borrow_mut().take()),
            worker: WORKER.with(|worker| worker.borrow_mut().take()),
            suspended: derivation::SUSPENDED.with(|suspended| suspended.borrow().len()),
            untracked: derivation::UNTRACKED.with(|depth| depth.replace(0)),
            rehoming: derivation::REHOMING.with(|depth| depth.replace(0)),
            last_write: None,
            thread: PhantomData,
        }
    }

    fn worker() -> Self {
        let mut saved = Self::take();
        saved.last_write = Some(derivation::LAST_WRITE.with(|stamp| stamp.replace(0)));
        saved
    }
}

impl Drop for SavedRecording {
    fn drop(&mut self) {
        derivation::CURRENT.with(|current| *current.borrow_mut() = self.current.take());
        derivation::SUSPENDED.with(|suspended| suspended.borrow_mut().truncate(self.suspended));
        derivation::KEYS.with(|keys| *keys.borrow_mut() = self.keys.take());
        derivation::VIEW.with(|view| *view.borrow_mut() = self.view.take());
        WORKER.with(|worker| *worker.borrow_mut() = self.worker.take());
        derivation::UNTRACKED.with(|depth| depth.set(self.untracked));
        derivation::REHOMING.with(|depth| depth.set(self.rehoming));
        if let Some(stamp) = self.last_write {
            derivation::LAST_WRITE.with(|last| last.set(stamp));
        }
    }
}

pub(crate) fn compute_recorded<T>(
    snapshot: &RecordingSnapshot,
    id: DerivationId,
    compute: impl FnOnce() -> T,
    fingerprint: impl Fn(&HashedKey) -> Fingerprint,
) -> WorkerResult<T> {
    let saved = SavedRecording::worker();
    let requires_canonical = Arc::new(AtomicBool::new(false));
    let query_scope = QueryScope(QUERY_VIEW.with(|view| {
        view.replace(Some(QueryView {
            snapshot: Arc::clone(&snapshot.0),
            requires_canonical: Arc::clone(&requires_canonical),
        }))
    }));
    RECORDING_SCOPES.fetch_add(1, Ordering::Relaxed);
    WORKER.with(|worker| {
        *worker.borrow_mut() = Some(WorkerContext {
            snapshot: Arc::clone(&snapshot.0),
            keys: WorkerKeys::default(),
            queries: FxHashMap::default(),
            replayed: FxHashMap::default(),
            replay_counts: ReplayCounts::default(),
            requires_canonical,
            task: None,
        })
    });
    let _recording = derivation::begin(id);
    let value = compute();
    let evaluation = derivation::suspend();
    assert!(
        evaluation.frames.is_empty(),
        "worker computation with open frames"
    );
    assert!(
        evaluation.outputs.is_empty() && evaluation.moved.is_empty(),
        "worker computation wrote domain facts"
    );
    assert!(evaluation.stamp > snapshot.0.started);
    let worker = WORKER
        .with(|worker| worker.borrow_mut().take())
        .expect("compute_recorded installed the worker context");
    if crate::profile::profiling() {
        worker.replay_counts.publish();
    }
    let mut inputs = ComputeInputs::capture(&worker, fingerprint);
    inputs.complete &= !worker.requires_canonical.load(Ordering::Relaxed);
    let evaluation = WorkerEvaluation {
        evaluation,
        keys: worker.keys,
        inputs,
        started: snapshot.0.started,
    };
    drop(query_scope);
    drop(saved);
    WorkerResult { evaluation, value }
}

/// What a task's evaluation saw of one of its reads, short of fingerprinting
/// it: whether another evaluation of its group moved it since it ended, the
/// answer it took from the snapshot if it is a query output, the snapshot
/// slot of a fact the task left as the snapshot holds it, or the key of one
/// it wrote or the snapshot has no slot for.
#[derive(Debug, Clone)]
pub(crate) enum ReadState {
    Unseen,
    Query(Fingerprint),
    Snapshot(KeyIdx),
    Fact(HashedKey),
}

/// Records the evaluations of one superstep task against a snapshot of the
/// reader graph: reads and writes go to the task's own entries, a memo one
/// evaluation of the task made holds for a later one while what it read was
/// not written since, and nothing touches the snapshot until the barrier
/// files the task's evaluations with [`TaskRecording::canonicalise`].
pub(crate) struct TaskRecorder {
    query_scope: Option<QueryScope>,
    saved: Option<SavedRecording>,
    started: u64,
    /// A stamp the task took inside the snapshot's release epoch.
    stamp: u64,
}

impl TaskRecorder {
    pub(crate) fn start(snapshot: &RecordingSnapshot) -> Self {
        let saved = SavedRecording::worker();
        let requires_canonical = Arc::new(AtomicBool::new(false));
        let query_scope = QueryScope(QUERY_VIEW.with(|view| {
            view.replace(Some(QueryView {
                snapshot: Arc::clone(&snapshot.0),
                requires_canonical: Arc::clone(&requires_canonical),
            }))
        }));
        RECORDING_SCOPES.fetch_add(1, Ordering::Relaxed);
        WORKER.with(|worker| {
            *worker.borrow_mut() = Some(WorkerContext {
                snapshot: Arc::clone(&snapshot.0),
                keys: WorkerKeys::default(),
                queries: FxHashMap::default(),
                replayed: FxHashMap::default(),
                replay_counts: ReplayCounts::default(),
                requires_canonical,
                task: Some(TaskLogs::default()),
            })
        });
        Self {
            query_scope: Some(query_scope),
            saved: Some(saved),
            started: snapshot.0.started,
            stamp: derivation::new_stamp(),
        }
    }

    /// Starts recording the evaluation of `id`.
    pub(crate) fn begin(&self, id: DerivationId) {
        with_keys(|worker| worker.requires_canonical.store(false, Ordering::Relaxed));
        let _recording = derivation::begin(id);
    }

    /// Ends the evaluation [`Self::begin`] started and keeps it with the
    /// task's; returns its index among them.
    pub(crate) fn end(&self) -> usize {
        let evaluation = derivation::end_unfrozen();
        assert!(evaluation.stamp > self.started);
        with_keys(|worker| {
            let requires_canonical = worker.requires_canonical.load(Ordering::Relaxed);
            let task = worker.task.as_mut().expect("a task records into its logs");
            let at = task.evaluations.len();
            task.by_log
                .insert(Arc::as_ptr(&evaluation.log_handle) as usize, at);
            task.evaluations
                .push((evaluation.into_ended(), requires_canonical));
            at
        })
        .expect("the task recorder is installed")
    }

    /// Runs `f` on the evaluation the task ended at `at` and whether it read
    /// a query the snapshot had no answer for.
    pub(crate) fn with_ended<T>(
        &self,
        at: usize,
        f: impl FnOnce(&EndedEvaluation, bool) -> T,
    ) -> T {
        with_keys(|worker| {
            let (evaluation, requires_canonical) = &worker
                .task
                .as_ref()
                .expect("a task records into its logs")
                .evaluations[at];
            f(evaluation, *requires_canonical)
        })
        .expect("the task recorder is installed")
    }

    /// What the evaluation the task ended at `at` saw of each of its reads,
    /// in order: a read of a key another member of its group wrote after it
    /// ended at stamp `ended`, unless `reset` holds the key, is unseen.
    pub(crate) fn read_states(
        &self,
        at: usize,
        ended: Option<u64>,
        reset: &FxHashSet<HashedKey>,
    ) -> Vec<ReadState> {
        with_keys(|worker| {
            let task = worker.task.as_ref().expect("a task records into its logs");
            let snapshot = &worker.snapshot;
            task.evaluations[at]
                .0
                .read
                .iter()
                .map(|local| {
                    let key = worker.keys.key(*local, &snapshot.graph);
                    if ended.is_some_and(|ended| worker.keys.written_after(*local, ended))
                        && !reset.contains(key)
                    {
                        return ReadState::Unseen;
                    }
                    if query_key(key.key()) {
                        return ReadState::Query(
                            worker
                                .queries
                                .get(local)
                                .map(|witness| witness.seen)
                                .or_else(|| snapshot.queries.get(key).copied())
                                .unwrap_or(derivation::UNSEEN),
                        );
                    }
                    match worker.keys.snapshot_slot(*local) {
                        Some(slot) if !worker.keys.written_here_local(*local) => {
                            ReadState::Snapshot(slot)
                        }
                        _ => ReadState::Fact(key.clone()),
                    }
                })
                .collect()
        })
        .expect("the task recorder is installed")
    }

    /// Whether the task wrote `key`, or a key it is read out of.
    pub(crate) fn written_here(&self, key: &HashedKey) -> bool {
        with_keys(|worker| worker.keys.written_here(key, &worker.snapshot.graph))
            .expect("the task recorder is installed")
    }

    /// Stops recording and hands back what the task recorded.
    pub(crate) fn finish(mut self) -> TaskRecording {
        let worker = WORKER
            .with(|worker| worker.borrow_mut().take())
            .expect("the task recorder is installed");
        if crate::profile::profiling() {
            worker.replay_counts.publish();
        }
        let task = worker.task.expect("a task records into its logs");
        let written = worker
            .keys
            .written_keys(&worker.snapshot.graph)
            .cloned()
            .collect();
        self.query_scope.take();
        self.saved.take();
        TaskRecording {
            keys: worker.keys,
            evaluations: task.evaluations,
            written,
            stamp: self.stamp,
        }
    }
}

impl Drop for TaskRecorder {
    fn drop(&mut self) {
        if self.saved.is_some() {
            WORKER.with(|worker| worker.borrow_mut().take());
        }
    }
}

/// What a task recorded: its evaluations, in the order they ended, with the
/// entries their logs name, and every key it wrote.
pub(crate) struct TaskRecording {
    keys: WorkerKeys,
    evaluations: Vec<(EndedEvaluation, bool)>,
    written: Vec<HashedKey>,
    stamp: u64,
}

impl TaskRecording {
    /// Takes every key the task wrote, each once.
    pub(crate) fn take_written(&mut self) -> Vec<HashedKey> {
        std::mem::take(&mut self.written)
    }

    /// The task's evaluations with their reads as slots of `table`, the keys
    /// new to it interned in the order the task first read them, and their
    /// logs frozen for the memos that point into them; each with whether it
    /// read a query the snapshot had no answer for. `None` when a slot the
    /// task read was released since.
    #[cfg(test)]
    pub(crate) fn canonicalise(mut self, table: &mut KeyTable) -> Option<Vec<(Evaluation, bool)>> {
        let new_slots = self.intern(table);
        self.canonicalise_interned(&new_slots, table)
    }

    /// Interns the keys new to `table` in the order the task first read
    /// them: the part of [`Self::canonicalise`] that changes the table.
    pub(crate) fn intern(&mut self, table: &mut KeyTable) -> Vec<KeyIdx> {
        self.keys.intern_new(table, self.stamp)
    }

    /// [`Self::canonicalise`] once [`Self::intern`] put the new keys at
    /// `new_slots`.
    pub(crate) fn canonicalise_interned(
        self,
        new_slots: &[KeyIdx],
        table: &KeyTable,
    ) -> Option<Vec<(Evaluation, bool)>> {
        use rayon::prelude::*;
        if !self.keys.known_live(table) {
            return None;
        }
        let slots = self.keys.slots(new_slots);
        Some(
            self.evaluations
                .into_par_iter()
                .map(|(ended, requires_canonical)| {
                    let mut evaluation = ended.into_evaluation();
                    super::remap(&mut evaluation, &slots);
                    evaluation.log_handle.freeze(&evaluation.log);
                    (evaluation, requires_canonical)
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileId, LuaDeclId, LuaSignatureId, LuaTypeOwner};
    use derivation::{DerivationKind, FactRead, Reuse};
    use rowan::TextSize;

    fn id() -> DerivationId {
        DerivationId::workspace(DerivationKind::ValidGuards)
    }

    fn key(file: u32) -> HashedKey {
        HashedKey::new(FactKey::GmodRealm(FileId::new(file)))
    }

    fn snapshot(mut graph: ReaderGraph) -> (RecordingSnapshot, super::super::ReleaseQuarantine) {
        let guard = graph.quarantine_releases();
        (
            RecordingSnapshot::new(Arc::new(graph), guard.started(), FxHashMap::default()),
            guard,
        )
    }

    fn recover(snapshot: RecordingSnapshot) -> ReaderGraph {
        let data = Arc::try_unwrap(snapshot.0).ok().unwrap();
        Arc::try_unwrap(data.graph).ok().unwrap()
    }

    fn canonical_proof() -> (ReaderGraph, Provenance) {
        let mut graph = ReaderGraph::default();
        derivation::lend_keys(graph.lend_keys());
        let _recording = derivation::begin(id());
        let frame = derivation::open_frame().unwrap();
        derivation::note_read(|| key(1).key().clone());
        let proof = derivation::close_frame(frame, 0).unwrap();
        derivation::end();
        graph.restore_keys(derivation::take_keys());
        (graph, proof)
    }

    /// The memo of reading `key` in a frame.
    fn memo_of(key: &HashedKey) -> Provenance {
        let frame = derivation::open_frame().unwrap();
        derivation::note_read(|| key.key().clone());
        derivation::close_frame(frame, 0).unwrap()
    }

    fn write(key: &HashedKey) {
        derivation::note_write(|| key.key().clone());
    }

    /// Runs `f` as one evaluation of the task `recorder` records.
    fn evaluate<T>(recorder: &TaskRecorder, f: impl FnOnce() -> T) -> (T, usize) {
        recorder.begin(id());
        let value = f();
        (value, recorder.end())
    }

    #[test]
    fn a_task_reuses_the_memo_an_earlier_evaluation_of_it_made_and_files_it_canonically() {
        let mut graph = ReaderGraph::default();
        graph.intern_read(key(99));
        let (snapshot, guard) = snapshot(graph);
        let recorder = TaskRecorder::start(&snapshot);
        let (proof, _) = evaluate(&recorder, || memo_of(&key(1)));
        let (_, second) = evaluate(&recorder, || {
            let frame = derivation::open_frame().unwrap();
            assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
            derivation::close_frame(frame, 0).unwrap();
        });
        recorder.with_ended(second, |evaluation, requires_canonical| {
            assert_eq!(evaluation.log, vec![0]);
            assert!(!requires_canonical);
        });
        let mut recording = recorder.finish();
        assert!(recording.take_written().is_empty());
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        let evaluations = recording.canonicalise(&mut table).unwrap();
        let slot = table.find(&key(1)).unwrap();
        assert!(
            evaluations
                .iter()
                .all(|(evaluation, _)| evaluation.log == vec![slot])
        );
        derivation::lend_keys(table);
        let _recording = derivation::begin(id());
        assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
        assert_eq!(derivation::end().into_recorded().1, vec![slot]);
        let mut table = derivation::take_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn a_task_memo_goes_stale_when_a_later_evaluation_writes_what_it_read() {
        let (snapshot, guard) = snapshot(ReaderGraph::default());
        let recorder = TaskRecorder::start(&snapshot);
        let (read_one, _) = evaluate(&recorder, || memo_of(&key(1)));
        let (read_two, _) = evaluate(&recorder, || memo_of(&key(2)));
        evaluate(&recorder, || write(&key(1)));
        evaluate(&recorder, || {
            assert_eq!(derivation::reuse(&read_one, 0, &[], None), Reuse::Stale);
            assert_eq!(derivation::reuse(&read_two, 0, &[], None), Reuse::Held);
        });
        let mut recording = recorder.finish();
        assert_eq!(recording.take_written(), vec![key(1)]);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn a_task_write_makes_a_memo_from_before_the_task_stale_through_its_base() {
        let child = HashedKey::new(FactKey::GmodRealmAt(FileId::new(1), TextSize::new(5)));
        let mut graph = ReaderGraph::default();
        derivation::lend_keys(graph.lend_keys());
        let _recording = derivation::begin(id());
        let read_child = memo_of(&child);
        let read_other = memo_of(&key(2));
        derivation::end();
        graph.restore_keys(derivation::take_keys());
        let (snapshot, guard) = snapshot(graph);
        let recorder = TaskRecorder::start(&snapshot);
        evaluate(&recorder, || {
            assert_eq!(derivation::reuse(&read_child, 0, &[], None), Reuse::Held);
        });
        evaluate(&recorder, || write(&key(1)));
        evaluate(&recorder, || {
            assert_eq!(derivation::reuse(&read_child, 0, &[], None), Reuse::Stale);
            assert_eq!(derivation::reuse(&read_other, 0, &[], None), Reuse::Held);
        });
        drop(recorder.finish());
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn a_task_memo_holds_again_when_what_it_read_is_computed_back_unchanged() {
        for unchanged in [true, false] {
            let (snapshot, guard) = snapshot(ReaderGraph::default());
            let recorder = TaskRecorder::start(&snapshot);
            let (proof, _) = evaluate(&recorder, || memo_of(&key(1)));
            derivation::note_recomputing([key(1)]);
            evaluate(&recorder, || {
                assert!(derivation::recomputes(key(1).key().clone()));
                write(&key(1));
            });
            derivation::note_recomputed(|_| unchanged);
            evaluate(&recorder, || {
                assert!(!derivation::recomputes(key(1).key().clone()));
                let expected = if unchanged { Reuse::Held } else { Reuse::Stale };
                assert_eq!(derivation::reuse(&proof, 0, &[], None), expected);
            });
            drop(recorder.finish());
            let mut graph = recover(snapshot);
            let mut table = graph.lend_keys();
            guard.finish(&mut table);
            graph.restore_keys(table);
        }
    }

    #[test]
    fn a_task_write_moves_the_write_generation_only_when_the_evaluation_read_it() {
        let mut graph = ReaderGraph::default();
        graph.intern_read(key(3));
        let (snapshot, guard) = snapshot(graph);
        let recorder = TaskRecorder::start(&snapshot);
        evaluate(&recorder, || derivation::note_read(|| key(2).key().clone()));
        evaluate(&recorder, || {
            derivation::note_read(|| key(1).key().clone());
            let before = derivation::write_generation();
            write(&key(2));
            write(&key(3));
            write(&key(4));
            assert_eq!(derivation::write_generation(), before);
            write(&key(1));
            assert_eq!(derivation::write_generation(), before + 1);
        });
        drop(recorder.finish());
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_replay_visitation_preserves_nested_and_sibling_reads() {
        let (graph, proof) = canonical_proof();
        let (snapshot, guard) = snapshot(graph);
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let outer = derivation::open_frame().unwrap();
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                assert_eq!(derivation::reuse(&proof.clone(), 0, &[], None), Reuse::Held);
                let inner = derivation::open_frame().unwrap();
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                let inner = derivation::close_frame(inner, 0).unwrap();
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                let sibling = derivation::open_frame().unwrap();
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                let sibling = derivation::close_frame(sibling, 0).unwrap();
                let outer = derivation::close_frame(outer, 0).unwrap();
                let counts = with_keys(|worker| {
                    (
                        worker.replay_counts.checks,
                        worker.replay_counts.already,
                        worker.replay_counts.performed,
                    )
                })
                .unwrap();
                assert_eq!(counts, (7, 3, 4));
                (outer, inner, sibling)
            },
            |_| 17,
        );
        assert_eq!(result.evaluation.evaluation.log, vec![0, 0, 0]);
        assert_eq!(result.evaluation.evaluation.read, vec![0]);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        let slot = table.find(&key(1)).unwrap();
        let evaluation = result.evaluation.canonicalise(&mut table).unwrap();
        derivation::lend_keys(table);
        let _recording = derivation::resume(evaluation);
        derivation::end();
        for proof in [result.value.0, result.value.1, result.value.2] {
            let _recording = derivation::begin(id());
            assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
            assert_eq!(derivation::end().into_recorded().1, vec![slot]);
        }
        let mut table = derivation::take_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    /// Two evaluations of one epoch read a key the snapshot lacks, and the one
    /// that began later publishes first: the earlier one's memo still holds.
    #[test]
    fn parallel_transport_a_key_new_to_the_snapshot_dates_from_the_epoch() {
        let mut graph = ReaderGraph::default();
        graph.intern_read(key(99));
        let (snapshot, guard) = snapshot(graph);
        let read = || {
            let frame = derivation::open_frame().unwrap();
            derivation::note_read(|| key(1).key().clone());
            derivation::close_frame(frame, 0).unwrap()
        };
        let early = compute_recorded(&snapshot, id(), read, |_| 17);
        let late = compute_recorded(&snapshot, id(), read, |_| 17);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        let late = late.evaluation.canonicalise(&mut table).unwrap();
        let early_evaluation = early.evaluation.canonicalise(&mut table).unwrap();
        derivation::lend_keys(table);
        for evaluation in [late, early_evaluation] {
            let _recording = derivation::resume(evaluation);
            derivation::end();
        }
        derivation::note_written([key(99)]);
        let _recording = derivation::begin(id());
        assert_eq!(derivation::reuse(&early.value, 0, &[], None), Reuse::Held);
        derivation::end();
        let mut table = derivation::take_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_replay_visitation_is_independent_for_concurrent_clones() {
        let (graph, proof) = canonical_proof();
        let (snapshot, guard) = snapshot(graph);
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let compute = || {
                compute_recorded(
                    &snapshot,
                    id(),
                    || {
                        let frame = derivation::open_frame().unwrap();
                        assert_eq!(derivation::reuse(&proof.clone(), 0, &[], None), Reuse::Held);
                        barrier.wait();
                        assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                        derivation::close_frame(frame, 0).unwrap();
                        with_keys(|worker| {
                            (
                                worker.replay_counts.checks,
                                worker.replay_counts.already,
                                worker.replay_counts.performed,
                            )
                        })
                        .unwrap()
                    },
                    |_| 17,
                )
            };
            let first = scope.spawn(compute);
            let second = scope.spawn(compute);
            [first.join().unwrap(), second.join().unwrap()]
        });
        for result in results {
            assert_eq!(result.value, (2, 1, 1));
            assert_eq!(result.evaluation.evaluation.log, vec![0]);
            assert_eq!(result.evaluation.evaluation.read, vec![0]);
        }
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_hints_stay_global_and_local_proofs_remap_once() {
        use std::sync::atomic::Ordering;
        let owner = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(4), TextSize::new(8)));
        let fact = HashedKey::new(FactKey::TypeCache(owner));
        let mut graph = ReaderGraph::default();
        graph.intern_read(key(99));
        let global = graph.intern_read(fact.clone());
        let (snapshot, guard) = snapshot(graph);
        let handle = FactRead::default();
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = derivation::open_frame().unwrap();
                assert!(handle.note(|| fact.key().clone(), |key| key == fact.key()));
                assert_eq!(handle.0.load(Ordering::Relaxed), global);
                derivation::note_file_fact_read(derivation::FileFact::GmodRealm, FileId::new(1));
                let proof = derivation::close_frame(frame, 0).unwrap();
                let frame = derivation::open_frame().unwrap();
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Held);
                let replay = derivation::close_frame(frame, 0).unwrap();
                (proof, replay)
            },
            |_| 17,
        );
        assert_eq!(result.evaluation.keys.entries.len(), 2);
        assert_eq!(result.evaluation.evaluation.log, vec![0, 1, 0, 1]);
        assert_eq!(Arc::strong_count(&snapshot.0.graph), 1);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(result.evaluation.validate_table(&table, |_, _| Some(17)));
        let evaluation = result.evaluation.canonicalise(&mut table).unwrap();
        let other = table.find(&key(1)).unwrap();
        assert_eq!(evaluation.log, vec![global, other, global, other]);
        derivation::lend_keys(table);
        let _recording = derivation::resume(evaluation);
        derivation::end();
        let _recording = derivation::begin(id());
        assert_eq!(
            derivation::reuse(&result.value.0, 0, &[], None),
            Reuse::Held
        );
        assert_eq!(
            derivation::reuse(&result.value.1, 0, &[], None),
            Reuse::Held
        );
        assert_eq!(derivation::end().into_recorded().1, vec![global, other]);
        let mut table = derivation::take_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_getter_window_and_equal_value_flags_are_validated() {
        let owner = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), TextSize::new(8)));
        let fact = HashedKey::new(FactKey::TypeCache(owner.clone()));
        let mut graph = ReaderGraph::default();
        graph.intern_read(fact.clone());
        let mut table = graph.lend_keys();
        table.open_window(&fact, derivation::new_stamp());
        graph.restore_keys(table);
        let (snapshot, guard) = snapshot(graph);
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = derivation::open_frame().unwrap();
                let handle = FactRead::default();
                assert!(handle.recomputes(&owner));
                assert!(derivation::recomputes(fact.key().clone()));
                handle.note(|| fact.key().clone(), |key| key == fact.key());
                assert!(derivation::close_frame(frame, 0).is_none());
            },
            |_| 17,
        );
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(result.evaluation.validate_table(&table, |_, _| Some(17)));
        table.close_windows(|_| true);
        assert!(!result.evaluation.validate_table(&table, |_, _| Some(17)));
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_absent_base_flags_are_not_value_dependencies() {
        let (snapshot, guard) = snapshot(ReaderGraph::default());
        let child = HashedKey::new(FactKey::GmodRealmAt(FileId::new(1), TextSize::new(5)));
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                derivation::note_read(|| child.key().clone());
            },
            |key| {
                assert_eq!(key, &child);
                17
            },
        );
        assert_eq!(result.evaluation.inputs.reads.len(), 1);
        assert_eq!(result.evaluation.inputs.absent_base_flags, vec![key(1)]);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(result.evaluation.validate_table(&table, |_, _| Some(17)));
        table.intern_read(key(1), derivation::new_stamp());
        table.hold_window(&key(1), derivation::new_stamp());
        assert!(!result.evaluation.validate_table(&table, |_, _| Some(17)));
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_queries_capture_actual_winner_and_reject_missing_current() {
        let query = HashedKey::new(FactKey::CallbackParams(LuaSignatureId::new(
            FileId::new(1),
            TextSize::new(8),
        )));
        let mut graph = ReaderGraph::default();
        graph.intern_read(query.clone());
        let guard = graph.quarantine_releases();
        let snapshot = RecordingSnapshot::new(
            Arc::new(graph),
            guard.started(),
            [(query.clone(), 17)].into_iter().collect(),
        );
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = derivation::open_frame().unwrap();
                assert_eq!(worker_query_readiness(query.key()), Some(Some(17)));
                derivation::note_read(|| query.key().clone());
                note_observed_query_read(query.key().clone(), 17);
                derivation::close_frame(frame, 0).unwrap()
            },
            |_| panic!("query fingerprint must come from the returned winner"),
        );
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(result.evaluation.validate_table(&table, |_, _| Some(17)));
        assert!(!result.evaluation.validate_table(&table, |_, _| None));
        assert!(!result.evaluation.validate_table(&table, |_, _| Some(18)));
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_blocked_detached_queries_invalidate_parent_task() {
        let query = FactKey::CallbackParams(LuaSignatureId::new(FileId::new(1), TextSize::new(8)));
        let (snapshot, guard) = snapshot(ReaderGraph::default());
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = derivation::open_frame().unwrap();
                derivation::detached(id(), || {
                    assert!(!active());
                    assert_eq!(worker_query_readiness(&query), Some(None));
                    require_canonical_query();
                    assert!(query_requires_canonical());
                });
                assert!(active());
                assert!(query_requires_canonical());
                derivation::close_frame(frame, 0)
            },
            |_| 17,
        );
        assert!(!query_requires_canonical());
        assert_eq!(worker_query_readiness(&query), None);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(!result.evaluation.validate_table(&table, |_, _| Some(17)));
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_unwind_restores_parent_and_drops_snapshot_views() {
        let (snapshot, guard) = snapshot(ReaderGraph::default());
        derivation::lend_keys(KeyTable::default());
        let _recording = derivation::begin(id());
        let frame = derivation::open_frame().unwrap();
        derivation::note_read(|| key(2).key().clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compute_recorded(
                &snapshot,
                id(),
                || {
                    let _ = derivation::open_frame().unwrap();
                    derivation::note_read(|| key(3).key().clone());
                    panic!("abort worker");
                },
                |_| 17,
            )
        }));
        assert!(result.is_err());
        assert!(!active());
        assert!(!query_requires_canonical());
        assert_eq!(Arc::strong_count(&snapshot.0.graph), 1);
        derivation::note_read(|| key(4).key().clone());
        assert!(derivation::close_frame(frame, 0).is_some());
        assert_eq!(derivation::end().into_recorded().1.len(), 2);
        derivation::take_keys();
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_canonical_replay_preserves_context_and_complete_inputs() {
        let mut graph = ReaderGraph::default();
        derivation::lend_keys(graph.lend_keys());
        let _recording = derivation::begin(id());
        let frame = derivation::open_frame().unwrap();
        derivation::note_read(|| key(1).key().clone());
        derivation::consult_gates(1);
        derivation::consult_unkeyed(77);
        let proof = derivation::close_frame(frame, 1).unwrap();
        derivation::end();
        graph.restore_keys(derivation::take_keys());
        let (snapshot, guard) = snapshot(graph);
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                assert_eq!(derivation::reuse(&proof, 0, &[], None), Reuse::Elsewhere);
                assert_eq!(derivation::reuse(&proof, 1, &[77], None), Reuse::Elsewhere);
                let frame = derivation::open_frame().unwrap();
                assert_eq!(derivation::reuse(&proof, 1, &[], None), Reuse::Held);
                derivation::close_frame(frame, 1).unwrap()
            },
            |_| 17,
        );
        assert_eq!(result.evaluation.inputs.reads.len(), 1);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(result.evaluation.validate_table(&table, |_, _| Some(17)));
        assert!(!result.evaluation.validate_table(&table, |_, _| Some(18)));
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_mixed_query_winners_cannot_publish() {
        let query = HashedKey::new(FactKey::CallbackParams(LuaSignatureId::new(
            FileId::new(1),
            TextSize::new(8),
        )));
        let mut graph = ReaderGraph::default();
        let guard = graph.quarantine_releases();
        let snapshot = RecordingSnapshot::new(
            Arc::new(graph),
            guard.started(),
            [(query.clone(), 17)].into_iter().collect(),
        );
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = derivation::open_frame().unwrap();
                note_observed_query_read(query.key().clone(), 17);
                note_observed_query_read(query.key().clone(), 18);
                assert!(query_requires_canonical());
                assert!(derivation::close_frame(frame, 0).is_none());
            },
            |_| panic!("query fingerprint must be observed"),
        );
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(!result.evaluation.validate_table(&table, |_, _| Some(17)));
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_worker_named_repeats_and_missing_handles_keep_global_hints() {
        use crate::{LuaMemberKey, LuaMemberOwner};
        let (snapshot, guard) = snapshot(ReaderGraph::default());
        let owner = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), TextSize::new(8)));
        let fact = FactKey::TypeCache(owner);
        let handle = FactRead::default();
        handle.0.store(123, Ordering::Relaxed);
        let result = compute_recorded(
            &snapshot,
            id(),
            || {
                let frame = derivation::open_frame().unwrap();
                assert!(handle.note(|| fact.clone(), |key| key == &fact));
                assert_eq!(handle.0.load(Ordering::Relaxed), 123);
                let owner = LuaMemberOwner::LocalUnresolve;
                let name = LuaMemberKey::Name("field".into());
                derivation::note_member_slot_read(&owner, &name);
                derivation::note_member_slot_read(&owner, &name);
                let proof = derivation::close_frame(frame, 0).unwrap();
                let frame = derivation::open_frame().unwrap();
                derivation::note_member_slot_read(&owner, &name);
                let replay = derivation::close_frame(frame, 0).unwrap();
                (proof, replay)
            },
            |_| 17,
        );
        assert_eq!(result.evaluation.evaluation.log, vec![0, 1, 1]);
        assert_eq!(result.evaluation.evaluation.named_reads.len(), 1);
        let mut graph = recover(snapshot);
        let mut table = graph.lend_keys();
        assert!(result.evaluation.validate_table(&table, |_, _| Some(17)));
        let evaluation = result.evaluation.canonicalise(&mut table).unwrap();
        assert_eq!(
            evaluation.named_reads.iter().next().unwrap().slot,
            evaluation.log[1]
        );
        derivation::lend_keys(table);
        let _recording = derivation::resume(evaluation);
        derivation::end();
        let mut table = derivation::take_keys();
        guard.finish(&mut table);
        graph.restore_keys(table);
    }
}
