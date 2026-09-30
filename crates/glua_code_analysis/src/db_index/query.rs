//! Queries: facts computed when first read, each a pure function of what it
//! reads, memoised per key.
//!
//! A query is computed nested inside whatever reads it first, so it only reads
//! the index. Its reads are filed in the reader graph under its own
//! [`DerivationId`], which makes it a reader like any derivation: when a fact
//! it read moves, the engine marks its memo stale and computes it again, and
//! its readers run again only if its value's fingerprint moved.

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
        self.value(db, key)
    }

    /// The query's value, computed if it has no current memo.
    fn value(&self, db: &DbIndex, key: LuaSignatureId) -> Arc<F::Value> {
        derivation::unmapped(|| self.value_unmapped(db, key))
    }

    fn value_unmapped(&self, db: &DbIndex, key: LuaSignatureId) -> Arc<F::Value> {
        let id = Self::id(key);
        if let Some(memo) = self.memos().get(&key).filter(|memo| !memo.stale).cloned() {
            Counters::bump(&self.counters.hits);
            if check_mode() {
                self.check(db, key, &memo);
            }
            return memo.value;
        }
        if is_computing(id) {
            Counters::bump(&self.counters.cycles);
            return self
                .memos()
                .get(&key)
                .map(|memo| Arc::clone(&memo.value))
                .unwrap_or_else(|| Arc::new(F::floor()));
        }
        let (value, reads) = computing(id, || derivation::detached(id, || F::compute(db, key)));
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
            return Arc::clone(&memo.value);
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
        Counters::bump(&self.counters.computed);
        self.pending().push(Filing { id, reads, moved });
        Counters::bump(&self.pending_count);
        value
    }

    /// Recomputes a memo hit fresh and reports a disagreement.
    fn check(&self, db: &DbIndex, key: LuaSignatureId, memo: &Memo<F::Value>) {
        let id = Self::id(key);
        if is_computing(id) {
            return;
        }
        let (fresh, _) = computing(id, || derivation::detached(id, || F::compute(db, key)));
        if Self::fingerprint_of(&fresh) != memo.fingerprint {
            Counters::bump(&self.counters.mismatches);
            eprintln!(
                "[query] {:?} {key:?} mismatch: memo {:?}, fresh {fresh:?}",
                F::KIND,
                memo.value
            );
        }
    }

    /// The fingerprint of the query's output, computing it if it has no
    /// current memo. Not a read.
    pub(crate) fn fingerprint(&self, db: &DbIndex, key: LuaSignatureId) -> Fingerprint {
        let value = self.value(db, key);
        if derivation::mapping_back() {
            return Self::fingerprint_of(&value);
        }
        self.memos()
            .get(&key)
            .map_or_else(|| Self::fingerprint_of(&value), |memo| memo.fingerprint)
    }

    /// Computes the query again inside the engine's evaluation of it, which
    /// records its reads, and writes its output.
    pub(crate) fn recompute(&self, db: &DbIndex, id: DerivationId) {
        let key = Self::key(id);
        let value = Arc::new(computing(id, || F::compute(db, key)));
        let fingerprint = Self::fingerprint_of(&value);
        let previous = self.memos().insert(
            key,
            Memo {
                value,
                fingerprint,
                stale: false,
            },
        );
        Counters::bump(&self.counters.recomputed);
        if previous.is_some_and(|previous| previous.fingerprint == fingerprint) {
            Counters::bump(&self.counters.cut_off);
        }
        derivation::note_write(|| F::output(key));
    }

    fn set_stale(&self, id: DerivationId, stale: bool) {
        if let Some(memo) = self.memos().get_mut(&Self::key(id)) {
            memo.stale = stale;
        }
    }

    /// Forgets `file_id`'s queries, writing their outputs so their readers
    /// check them again.
    fn remove_file(&self, file_id: FileId) {
        let mut removed = Vec::new();
        self.memos().retain(|key, _| {
            let keep = key.get_file_id() != file_id;
            if !keep {
                removed.push(*key);
            }
            keep
        });
        removed.sort_by_key(|key| key.get_position());
        for key in removed {
            derivation::note_write(|| F::output(key));
        }
        let mut pending = self.pending();
        pending.retain(|filing| filing.id.file_id != file_id);
        self.pending_count.store(pending.len(), Ordering::Relaxed);
    }

    fn has_pending(&self) -> bool {
        self.pending_count.load(Ordering::Relaxed) != 0
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

    /// Marks query `id`'s memo stale, or current again.
    pub(crate) fn set_stale(&self, id: DerivationId, stale: bool) {
        match id.kind {
            DerivationKind::ReceiverAuthority => self.receiver_authority.set_stale(id, stale),
            DerivationKind::CallbackParams => self.callback_params.set_stale(id, stale),
            kind => unreachable!("{kind:?} is not a query"),
        }
    }

    pub(crate) fn remove_file(&self, file_id: FileId) {
        self.receiver_authority.remove_file(file_id);
        self.callback_params.remove_file(file_id);
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
