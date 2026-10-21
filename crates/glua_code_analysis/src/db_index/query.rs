//! Queries: facts computed when first read, each a pure function of what it
//! reads, memoised per key.
//!
//! A query is computed nested inside whatever reads it first, so it only reads
//! the index. Its reads are filed in the reader graph under its own
//! [`DerivationId`], which makes it a reader like any derivation: when a fact
//! it read moves, the engine marks its memo stale and computes it again, and
//! its readers run again only if its value's fingerprint moved.

use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
use rowan::TextRange;
use rustc_hash::FxHashMap;

use super::derivation::{self, DerivationId, DerivationKind, FactKey, Fingerprint, HashedKey};
use crate::compilation::{CallbackParamsQuery, ReceiverAuthorityQuery};
use crate::{DbIndex, FileId, LuaSignatureId};

/// A family of queries keyed by a function's signature.
pub(crate) trait QueryFamily {
    type Value: std::hash::Hash + std::fmt::Debug + Send + Sync + 'static;
    const KIND: DerivationKind;

    fn output(key: LuaSignatureId) -> FactKey;

    fn compute(db: &DbIndex, key: LuaSignatureId) -> Self::Value;

    /// What a query reads of itself while it computes: nothing is known yet.
    fn floor() -> Self::Value;
}

/// What the queries of one family did, for the profile and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStats {
    /// Computed when read.
    pub computed: usize,
    /// Computed again by the engine after a read of theirs moved.
    pub recomputed: usize,
    /// Of those, the ones whose value came out unchanged.
    pub cut_off: usize,
    /// Reads answered from the memo.
    pub hits: usize,
    /// Reads made while the query itself was computing.
    pub cycles: usize,
    /// Under `GLUALS_CHECK_QUERY`, memo hits a fresh computation disagreed with.
    pub mismatches: usize,
}

#[derive(Debug, Default)]
struct Counters {
    computed: AtomicUsize,
    recomputed: AtomicUsize,
    cut_off: AtomicUsize,
    hits: AtomicUsize,
    cycles: AtomicUsize,
    mismatches: AtomicUsize,
}

impl Counters {
    fn bump(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> QueryStats {
        QueryStats {
            computed: self.computed.load(Ordering::Relaxed),
            recomputed: self.recomputed.load(Ordering::Relaxed),
            cut_off: self.cut_off.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            cycles: self.cycles.load(Ordering::Relaxed),
            mismatches: self.mismatches.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct Memo<V> {
    value: Arc<V>,
    fingerprint: Fingerprint,
    /// A read of the query moved since it was computed.
    stale: bool,
}

struct QueryValue<V> {
    value: Arc<V>,
    fingerprint: Fingerprint,
}

impl<V> Clone for Memo<V> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
            fingerprint: self.fingerprint,
            stale: self.stale,
        }
    }
}

/// A query computed outside the engine's evaluations, whose reads the engine
/// files before it next queues readers.
#[derive(Debug, Clone)]
pub(crate) struct Filing {
    pub id: DerivationId,
    pub reads: Vec<(HashedKey, Fingerprint)>,
    /// The query's output, when the computation moved it.
    pub moved: Option<HashedKey>,
}

fn check_mode() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_QUERY").is_some())
}

thread_local! {
    /// The queries computing on this thread, innermost last.
    static COMPUTING: std::cell::RefCell<Vec<DerivationId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` with `id` on this thread's query stack.
fn computing<T>(id: DerivationId, f: impl FnOnce() -> T) -> T {
    COMPUTING.with(|stack| stack.borrow_mut().push(id));
    let result = f();
    COMPUTING.with(|stack| stack.borrow_mut().pop());
    result
}

fn is_computing(id: DerivationId) -> bool {
    COMPUTING.with(|stack| stack.borrow().contains(&id))
}

/// The memos of one family.
#[derive(Debug)]
pub(crate) struct QueryTable<F: QueryFamily> {
    memos: Mutex<FxHashMap<LuaSignatureId, Memo<F::Value>>>,
    pending: Mutex<Vec<Filing>>,
    /// How many filings `pending` holds, read without the lock.
    pending_count: AtomicUsize,
    counters: Counters,
}

impl<F: QueryFamily> Default for QueryTable<F> {
    fn default() -> Self {
        Self {
            memos: Mutex::default(),
            pending: Mutex::default(),
            pending_count: AtomicUsize::new(0),
            counters: Counters::default(),
        }
    }
}

impl<F: QueryFamily> Clone for QueryTable<F> {
    fn clone(&self) -> Self {
        Self {
            memos: Mutex::new(self.memos().clone()),
            pending: Mutex::new(self.pending().clone()),
            pending_count: AtomicUsize::new(self.pending_count.load(Ordering::Relaxed)),
            counters: Counters::default(),
        }
    }
}

impl<F: QueryFamily> QueryTable<F> {
    fn memos(&self) -> std::sync::MutexGuard<'_, FxHashMap<LuaSignatureId, Memo<F::Value>>> {
        self.memos
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Vec<Filing>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The derivation a query of this family runs as: the function's closure.
    pub(crate) fn id(key: LuaSignatureId) -> DerivationId {
        DerivationId::new(
            F::KIND,
            key.get_file_id(),
            LuaSyntaxId::new(
                LuaSyntaxKind::ClosureExpr.into(),
                TextRange::empty(key.get_position()),
            ),
        )
    }

    fn key(id: DerivationId) -> LuaSignatureId {
        LuaSignatureId::new(id.file_id, id.syntax_id().get_range().start())
    }

    fn fingerprint_of(value: &F::Value) -> Fingerprint {
        derivation::fingerprint_of(value)
    }

    /// The query's value, a recorded read of its output.
    pub(crate) fn get(&self, db: &DbIndex, key: LuaSignatureId) -> Arc<F::Value> {
        derivation::note_read(|| F::output(key));
        let observed = self.value_with_fingerprint(db, key);
        derivation::note_observed_query_read(F::output(key), observed.fingerprint);
        observed.value
    }

    fn value_with_fingerprint(&self, db: &DbIndex, key: LuaSignatureId) -> QueryValue<F::Value> {
        derivation::unmapped(|| self.value_unmapped(db, key))
    }

    fn value_unmapped(&self, db: &DbIndex, key: LuaSignatureId) -> QueryValue<F::Value> {
        if let Some(ready) = derivation::worker_query_readiness(&F::output(key)) {
            let memo = self.memos().get(&key).filter(|memo| !memo.stale).cloned();
            if let Some(memo) = memo.filter(|memo| ready == Some(memo.fingerprint)) {
                Counters::bump(&self.counters.hits);
                if check_mode() {
                    self.check(db, key, &memo);
                }
                return QueryValue {
                    value: memo.value,
                    fingerprint: memo.fingerprint,
                };
            }
            derivation::require_canonical_query();
            let value = Arc::new(F::floor());
            return QueryValue {
                fingerprint: Self::fingerprint_of(&value),
                value,
            };
        }
        let id = Self::id(key);
        if let Some(memo) = self.memos().get(&key).filter(|memo| !memo.stale).cloned() {
            Counters::bump(&self.counters.hits);
            if check_mode() {
                self.check(db, key, &memo);
            }
            return QueryValue {
                value: memo.value,
                fingerprint: memo.fingerprint,
            };
        }
        if is_computing(id) {
            Counters::bump(&self.counters.cycles);
            return self.memos().get(&key).map_or_else(
                || {
                    let value = Arc::new(F::floor());
                    QueryValue {
                        fingerprint: Self::fingerprint_of(&value),
                        value,
                    }
                },
                |memo| QueryValue {
                    value: Arc::clone(&memo.value),
                    fingerprint: memo.fingerprint,
                },
            );
        }
        let (value, reads) = computing(id, || {
            let _transaction = crate::semantic::detach_transaction();
            derivation::detached(id, || F::compute(db, key))
        });
        let value = Arc::new(value);
        let fingerprint = Self::fingerprint_of(&value);
        let reads = reads
            .into_iter()
            .map(|read| {
                let seen = db.fact_fingerprint(&read);
                (read, seen)
            })
            .collect();
        let mut memos = self.memos();
        if let Some(memo) = memos.get(&key).filter(|memo| !memo.stale) {
            return QueryValue {
                value: Arc::clone(&memo.value),
                fingerprint: memo.fingerprint,
            };
        }
        let moved = memos
            .insert(
                key,
                Memo {
                    value: Arc::clone(&value),
                    fingerprint,
                    stale: false,
                },
            )
            .is_some_and(|previous| previous.fingerprint != fingerprint)
            .then(|| HashedKey::new(F::output(key)));
        drop(memos);
        if moved.is_some() {
            // The evaluation that demanded it moved the output, as a write
            // would: its readers are checked once it is recorded.
            derivation::rehoming(|| derivation::note_write(|| F::output(key)));
        }
        Counters::bump(&self.counters.computed);
        self.pending().push(Filing { id, reads, moved });
        Counters::bump(&self.pending_count);
        QueryValue { value, fingerprint }
    }

    /// Recomputes a memo hit fresh and reports a disagreement.
    fn check(&self, db: &DbIndex, key: LuaSignatureId, memo: &Memo<F::Value>) {
        let id = Self::id(key);
        if is_computing(id) {
            return;
        }
        let (fresh, _) = computing(id, || {
            let _transaction = crate::semantic::detach_transaction();
            derivation::detached(id, || F::compute(db, key))
        });
        if derivation::worker_requires_canonical() {
            return;
        }
        if Self::fingerprint_of(&fresh) != memo.fingerprint {
            Counters::bump(&self.counters.mismatches);
            eprintln!(
                "[query] {:?} {key:?} mismatch: memo {:?}, fresh {fresh:?}",
                F::KIND,
                memo.value
            );
        }
    }

    /// The fingerprint of the query's output: its memo's, stale or not, as a
    /// stored fact's holds until it is written; computed only when there is
    /// no memo, and absent for a signature the index does not hold. Not a
    /// read.
    pub(crate) fn fingerprint(&self, db: &DbIndex, key: LuaSignatureId) -> Fingerprint {
        if let Some(ready) = derivation::worker_query_readiness(&F::output(key)) {
            let memo = self.memos().get(&key).filter(|memo| !memo.stale).cloned();
            if let Some(memo) = memo.filter(|memo| ready == Some(memo.fingerprint)) {
                return if derivation::mapping_back() {
                    Self::fingerprint_of(&memo.value)
                } else {
                    memo.fingerprint
                };
            }
            derivation::require_canonical_query();
            return derivation::ABSENT;
        }
        let memo = self.memos().get(&key).cloned();
        let (value, fingerprint) = match memo {
            Some(memo) => (memo.value, memo.fingerprint),
            None if db.get_signature_index().get(&key).is_none() => return derivation::ABSENT,
            None => {
                let observed = self.value_with_fingerprint(db, key);
                (observed.value, observed.fingerprint)
            }
        };
        if derivation::mapping_back() {
            return Self::fingerprint_of(&value);
        }
        fingerprint
    }

    fn ready_fingerprint(&self, key: LuaSignatureId) -> Option<Fingerprint> {
        self.memos()
            .get(&key)
            .filter(|memo| !memo.stale)
            .map(|memo| {
                if derivation::mapping_back() {
                    Self::fingerprint_of(&memo.value)
                } else {
                    memo.fingerprint
                }
            })
    }

    fn ready_outputs(&self, outputs: &mut FxHashMap<HashedKey, Fingerprint>) {
        for (key, memo) in self.memos().iter().filter(|(_, memo)| !memo.stale) {
            let fingerprint = if derivation::mapping_back() {
                Self::fingerprint_of(&memo.value)
            } else {
                memo.fingerprint
            };
            outputs.insert(HashedKey::new(F::output(*key)), fingerprint);
        }
    }

    /// Computes the query again inside the engine's evaluation of it, which
    /// records its reads, and writes its output.
    pub(crate) fn recompute(&self, db: &DbIndex, id: DerivationId) {
        let key = Self::key(id);
        if derivation::worker_query_readiness(&F::output(key)).is_some() {
            derivation::require_canonical_query();
            return;
        }
        let value = Arc::new(computing(id, || F::compute(db, key)));
        self.store(id, value);
    }

    /// Computes query `id` without storing it, for [`Self::publish`]: what
    /// [`Self::recompute`] computes, on a worker.
    pub(crate) fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn Any + Send> {
        let key = Self::key(id);
        Box::new(Arc::new(computing(id, || F::compute(db, key))))
    }

    /// Stores the value [`Self::compute`] returned and writes the query's
    /// output, inside the engine's evaluation of it.
    pub(crate) fn publish(&self, id: DerivationId, computed: Box<dyn Any + Send>) {
        let value = *computed
            .downcast::<Arc<F::Value>>()
            .expect("a computed query value");
        self.store(id, value);
    }

    fn store(&self, id: DerivationId, value: Arc<F::Value>) {
        let key = Self::key(id);
        let fingerprint = Self::fingerprint_of(&value);
        let previous = self.memos().insert(
            key,
            Memo {
                value,
                fingerprint,
                stale: false,
            },
        );
        match previous {
            None => Counters::bump(&self.counters.computed),
            Some(previous) => {
                Counters::bump(&self.counters.recomputed);
                if previous.fingerprint == fingerprint {
                    Counters::bump(&self.counters.cut_off);
                }
            }
        }
        derivation::note_write(|| F::output(key));
    }

    fn set_stale(&self, id: DerivationId, stale: bool) {
        if let Some(memo) = self.memos().get_mut(&Self::key(id)) {
            memo.stale = stale;
        }
    }

    fn is_stale(&self, id: DerivationId) -> bool {
        self.memos()
            .get(&Self::key(id))
            .is_some_and(|memo| memo.stale)
    }

    /// Forgets the queries of `file_ids`, sorted, writing their outputs so
    /// their readers check them again.
    fn remove_files(&self, file_ids: &[FileId]) {
        let removing = |file_id: &FileId| file_ids.binary_search(file_id).is_ok();
        let mut removed = Vec::new();
        self.memos().retain(|key, _| {
            let keep = !removing(&key.get_file_id());
            if !keep {
                removed.push(*key);
            }
            keep
        });
        removed.sort_by_key(|key| (key.get_file_id(), key.get_position()));
        for key in removed {
            derivation::note_write(|| F::output(key));
        }
        let mut pending = self.pending();
        pending.retain(|filing| !removing(&filing.id.file_id));
        self.pending_count.store(pending.len(), Ordering::Relaxed);
    }

    fn has_pending(&self) -> bool {
        self.pending_count.load(Ordering::Relaxed) != 0
    }

    /// Queues a filing of `id` with `reads`, as if this family had just
    /// computed it.
    #[cfg(test)]
    pub(crate) fn queue_filing(&self, id: DerivationId, reads: Vec<(HashedKey, Fingerprint)>) {
        let mut pending = self.pending();
        pending.push(Filing {
            id,
            reads,
            moved: None,
        });
        self.pending_count.store(pending.len(), Ordering::Relaxed);
    }

    fn take_pending(&self) -> Vec<Filing> {
        let filings = std::mem::take(&mut *self.pending());
        self.pending_count.store(0, Ordering::Relaxed);
        filings
    }

    pub(crate) fn stats(&self) -> QueryStats {
        self.counters.snapshot()
    }
}

/// Every query family's memos.
#[derive(Debug, Default, Clone)]
pub(crate) struct Queries {
    pub(crate) receiver_authority: QueryTable<ReceiverAuthorityQuery>,
    pub(crate) callback_params: QueryTable<CallbackParamsQuery>,
}

impl Queries {
    pub(crate) fn ready_outputs(&self) -> FxHashMap<HashedKey, Fingerprint> {
        let mut outputs = FxHashMap::default();
        self.receiver_authority.ready_outputs(&mut outputs);
        self.callback_params.ready_outputs(&mut outputs);
        outputs
    }

    /// The derivation that computes query output `key`.
    pub(crate) fn id_of(&self, key: &FactKey) -> DerivationId {
        match key {
            FactKey::ReceiverAuthority(signature_id) => {
                QueryTable::<ReceiverAuthorityQuery>::id(*signature_id)
            }
            FactKey::CallbackParams(signature_id) => {
                QueryTable::<CallbackParamsQuery>::id(*signature_id)
            }
            _ => unreachable!("{key:?} is not a query output"),
        }
    }

    pub(crate) fn ready_fingerprint(&self, key: &FactKey) -> Option<Fingerprint> {
        match key {
            FactKey::ReceiverAuthority(signature_id) => {
                self.receiver_authority.ready_fingerprint(*signature_id)
            }
            FactKey::CallbackParams(signature_id) => {
                self.callback_params.ready_fingerprint(*signature_id)
            }
            _ => unreachable!("{key:?} is not a query output"),
        }
    }

    /// The fingerprint of query output `key`, computing it if needed.
    pub(crate) fn fingerprint(&self, db: &DbIndex, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::ReceiverAuthority(signature_id) => {
                self.receiver_authority.fingerprint(db, *signature_id)
            }
            FactKey::CallbackParams(signature_id) => {
                self.callback_params.fingerprint(db, *signature_id)
            }
            _ => unreachable!("{key:?} is not a query output"),
        }
    }

    /// Computes query `id` again; see [`QueryTable::recompute`].
    pub(crate) fn recompute(&self, db: &DbIndex, id: DerivationId) {
        match id.kind {
            DerivationKind::ReceiverAuthority => self.receiver_authority.recompute(db, id),
            DerivationKind::CallbackParams => self.callback_params.recompute(db, id),
            kind => unreachable!("{kind:?} is not a query"),
        }
    }

    /// Computes query `id` without storing it; see [`QueryTable::compute`].
    pub(crate) fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn Any + Send> {
        match id.kind {
            DerivationKind::ReceiverAuthority => self.receiver_authority.compute(db, id),
            DerivationKind::CallbackParams => self.callback_params.compute(db, id),
            kind => unreachable!("{kind:?} is not a query"),
        }
    }

    /// Stores what [`Self::compute`] returned; see [`QueryTable::publish`].
    pub(crate) fn publish(&self, id: DerivationId, computed: Box<dyn Any + Send>) {
        match id.kind {
            DerivationKind::ReceiverAuthority => self.receiver_authority.publish(id, computed),
            DerivationKind::CallbackParams => self.callback_params.publish(id, computed),
            kind => unreachable!("{kind:?} is not a query"),
        }
    }

    /// Marks query `id`'s memo stale, or current again.
    pub(crate) fn set_stale(&self, id: DerivationId, stale: bool) {
        match id.kind {
            DerivationKind::ReceiverAuthority => self.receiver_authority.set_stale(id, stale),
            DerivationKind::CallbackParams => self.callback_params.set_stale(id, stale),
            kind => unreachable!("{kind:?} is not a query"),
        }
    }

    /// Whether `id`'s answer is marked as one a read of it moved under.
    pub(crate) fn is_stale(&self, id: DerivationId) -> bool {
        match id.kind {
            DerivationKind::ReceiverAuthority => self.receiver_authority.is_stale(id),
            DerivationKind::CallbackParams => self.callback_params.is_stale(id),
            kind => unreachable!("{kind:?} is not a query"),
        }
    }

    /// Forgets the queries of `file_ids`, sorted.
    pub(crate) fn remove_files(&self, file_ids: &[FileId]) {
        self.receiver_authority.remove_files(file_ids);
        self.callback_params.remove_files(file_ids);
    }

    /// Whether a query was computed since [`Self::take_pending`] last ran.
    pub(crate) fn has_pending(&self) -> bool {
        self.receiver_authority.has_pending() || self.callback_params.has_pending()
    }

    /// The queries computed since the last call, to file in the reader graph.
    pub(crate) fn take_pending(&self) -> Vec<Filing> {
        let mut filings = self.receiver_authority.take_pending();
        filings.extend(self.callback_params.take_pending());
        filings
    }

    /// Each family's counters, by kind.
    pub(crate) fn stats(&self) -> [(DerivationKind, QueryStats); 2] {
        [
            (
                DerivationKind::ReceiverAuthority,
                self.receiver_authority.stats(),
            ),
            (DerivationKind::CallbackParams, self.callback_params.stats()),
        ]
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;

    struct RacingCompute {
        value: u64,
        barrier: Arc<Barrier>,
        published: Arc<AtomicBool>,
    }

    thread_local! {
        static RACING: RefCell<Option<RacingCompute>> = const { RefCell::new(None) };
        static COMPUTE_CHILD: Cell<bool> = const { Cell::new(false) };
    }

    struct TestQuery;

    impl QueryFamily for TestQuery {
        type Value = u64;
        const KIND: DerivationKind = DerivationKind::ReceiverAuthority;

        fn output(key: LuaSignatureId) -> FactKey {
            FactKey::ReceiverAuthority(key)
        }

        fn compute(db: &DbIndex, _: LuaSignatureId) -> u64 {
            if COMPUTE_CHILD.with(Cell::get) {
                db.queries()
                    .receiver_authority
                    .get(db, LuaSignatureId::new(FileId::new(8), 3.into()));
            }
            RACING.with(|context| {
                let context = context.borrow();
                let Some(context) = context.as_ref() else {
                    return 23;
                };
                context.barrier.wait();
                if context.value == 2 {
                    while !context.published.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                }
                context.value
            })
        }

        fn floor() -> u64 {
            7
        }
    }

    fn key() -> LuaSignatureId {
        LuaSignatureId::new(FileId::new(1), 2.into())
    }

    fn snapshot(
        outputs: FxHashMap<HashedKey, Fingerprint>,
    ) -> (
        derivation::RecordingSnapshot,
        Arc<derivation::ReaderGraph>,
        derivation::ReleaseQuarantine,
    ) {
        let mut graph = derivation::ReaderGraph::default();
        let quarantine = graph.quarantine_releases();
        let graph = Arc::new(graph);
        (
            derivation::RecordingSnapshot::new(Arc::clone(&graph), quarantine.started(), outputs),
            graph,
            quarantine,
        )
    }

    fn memo(table: &QueryTable<TestQuery>, stale: bool) -> Fingerprint {
        let fingerprint = derivation::fingerprint_of(&13u64);
        table.memos().insert(
            key(),
            Memo {
                value: Arc::new(13),
                fingerprint,
                stale,
            },
        );
        fingerprint
    }

    /// Removing files forgets exactly their queries and writes each one's
    /// output, so its readers check it again.
    #[test]
    fn removing_files_forgets_only_their_queries() {
        let table = QueryTable::<TestQuery>::default();
        let id = |file: u32, position: u32| LuaSignatureId::new(FileId::new(file), position.into());
        for key in [id(3, 1), id(1, 5), id(2, 4), id(1, 2)] {
            table.memos().insert(
                key,
                Memo {
                    value: Arc::new(13),
                    fingerprint: 1,
                    stale: false,
                },
            );
        }

        let written =
            derivation::capture_writes(|| table.remove_files(&[FileId::new(1), FileId::new(3)]));

        let outputs = |keys: &[LuaSignatureId]| {
            keys.iter()
                .map(|key| HashedKey::new(TestQuery::output(*key)))
                .collect::<rustc_hash::FxHashSet<_>>()
        };
        assert_eq!(written, outputs(&[id(1, 2), id(1, 5), id(3, 1)]));
        assert_eq!(
            table.memos().keys().copied().collect::<Vec<_>>(),
            [id(2, 4)]
        );
    }

    #[test]
    fn parallel_query_worker_hit_records_actual_value_and_requires_fresh_publication() {
        let table = QueryTable::<TestQuery>::default();
        let fingerprint = memo(&table, false);
        let (snapshot, graph, _quarantine) = snapshot(FxHashMap::from_iter([(
            HashedKey::new(TestQuery::output(key())),
            fingerprint,
        )]));
        let db = DbIndex::new();
        let result = derivation::compute_recorded(
            &snapshot,
            QueryTable::<TestQuery>::id(key()),
            || table.get(&db, key()),
            |_| unreachable!("query fingerprints use observed values"),
        );
        assert_eq!(*result.value, 13);
        assert!(
            result
                .evaluation
                .validate(&graph, |_, _| table.ready_fingerprint(key()))
        );
        table.set_stale(QueryTable::<TestQuery>::id(key()), true);
        assert!(
            !result
                .evaluation
                .validate(&graph, |_, _| table.ready_fingerprint(key()))
        );
        assert_eq!(table.stats().computed, 0);
        assert!(!table.has_pending());
    }

    #[test]
    fn parallel_query_worker_blocks_stale_fingerprints_and_missing_values_without_filling() {
        for state in [None, Some(true), Some(false)] {
            let table = QueryTable::<TestQuery>::default();
            if let Some(stale) = state {
                memo(&table, stale);
            }
            let (snapshot, graph, _quarantine) = snapshot(FxHashMap::default());
            let db = DbIndex::new();
            let result = derivation::compute_recorded(
                &snapshot,
                QueryTable::<TestQuery>::id(key()),
                || (table.fingerprint(&db, key()), table.get(&db, key())),
                |_| unreachable!("query fingerprints use observed values"),
            );
            assert_eq!(result.value.0, derivation::ABSENT);
            assert_eq!(*result.value.1, 7);
            assert!(
                !result
                    .evaluation
                    .validate(&graph, |_, _| table.ready_fingerprint(key()))
            );
            assert_eq!(table.stats().computed, 0);
            assert!(!table.has_pending());
            assert_eq!(table.memos().len(), usize::from(state.is_some()));
        }
    }

    #[test]
    fn parallel_query_check_defers_comparison_after_a_blocked_detached_child() {
        let table = QueryTable::<TestQuery>::default();
        let fingerprint = memo(&table, false);
        let (snapshot, graph, _quarantine) = snapshot(FxHashMap::from_iter([(
            HashedKey::new(TestQuery::output(key())),
            fingerprint,
        )]));
        let db = DbIndex::new();
        let result = derivation::compute_recorded(
            &snapshot,
            QueryTable::<TestQuery>::id(key()),
            || {
                COMPUTE_CHILD.with(|child| child.set(true));
                table.check(&db, key(), &table.memos().get(&key()).unwrap().clone());
                COMPUTE_CHILD.with(|child| child.set(false));
                assert!(derivation::worker_requires_canonical());
            },
            |_| unreachable!("no query read remains in the parent"),
        );
        assert!(!result.evaluation.validate(&graph, |_, _| None));
        assert_eq!(table.stats().mismatches, 0);
        assert_eq!(db.queries().receiver_authority.stats().computed, 0);
        assert!(!db.queries().has_pending());
    }

    #[test]
    fn parallel_query_readiness_excludes_stale_and_missing_memos_without_computing() {
        let table = QueryTable::<TestQuery>::default();
        assert_eq!(table.ready_fingerprint(key()), None);
        let mut outputs = FxHashMap::default();
        table.ready_outputs(&mut outputs);
        assert!(outputs.is_empty());
        let value = Arc::new(13);
        let fingerprint = QueryTable::<TestQuery>::fingerprint_of(&value);
        table.memos().insert(
            key(),
            Memo {
                value,
                fingerprint,
                stale: false,
            },
        );
        assert_eq!(table.ready_fingerprint(key()), Some(fingerprint));
        table.ready_outputs(&mut outputs);
        assert_eq!(
            outputs[&HashedKey::new(TestQuery::output(key()))],
            fingerprint
        );
        table.set_stale(QueryTable::<TestQuery>::id(key()), true);
        assert_eq!(table.ready_fingerprint(key()), None);
        outputs.clear();
        table.ready_outputs(&mut outputs);
        assert!(outputs.is_empty());
        assert_eq!(table.stats().computed, 0);
        let queries = Queries::default();
        assert!(queries.ready_outputs().is_empty());
        assert_eq!(queries.ready_fingerprint(&TestQuery::output(key())), None);
    }

    #[test]
    fn parallel_query_recursive_returns_preserve_their_actual_value_fingerprint() {
        let table = QueryTable::<TestQuery>::default();
        let db = DbIndex::new();
        let observed = computing(QueryTable::<TestQuery>::id(key()), || {
            table.value_with_fingerprint(&db, key())
        });
        assert_eq!(*observed.value, 7);
        assert_eq!(observed.fingerprint, derivation::fingerprint_of(&7u64));
        table.memos().insert(
            key(),
            Memo {
                value: Arc::new(13),
                fingerprint: derivation::fingerprint_of(&13u64),
                stale: true,
            },
        );
        let observed = computing(QueryTable::<TestQuery>::id(key()), || {
            table.value_with_fingerprint(&db, key())
        });
        assert_eq!(*observed.value, 13);
        assert_eq!(observed.fingerprint, derivation::fingerprint_of(&13u64));
        assert_eq!(table.stats().computed, 0);
    }

    #[test]
    fn parallel_query_losing_first_fill_observes_the_published_winner() {
        let db = DbIndex::new();
        let table = QueryTable::<TestQuery>::default();
        let barrier = Arc::new(Barrier::new(2));
        let published = Arc::new(AtomicBool::new(false));
        let results = std::thread::scope(|scope| {
            let handles = (1..=2)
                .map(|value| {
                    let db = &db;
                    let table = &table;
                    let barrier = Arc::clone(&barrier);
                    let published = Arc::clone(&published);
                    scope.spawn(move || {
                        RACING.with(|context| {
                            *context.borrow_mut() = Some(RacingCompute {
                                value,
                                barrier,
                                published: Arc::clone(&published),
                            })
                        });
                        let observed = table.value_with_fingerprint(db, key());
                        if value == 1 {
                            published.store(true, Ordering::Release);
                        }
                        RACING.with(|context| context.borrow_mut().take());
                        (*observed.value, observed.fingerprint)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results, vec![(1, derivation::fingerprint_of(&1u64)); 2]);
        assert_eq!(table.stats().computed, 1);
        assert_eq!(
            table.ready_fingerprint(key()),
            Some(derivation::fingerprint_of(&1u64))
        );
    }
}
