//! The derivation engine: evaluates dirty derivations until every stored
//! derived fact agrees with the facts its producer read.
//!
//! A derivation is evaluated together with its co-producers — every
//! derivation that stored a value under one of the same primary keys. Their
//! keys are reset to the source layer first and the group re-runs in canonical
//! order, so a key several derivations write always holds what a cold build's
//! canonical order would leave there. Each evaluation records what it read;
//! a reader whose recorded fingerprint of a touched key no longer matches is
//! queued, and re-checked when reached, so a key that moved and moved back (a
//! burst, a transient) costs nothing. Cold builds and incremental operations
//! both end here, so they cannot disagree about a fact.
//!
//! The worklist runs in rounds. A round takes ranks in order, so a producer
//! runs before the readers that rank after it, and within a rank runs every
//! changed derivation before re-checking any reader: statements read each
//! other in both directions, and a reader checked once after the rank's new
//! work has run is checked once, not after each writer. A reader that ranks
//! before the derivation that queued it (a back edge) waits for the next
//! round, which re-checks it once however many later producers moved it.
//!
//! A re-analysis settles the re-analysed files first. Until their derivations
//! settle through every stage, a reader in another file only waits: what it
//! would re-check is still being rebuilt, and most of it comes back as it was.
//! Once they settle, the readers whose reads still moved run from the first
//! stage again, like any other work.
//!
//! A run can advance in slices: it stops between two evaluations and picks up
//! where it stopped, so readers can use the index in between.
//!
//! Statements on a loop read, through one another, what each writes. Settled
//! from nothing, a loop holds what the facts it reads from outside support.
//! Once it holds values, a member that runs again reads them back from the
//! others, so a value an outside fact stops supporting stays: a loop that
//! settled before some outside fact it read did, in a cold build as in an
//! update, keeps what that fact supported on the way. Before the run ends,
//! each loop a fact from outside moved under since it last settled is
//! cleared and settles again from the first stage, until none is: what a loop
//! holds is a function of what it reads, not of the order it read it in.
//!
//! A run settles in stages (see [`derivation::AnalysisStage`]). Some reads
//! only have a final answer once nothing can add a fact any more: a field no
//! writer declared yet, a member slot one of whose writers is still untyped.
//! Until the stage that answers them they read as not landed, and the reader
//! records the rule it waited on. At quiescence the run moves to the next
//! stage and re-evaluates exactly the readers of the rules that stage starts
//! to hold, so what they conclude is no longer a question of which derivation
//! happened to run first.

use std::collections::{BTreeMap, BTreeSet};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::DbIndex;
use crate::db_index::derivation::{
    self, AnalysisStage, DerivationId, DerivationKind, FactKey, HashedKey, StageRule,
};

/// Why a derivation is on the worklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// Its site is new or its outputs were dropped: always evaluated.
    Changed,
    /// A fact it read may have changed: evaluated only if one did.
    Read,
}

/// The order a round takes files in within a rank. A file's derivations
/// always run in source order, and co-producers in canonical order; only
/// which file goes first varies. The settled result must not depend on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Schedule {
    /// Files in file-id order.
    #[default]
    Canonical,
    /// Files in reverse file-id order.
    Reversed,
    /// Files in an order fixed by the seed.
    Shuffled(u64),
    /// Files in file-id order, and a reader queued by an evaluation waits for
    /// the next round instead of running again in this one.
    Deferred,
}

impl Schedule {
    fn file_key(self, file_id: crate::FileId) -> u64 {
        match self {
            Self::Canonical | Self::Deferred => u64::from(file_id.id),
            Self::Reversed => u64::MAX - u64::from(file_id.id),
            Self::Shuffled(seed) => {
                use std::hash::BuildHasher;
                rustc_hash::FxBuildHasher.hash_one((seed, file_id.id))
            }
        }
    }
}

/// How far one call of [`Engine::run`] goes before it stops, settled or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slice {
    /// Until the run settles.
    Whole,
    /// Until this many more derivations were evaluated.
    Evaluations(usize),
    /// Until this instant.
    Until(std::time::Instant),
}

impl Slice {
    /// What is left of the slice after `evaluated` evaluations in it.
    pub(crate) fn remaining(self, evaluated: usize) -> Self {
        match self {
            Self::Evaluations(limit) => Self::Evaluations(limit.saturating_sub(evaluated)),
            _ => self,
        }
    }

    /// Whether the slice is over after `evaluated` evaluations in it.
    fn over(self, evaluated: usize) -> bool {
        match self {
            Self::Whole => false,
            Self::Evaluations(limit) => evaluated >= limit,
            Self::Until(deadline) => std::time::Instant::now() >= deadline,
        }
    }
}

/// Queued derivations in the order a round takes them.
#[derive(Default)]
struct Queue {
    schedule: Schedule,
    order: BTreeSet<(u8, Reason, u64, DerivationId)>,
    reasons: FxHashMap<DerivationId, Reason>,
}

impl Queue {
    fn new(schedule: Schedule) -> Self {
        Self {
            schedule,
            ..Self::default()
        }
    }

    fn entry(&self, id: DerivationId, reason: Reason) -> (u8, Reason, u64, DerivationId) {
        (
            id.kind.rank(),
            reason,
            self.schedule.file_key(id.file_id),
            id,
        )
    }

    /// Queues `id`; a changed entry stays changed.
    fn insert(&mut self, id: DerivationId, reason: Reason) {
        if let Some(queued) = self.reasons.get(&id) {
            if *queued <= reason {
                return;
            }
            let entry = self.entry(id, *queued);
            self.order.remove(&entry);
        }
        self.reasons.insert(id, reason);
        self.order.insert(self.entry(id, reason));
    }

    fn remove(&mut self, id: DerivationId) {
        if let Some(reason) = self.reasons.remove(&id) {
            let entry = self.entry(id, reason);
            self.order.remove(&entry);
        }
    }

    fn pop_first(&mut self) -> Option<(DerivationId, Reason)> {
        let (_, reason, _, id) = self.order.pop_first()?;
        self.reasons.remove(&id);
        Some((id, reason))
    }

    /// Pops the first entry if it is of `kind`.
    fn pop_first_of(&mut self, kind: DerivationKind) -> Option<(DerivationId, Reason)> {
        let (_, _, _, id) = self.order.first()?;
        if id.kind != kind {
            return None;
        }
        self.pop_first()
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Takes out every entry `take` accepts.
    fn take_where(
        &mut self,
        mut take: impl FnMut(DerivationId) -> bool,
    ) -> Vec<(DerivationId, Reason)> {
        let taken = self
            .reasons
            .iter()
            .filter(|(id, _)| take(**id))
            .map(|(id, reason)| (*id, *reason))
            .collect::<Vec<_>>();
        for (id, _) in &taken {
            self.remove(*id);
        }
        taken
    }

    fn files(&self) -> impl Iterator<Item = crate::FileId> + '_ {
        self.reasons.keys().map(|id| id.file_id)
    }
}

/// How the engine evaluates a derivation.
pub(crate) trait Recipe: Sync {
    fn evaluate(&self, db: &mut DbIndex, id: DerivationId);

    /// Whether a derivation of `kind` splits into a read-only [`Self::compute`]
    /// that reads nothing another derivation of `kind` writes, and an
    /// [`Self::apply`] of its result. Such derivations queued together compute
    /// concurrently.
    fn splits(&self, kind: DerivationKind) -> bool;

    fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn std::any::Any + Send>;

    fn apply(&self, db: &mut DbIndex, id: DerivationId, computed: Box<dyn std::any::Any + Send>);

    /// Whether a derivation of `kind` computes on a pool worker with its reads
    /// recorded, through [`Self::compute`] and [`Self::apply`], its result
    /// published only if those reads still hold.
    fn records_on_workers(&self, _kind: DerivationKind) -> bool {
        false
    }

    /// Drops a worker result [`Self::apply`] will not publish.
    fn discard(&self, _id: DerivationId, _computed: Box<dyn std::any::Any + Send>) {}
}

#[derive(Default)]
pub(crate) struct Engine {
    /// The order the run takes files in.
    schedule: Schedule,
    /// This round's work.
    worklist: Queue,
    /// Back edges: readers ranked before the derivation that queued them.
    next_round: Queue,
    /// The derivation this round evaluated last.
    cursor: Option<DerivationId>,
    stats: EngineStats,
    /// Files whose source passes read a fact that moved. A file's source
    /// layer is only ever rebuilt whole, so the caller re-analyses them.
    stale_sources: BTreeSet<crate::FileId>,
    /// Files whose diagnostics read a fact that moved: checked once the run
    /// settles, since the fact can move back before then.
    diagnostics: BTreeSet<crate::FileId>,
    /// Every key whose value may have changed while the runs had edits: a
    /// read of one may hold only with the edits mapped back.
    touched: FxHashSet<HashedKey>,
    /// The files whose signatures or inferred guards may have changed.
    payload_holders: BTreeSet<crate::FileId>,
    /// The files that settle first, until they do: the files being
    /// re-analysed, and any prioritised.
    focus: FxHashSet<crate::FileId>,
    /// Work in other files queued while the focus settles, with why it was
    /// queued.
    waiting: BTreeMap<DerivationId, Reason>,
    /// Work queued before its kind's first stage, held until the run
    /// reaches it.
    held: BTreeMap<DerivationId, Reason>,
    /// The files whose derivations this engine evaluated.
    evaluated: FxHashSet<crate::FileId>,
    /// Where the edits being re-analysed moved positions: a read of a fact
    /// whose value only moved still holds.
    edits: derivation::EditMaps,
    /// The edits other files' references have yet to follow.
    to_follow: derivation::EditMaps,
    /// Fingerprints of keys with `edits` undone, taken once per value: the
    /// same moved key is checked for every reader it queued.
    before_edits: std::cell::RefCell<FxHashMap<HashedKey, derivation::Fingerprint>>,
    /// Stage rules that moved since the key table was last lent: memos that
    /// read them no longer hold.
    moved_rules: std::cell::RefCell<Vec<HashedKey>>,
    /// The outputs reset for the group about to run, with their fingerprints
    /// before: memos that read one hold again if it comes back unchanged.
    recomputing: std::cell::RefCell<FxHashMap<HashedKey, derivation::Fingerprint>>,
    /// Under `GLUALS_TRACE_EVALUATIONS`: the key that first queued each
    /// reader, and the derivation that moved it.
    queued_by: FxHashMap<DerivationId, (HashedKey, Option<DerivationId>)>,
    /// The statements this run evaluated, and of them those that had a
    /// record from before the run: what they hold, a run that settled before
    /// left.
    ran: FxHashSet<DerivationId>,
    from_before: FxHashSet<DerivationId>,
    /// Statements queued because facts they read moved, with what each had
    /// seen of them before they first moved.
    moved_from: FxHashMap<DerivationId, Vec<Moved>>,
    /// The statements evaluated since the last loop check, with the facts
    /// that moved under them and what they had settled on of each.
    settled_on: FxHashMap<DerivationId, Vec<Moved>>,
    /// What the derivations of the files re-analysed wrote, as it stood
    /// before they were removed: what a reader settled on of it, whatever it
    /// saw while the files were rebuilt.
    removed_outputs: FxHashMap<HashedKey, derivation::Fingerprint>,
    /// The statements of the loops this run cleared. A loop is cleared once
    /// per run: settled again from nothing, it holds nothing from before the
    /// run, and what moves under it later it follows as any reader does. A
    /// loop that runs through a whole-file pass, which a loop of statements
    /// leaves out, could otherwise be cleared without end, each time from
    /// what the pass made of the last.
    cleared: FxHashSet<DerivationId>,
    /// Since the last loop check cleared loops: every key whose readers were
    /// checked. A loop the next check can clear lies downstream of one; `None`
    /// has the next check search every statement that settled on a moved fact.
    since_search: Option<FxHashSet<HashedKey>>,
    /// The files whose derivations this run removed and builds again.
    rebuilt: FxHashSet<crate::FileId>,
    /// The derivations of the loops being cleared, which settle before any
    /// other work: a reader outside them waits, and runs only if what they
    /// settle to moved a fact it read.
    loop_focus: FxHashSet<DerivationId>,
    /// The files prioritised while the loops being cleared settle, put ahead
    /// once they have.
    after_loops: Vec<crate::FileId>,
    /// What the outputs of the loops being cleared held before: a memo that
    /// read one holds again once the loops settle if it settles back to it.
    loop_before: FxHashMap<HashedKey, derivation::Fingerprint>,
    /// Under `GLUALS_CHECK_CONVERGENCE` or debug assertions: how often each
    /// derivation was evaluated since the run last entered a stage.
    stage_evaluations: FxHashMap<DerivationId, u32>,
    batch_slot_notifications: bool,
    pending_slot_notifications: FxHashSet<HashedKey>,
}

/// A fact that moved under a statement: what the statement had seen of it,
/// and whether it moved once the run was settled, at the last stage.
#[derive(Debug, Clone)]
struct Moved {
    key: HashedKey,
    seen: derivation::Fingerprint,
    settled: bool,
}

#[cfg(test)]
thread_local! {
    static TEST_SLOT_BATCH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

impl Engine {
    pub(crate) fn new(edits: derivation::EditMaps, schedule: Schedule) -> Self {
        let batch_slot_notifications = true;
        #[cfg(test)]
        let batch_slot_notifications =
            TEST_SLOT_BATCH.with(|value| value.get().unwrap_or(batch_slot_notifications));
        Self {
            schedule,
            batch_slot_notifications,
            worklist: Queue::new(schedule),
            next_round: Queue::new(schedule),
            to_follow: edits.clone(),
            edits,
            ..Self::default()
        }
    }

    /// The edits other files' references have yet to follow, once.
    pub(crate) fn edits_to_apply(&mut self) -> Option<derivation::EditMaps> {
        (!self.to_follow.is_empty()).then(|| std::mem::take(&mut self.to_follow))
    }

    /// Takes in `later`, edits of the text the edits so far produced: a read
    /// holds if it saw a value from before any of them, and what other files
    /// reference follows `later` alone.
    pub(crate) fn take_edits(&mut self, later: derivation::EditMaps) {
        self.edits = self.edits.then(&later);
        self.touched = std::mem::take(&mut self.touched)
            .into_iter()
            .flat_map(|key| {
                let moved = later
                    .map_key(&key)
                    .filter(|moved| moved != key.key())
                    .map(HashedKey::new);
                std::iter::once(key).chain(moved)
            })
            .collect();
        self.pending_slot_notifications = std::mem::take(&mut self.pending_slot_notifications)
            .into_iter()
            .flat_map(|key| {
                let moved = later
                    .map_key(&key)
                    .filter(|moved| moved != key.key())
                    .map(HashedKey::new);
                std::iter::once(key).chain(moved)
            })
            .collect();
        self.to_follow = later;
        self.before_edits.get_mut().clear();
        self.since_search = None;
    }

    /// Whether reads of `key` that saw a fingerprint still hold: the value is
    /// the same, or the same once the edits' moves are undone.
    fn key_holds<'a>(
        &'a self,
        db: &'a DbIndex,
        key: &'a HashedKey,
    ) -> impl Fn(derivation::Fingerprint) -> bool + 'a {
        let current = std::cell::OnceCell::new();
        let before_edits = std::cell::OnceCell::new();
        move |seen| {
            seen == *current.get_or_init(|| self.fingerprint(db, key))
                || (!self.edits.is_empty()
                    && seen == *before_edits.get_or_init(|| self.fingerprint_before_edits(db, key)))
        }
    }

    fn fingerprint_before_edits(&self, db: &DbIndex, key: &HashedKey) -> derivation::Fingerprint {
        if let Some(fingerprint) = self.before_edits.borrow().get(key) {
            return *fingerprint;
        }
        let fingerprint =
            derivation::fingerprint_before_edits(&self.edits, || db.fact_fingerprint(key));
        self.before_edits
            .borrow_mut()
            .insert(key.clone(), fingerprint);
        fingerprint
    }

    /// `key`'s current fingerprint, taken once per value. Every write inside
    /// an evaluation is one of its outputs, so a group's touched keys are the
    /// only ones it can move; writes outside `run` forget every fingerprint.
    fn fingerprint(&self, db: &DbIndex, key: &HashedKey) -> derivation::Fingerprint {
        db.reader_graph()
            .current_fingerprint(key, |key| db.fact_fingerprint(key))
    }

    /// Under `GLUALS_CHECK_FINGERPRINTS`, panics if a cached key other than
    /// `except` no longer holds its cached fingerprint.
    fn verify_fingerprints(
        &self,
        db: &DbIndex,
        except: &FxHashSet<HashedKey>,
        step: impl Fn() -> String,
    ) {
        if !check_fingerprints() {
            return;
        }
        for (key, cached) in db.reader_graph().cached_fingerprints() {
            let excepted = except.contains(key)
                || key
                    .base()
                    .is_some_and(|base| except.contains(&HashedKey::new(base)));
            assert!(
                excepted || cached == db.fact_fingerprint(key),
                "{key:?} moved without being written while {}",
                step()
            );
        }
    }

    /// Notes what `key` held before the derivation that wrote it was removed,
    /// unless an earlier removal in this run noted it.
    pub(crate) fn note_removed_output(
        &mut self,
        key: HashedKey,
        fingerprint: derivation::Fingerprint,
    ) {
        self.removed_outputs.entry(key).or_insert(fingerprint);
    }

    /// Carries the removed outputs noted so far to where `edits` moved them.
    pub(crate) fn follow_removed_outputs(&mut self, edits: &derivation::EditMaps) {
        self.removed_outputs = std::mem::take(&mut self.removed_outputs)
            .into_iter()
            .map(|(key, fingerprint)| {
                let key = edits
                    .map_key(&key)
                    .filter(|moved| moved != key.key())
                    .map_or(key, HashedKey::new);
                (key, fingerprint)
            })
            .collect();
    }

    pub(crate) fn changed(&mut self, id: DerivationId) {
        self.stats
            .requeue_cause(id, "changed: its file re-analysed or a producer removed");
        self.queue(id, Reason::Changed);
    }

    /// Queues `id` in this round unless it ranks before the derivation being
    /// evaluated, else in the next. A changed entry stays changed.
    fn queue(&mut self, id: DerivationId, reason: Reason) {
        if self.cursor.is_some_and(|cursor| {
            self.worklist.schedule == Schedule::Deferred || id.kind.rank() < cursor.kind.rank()
        }) {
            self.next_round.insert(id, reason);
        } else {
            self.worklist.insert(id, reason);
        }
    }

    fn unqueue(&mut self, id: DerivationId) {
        self.worklist.remove(id);
        self.next_round.remove(id);
        self.held.remove(&id);
    }

    pub(crate) fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// Queues the readers of `keys`, and of every key read out of them, whose
    /// recorded fingerprint differs from the current value.
    pub(crate) fn readers_of<'a>(
        &mut self,
        db: &DbIndex,
        keys: impl IntoIterator<Item = &'a HashedKey>,
    ) {
        for key in keys {
            self.queue_readers(db, key);
            if !key.is_base() {
                continue;
            }
            let now = self.fingerprint(db, key);
            for derived in db.reader_graph().derived_keys_to_check(key, now) {
                self.queue_readers(db, &derived);
            }
        }
    }

    /// Queues the readers of `key` whose recorded fingerprint differs from its
    /// current value.
    fn queue_readers(&mut self, db: &DbIndex, key: &HashedKey) {
        if self.batch_slot_notifications
            && self.loop_focus.is_empty()
            && matches!(
                key.key(),
                FactKey::MemberSlot(..) | FactKey::MemberOwnerMap(_)
            )
        {
            if let Some(keys) = &mut self.since_search {
                keys.insert(key.clone());
            }
            if !self.edits.is_empty() {
                self.before_edits.get_mut().remove(key);
                self.touched.insert(key.clone());
            }
            let readers = db
                .reader_graph()
                .stale_readers_seen(key, self.key_holds(db, key))
                .collect::<Vec<_>>();
            for (reader, seen) in readers {
                self.remember_moved_read(db, key, reader, seen);
                if reader.kind.is_query() {
                    db.queries().set_stale(reader, true);
                }
            }
            self.pending_slot_notifications.insert(key.clone());
            return;
        }
        self.queue_readers_now(db, key);
    }

    pub(crate) fn flush_slot_notifications(&mut self, db: &DbIndex) {
        let mut pending = std::mem::take(&mut self.pending_slot_notifications)
            .into_iter()
            .collect::<Vec<_>>();
        crate::profile::phase("engine: sort slot notifications", || {
            pending.sort_by_cached_key(|key| format!("{:?}", key.key()));
        });
        for key in pending {
            self.queue_readers_now(db, &key);
        }
    }

    fn remember_moved_read(
        &mut self,
        db: &DbIndex,
        key: &HashedKey,
        reader: DerivationId,
        seen: derivation::Fingerprint,
    ) {
        if reader.kind.is_statement() {
            let settled = db.analysis_stage() == AnalysisStage::Closed;
            let moved = self.moved_from.entry(reader).or_default();
            if moved.iter().all(|moved| moved.key != *key) {
                moved.push(Moved {
                    key: key.clone(),
                    seen,
                    settled,
                });
            }
        }
    }

    fn queue_readers_now(&mut self, db: &DbIndex, key: &HashedKey) {
        if let Some(keys) = &mut self.since_search {
            keys.insert(key.clone());
        }
        if !self.edits.is_empty() {
            self.before_edits.get_mut().remove(key);
            self.touched.insert(key.clone());
        }
        if let FactKey::Signature(signature_id) | FactKey::InferredGuard(signature_id) = key.key() {
            self.payload_holders.insert(signature_id.get_file_id());
        }
        let readers = db
            .reader_graph()
            .stale_readers_seen(key, self.key_holds(db, key))
            .collect::<Vec<_>>();
        for (reader, seen) in readers {
            self.stats
                .requeued(key, self.cursor, reader, seen == derivation::ABSENT);
            self.remember_moved_read(db, key, reader, seen);
            if reader.kind.is_query() {
                db.queries().set_stale(reader, true);
            }
            if crate::profile::tracing_evaluations() {
                self.queued_by
                    .entry(reader)
                    .or_insert_with(|| (key.clone(), self.cursor));
            }
            match reader.kind {
                DerivationKind::Diagnostics => {
                    self.diagnostics.insert(reader.file_id);
                }
                _ if self.waits(reader) => {
                    self.wait(reader, Reason::Read);
                }
                DerivationKind::Source => {
                    if crate::profile::tracing_evaluations() {
                        self.trace(db, reader, "source stale", &[]);
                    }
                    self.stale_sources.insert(reader.file_id);
                }
                _ => self.queue(reader, Reason::Read),
            }
        }
    }

    /// Adds `file_ids` to what settles first, starting a focus if none is
    /// settling: their waiting work is queued again, other files' queued work
    /// waits, and so does every reader in another file queued until the focus
    /// settles. While cleared loops settle, which comes before any other work,
    /// they are put ahead once the loops have settled.
    pub(crate) fn prioritise(&mut self, file_ids: &[crate::FileId]) {
        if !self.loop_focus.is_empty() {
            self.after_loops.extend_from_slice(file_ids);
            return;
        }
        let added = file_ids
            .iter()
            .filter(|file_id| **file_id != crate::FileId::VIRTUAL)
            .filter(|file_id| self.focus.insert(**file_id))
            .count();
        if added == 0 {
            return;
        }
        let resumed = self
            .waiting
            .extract_if(.., |id, _| self.focus.contains(&id.file_id))
            .collect::<Vec<_>>();
        for (id, reason) in resumed {
            self.queue(id, reason);
        }
        self.park_outside_focus();
    }

    /// Moves queued work outside the focus to the waiting set.
    fn park_outside_focus(&mut self) {
        if self.focus.is_empty() {
            return;
        }
        let focus = &self.focus;
        let mut parked = Vec::new();
        for queue in [&mut self.worklist, &mut self.next_round] {
            parked.extend(queue.take_where(|id| {
                id.file_id != crate::FileId::VIRTUAL && !focus.contains(&id.file_id)
            }));
        }
        for (id, reason) in parked {
            self.wait(id, reason);
        }
    }

    /// Parks `id` until the focus settles; a changed entry stays changed.
    fn wait(&mut self, id: DerivationId, reason: Reason) {
        let parked = self.waiting.entry(id).or_insert(reason);
        *parked = (*parked).min(reason);
    }

    /// Forgets the queued work of `file_ids`, whose derivations are about to
    /// be replaced and queued again.
    pub(crate) fn replacing(&mut self, file_ids: &[crate::FileId]) {
        self.since_search = None;
        let replaced = |id: DerivationId| file_ids.contains(&id.file_id);
        self.worklist.take_where(replaced);
        self.next_round.take_where(replaced);
        self.waiting.retain(|id, _| !replaced(*id));
        self.held.retain(|id, _| !replaced(*id));
        self.stale_sources
            .retain(|file_id| !file_ids.contains(file_id));
    }

    /// Notes that `file_id`'s derivations are removed and built again.
    pub(crate) fn rebuilding(&mut self, file_id: crate::FileId) {
        self.rebuilt.insert(file_id);
    }

    /// Whether `file_id` has work still to settle: queued, waiting, in the
    /// focus or a source layer to rebuild.
    pub(crate) fn is_settling(&self, file_id: crate::FileId) -> bool {
        self.focus.contains(&file_id)
            || self.stale_sources.contains(&file_id)
            || self
                .worklist
                .files()
                .chain(self.next_round.files())
                .chain(self.waiting.keys().map(|id| id.file_id))
                .chain(self.held.keys().map(|id| id.file_id))
                .any(|queued| queued == file_id)
    }

    /// Whether `file_id` is in the focus that is still settling.
    pub(crate) fn in_focus(&self, file_id: crate::FileId) -> bool {
        self.focus.contains(&file_id)
    }

    /// Whether `reader` waits for the focus to settle.
    fn waits(&self, reader: DerivationId) -> bool {
        if !self.loop_focus.is_empty() {
            return !self.loop_focus.contains(&reader);
        }
        !self.focus.is_empty()
            && reader.file_id != crate::FileId::VIRTUAL
            && !self.focus.contains(&reader.file_id)
    }

    /// Marks `file_id`'s source layer as needing a rebuild.
    pub(crate) fn source_stale(&mut self, file_id: crate::FileId) {
        self.stale_sources.insert(file_id);
    }

    /// Checks `file_id`'s diagnostics once the run settles.
    pub(crate) fn check_diagnostics(&mut self, file_id: crate::FileId) {
        self.diagnostics.insert(file_id);
    }

    /// The files whose diagnostics read a fact that moved and holds neither
    /// as it is nor with the edits mapped back, sorted.
    pub(crate) fn stale_diagnostics(&self, db: &DbIndex) -> Vec<crate::FileId> {
        let graph = db.reader_graph();
        self.diagnostics
            .iter()
            .copied()
            .filter(|file_id| {
                graph.is_stale(
                    DerivationId::diagnostics(*file_id),
                    |key| db.fact_fingerprint(key),
                    |_, _| false,
                )
            })
            .collect()
    }

    /// The files whose signatures or inferred guards may have changed so far.
    pub(crate) fn payload_holders(&self) -> impl Iterator<Item = crate::FileId> + '_ {
        self.payload_holders.iter().copied()
    }

    /// Notes that `file_ids`' source layers were rebuilt.
    pub(crate) fn note_rebuilt(&mut self, file_ids: &[crate::FileId]) {
        self.evaluated.extend(file_ids.iter().copied());
    }

    /// The files whose derivations were evaluated, sorted.
    pub(crate) fn evaluated_files(&self) -> Vec<crate::FileId> {
        let mut files = self.evaluated.iter().copied().collect::<Vec<_>>();
        files.sort_unstable();
        files
    }

    /// The files whose source layer must be rebuilt, emptied.
    pub(crate) fn take_stale_sources(&mut self) -> BTreeSet<crate::FileId> {
        self.stats.rebuilt_sources += self.stale_sources.len();
        std::mem::take(&mut self.stale_sources)
    }

    /// Starts the run over from the first stage.
    pub(crate) fn open(&mut self, db: &mut DbIndex) {
        self.enter_stage(db, AnalysisStage::Open);
    }

    /// Evaluates the worklist stage by stage until it settles or `slice` is
    /// over. Returns whether it settled; one that did not stopped between two
    /// evaluations and goes on from there.
    pub(crate) fn run(&mut self, db: &mut DbIndex, recipe: &dyn Recipe, slice: Slice) -> bool {
        let started = self.stats.total_evaluations();
        loop {
            let moved = file_queries(db);
            if !moved.is_empty() {
                self.readers_of(db, moved.iter());
            }
            if slice.over(self.stats.total_evaluations() - started) {
                self.flush_slot_notifications(db);
                return false;
            }
            let Some((id, reason)) = self.worklist.pop_first() else {
                self.flush_slot_notifications(db);
                if !self.worklist.is_empty() {
                    continue;
                }
                if self.next_round.is_empty() {
                    if crate::profile::phase("engine: stage and release", || {
                        self.advance_stage(db) || self.release_waiting(db)
                    }) || crate::profile::phase("engine: loops", || self.clear_loops(db))
                    {
                        continue;
                    }
                    break;
                }
                let schedule = self.next_round.schedule;
                self.worklist = std::mem::replace(&mut self.next_round, Queue::new(schedule));
                self.cursor = None;
                self.stats.rounds += 1;
                self.stats.round_started(self.worklist.order.len());
                continue;
            };
            if db.analysis_stage() < id.kind.first_stage() {
                let held = self.held.entry(id).or_insert(reason);
                *held = (*held).min(reason);
                continue;
            }
            self.cursor = Some(id);
            let stale = crate::profile::phase("engine: stale check", || {
                self.needs_evaluation(db, id, reason)
            });
            if !stale {
                self.moved_from.remove(&id);
                if id.kind.is_query() {
                    db.queries().set_stale(id, false);
                }
                continue;
            }
            self.trace_evaluation(db, id, reason);
            self.check_convergence(id);
            let group = crate::profile::phase("engine: co-producers", || co_producers(db, id));
            if group.len() == 1
                && recipe.records_on_workers(id.kind)
                && matches!(slice, Slice::Whole)
                && super::parallel::records_on_workers()
                && crate::profile::phase("engine: recorded batch", || {
                    self.run_recorded_batch(db, id, recipe)
                })
            {
                self.queue_reruns();
                continue;
            }
            if group.len() == 1 && recipe.splits(id.kind) {
                crate::profile::phase("engine: batch", || self.run_batch(db, id, recipe));
                self.queue_reruns();
                continue;
            }
            let rerun = self.stats.profiling()
                && group
                    .iter()
                    .all(|member| self.stats.distinct.contains(member));
            for member in &group {
                if *member != id {
                    self.stats
                        .requeue_cause(*member, "run with a queued co-producer");
                }
                self.unqueue(*member);
                self.stats.evaluated(*member);
                if member.kind.is_statement()
                    && self.ran.insert(*member)
                    && db.reader_graph().contains(*member)
                {
                    self.from_before.insert(*member);
                }
                let moved = self.moved_from.remove(member).unwrap_or_default();
                if member.kind.is_statement() {
                    self.settled_on(*member, moved);
                }
            }
            self.evaluated.extend(
                group
                    .iter()
                    .map(|member| member.file_id)
                    .filter(|file_id| *file_id != crate::FileId::VIRTUAL),
            );
            let rebuilt = group
                .iter()
                .copied()
                .filter(|member| {
                    member.kind.is_statement() && self.rebuilt.contains(&member.file_id)
                })
                .collect::<Vec<_>>();
            let started = self.stats.profiling().then(std::time::Instant::now);
            let touched = crate::profile::phase("engine: evaluate", || {
                self.evaluate_group(db, &group, recipe)
            });
            if let Some(started) = started {
                let elapsed = started.elapsed();
                for member in &group {
                    self.stats
                        .timed_evaluation(*member, elapsed / group.len() as u32);
                }
                self.stats.timed(id.kind, elapsed, usize::from(rerun), 1);
                for member in &group {
                    for key in db.reader_graph().reads_of(*member) {
                        *self.stats.reads.entry(variant_name(key)).or_default() += 1;
                    }
                }
            }
            // A member that now shares a key with a derivation outside the
            // group ran without it; the next evaluation takes both.
            let widened = group.iter().find_map(|member| {
                db.reader_graph().outputs_of(*member).find_map(|key| {
                    if !key.is_primary() {
                        return None;
                    }
                    db.reader_graph()
                        .producers(key)
                        .find(|producer| {
                            producer.kind != DerivationKind::Source && !group.contains(producer)
                        })
                        .map(|producer| (key.clone(), producer))
                })
            });
            if widened.is_some() {
                // Again in this round: nothing ran after it yet.
                self.stats.requeue_cause(id, "group widened");
                self.worklist.insert(id, Reason::Changed);
            }
            self.queue_reruns();
            crate::profile::phase("engine: readers of", || self.readers_of(db, touched.iter()));
            // A statement of a rebuilt file settled, before the run, on what
            // the removed derivations wrote.
            for member in rebuilt {
                let removed = db
                    .reader_graph()
                    .reads_of(member)
                    .filter_map(|key| {
                        Some(Moved {
                            key: key.clone(),
                            seen: *self.removed_outputs.get(key)?,
                            settled: true,
                        })
                    })
                    .collect::<Vec<_>>();
                self.settled_on(member, removed);
            }
        }
        self.cursor = None;
        crate::profile::phase("engine: rebase moved reads", || self.rebase_moved_reads(db));
        true
    }

    /// Notes that statement `member` settled on what it saw of each fact in
    /// `moved`, keeping the earliest sight of a fact it already settled on.
    fn settled_on(&mut self, member: DerivationId, moved: Vec<Moved>) {
        if moved.is_empty() {
            return;
        }
        let settled_on = self.settled_on.entry(member).or_default();
        for moved in moved {
            if settled_on.iter().all(|settled| settled.key != moved.key) {
                settled_on.push(moved);
            }
        }
    }

    /// Whether the fact `moved` names, read by `member`, now holds another
    /// value than the one `member` had settled on. A statement settling for
    /// the first time counts only a fact it read while nothing held it that
    /// lands at the last stage, where the floor answer it took from the absent
    /// fact, or from one only a whole-file pass adds there, is replaced; while
    /// the run settles, facts otherwise land and grow under their readers. A
    /// removed derivation's output counts against what it held before the
    /// removal.
    fn net_moved(&self, db: &DbIndex, member: DerivationId, moved: &Moved) -> bool {
        let removed = self.removed_outputs.get(&moved.key).copied();
        ((moved.settled && moved.seen == derivation::ABSENT)
            || removed.is_some()
            || self.from_before.contains(&member))
            && !self.key_holds(db, &moved.key)(removed.unwrap_or(moved.seen))
    }

    /// Whether a fact `member` of `lp` reads from outside the loop now holds
    /// another value than the one `member` had settled on.
    fn moved_from_outside(&self, db: &DbIndex, lp: &[DerivationId], member: DerivationId) -> bool {
        let graph = db.reader_graph();
        self.settled_on.get(&member).is_some_and(|moved| {
            moved.iter().any(|moved| {
                // A fact a removed derivation wrote is read from outside the
                // loop: what the removed writer supplied, nothing in the loop
                // supplies any more.
                self.net_moved(db, member, moved)
                    && (self.removed_outputs.contains_key(&moved.key) || {
                        let mut producers = graph.producers(&moved.key).peekable();
                        producers.peek().is_none()
                            || producers.any(|producer| !lp.contains(&producer))
                    })
            })
        })
    }

    /// At quiescence, clears every loop of statements one of whose members
    /// reads from outside the loop a fact that moved since it settled on it:
    /// each member's primary outputs, and those of whatever shares them, go
    /// back to the source layer, they run again, and the run starts over from
    /// the first stage, as a cold build settles a loop through them. Returns
    /// whether it cleared any.
    fn clear_loops(&mut self, db: &mut DbIndex) -> bool {
        // After a check that cleared loops, what a loop's members read and
        // write, and what their facts hold, moved only through the keys
        // checked since: a loop it left that the next one clears is
        // downstream of one.
        let since = self.since_search.take().map(|keys| {
            db.reader_graph()
                .readers_downstream(keys.iter(), DerivationKind::is_statement)
        });
        // A loop is cleared for a member that read a fact that net-moved, so
        // only such members can be roots of one.
        let roots = self
            .settled_on
            .iter()
            .filter(|(member, _)| since.as_ref().is_none_or(|since| since.contains(*member)))
            .filter(|(member, moved)| {
                moved
                    .iter()
                    .any(|moved| self.net_moved(db, **member, moved))
            })
            .map(|(member, _)| *member)
            .collect::<BTreeSet<_>>();
        if roots.is_empty() {
            self.settled_on.clear();
            return false;
        }
        // A loop downstream of another that is cleared waits for it to
        // settle: cleared together, it would settle from what the other held
        // on the way, not from what the other settles to.
        let loops = db.reader_graph().read_loops(
            roots.iter().copied(),
            DerivationKind::is_statement,
            |lp| {
                lp.iter().any(|member| !self.cleared.contains(member))
                    && lp
                        .iter()
                        .any(|member| self.moved_from_outside(db, lp, *member))
            },
        );
        if loops.is_empty() {
            self.settled_on.clear();
            return false;
        }
        self.stats.loops_cleared += loops.len();
        self.since_search = Some(FxHashSet::default());
        self.cleared.extend(loops.iter().flatten().copied());
        let mut members = BTreeSet::new();
        for statement in loops.iter().flatten() {
            members.extend(co_producers(db, *statement));
        }

        if crate::profile::tracing_evaluations() {
            for member in &members {
                self.trace(db, *member, "loop cleared", &[]);
            }
        }
        self.stats.loop_members_cleared += members.len();
        let mut touched = FxHashSet::default();
        for member in &members {
            touched.extend(db.reader_graph().outputs_of(*member).cloned());
        }
        self.loop_before = touched
            .iter()
            .map(|key| (key.clone(), self.fingerprint(db, key)))
            .collect();
        let reset = derivation::capture_writes(|| {
            for key in touched.iter().filter(|key| key.is_primary()) {
                db.reset_fact(key);
            }
        });
        touched.extend(reset);
        // Held until the loops settle: what reads them meanwhile is not
        // shared, and a memo that read them before holds again if they settle
        // back to what they were.
        db.reader_graph_mut().hold_windows(touched.iter());
        for key in &touched {
            db.reader_graph().forget_fingerprint(key);
            forget_before_edits(self.before_edits.get_mut(), db.reader_graph(), key);
        }
        self.loop_focus = members.iter().copied().collect();
        // Settled from nothing, a loop whose rules leave it more than one
        // fixed point lands on the one the order it runs in reaches, so it
        // settles in file-id order whatever order the run takes files in.
        debug_assert!(self.worklist.is_empty() && self.next_round.is_empty());
        self.worklist = Queue::new(Schedule::Canonical);
        self.next_round = Queue::new(Schedule::Canonical);
        self.readers_of(db, touched.iter());
        for member in members {
            // The loop's own reset is not a fact from outside it.
            self.moved_from.remove(&member);
            self.settled_on.remove(&member);
            self.stats.requeue_cause(member, "loop cleared");
            self.queue(member, Reason::Changed);
        }
        self.enter_stage(db, AnalysisStage::Open);
        self.cursor = None;
        true
    }

    /// Queues again, in this round, the evaluations that asked to run again
    /// (see [`derivation::rerun_current`]).
    fn queue_reruns(&mut self) {
        for id in derivation::take_reruns() {
            self.stats
                .requeue_cause(id, "wrote a type under a fallback it may have read");
            self.worklist.insert(id, Reason::Changed);
        }
    }

    /// Re-records every read that held only with the edits mapped back as a
    /// read of the value as it stands now, so the next edit of the same file
    /// compares against it rather than against the text before this one.
    fn rebase_moved_reads(&self, db: &mut DbIndex) {
        if self.edits.is_empty() {
            return;
        }
        // Only a reader that saw another value can have seen it before the
        // edits; the others read the value as it stands.
        let graph = db.reader_graph();
        let moved = self
            .touched
            .iter()
            .filter(|key| graph.read_other_than(key, || self.fingerprint(db, key)))
            .filter_map(|key| {
                let current = self.fingerprint(db, key);
                let before = self.fingerprint_before_edits(db, key);
                (before != current).then(|| (key.clone(), before, current))
            })
            .collect::<Vec<_>>();
        for (key, before, current) in moved {
            db.reader_graph_mut().rebase_readers(&key, before, current);
        }
    }

    /// Under `GLUALS_CHECK_CONVERGENCE` or debug assertions, panics once `id`
    /// was evaluated more often within one stage than settling can need: the
    /// facts it reads keep moving, so some rule among them does not converge.
    fn check_convergence(&mut self, id: DerivationId) {
        if !(cfg!(debug_assertions) || check_convergence()) {
            return;
        }
        let evaluations = self.stage_evaluations.entry(id).or_default();
        *evaluations += 1;
        assert!(
            *evaluations <= MAX_STAGE_EVALUATIONS,
            "{id:?} evaluated {evaluations} times without the run moving to another stage"
        );
    }

    /// Moves the run to `stage`. Returns the keys of the rules that moved.
    fn enter_stage(&mut self, db: &mut DbIndex, stage: AnalysisStage) -> Vec<HashedKey> {
        self.stage_evaluations.clear();
        let before = db.analysis_stage();
        db.set_analysis_stage(stage);
        let moved = StageRule::ALL
            .into_iter()
            .filter(|rule| rule.holds_at(before) != rule.holds_at(stage))
            .map(|rule| HashedKey::new(FactKey::StageRule(rule)))
            .collect::<Vec<_>>();
        self.moved_rules.get_mut().extend(moved.iter().cloned());
        moved
    }

    /// At quiescence, moves the run to its next stage and queues every
    /// derivation that waited on a rule the stage starts to hold, and the work
    /// held until its kind's first stage. Returns whether it queued any;
    /// `false` once the run is closed.
    fn advance_stage(&mut self, db: &mut DbIndex) -> bool {
        while let Some(stage) = db.analysis_stage().next() {
            let moved = self.enter_stage(db, stage);
            let mut waited = BTreeSet::new();
            for key in &moved {
                let current = db.fact_fingerprint(key);
                waited.extend(db.reader_graph().stale_readers(key, |seen| seen == current));
            }
            let mut queued = 0;
            let reached = self
                .held
                .extract_if(.., |id, _| id.kind.first_stage() <= stage)
                .collect::<Vec<_>>();
            for (id, reason) in reached {
                self.worklist.insert(id, reason);
                queued += 1;
            }
            for reader in waited {
                self.stats.stage_requeued(reader);
                if crate::profile::tracing_evaluations() {
                    self.trace(db, reader, "stage barrier queues", &[]);
                }
                match reader.kind {
                    DerivationKind::Diagnostics => {
                        self.diagnostics.insert(reader.file_id);
                    }
                    DerivationKind::Source => {
                        self.stale_sources.insert(reader.file_id);
                    }
                    _ => {
                        self.worklist.insert(reader, Reason::Changed);
                        queued += 1;
                    }
                }
            }
            self.stats.stage_started(stage, queued);
            if queued > 0 {
                self.cursor = None;
                return true;
            }
        }
        false
    }

    /// Once the focus settled, queues the waiting readers whose reads still
    /// moved and restarts the stages for them, and marks the source layers
    /// that still read a moved fact. Returns whether it queued any.
    fn release_waiting(&mut self, db: &mut DbIndex) -> bool {
        self.focus.clear();
        if !self.loop_focus.is_empty() {
            self.loop_focus.clear();
            let unchanged = std::mem::take(&mut self.loop_before)
                .into_iter()
                .filter(|(key, before)| db.fact_fingerprint(key) == *before)
                .map(|(key, _)| key)
                .collect::<FxHashSet<_>>();
            db.reader_graph_mut()
                .release_held(|key| unchanged.contains(key));
            debug_assert!(self.worklist.is_empty() && self.next_round.is_empty());
            self.worklist = Queue::new(self.schedule);
            self.next_round = Queue::new(self.schedule);
        }
        let mut queued = 0;
        for (reader, reason) in std::mem::take(&mut self.waiting) {
            if reason == Reason::Changed {
                self.stats.requeue_cause(reader, "released after the focus");
                self.worklist.insert(reader, Reason::Changed);
                queued += 1;
                continue;
            }
            let moved = if crate::profile::tracing_evaluations() {
                self.moved_reads(db, reader)
            } else {
                Vec::new()
            };
            if !self.needs_evaluation(db, reader, Reason::Read) {
                self.moved_from.remove(&reader);
                continue;
            }
            if crate::profile::tracing_evaluations() {
                self.trace(db, reader, "released", &moved);
            }
            if reader.kind == DerivationKind::Source {
                self.stale_sources.insert(reader.file_id);
            } else {
                // Checking rebased its reads onto the values as they stand, so
                // it is queued as changed rather than checked again.
                self.stats.requeue_cause(reader, "released after the focus");
                self.worklist.insert(reader, Reason::Changed);
                queued += 1;
            }
        }
        self.stats.focus_settled(queued);
        let after_loops = std::mem::take(&mut self.after_loops);
        if queued == 0 {
            return false;
        }
        if !after_loops.is_empty() {
            self.prioritise(&after_loops);
        }
        self.enter_stage(db, AnalysisStage::Open);
        self.cursor = None;
        true
    }

    /// Whether `id`, queued for `reason`, has to run: a reader runs only if a
    /// fact it read moved. A stage rule it saw holds: only a stage barrier
    /// moves one, and the barrier queues the rule's readers itself.
    fn needs_evaluation(&mut self, db: &mut DbIndex, id: DerivationId, reason: Reason) -> bool {
        if reason == Reason::Changed
            || db.reader_graph().is_stale(
                id,
                |key| db.fact_fingerprint(key),
                |key, seen| {
                    key.is_stage_rule()
                        || (!self.edits.is_empty()
                            && seen == self.fingerprint_before_edits(db, key))
                },
            )
        {
            return true;
        }
        if self.edits.is_empty() {
            return false;
        }
        let fingerprints = db
            .reader_graph()
            .reads_of(id)
            .map(|key| self.fingerprint(db, key))
            .collect();
        // A read that held only with the edit mapped back saw positions the
        // edit moved. An output made from them is still right if the edit's
        // remap rewrote it, or if it names none of the positions the edit
        // moved: with the edit mapped back it fingerprints as it does now. A
        // fold of every writer's share is judged by this writer's share, which
        // it also wrote: the fold holds the edited files' shares too.
        let moved = db.reader_graph_mut().set_fingerprints(id, fingerprints);
        if !moved {
            return false;
        }
        let outputs = db.reader_graph().outputs_of(id).collect::<Vec<_>>();
        !outputs.iter().all(|key| {
            key.follows_edits()
                || key
                    .writer_share(id)
                    .is_some_and(|share| outputs.iter().any(|output| *output.key() == share))
                || self.fingerprint(db, key) == self.fingerprint_before_edits(db, key)
        })
    }

    /// Under `GLUALS_TRACE_EVALUATIONS`, prints `id`'s file, why it was queued
    /// and which of its recorded reads no longer hold.
    fn trace_evaluation(&self, db: &DbIndex, id: DerivationId, reason: Reason) {
        if crate::profile::tracing_evaluations() {
            self.trace(
                db,
                id,
                &format!("evaluate reason={reason:?}"),
                &self.moved_reads(db, id),
            );
        }
    }

    /// `id`'s recorded reads that hold neither as they are nor with the edits
    /// mapped back, for the trace.
    fn moved_reads(&self, db: &DbIndex, id: DerivationId) -> Vec<String> {
        let graph = db.reader_graph();
        graph
            .reads_of(id)
            .zip(graph.recorded_slots(id))
            .filter(|(key, (_, seen))| {
                !key.is_stage_rule()
                    && *seen != self.fingerprint(db, key)
                    && (self.edits.is_empty() || *seen != self.fingerprint_before_edits(db, key))
            })
            .map(|(key, _)| format!("{:?}", key.key()))
            .collect()
    }

    /// Prints one traced event of a derivation outside the files being
    /// re-analysed.
    fn trace(&self, db: &DbIndex, id: DerivationId, event: &str, moved: &[String]) {
        if self.focus.contains(&id.file_id) {
            return;
        }
        let path = db
            .get_vfs()
            .get_file_path(&id.file_id)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let queued_by = self
            .queued_by
            .get(&id)
            .map(|(key, writer)| format!("{:?} by {writer:?}", key.key()));
        eprintln!(
            "[trace] {event} {id:?} {path} stage={:?} queued_by={queued_by:?} moved={moved:?}",
            db.analysis_stage()
        );
    }

    /// Evaluates `first` with every derivation of its kind queued right after
    /// it that has to run, computing them concurrently: none reads what
    /// another writes, so each sees what it would have run alone.
    fn run_batch(&mut self, db: &mut DbIndex, first: DerivationId, recipe: &dyn Recipe) {
        let mut batch = vec![first];
        let mut alone = Vec::new();
        while let Some((id, reason)) = self.worklist.pop_first_of(first.kind) {
            if !self.needs_evaluation(db, id, reason) {
                continue;
            }
            self.trace_evaluation(db, id, reason);
            if co_producers(db, id).len() == 1 {
                batch.push(id);
            } else {
                alone.push(id);
            }
        }
        let reruns = batch
            .iter()
            .filter(|member| self.stats.distinct.contains(member))
            .count();
        for member in &batch {
            self.unqueue(*member);
            self.stats.evaluated(*member);
        }
        self.evaluated
            .extend(batch.iter().map(|member| member.file_id));
        let started = self.stats.profiling().then(std::time::Instant::now);
        let mut touched = self.reset_outputs(
            db,
            &batch,
            &FxHashSet::default(),
            &FxHashSet::default(),
            None,
        );
        let profiling = self.stats.profiling();
        let computed = {
            let db: &DbIndex = db;
            let by_file = batch
                .iter()
                .map(|member| (member.file_id, *member))
                .collect::<FxHashMap<_, _>>();
            let files = batch
                .iter()
                .map(|member| member.file_id)
                .collect::<Vec<_>>();
            super::parallel::map_files_collect(db, &files, |db, file_id| {
                let id = by_file[&file_id];
                derivation::begin(id);
                let started = profiling.then(std::time::Instant::now);
                let computed = recipe.compute(db, id);
                let elapsed = started.map(|started| started.elapsed()).unwrap_or_default();
                let evaluation = derivation::end();
                evaluation
                    .fingerprint_unlent_reads(db.reader_graph(), |key| db.fact_fingerprint(key));
                (evaluation, computed, elapsed)
            })
        };
        let mut computed_reads = check_batch_reads().then(|| {
            computed
                .iter()
                .map(|(evaluation, _, _)| {
                    evaluation
                        .unlent_reads()
                        .map(|key| (key.clone(), db.fact_fingerprint(key)))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        });
        let mut times = Vec::with_capacity(batch.len());
        let evaluations: Vec<(derivation::Evaluation, Option<u64>)> =
            self.with_lent_keys(db, &touched, |db| {
                batch
                    .iter()
                    .zip(computed)
                    .enumerate()
                    .map(|(position, (member, (evaluation, computed, computing)))| {
                        let reads = computed_reads.as_mut().map(|reads| &mut reads[position]);
                        if let Some(reads) = &reads {
                            assert_batch_reads_held(db, *member, reads);
                        }
                        derivation::resume(evaluation);
                        let started = profiling.then(std::time::Instant::now);
                        recipe.apply(db, *member, computed);
                        let applying = started.map(|started| started.elapsed()).unwrap_or_default();
                        times.push((*member, computing + applying));
                        let evaluation = derivation::end();
                        if let Some(reads) = reads {
                            for (key, seen) in reads.iter_mut() {
                                *seen = db.fact_fingerprint(key);
                            }
                        }
                        (evaluation, None)
                    })
                    .collect()
            });
        for (member, elapsed) in times {
            self.stats.timed_evaluation(member, elapsed);
        }
        for (member, reads) in batch.iter().zip(computed_reads.iter().flatten()) {
            assert_batch_reads_held(db, *member, reads);
        }
        self.record(db, evaluations, &mut touched, &FxHashSet::default(), || {
            format!("evaluating {batch:?}")
        });
        if let Some(started) = started {
            self.stats
                .timed(first.kind, started.elapsed(), reruns, batch.len());
        }
        // A member that now shares a key with another derivation ran without
        // it; the next evaluation takes both.
        for member in &batch {
            let widened = db.reader_graph().outputs_of(*member).any(|key| {
                key.is_primary()
                    && db.reader_graph().producers(key).any(|producer| {
                        producer.kind != DerivationKind::Source && producer != *member
                    })
            });
            if widened {
                self.stats.requeue_cause(*member, "batch member widened");
                self.worklist.insert(*member, Reason::Changed);
            }
        }
        self.readers_of(db, touched.iter());
        for id in alone {
            self.stats
                .requeue_cause(id, "co-producers left out of a batch");
            self.worklist.insert(id, Reason::Changed);
        }
    }

    /// Evaluates `first` with the queued derivations of its kind that have to
    /// run, computing each on a pool worker against the canonical index and
    /// publishing them in queue order on this thread. A result whose reads no
    /// longer hold, or that read another member's reset output, is discarded
    /// and evaluated here instead. Returns false, with the queue restored, when
    /// too few run to pay for the workers: `first` then runs as usual.
    fn run_recorded_batch(
        &mut self,
        db: &mut DbIndex,
        first: DerivationId,
        recipe: &dyn Recipe,
    ) -> bool {
        let mut batch = vec![first];
        let mut popped = Vec::new();
        while let Some((id, reason)) = self.worklist.pop_first_of(first.kind) {
            if !self.needs_evaluation(db, id, reason) {
                continue;
            }
            let batched = co_producers(db, id).len() == 1;
            if batched {
                batch.push(id);
            }
            popped.push((id, reason, batched));
        }
        if super::parallel::runs_inline(batch.len()) {
            for (id, reason, _) in popped {
                self.worklist.insert(id, reason);
            }
            return false;
        }
        let mut alone = Vec::new();
        for (id, reason, batched) in popped {
            self.trace_evaluation(db, id, reason);
            if !batched {
                alone.push(id);
            }
        }
        let owners = batch
            .iter()
            .flat_map(|id| {
                db.reader_graph()
                    .outputs_of(*id)
                    .map(|key| (key.clone(), *id))
            })
            .collect::<FxHashMap<_, _>>();
        let profiling = self.stats.profiling();
        let started = profiling.then(std::time::Instant::now);
        let reruns = batch
            .iter()
            .filter(|id| self.stats.distinct.contains(id))
            .count();
        for id in &batch {
            self.unqueue(*id);
            self.stats.evaluated(*id);
            self.evaluated.insert(id.file_id);
        }
        let reset = self.reset_outputs(
            db,
            &batch,
            &FxHashSet::default(),
            &FxHashSet::default(),
            None,
        );
        let mut touched = reset.clone();
        let recomputing = self.lend_evaluation_keys(db, &touched);
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        touched.extend(file_queries(db));
        for key in &touched {
            db.reader_graph().forget_fingerprint(key);
        }
        let quarantine = db.reader_graph_mut().quarantine_releases();
        let paths = crate::semantic::take_access_path_snapshot();
        let computed = {
            let snapshot = derivation::RecordingSnapshot::new(
                db.reader_graph_shared(),
                quarantine.started(),
                db.queries().ready_outputs(),
            );
            let by_file = batch
                .iter()
                .map(|id| (id.file_id, *id))
                .collect::<FxHashMap<_, _>>();
            let files = batch.iter().map(|id| id.file_id).collect::<Vec<_>>();
            super::parallel::map_files_collect(db, &files, |db, file_id| {
                let id = by_file[&file_id];
                let started = profiling.then(std::time::Instant::now);
                let (result, additions) =
                    crate::semantic::with_access_paths(paths.as_ref(), || {
                        derivation::compute_recorded(
                            &snapshot,
                            id,
                            || recipe.compute(db, id),
                            |key| {
                                db.reader_graph()
                                    .current_fingerprint(key, |key| db.fact_fingerprint(key))
                            },
                        )
                    });
                (
                    (
                        result,
                        started.map(|started| started.elapsed()).unwrap_or_default(),
                    ),
                    additions,
                )
            })
        };
        let (computed, additions): (Vec<_>, Vec<_>) = computed.into_iter().unzip();
        crate::semantic::restore_access_path_snapshot(paths, additions);
        for (id, (result, computing)) in batch.iter().zip(computed) {
            let started = profiling.then(std::time::Instant::now);
            let valid = result.evaluation.validate(db.reader_graph(), |key, slot| {
                if matches!(
                    key.key(),
                    FactKey::ReceiverAuthority(_) | FactKey::CallbackParams(_)
                ) {
                    db.queries().ready_fingerprint(key)
                } else if let Some(slot) = slot {
                    Some(
                        db.reader_graph()
                            .slot_fingerprint(slot, |key| db.fact_fingerprint(key)),
                    )
                } else {
                    Some(self.fingerprint(db, key))
                }
            }) && result
                .evaluation
                .read_keys(db.reader_graph())
                .all(|key| !reset.contains(key) || owners.get(key) == Some(id));
            if profiling {
                self.stats.parallel_attempts += 1;
            }
            let mut table = db.reader_graph_mut().lend_keys();
            let evaluation = if valid {
                #[cfg(test)]
                WORKER_PUBLISHED.with(|published| published.set(published.get() + 1));
                let evaluation = result
                    .evaluation
                    .canonicalise(&mut table)
                    .expect("validated worker slots");
                derivation::lend_keys(table);
                let lent = scopeguard_lent_keys();
                derivation::resume(evaluation);
                recipe.apply(db, *id, result.value);
                let evaluation = derivation::end();
                db.reader_graph_mut().restore_keys(derivation::take_keys());
                std::mem::forget(lent);
                evaluation
            } else {
                if profiling {
                    self.stats.parallel_fallbacks += 1;
                }
                recipe.discard(*id, result.value);
                derivation::lend_keys(table);
                let lent = scopeguard_lent_keys();
                derivation::begin(*id);
                recipe.evaluate(db, *id);
                let evaluation = derivation::end();
                db.reader_graph_mut().restore_keys(derivation::take_keys());
                std::mem::forget(lent);
                evaluation
            };
            let mut published = FxHashSet::default();
            self.record(
                db,
                vec![(evaluation, None)],
                &mut published,
                &FxHashSet::default(),
                || format!("publishing {id:?}"),
            );
            touched.extend(published);
            self.stats.timed_evaluation(
                *id,
                computing + started.map(|started| started.elapsed()).unwrap_or_default(),
            );
        }
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let lent = scopeguard_lent_keys();
        derivation::note_recomputed(|key| recomputing.get(key) == Some(&db.fact_fingerprint(key)));
        let mut table = derivation::take_keys();
        std::mem::forget(lent);
        quarantine.finish(&mut table);
        db.reader_graph_mut().restore_keys(table);
        if let Some(started) = started {
            self.stats
                .timed(first.kind, started.elapsed(), reruns, batch.len());
        }
        self.readers_of(db, touched.iter());
        for id in alone {
            self.worklist.insert(id, Reason::Changed);
        }
        true
    }

    /// Resets the primary outputs of `members` to the source layer. Returns
    /// every key that moved or may move: their previous outputs.
    /// Resets `members`' primary outputs and the primary keys in `also`, but
    /// `deferred`, and takes the fingerprints each of them had before. Returns
    /// them with what the reset wrote, which `written` also takes.
    fn reset_outputs(
        &self,
        db: &mut DbIndex,
        members: &[DerivationId],
        also: &FxHashSet<HashedKey>,
        deferred: &FxHashSet<HashedKey>,
        written: Option<&mut FxHashSet<HashedKey>>,
    ) -> FxHashSet<HashedKey> {
        let mut touched = also.clone();
        for member in members {
            touched.extend(db.reader_graph().outputs_of(*member).cloned());
        }
        *self.recomputing.borrow_mut() = touched
            .iter()
            .map(|key| (key.clone(), self.fingerprint(db, key)))
            .collect();
        let reset = derivation::capture_writes(|| {
            for key in touched
                .iter()
                .filter(|key| key.is_primary() && !deferred.contains(*key))
            {
                db.reset_fact(key);
            }
        });
        if let Some(written) = written {
            written.extend(reset.iter().cloned());
        }
        touched.extend(reset);
        self.verify_fingerprints(db, &touched, || format!("resetting {members:?}"));
        touched
    }

    /// Runs `f` with the reader graph's key table lent to this thread.
    /// `touched` moved before the table was lent: memos that read them are
    /// stale from here on, except that a reset output which comes back
    /// unchanged reads as it did before.
    fn with_lent_keys<T>(
        &self,
        db: &mut DbIndex,
        touched: &FxHashSet<HashedKey>,
        f: impl FnOnce(&mut DbIndex) -> T,
    ) -> T {
        let recomputing = self.lend_evaluation_keys(db, touched);
        let lent = scopeguard_lent_keys();
        let evaluations = f(db);
        derivation::note_recomputed(|key| recomputing.get(key) == Some(&db.fact_fingerprint(key)));
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        std::mem::forget(lent);
        evaluations
    }

    fn lend_evaluation_keys(
        &self,
        db: &mut DbIndex,
        touched: &FxHashSet<HashedKey>,
    ) -> FxHashMap<HashedKey, derivation::Fingerprint> {
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let recomputing = std::mem::take(&mut *self.recomputing.borrow_mut());
        derivation::note_recomputing(recomputing.keys().cloned());
        derivation::note_written(
            touched
                .iter()
                .filter(|key| !recomputing.contains_key(*key))
                .cloned()
                .chain(self.moved_rules.borrow_mut().drain(..)),
        );
        recomputing
    }

    /// Resets the group's primary outputs and re-runs it. Returns every key the
    /// group touched: its previous outputs and its new ones.
    ///
    /// A key two members write is reset before any of them runs, and read
    /// inside the group as the group leaves it: the group re-runs as a unit,
    /// so what a member read of it is internal to the group, not a stale
    /// input. A key one later member writes alone is reset only when that
    /// member runs, so an earlier member reads it as that member last left it
    /// and is recorded as having seen that: members settle as they would one
    /// by one, whatever grouped them. Which keys two members write comes from
    /// their records; a run that shows two members writing a key not reset
    /// before either ran is discarded, and the group runs again with it reset
    /// first, along with everything the discarded run wrote.
    fn evaluate_group(
        &self,
        db: &mut DbIndex,
        group: &[DerivationId],
        recipe: &dyn Recipe,
    ) -> FxHashSet<HashedKey> {
        let mut touched = FxHashSet::default();
        // Keys runs of this evaluation showed two members writing, and what
        // the discarded runs wrote.
        let mut shared = FxHashSet::default();
        let mut discarded = FxHashSet::default();
        loop {
            let (own, mut reset) = group_resets(db, group, &shared, &discarded);
            let deferred = own.iter().flatten().cloned().collect::<FxHashSet<_>>();
            let grouped = group.len() > 1;
            touched.extend(self.reset_outputs(
                db,
                group,
                &discarded,
                &deferred,
                grouped.then_some(&mut reset),
            ));
            let mut cleared = FxHashSet::default();
            let (evaluations, found) = self.with_lent_keys(db, &touched, |db| {
                run_group(db, group, own, &reset, recipe, &mut cleared)
            });
            touched.extend(cleared);
            if found.is_empty() {
                crate::profile::phase("engine: record", || {
                    self.record(db, evaluations, &mut touched, &reset, || {
                        format!("evaluating {group:?}")
                    })
                });
                return touched;
            }
            for (evaluation, _) in &evaluations {
                discarded.extend(evaluation.outputs().chain(evaluation.moved()).cloned());
            }
            touched.extend(discarded.iter().cloned());
            shared.extend(found);
        }
    }

    /// Replaces the records of `evaluations`, fingerprinting their reads as
    /// they stand now, and adds their outputs to `touched`.
    ///
    /// A group member others ran after carries the stamp it ended at: a read
    /// of what they moved since is recorded as unseen (see
    /// [`derivation::ReaderGraph::seen_by_member`]), `reset` holding the keys
    /// reset before the group ran.
    fn record(
        &self,
        db: &mut DbIndex,
        evaluations: Vec<(derivation::Evaluation, Option<u64>)>,
        touched: &mut FxHashSet<HashedKey>,
        reset: &FxHashSet<HashedKey>,
        step: impl Fn() -> String,
    ) {
        touched.extend(file_queries(db));
        for key in touched.iter().chain(
            evaluations
                .iter()
                .flat_map(|(evaluation, _)| evaluation.outputs().chain(evaluation.moved())),
        ) {
            db.reader_graph().forget_fingerprint(key);
            forget_before_edits(&mut self.before_edits.borrow_mut(), db.reader_graph(), key);
        }
        self.verify_fingerprints(db, touched, step);
        // Out of the index while its records are replaced, so the reads can be
        // fingerprinted against it.
        let mut graph = std::mem::take(db.reader_graph_mut());
        let mut released = Vec::new();
        for (evaluation, ended) in evaluations {
            touched.extend(evaluation.moved().cloned());
            let (id, mut slots, reads, outputs) = evaluation.into_recorded();
            slots.extend(reads.into_iter().map(|key| graph.intern_read(key)));
            // A resumed evaluation read some keys both before and after the
            // table was lent.
            slots.sort_unstable();
            slots.dedup();
            touched.extend(outputs.iter().cloned());
            let slots = match ended {
                Some(ended) => graph.seen_by_member(slots, ended, reset),
                None => slots.into_iter().map(|slot| (slot, None)).collect(),
            };
            graph.replace_slots(
                id,
                slots,
                outputs,
                |key| db.fact_fingerprint(key),
                &mut released,
            );
        }
        graph.release_keys(released);
        *db.reader_graph_mut() = graph;
    }
}

/// What the engine evaluated, for the profile and for tests that pin how much
/// work an operation costs.
#[derive(Debug, Clone, Default)]
pub(crate) struct EngineStats {
    parallel_attempts: usize,
    parallel_fallbacks: usize,
    evaluations: BTreeMap<DerivationKind, usize>,
    /// Rounds after the first.
    pub(crate) rounds: usize,
    /// Loops of statements cleared, and the derivations they ran again.
    pub(crate) loops_cleared: usize,
    pub(crate) loop_members_cleared: usize,
    /// Source layers rebuilt because a fact they read moved.
    pub(crate) rebuilt_sources: usize,
    /// Under `GLUALS_PROFILE`: time per kind, the part of it spent
    /// re-evaluating derivations already evaluated, and how often a key's move
    /// queued a reader, by key variant and the writer's and reader's kinds.
    time: BTreeMap<DerivationKind, std::time::Duration>,
    rerun_time: BTreeMap<DerivationKind, std::time::Duration>,
    requeues: BTreeMap<(String, Option<DerivationKind>, DerivationKind), usize>,
    /// Recorded reads by key variant.
    reads: BTreeMap<String, usize>,
    /// The derivations each stage barrier queued.
    stages: Vec<(AnalysisStage, usize)>,
    /// Under `GLUALS_PROFILE`: every derivation evaluated, to count repeats.
    distinct: FxHashSet<DerivationId>,
    /// Under `GLUALS_PROFILE`: each derivation's last evaluation time, and
    /// why it was queued again since.
    last_times: FxHashMap<DerivationId, std::time::Duration>,
    causes: FxHashMap<DerivationId, (String, Option<DerivationKind>)>,
    /// Under `GLUALS_PROFILE`: the time of evaluations a later evaluation of
    /// the same derivation superseded, by what queued the later one (the
    /// moved key's variant and its writer's kind) and the reader's kind.
    superseded:
        BTreeMap<(String, Option<DerivationKind>, DerivationKind), (std::time::Duration, usize)>,
}

fn variant_name(key: &FactKey) -> String {
    if let FactKey::DynamicField(_, None) = key {
        return "DynamicField(wildcard)".to_string();
    }
    let name = format!("{key:?}");
    name.split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_string()
}

impl EngineStats {
    fn profiling(&self) -> bool {
        crate::profile::profiling()
    }

    /// Adds `elapsed` to `kind`'s time: `reruns` of the `evaluated`
    /// derivations it covers had been evaluated before.
    fn timed(
        &mut self,
        kind: DerivationKind,
        elapsed: std::time::Duration,
        reruns: usize,
        evaluated: usize,
    ) {
        *self.time.entry(kind).or_default() += elapsed;
        if reruns > 0 {
            *self.rerun_time.entry(kind).or_default() +=
                elapsed.mul_f64(reruns as f64 / evaluated.max(1) as f64);
        }
    }

    /// Under `GLUALS_PROFILE`, prints each round's start as it happens, so a
    /// run that settles slowly shows where its rounds go.
    fn round_started(&self, queued: usize) {
        if self.profiling() {
            eprintln!(
                "[engine] round {} starts with {queued} queued after {} evaluations",
                self.rounds,
                self.total_evaluations()
            );
        }
    }

    /// Notes a stage barrier that queued `queued` derivations; under
    /// `GLUALS_PROFILE` prints it as it happens.
    fn stage_started(&mut self, stage: AnalysisStage, queued: usize) {
        self.stages.push((stage, queued));
        if self.profiling() {
            eprintln!(
                "[engine] stage {stage:?} queues {queued} after {} evaluations",
                self.total_evaluations()
            );
        }
    }

    /// Under `GLUALS_PROFILE`, prints how many waiting readers the settled
    /// focus released.
    fn focus_settled(&self, queued: usize) {
        if self.profiling() {
            eprintln!(
                "[engine] focus settled after {} evaluations, releasing {queued}",
                self.total_evaluations()
            );
        }
    }

    fn requeued(
        &mut self,
        key: &FactKey,
        writer: Option<DerivationId>,
        reader: DerivationId,
        unproduced: bool,
    ) {
        if !self.profiling() {
            return;
        }
        let variant = variant_name(key);
        let across = match (writer, unproduced) {
            (Some(writer), false) if writer.file_id == reader.file_id => "in its file",
            (Some(writer), true) if writer.file_id == reader.file_id => "in its file, unproduced",
            (Some(_), false) => "from another file",
            (Some(_), true) => "from another file, unproduced",
            (None, _) => "outside an evaluation",
        };
        let writer = writer.map(|writer| writer.kind);
        self.causes
            .entry(reader)
            .or_insert_with(|| (format!("{variant} {across}"), writer));
        *self
            .requeues
            .entry((variant, writer, reader.kind))
            .or_default() += 1;
    }

    /// Under `GLUALS_PROFILE`, notes that a stage barrier queued `reader`.
    fn stage_requeued(&mut self, reader: DerivationId) {
        self.requeue_cause(reader, "stage barrier");
    }

    /// Under `GLUALS_PROFILE`, notes what queued `id` again when no moved
    /// key did.
    fn requeue_cause(&mut self, id: DerivationId, cause: &'static str) {
        if self.profiling() {
            self.causes
                .entry(id)
                .or_insert_with(|| (cause.to_string(), None));
        }
    }

    /// Under `GLUALS_PROFILE`, notes that `id` was evaluated in `elapsed`:
    /// its previous evaluation, if any, was superseded, and is charged to
    /// what queued it again.
    fn timed_evaluation(&mut self, id: DerivationId, elapsed: std::time::Duration) {
        if !self.profiling() {
            return;
        }
        let cause = self
            .causes
            .remove(&id)
            .unwrap_or_else(|| ("unattributed".to_string(), None));
        if let Some(previous) = self.last_times.insert(id, elapsed) {
            let entry = self
                .superseded
                .entry((cause.0, cause.1, id.kind))
                .or_default();
            entry.0 += previous;
            entry.1 += 1;
        }
    }

    fn evaluated(&mut self, id: DerivationId) {
        *self.evaluations.entry(id.kind).or_default() += 1;
        if self.profiling() {
            self.distinct.insert(id);
        }
    }

    /// How many times derivations of `kind` were evaluated.
    #[cfg(test)]
    pub(crate) fn evaluations(&self, kind: DerivationKind) -> usize {
        self.evaluations.get(&kind).copied().unwrap_or(0)
    }

    pub(crate) fn total_evaluations(&self) -> usize {
        self.evaluations.values().sum()
    }

    /// Prints the counts under `GLUALS_PROFILE`, with how many files had
    /// derivations evaluated or their source layer rebuilt.
    pub(crate) fn report(&self, files: usize) {
        if !self.profiling() || self.evaluations.is_empty() {
            return;
        }
        let kinds = self
            .evaluations
            .iter()
            .map(|(kind, count)| format!("{kind:?}={count}"))
            .collect::<Vec<_>>();
        eprintln!(
            "[engine] evaluations={} rounds={} rebuilt sources={} loops cleared={} ({} derivations) files={files} {}",
            self.total_evaluations(),
            self.rounds,
            self.rebuilt_sources,
            self.loops_cleared,
            self.loop_members_cleared,
            kinds.join(" ")
        );
        eprintln!(
            "[engine] parallel attempts={} fallbacks={}",
            self.parallel_attempts, self.parallel_fallbacks
        );
        let mut distinct = BTreeMap::<DerivationKind, usize>::new();
        for id in &self.distinct {
            *distinct.entry(id.kind).or_default() += 1;
        }
        eprintln!(
            "[engine] distinct={} {}",
            self.distinct.len(),
            distinct
                .iter()
                .map(|(kind, count)| format!("{kind:?}={count}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut time = self.time.iter().collect::<Vec<_>>();
        time.sort_by(|left, right| right.1.cmp(left.1));
        for (kind, time) in time {
            eprintln!(
                "[engine] time {kind:?} {time:?} rerun {:?} evaluations {} distinct {}",
                self.rerun_time.get(kind).copied().unwrap_or_default(),
                self.evaluations.get(kind).copied().unwrap_or(0),
                distinct.get(kind).copied().unwrap_or(0)
            );
        }
        let mut reads = self.reads.iter().collect::<Vec<_>>();
        reads.sort_by(|left, right| right.1.cmp(left.1));
        let total = reads.iter().map(|(_, count)| **count).sum::<usize>();
        eprintln!(
            "[engine] recorded reads {total}, noted {}",
            derivation::take_noted_reads()
        );
        for (variant, count) in reads.into_iter().take(25) {
            eprintln!("[engine] reads {count} {variant}");
        }
        derivation::report_frames();
        let mut requeues = self.requeues.iter().collect::<Vec<_>>();
        requeues.sort_by(|left, right| right.1.cmp(left.1));
        for ((variant, writer, reader), count) in requeues.into_iter().take(60) {
            let back = if writer.is_some_and(|writer| reader.rank() < writer.rank()) {
                " back"
            } else {
                ""
            };
            eprintln!("[engine] queued {count} {variant} {writer:?} -> {reader:?}{back}");
        }
        let superseded = self
            .superseded
            .values()
            .map(|(time, _)| *time)
            .sum::<std::time::Duration>();
        let count = self
            .superseded
            .values()
            .map(|(_, count)| count)
            .sum::<usize>();
        let last = self.last_times.values().sum::<std::time::Duration>();
        eprintln!(
            "[engine] superseded {superseded:?} over {count} evaluations; last evaluations {last:?}"
        );
        let mut by_cause = BTreeMap::<&str, (std::time::Duration, usize)>::new();
        for ((cause, _, _), (time, count)) in &self.superseded {
            let entry = by_cause.entry(cause.as_str()).or_default();
            entry.0 += *time;
            entry.1 += count;
        }
        let mut by_cause = by_cause.into_iter().collect::<Vec<_>>();
        by_cause.sort_by_key(|(_, (time, _))| std::cmp::Reverse(*time));
        for (cause, (time, count)) in by_cause {
            eprintln!("[engine] superseded by cause {time:?} over {count}: {cause}");
        }
        let mut rows = self.superseded.iter().collect::<Vec<_>>();
        rows.sort_by_key(|(_, (time, _))| std::cmp::Reverse(*time));
        for ((cause, writer, reader), (time, count)) in rows.into_iter().take(40) {
            eprintln!(
                "[engine] superseded {time:?} over {count}: {cause} by {writer:?} -> {reader:?}"
            );
        }
    }
}

/// `id` and every derivation that shares a primary output key with it,
/// transitively, in canonical order.
/// Which keys a run of `group` resets when: each later member's primary
/// outputs no other member writes (by the records, and not in `shared`), reset
/// when it runs; and, for a group of several, the keys reset before any member
/// runs, the rest of their primary outputs and those in `discarded`.
fn group_resets(
    db: &DbIndex,
    group: &[DerivationId],
    shared: &FxHashSet<HashedKey>,
    discarded: &FxHashSet<HashedKey>,
) -> (Vec<Vec<HashedKey>>, FxHashSet<HashedKey>) {
    let graph = db.reader_graph();
    let own = group
        .iter()
        .enumerate()
        .map(|(position, member)| {
            if position == 0 {
                return Vec::new();
            }
            graph
                .outputs_of(*member)
                .filter(|key| {
                    key.is_primary()
                        && !shared.contains(*key)
                        && !discarded.contains(*key)
                        && graph.producers(key).all(|producer| {
                            producer == *member || producer.kind == DerivationKind::Source
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut reset = FxHashSet::default();
    if group.len() > 1 {
        let deferred = own.iter().flatten().collect::<FxHashSet<_>>();
        reset.extend(
            group
                .iter()
                .flat_map(|member| graph.outputs_of(*member))
                .chain(discarded)
                .filter(|key| key.is_primary() && !deferred.contains(key))
                .cloned(),
        );
    }
    (own, reset)
}

/// Runs `group`'s members in order, each resetting its `own` keys first. Stops
/// at the first sign that two members write a primary key not `reset` before
/// either ran: a member's own key an earlier one wrote, or a key two members
/// wrote. Returns the evaluations, each but the last with the stamp it ended
/// at, and the keys that stopped it (none if it ran through).
fn run_group(
    db: &mut DbIndex,
    group: &[DerivationId],
    own: Vec<Vec<HashedKey>>,
    reset: &FxHashSet<HashedKey>,
    recipe: &dyn Recipe,
    cleared: &mut FxHashSet<HashedKey>,
) -> (
    Vec<(derivation::Evaluation, Option<u64>)>,
    FxHashSet<HashedKey>,
) {
    let mut evaluations = Vec::with_capacity(group.len());
    // Each primary key written that was not reset before the group, with the
    // position of the member that wrote it first.
    let mut writers = FxHashMap::<HashedKey, usize>::default();
    for (position, (member, own)) in group.iter().zip(own).enumerate() {
        let taken = own
            .iter()
            .filter(|key| writers.contains_key(*key))
            .cloned()
            .collect::<FxHashSet<_>>();
        if !taken.is_empty() {
            return (evaluations, taken);
        }
        if !own.is_empty() {
            let moved = derivation::capture_writes(|| {
                for key in &own {
                    db.reset_fact(key);
                }
            });
            derivation::note_written(moved.iter().cloned());
            cleared.extend(moved);
        }
        derivation::begin(*member);
        recipe.evaluate(db, *member);
        let evaluation = derivation::end();
        let mut found = FxHashSet::default();
        for key in evaluation
            .outputs()
            .filter(|key| key.is_primary() && !reset.contains(*key))
        {
            let first = *writers.entry(key.clone()).or_insert(position);
            if first != position {
                found.insert(key.clone());
            }
        }
        let ended = (position + 1 < group.len()).then(derivation::new_stamp);
        evaluations.push((evaluation, ended));
        if !found.is_empty() {
            return (evaluations, found);
        }
    }
    (evaluations, FxHashSet::default())
}

/// Files the reads of the queries computed since this last ran, which makes
/// each a reader of what it read. Returns the outputs their computation moved.
/// Drops what `key` and every key read out of it held before the edits.
fn forget_before_edits(
    before_edits: &mut FxHashMap<HashedKey, derivation::Fingerprint>,
    graph: &derivation::ReaderGraph,
    key: &HashedKey,
) {
    if before_edits.is_empty() {
        return;
    }
    before_edits.remove(key);
    for derived in graph.derived_keys(key) {
        before_edits.remove(&derived);
    }
}

pub(super) fn file_queries(db: &mut DbIndex) -> Vec<HashedKey> {
    if !db.queries().has_pending() {
        return Vec::new();
    }
    let filings = db.queries().take_pending();
    let graph = db.reader_graph_mut();
    let mut moved = Vec::new();
    let mut released = Vec::new();
    for filing in filings {
        if let Some(key) = filing.moved {
            graph.forget_fingerprint(&key);
            moved.push(key);
        }
        graph.file_viewed(
            filing.id,
            derivation::ViewedReads::from_keys(filing.reads),
            &mut released,
        );
    }
    graph.release_keys(released);
    moved
}

/// Panics when `member` computed from a fact that moved since `reads` were
/// fingerprinted, which only another member's apply writes: the record
/// fingerprints reads once every member applied, so the member would hold a
/// result computed from a value it never saw.
fn assert_batch_reads_held(
    db: &DbIndex,
    member: DerivationId,
    reads: &[(HashedKey, derivation::Fingerprint)],
) {
    for (key, seen) in reads {
        assert!(
            db.fact_fingerprint(key) == *seen,
            "{member:?} computed from {key:?}, which another member of its batch moved"
        );
    }
}

fn co_producers(db: &DbIndex, id: DerivationId) -> Vec<DerivationId> {
    let graph = db.reader_graph();
    let mut group = BTreeSet::from([id]);
    let mut pending = vec![id];
    while let Some(member) = pending.pop() {
        for key in graph.outputs_of(member).filter(|key| key.is_primary()) {
            // The source layer is what a reset restores, not a co-producer.
            for producer in graph
                .producers(key)
                .filter(|producer| producer.kind != DerivationKind::Source)
            {
                if group.insert(producer) {
                    pending.push(producer);
                }
            }
        }
    }
    group.into_iter().collect()
}

/// Runs its function when dropped, including while unwinding.
struct OnDrop(fn());

impl Drop for OnDrop {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Forgets the lent key table if evaluating a group unwinds.
fn scopeguard_lent_keys() -> OnDrop {
    OnDrop(derivation::forget_lent_keys)
}

/// `GLUALS_CHECK_FINGERPRINTS`: recompute every cached fingerprint and panic on
/// a key that moved without being written.
/// How often one derivation may be evaluated within one stage before
/// [`Engine::check_convergence`] reports that the facts it reads do not settle.
const MAX_STAGE_EVALUATIONS: u32 = 1000;

#[cfg(test)]
thread_local! {
    /// Worker results this thread published, for tests that must see
    /// [`Engine::run_recorded_batch`] publish.
    static WORKER_PUBLISHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn check_convergence() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_CONVERGENCE").is_some())
}

fn check_fingerprints() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_FINGERPRINTS").is_some())
}

/// Under debug assertions or `GLUALS_CHECK_FINGERPRINTS`: whether a batch
/// checks that no member computed from a fact another member moved.
fn check_batch_reads() -> bool {
    cfg!(debug_assertions) || check_fingerprints()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use internment::ArcIntern;
    use smol_str::SmolStr;

    use super::super::parallel;
    use super::*;
    use crate::{
        FileId, LuaDeclId, LuaType, LuaTypeCache, LuaTypeOwner, TypeOps, VirtualWorkspace,
    };

    const FILE: u32 = 90_000;
    const COPIED: u32 = 1;
    const SHARED: u32 = 2;
    const WRITTEN: u32 = 3;

    struct RecordedTargets {
        blocked: FileId,
        applied: Mutex<Vec<(FileId, bool)>>,
        discarded: std::sync::atomic::AtomicUsize,
    }

    impl Recipe for RecordedTargets {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            self.applied.lock().unwrap().push((id.file_id, false));
            db.get_call_target_index_mut()
                .set_file(id.file_id, Default::default());
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn records_on_workers(&self, kind: DerivationKind) -> bool {
            kind == DerivationKind::CallTargets
        }

        fn compute(&self, _: &DbIndex, id: DerivationId) -> Box<dyn std::any::Any + Send> {
            if id.file_id == self.blocked {
                derivation::require_canonical_query();
            }
            Box::new(id.file_id)
        }

        fn apply(&self, db: &mut DbIndex, id: DerivationId, value: Box<dyn std::any::Any + Send>) {
            assert_eq!(*value.downcast::<FileId>().unwrap(), id.file_id);
            self.applied.lock().unwrap().push((id.file_id, true));
            db.get_call_target_index_mut()
                .set_file(id.file_id, Default::default());
        }

        fn discard(&self, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            self.discarded
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn parallel_recorded_targets_retry_and_restore_unchanged_reset_proofs() {
        use glua_parser::LuaAstNode;

        let mut ws = VirtualWorkspace::new();
        let files = (0..8)
            .map(|index| {
                ws.def_file(
                    &format!("lua/autorun/recorded_targets_{index}.lua"),
                    "local value = 1",
                )
            })
            .collect::<Vec<_>>();
        let db = ws.get_db_mut();
        let ids = files
            .iter()
            .map(|file_id| {
                let root = db
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .unwrap()
                    .get_chunk_node();
                DerivationId::new(
                    DerivationKind::CallTargets,
                    *file_id,
                    LuaSyntaxId::from_node(root.syntax()),
                )
            })
            .collect::<Vec<_>>();
        let proof = {
            derivation::lend_keys(db.reader_graph_mut().lend_keys());
            let lent = scopeguard_lent_keys();
            derivation::begin(statement(0));
            let frame = derivation::open_frame().unwrap();
            derivation::note_read(|| FactKey::CallTargets(files[0]));
            let proof = derivation::close_frame(frame, 0).unwrap();
            derivation::end();
            db.reader_graph_mut().restore_keys(derivation::take_keys());
            std::mem::forget(lent);
            proof
        };
        let recipe = RecordedTargets {
            blocked: files[3],
            applied: Mutex::new(Vec::new()),
            discarded: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        for id in &ids {
            engine.worklist.insert(*id, Reason::Changed);
        }
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        parallel::set_analysis_threads(4);
        while let Some((first, _)) = engine.worklist.pop_first_of(DerivationKind::CallTargets) {
            assert!(engine.run_recorded_batch(db, first, &recipe));
        }
        parallel::set_analysis_threads(0);
        drop(threads);
        assert_eq!(
            recipe.discarded.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let applied = recipe.applied.lock().unwrap();
        assert_eq!(applied.len(), files.len());
        assert_eq!(
            applied.iter().filter(|(_, worker)| *worker).count(),
            files.len() - 1
        );
        assert!(applied.contains(&(files[3], false)));
        assert!(
            files
                .iter()
                .all(|file| db.get_call_target_index().file(*file).is_some())
        );
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let lent = scopeguard_lent_keys();
        derivation::begin(statement(0));
        assert_eq!(
            derivation::reuse(&proof, 0, &[], None),
            derivation::Reuse::Held
        );
        derivation::end();
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        std::mem::forget(lent);
    }

    /// Every recorded derivation with its reads, their fingerprints and its
    /// outputs, each file's call targets, and [`VirtualWorkspace::settled_digest`].
    fn recorded_digest(ws: &mut VirtualWorkspace) -> Vec<String> {
        let mut digest = ws.settled_digest();
        let db = ws.get_db_mut();
        let graph = db.reader_graph();
        let mut ids = graph.derivations().collect::<Vec<_>>();
        ids.sort();
        for id in ids {
            let mut reads = graph
                .reads_of(id)
                .zip(graph.recorded_slots(id))
                .map(|(key, (_, seen))| format!("{:?}={seen:?}", key.key()))
                .collect::<Vec<_>>();
            reads.sort();
            let mut outputs = graph
                .outputs_of(id)
                .map(|key| format!("{:?}", key.key()))
                .collect::<Vec<_>>();
            outputs.sort();
            digest.push(format!("{id:?} reads {reads:?} writes {outputs:?}"));
        }
        let mut targets = db
            .get_call_target_index()
            .file_fingerprints()
            .map(|targets| format!("{targets:?}"))
            .collect::<Vec<_>>();
        targets.sort();
        digest.extend(targets);
        digest
    }

    const INVENTORY: &str = r#"
            Inventory = Inventory or {}
            local META = {}
            META.__index = META
            function META:Add(item, count)
                self.items[item] = (self.items[item] or 0) + count
                return self
            end
            function META:Count(item) return self.items[item] or 0 end
            function Inventory.New(owner)
                return setmetatable({ owner = owner, items = {} }, META)
            end
            function Inventory.Format(value) return tostring(value) end
        "#;

    /// Shops that fill an [`INVENTORY`] and print from it, by path.
    fn inventory_shops() -> Vec<(String, String)> {
        (0..10)
            .map(|index| {
                (
                    format!("lua/autorun/server/sv_shop_{index}.lua"),
                    format!(
                        r#"
                        local inv = Inventory.New("shop{index}")
                        inv:Add("apple", {index}):Add("pear", 1)
                        local total = inv:Count("apple")
                        local label = Inventory.Format(total)
                        local function describe(value) return Inventory.Format(value + {index}) end
                        print(describe(total), label, Inventory.Clear)
                        "#
                    ),
                )
            })
            .collect()
    }

    /// The inventory library with `shops`, by path.
    fn inventory_files(shops: &[(String, String)]) -> Vec<(&str, &str)> {
        std::iter::once(("lua/autorun/sh_inventory.lua", INVENTORY))
            .chain(
                shops
                    .iter()
                    .map(|(path, text)| (path.as_str(), text.as_str())),
            )
            .collect()
    }

    #[test]
    fn recorded_call_targets_match_serial_evaluation_cold_and_after_an_edit() {
        let edited = INVENTORY.replace(
            "function Inventory.Format(value) return tostring(value) end",
            "function Inventory.Format(value) return value * 2 end
            function Inventory.Clear(inv) inv.items = {} end",
        );
        let shops = inventory_shops();
        let files = inventory_files(&shops);
        let edits = [("lua/autorun/sh_inventory.lua", edited.as_str())];
        let published = || WORKER_PUBLISHED.with(std::cell::Cell::get);
        let analyse = |threads: usize| {
            parallel::set_analysis_threads(threads);
            let start = published();
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files.clone());
            let cold = recorded_digest(&mut ws);
            let edit = published();
            for (path, text) in edits {
                ws.def_file(path, text);
            }
            let warm = recorded_digest(&mut ws);
            (cold, warm, edit - start, published() - edit)
        };
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let serial = analyse(1);
        let recorded = analyse(4);
        parallel::set_analysis_threads(0);
        drop(threads);
        assert_eq!((serial.2, serial.3), (0, 0));
        assert!(
            recorded.2 > 0,
            "no cold call targets were published from workers"
        );
        assert!(
            recorded.3 > 0,
            "no call targets were republished from workers after the edit"
        );
        assert_eq!(serial.0, recorded.0);
        assert_eq!(serial.1, recorded.1);
    }

    /// Each shop's dynamic fields infer `print` first, under the collecting
    /// view that reads no dynamic field; its call targets, which see dynamic
    /// fields, must still read the missing global's.
    #[test]
    fn call_targets_read_a_missing_globals_dynamic_field_after_the_collecting_pass() {
        let shops = inventory_shops();
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        parallel::set_analysis_threads(1);
        let mut ws = VirtualWorkspace::new();
        ws.def_files(inventory_files(&shops));
        parallel::set_analysis_threads(0);
        drop(threads);
        let read = FactKey::DynamicField(
            crate::DynamicFieldOwner::GlobalPath(crate::GlobalId::new("print")),
            Some("print".into()),
        );
        let graph = ws.get_db_mut().reader_graph();
        let readers = graph
            .derivations()
            .filter(|id| {
                id.kind == DerivationKind::CallTargets
                    && graph.reads_of(*id).any(|key| *key.key() == read)
            })
            .count();
        assert_eq!(readers, shops.len());
    }

    /// Readers of `REGISTRY[...]` on both sides of the file whose dynamic
    /// fields make `REGISTRY` a wildcard owner: each reader's dynamic fields
    /// infer the entry through that wildcard state, which the registry file's
    /// dynamic fields write in the same batch.
    #[test]
    fn dynamic_fields_reading_a_peers_wildcard_owner_match_serial_and_warm_analysis() {
        let registry = r#"
            REGISTRY = REGISTRY or {}
            function Register(name)
                REGISTRY[name] = { damage = 1 }
            end
            Register("pistol")
        "#;
        let readers = (0..10)
            .map(|index| {
                (
                    format!("lua/autorun/sh_reader_{index}.lua"),
                    format!(
                        "local entry = REGISTRY[\"pistol\"]
entry.extra{index} = {index}
print(entry.extra{index}, entry.damage)"
                    ),
                )
            })
            .collect::<Vec<_>>();
        let mut files = readers
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .chain([("lua/autorun/sh_reader_4_registry.lua", registry)])
            .collect::<Vec<_>>();
        files.sort();
        let published = || WORKER_PUBLISHED.with(std::cell::Cell::get);
        let analyse = |threads: usize| {
            parallel::set_analysis_threads(threads);
            let start = published();
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files.clone());
            (
                ws.settled_digest(),
                recorded_digest(&mut ws),
                published() - start,
            )
        };
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let serial = analyse(1);
        let recorded = analyse(4);
        parallel::set_analysis_threads(1);
        let mut warm = VirtualWorkspace::new();
        for (path, text) in &files {
            warm.def_file(path, text);
        }
        let warm = warm.settled_digest();
        parallel::set_analysis_threads(0);
        drop(threads);
        assert_eq!(serial.2, 0);
        assert!(
            recorded.2 > 0,
            "no dynamic fields were published from workers"
        );
        assert_eq!(serial.1, recorded.1);
        assert_eq!(recorded.0, warm);
    }

    fn statement(start: u32) -> DerivationId {
        DerivationId::new(
            DerivationKind::LocalStat,
            FileId::new(FILE),
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(start), TextSize::new(1)),
            ),
        )
    }

    fn owner(position: u32) -> LuaTypeOwner {
        LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(FILE), TextSize::new(position)))
    }

    fn bind(db: &mut DbIndex, position: u32, typ: LuaType) {
        db.get_type_index_mut()
            .force_bind_type(owner(position), LuaTypeCache::InferType(typ));
    }

    /// Two statements that both write `SHARED`, so they run as one group. The
    /// first copies `WRITTEN`, which only the second writes, and only once it
    /// has a value to write.
    struct Pair {
        written: Mutex<Option<LuaType>>,
    }

    const READER: u32 = 0;
    const WRITER: u32 = 10;

    fn settle(db: &mut DbIndex, engine: &mut Engine, recipe: &dyn Recipe, changed: &[u32]) -> bool {
        for start in changed {
            engine.changed(statement(*start));
        }
        engine.open(db);
        engine.run(db, recipe, Slice::Evaluations(100))
    }

    impl Recipe for Pair {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            if id == statement(READER) {
                let seen = db
                    .get_type_index()
                    .get_type_cache(&owner(WRITTEN))
                    .map_or(LuaType::Unknown, |cache| cache.as_type().clone());
                bind(db, COPIED, seen);
            } else if let Some(written) = self.written.lock().unwrap().clone() {
                bind(db, WRITTEN, written);
            }
            bind(db, SHARED, LuaType::Nil);
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("a pair does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("a pair does not split")
        }
    }

    /// Two statements that both write `SHARED`, so they run as one group. The
    /// second stores its value into `WRITTEN` over what is there; the first
    /// adds to `WRITTEN` only when something is there already.
    struct Push {
        value: Mutex<LuaType>,
    }

    impl Recipe for Push {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let existing = db
                .get_type_index()
                .get_type_cache(&owner(WRITTEN))
                .map(|cache| cache.as_type().clone());
            if id == statement(READER) {
                if let Some(existing) = existing {
                    let pushed = TypeOps::Union.apply(db, &existing, &LuaType::IntegerConst(1));
                    bind(db, WRITTEN, pushed);
                }
            } else {
                let value = self.value.lock().unwrap().clone();
                let stored = match existing {
                    Some(existing) => TypeOps::Union.apply(db, &existing, &value),
                    None => value,
                };
                bind(db, WRITTEN, stored);
            }
            bind(db, SHARED, LuaType::Nil);
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("a push does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("a push does not split")
        }
    }

    fn text(value: &str) -> LuaType {
        LuaType::StringConst(ArcIntern::from(SmolStr::new(value)))
    }

    /// What `WRITTEN` settles to when the writer's value takes each of
    /// `values` in turn.
    fn pushed_after(values: &[LuaType]) -> Option<LuaType> {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let push = Push {
            value: Mutex::new(values[0].clone()),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        settle(db, &mut engine, &push, &[READER, WRITER]);
        for value in &values[1..] {
            *push.value.lock().unwrap() = value.clone();
            settle(db, &mut engine, &push, &[WRITER]);
        }
        db.get_type_index()
            .get_type_cache(&owner(WRITTEN))
            .map(|cache| cache.as_type().clone())
    }

    #[test]
    fn a_member_writing_another_members_fact_settles_as_a_cold_build_would() {
        let edited = pushed_after(&[text("old"), text("mid"), text("new")]);

        assert_eq!(edited, pushed_after(&[text("new")]));
    }

    fn copied(db: &DbIndex) -> Option<LuaType> {
        db.get_type_index()
            .get_type_cache(&owner(COPIED))
            .map(|cache| cache.as_type().clone())
    }

    #[test]
    fn a_group_member_reads_a_later_members_own_fact_as_that_member_leaves_it() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let pair = Pair {
            written: Mutex::new(Some(LuaType::Integer)),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        settle(db, &mut engine, &pair, &[READER, WRITER]);

        *pair.written.lock().unwrap() = Some(LuaType::String);
        let settled = settle(db, &mut engine, &pair, &[WRITER]);

        assert_eq!((settled, copied(db)), (true, Some(LuaType::String)));
    }

    #[test]
    fn a_group_member_reads_again_a_fact_a_later_member_writes_for_the_first_time() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let pair = Pair {
            written: Mutex::new(None),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        settle(db, &mut engine, &pair, &[READER, WRITER]);

        *pair.written.lock().unwrap() = Some(LuaType::String);
        let settled = settle(db, &mut engine, &pair, &[WRITER]);

        assert_eq!((settled, copied(db)), (true, Some(LuaType::String)));
    }
    fn in_file(file: u32) -> DerivationId {
        DerivationId::new(
            DerivationKind::LocalStat,
            FileId::new(file),
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(0), TextSize::new(1)),
            ),
        )
    }

    /// A split kind whose reader in `FILE` computes from `WRITTEN`, which its
    /// writer in the next file applies.
    struct PeerRead;

    impl Recipe for PeerRead {
        fn evaluate(&self, _: &mut DbIndex, _: DerivationId) {
            unreachable!("peer reads run batched")
        }

        fn splits(&self, _: DerivationKind) -> bool {
            true
        }

        fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn std::any::Any + Send> {
            if id == in_file(FILE) {
                Box::new(
                    db.get_type_index()
                        .get_type_cache(&owner(WRITTEN))
                        .map_or(LuaType::Unknown, |cache| cache.as_type().clone()),
                )
            } else {
                Box::new(LuaType::String)
            }
        }

        fn apply(
            &self,
            db: &mut DbIndex,
            id: DerivationId,
            computed: Box<dyn std::any::Any + Send>,
        ) {
            let value = *computed.downcast::<LuaType>().expect("a type");
            bind(
                db,
                if id == in_file(FILE) { COPIED } else { WRITTEN },
                value,
            );
        }
    }

    #[test]
    #[should_panic(expected = "which another member of its batch moved")]
    fn a_batch_member_computing_from_what_another_applies_is_caught() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.changed(in_file(FILE));
        engine.changed(in_file(FILE + 1));
        engine.open(db);
        engine.run(db, &PeerRead, Slice::Evaluations(100));
    }

    struct SlotContributors {
        second: Mutex<bool>,
    }

    fn joined_owner() -> crate::LuaMemberOwner {
        crate::LuaMemberOwner::Type(crate::LuaTypeDeclId::global("RoundJoined"))
    }

    impl Recipe for SlotContributors {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let joined = joined_owner();
            if id == statement(READER) {
                let key = crate::LuaMemberKey::Name("value".into());
                let slot = db
                    .get_member_index()
                    .get_members_for_owner_key(&joined, &key)
                    .len();
                let members = db
                    .get_member_index()
                    .get_members(&joined)
                    .map_or(0, |members| members.len());
                bind(db, COPIED, LuaType::IntegerConst((slot + members) as i64));
            } else {
                let count = db
                    .get_type_index()
                    .get_type_cache(&owner(100 + u32::from(id.syntax_id().get_range().start())))
                    .and_then(|cache| match cache.as_type() {
                        LuaType::IntegerConst(count) => Some(*count),
                        _ => None,
                    })
                    .unwrap_or(0);
                if id != statement(WRITER) && !*self.second.lock().unwrap() {
                    return;
                }
                for offset in 0..count {
                    let syntax = LuaSyntaxId::new(
                        LuaSyntaxKind::NameExpr.into(),
                        TextRange::at(
                            id.syntax_id().get_range().start()
                                + TextSize::new(offset as u32 * 1000),
                            TextSize::new(1),
                        ),
                    );
                    let member = crate::LuaMember::new(
                        crate::LuaMemberId::new(syntax, id.file_id),
                        crate::LuaMemberKey::Name("value".into()),
                        crate::LuaMemberFeature::FileFieldDecl,
                        None,
                    );
                    db.get_member_index_mut().add_member(joined.clone(), member);
                }
            }
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }
        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!()
        }
        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!()
        }
    }

    fn slot_round_counts(batch: bool) -> (Option<LuaType>, usize) {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let recipe = SlotContributors {
            second: Mutex::new(true),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.batch_slot_notifications = batch;
        bind(db, 110, LuaType::IntegerConst(1));
        bind(db, 120, LuaType::IntegerConst(1));
        assert!(settle(db, &mut engine, &recipe, &[READER, WRITER, 20]));
        let answer = copied(db);
        let before = engine.stats.evaluations(DerivationKind::LocalStat);
        bind(db, 110, LuaType::IntegerConst(2));
        bind(db, 120, LuaType::IntegerConst(2));
        let inputs = [
            HashedKey::new(FactKey::TypeCache(owner(110))),
            HashedKey::new(FactKey::TypeCache(owner(120))),
        ];
        for input in &inputs {
            db.reader_graph().forget_fingerprint(input);
        }
        engine.readers_of(db, inputs.iter());
        engine.open(db);
        assert!(engine.run(db, &recipe, Slice::Evaluations(100)));
        assert_eq!(copied(db), Some(LuaType::IntegerConst(8)));
        let count = engine.stats.evaluations(DerivationKind::LocalStat) - before;
        bind(db, 110, LuaType::IntegerConst(1));
        bind(db, 120, LuaType::IntegerConst(1));
        for input in &inputs {
            db.reader_graph().forget_fingerprint(input);
        }
        assert!(settle(db, &mut engine, &recipe, &[WRITER, 20]));
        for present in [false, true, false, true] {
            *recipe.second.lock().unwrap() = present;
            assert!(settle(db, &mut engine, &recipe, &[20]));
            assert_eq!(
                copied(db),
                Some(LuaType::IntegerConst(if present { 4 } else { 2 }))
            );
        }
        (answer, count)
    }

    #[test]
    fn member_round_notifications_coalesce_two_contributors_and_survive_retraction() {
        let (baseline, eager_count) = slot_round_counts(false);
        let (batched, batched_count) = slot_round_counts(true);
        assert_eq!(baseline, Some(LuaType::IntegerConst(4)));
        assert_eq!(batched, baseline);
        assert!(
            batched_count < eager_count,
            "batch {batched_count}, eager {eager_count}"
        );
        eprintln!("slot round fixture: eager={eager_count}, batched={batched_count}");
    }
    #[test]
    fn member_round_notifications_preserve_first_absent_sighting_before_flush() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let recipe = SlotContributors {
            second: Mutex::new(true),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.batch_slot_notifications = true;
        bind(db, 110, LuaType::IntegerConst(1));
        engine.changed(statement(READER));
        engine.open(db);
        assert!(!engine.run(db, &recipe, Slice::Evaluations(1)));
        derivation::begin(statement(WRITER));
        recipe.evaluate(db, statement(WRITER));
        let evaluation = derivation::end();
        let mut touched = evaluation.outputs().cloned().collect::<FxHashSet<_>>();
        engine.record(
            db,
            vec![(evaluation, None)],
            &mut touched,
            &FxHashSet::default(),
            || "slot publication fixture".into(),
        );
        engine.readers_of(db, touched.iter());
        assert!(!engine.pending_slot_notifications.is_empty());
        assert!(
            engine
                .moved_from
                .get(&statement(READER))
                .is_some_and(|moved| moved.iter().any(|entry| matches!(
                    entry.key.key(),
                    FactKey::MemberOwnerMap(_)
                ) && entry.seen
                    == derivation::ABSENT))
        );
        assert!(engine.run(db, &recipe, Slice::Whole));
        assert_eq!(copied(db), Some(LuaType::IntegerConst(2)));
    }

    #[test]
    fn member_round_notifications_preserve_date_self_normalization_cycle() {
        let digest = |batch| {
            TEST_SLOT_BATCH.with(|value| value.set(Some(batch)));
            let _restore = OnDrop(|| TEST_SLOT_BATCH.with(|value| value.set(None)));
            let mut ws = VirtualWorkspace::new();
            ws.analysis.init_std_lib();
            ws.def_file(
                "lua/autorun/sh_date.lua",
                include_str!("../test/fixtures/member_round_date.lua"),
            );
            ws.settled_digest()
        };
        let eager = digest(false);
        let batched = digest(true);
        let eager_set = eager.iter().collect::<BTreeSet<_>>();
        let batched_set = batched.iter().collect::<BTreeSet<_>>();
        let differences = eager_set
            .symmetric_difference(&batched_set)
            .collect::<Vec<_>>();
        assert!(eager == batched, "{differences:#?}");
    }
    #[test]
    fn member_round_notifications_are_flushed_before_a_slice_returns() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let recipe = SlotContributors {
            second: Mutex::new(true),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.batch_slot_notifications = true;
        bind(db, 110, LuaType::IntegerConst(1));
        engine.changed(statement(READER));
        engine.changed(statement(WRITER));
        engine.open(db);
        assert!(!engine.run(db, &recipe, Slice::Evaluations(1)));
        assert!(!engine.run(db, &recipe, Slice::Evaluations(1)));
        assert!(engine.pending_slot_notifications.is_empty());
        assert!(engine.is_settling(FileId::new(FILE)));
        assert!(engine.run(db, &recipe, Slice::Whole));
        assert_eq!(copied(db), Some(LuaType::IntegerConst(2)));
    }
    #[test]
    fn member_round_notifications_mark_receiver_query_stale_before_flush() {
        use glua_parser::{LuaAstNode, LuaClosureExpr};
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/sh_receiver_query.lua",
            r#"
---@class BatchReceiver
local typed = {}
local holder = { receiver = typed }
function holder.receiver:Use() end
"#,
        );
        let db = ws.get_db_mut();
        let closure = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .unwrap()
            .get_chunk_node()
            .descendants::<LuaClosureExpr>()
            .next()
            .unwrap();
        let signature = crate::LuaSignatureId::new(file_id, closure.get_position());
        assert!(*db.queries().receiver_authority.get(db, signature));
        file_queries(db);
        let key = crate::LuaMemberKey::Name("receiver".into());
        let member = db
            .get_member_index()
            .get_current_members_for_key(&key)
            .into_iter()
            .find(|member| member.get_file_id() == file_id)
            .unwrap()
            .get_id();
        let owner = db
            .get_member_index()
            .get_member_owner(&member)
            .unwrap()
            .clone();
        db.get_member_index_mut()
            .rekey_member(member, crate::LuaMemberKey::Name("renamed".into()))
            .unwrap();
        let slot = HashedKey::new(FactKey::MemberSlot(owner.clone(), key));
        let map = HashedKey::new(FactKey::MemberOwnerMap(owner));
        db.reader_graph().forget_fingerprint(&slot);
        db.reader_graph().forget_fingerprint(&map);
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.readers_of(db, [&slot, &map]);
        assert!(!engine.pending_slot_notifications.is_empty());
        assert!(!*db.queries().receiver_authority.get(db, signature));
    }
}
