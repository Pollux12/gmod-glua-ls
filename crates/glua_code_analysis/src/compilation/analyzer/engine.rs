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
}

#[derive(Default)]
pub(crate) struct Engine {
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
}

impl Engine {
    pub(crate) fn new(edits: derivation::EditMaps, schedule: Schedule) -> Self {
        Self {
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
        self.to_follow = later;
        self.before_edits.get_mut().clear();
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

    pub(crate) fn changed(&mut self, id: DerivationId) {
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
        if !self.edits.is_empty() {
            self.before_edits.get_mut().remove(key);
            self.touched.insert(key.clone());
        }
        if let FactKey::Signature(signature_id) | FactKey::InferredGuard(signature_id) = key.key() {
            self.payload_holders.insert(signature_id.get_file_id());
        }
        let readers = db
            .reader_graph()
            .stale_readers(key, self.key_holds(db, key))
            .collect::<Vec<_>>();
        for reader in readers {
            self.stats.requeued(key, self.cursor, reader);
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
    /// settles.
    pub(crate) fn prioritise(&mut self, file_ids: &[crate::FileId]) {
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
        let replaced = |id: DerivationId| file_ids.contains(&id.file_id);
        self.worklist.take_where(replaced);
        self.next_round.take_where(replaced);
        self.waiting.retain(|id, _| !replaced(*id));
        self.held.retain(|id, _| !replaced(*id));
        self.stale_sources
            .retain(|file_id| !file_ids.contains(file_id));
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
            if slice.over(self.stats.total_evaluations() - started) {
                return false;
            }
            let Some((id, reason)) = self.worklist.pop_first() else {
                if self.next_round.is_empty() {
                    if crate::profile::phase("engine: stage and release", || {
                        self.advance_stage(db) || self.release_waiting(db)
                    }) {
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
                continue;
            }
            self.trace_evaluation(db, id, reason);
            let group = crate::profile::phase("engine: co-producers", || co_producers(db, id));
            if group.len() == 1 && recipe.splits(id.kind) {
                crate::profile::phase("engine: batch", || self.run_batch(db, id, recipe));
                continue;
            }
            let rerun = self.stats.profiling()
                && group
                    .iter()
                    .all(|member| self.stats.distinct.contains(member));
            for member in &group {
                self.unqueue(*member);
                self.stats.evaluated(*member);
            }
            self.evaluated.extend(
                group
                    .iter()
                    .map(|member| member.file_id)
                    .filter(|file_id| *file_id != crate::FileId::VIRTUAL),
            );
            let started = self.stats.profiling().then(std::time::Instant::now);
            let touched = crate::profile::phase("engine: evaluate", || {
                self.evaluate_group(db, &group, recipe)
            });
            if let Some(started) = started {
                self.stats
                    .timed(id.kind, started.elapsed(), usize::from(rerun), 1);
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
                self.worklist.insert(id, Reason::Changed);
            }
            crate::profile::phase("engine: readers of", || self.readers_of(db, touched.iter()));
        }
        self.cursor = None;
        crate::profile::phase("engine: rebase moved reads", || self.rebase_moved_reads(db));
        true
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

    /// Moves the run to `stage`. Returns the keys of the rules that moved.
    fn enter_stage(&mut self, db: &mut DbIndex, stage: AnalysisStage) -> Vec<HashedKey> {
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
        let mut queued = 0;
        for (reader, reason) in std::mem::take(&mut self.waiting) {
            if reason == Reason::Changed {
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
                self.worklist.insert(reader, Reason::Changed);
                queued += 1;
            }
        }
        self.stats.focus_settled(queued);
        if queued == 0 {
            return false;
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
        let computed = {
            let db: &DbIndex = db;
            let files = batch
                .iter()
                .map(|member| member.file_id)
                .collect::<Vec<_>>();
            super::parallel::map_files_collect(db, &files, |db, file_id| {
                let id = *batch
                    .iter()
                    .find(|member| member.file_id == file_id)
                    .expect("one batch member per file");
                derivation::begin(id);
                let computed = recipe.compute(db, id);
                (derivation::end(), computed)
            })
        };
        let evaluations = self.with_lent_keys(db, &touched, |db| {
            batch
                .iter()
                .zip(computed)
                .map(|(member, (evaluation, computed))| {
                    derivation::resume(evaluation);
                    recipe.apply(db, *member, computed);
                    (derivation::end(), None)
                })
                .collect()
        });
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
                self.worklist.insert(*member, Reason::Changed);
            }
        }
        self.readers_of(db, touched.iter());
        for id in alone {
            self.worklist.insert(id, Reason::Changed);
        }
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
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let lent = scopeguard_lent_keys();
        let recomputing = std::mem::take(&mut *self.recomputing.borrow_mut());
        derivation::note_recomputing(recomputing.keys().cloned());
        derivation::note_written(
            touched
                .iter()
                .filter(|key| !recomputing.contains_key(*key))
                .cloned()
                .chain(self.moved_rules.borrow_mut().drain(..)),
        );
        let evaluations = f(db);
        derivation::note_recomputed(|key| recomputing.get(key) == Some(&db.fact_fingerprint(key)));
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        std::mem::forget(lent);
        evaluations
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
        for key in touched.iter().chain(
            evaluations
                .iter()
                .flat_map(|(evaluation, _)| evaluation.outputs().chain(evaluation.moved())),
        ) {
            db.reader_graph().forget_fingerprint(key);
            let mut before_edits = self.before_edits.borrow_mut();
            before_edits.remove(key);
            for derived in db.reader_graph().derived_keys(key) {
                before_edits.remove(&derived);
            }
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
    evaluations: BTreeMap<DerivationKind, usize>,
    /// Rounds after the first.
    pub(crate) rounds: usize,
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

    fn requeued(&mut self, key: &FactKey, writer: Option<DerivationId>, reader: DerivationId) {
        if !self.profiling() {
            return;
        }
        let variant = variant_name(key);
        *self
            .requeues
            .entry((variant, writer.map(|writer| writer.kind), reader.kind))
            .or_default() += 1;
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
            "[engine] evaluations={} rounds={} rebuilt sources={} files={files} {}",
            self.total_evaluations(),
            self.rounds,
            self.rebuilt_sources,
            kinds.join(" ")
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
fn check_fingerprints() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_FINGERPRINTS").is_some())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use internment::ArcIntern;
    use smol_str::SmolStr;

    use super::*;
    use crate::{
        FileId, LuaDeclId, LuaType, LuaTypeCache, LuaTypeOwner, TypeOps, VirtualWorkspace,
    };

    const FILE: u32 = 90_000;
    const COPIED: u32 = 1;
    const SHARED: u32 = 2;
    const WRITTEN: u32 = 3;

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
}
