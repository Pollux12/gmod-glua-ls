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
}

impl Schedule {
    fn file_key(self, file_id: crate::FileId) -> u64 {
        match self {
            Self::Canonical => u64::from(file_id.id),
            Self::Reversed => u64::MAX - u64::from(file_id.id),
            Self::Shuffled(seed) => {
                use std::hash::BuildHasher;
                rustc_hash::FxBuildHasher.hash_one((seed, file_id.id))
            }
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
    /// Every key whose value may have changed during this engine's runs.
    touched: FxHashSet<HashedKey>,
    /// The files being re-analysed, while their derivations settle.
    focus: FxHashSet<crate::FileId>,
    /// Readers in other files, and their source layers, queued while the
    /// focus settles.
    waiting: BTreeSet<DerivationId>,
    /// The files whose derivations this engine evaluated.
    evaluated: FxHashSet<crate::FileId>,
    /// Where the edits being re-analysed moved positions: a read of a fact
    /// whose value only moved still holds.
    edits: derivation::EditMaps,
    /// Whether other files' references still have to follow `edits`.
    edits_pending: bool,
    /// Fingerprints of keys with `edits` undone, taken once per value: the
    /// same moved key is checked for every reader it queued.
    before_edits: std::cell::RefCell<FxHashMap<HashedKey, derivation::Fingerprint>>,
    /// Stage rules that moved since the key table was last lent: memos that
    /// read them no longer hold.
    moved_rules: std::cell::RefCell<Vec<HashedKey>>,
    /// Under `GLUALS_TRACE_EVALUATIONS`: the key that first queued each
    /// reader, and the derivation that moved it.
    queued_by: FxHashMap<DerivationId, (HashedKey, Option<DerivationId>)>,
}

impl Engine {
    pub(crate) fn new(edits: derivation::EditMaps, schedule: Schedule) -> Self {
        Self {
            worklist: Queue::new(schedule),
            next_round: Queue::new(schedule),
            edits_pending: !edits.is_empty(),
            edits,
            ..Self::default()
        }
    }

    /// The edits other files' references have yet to follow, once.
    pub(crate) fn edits_to_apply(&mut self) -> Option<derivation::EditMaps> {
        std::mem::take(&mut self.edits_pending).then(|| self.edits.clone())
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
        if self
            .cursor
            .is_some_and(|cursor| id.kind.rank() < cursor.kind.rank())
        {
            self.next_round.insert(id, reason);
        } else {
            self.worklist.insert(id, reason);
        }
    }

    fn unqueue(&mut self, id: DerivationId) {
        self.worklist.remove(id);
        self.next_round.remove(id);
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
        self.before_edits.get_mut().remove(key);
        self.touched.insert(key.clone());
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
                DerivationKind::Diagnostics => {}
                _ if self.waits(reader) => {
                    self.waiting.insert(reader);
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

    /// Settles `file_ids` before any reader in another file is re-checked.
    pub(crate) fn focus_on(&mut self, file_ids: &[crate::FileId]) {
        self.focus = file_ids.iter().copied().collect();
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

    /// Every key whose value may have changed, emptied.
    pub(crate) fn take_touched(&mut self) -> FxHashSet<HashedKey> {
        std::mem::take(&mut self.touched)
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

    /// Evaluates the worklist to quiescence, stage by stage.
    pub(crate) fn run(&mut self, db: &mut DbIndex, recipe: &dyn Recipe) {
        crate::semantic::keep_access_paths();
        let _paths = OnDrop(crate::semantic::stop_keeping_access_paths);
        self.enter_stage(db, AnalysisStage::Open);
        loop {
            let Some((id, reason)) = self.worklist.pop_first() else {
                if self.next_round.is_empty() {
                    if self.advance_stage(db) || self.release_waiting(db) {
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
                self.run_batch(db, id, recipe);
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
        self.rebase_moved_reads(db);
    }

    /// Re-records every read that held only with the edits mapped back as a
    /// read of the value as it stands now, so the next edit of the same file
    /// compares against it rather than against the text before this one.
    fn rebase_moved_reads(&self, db: &mut DbIndex) {
        if self.edits.is_empty() {
            return;
        }
        let moved = self
            .touched
            .iter()
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
        for key in &moved {
            db.reader_graph().forget_fingerprint(key);
        }
        self.moved_rules.get_mut().extend(moved.iter().cloned());
        moved
    }

    /// At quiescence, moves the run to its next stage and queues every
    /// derivation that waited on a rule the stage starts to hold. Returns
    /// whether it queued any; `false` once the run is closed.
    fn advance_stage(&mut self, db: &mut DbIndex) -> bool {
        while let Some(stage) = db.analysis_stage().next() {
            let moved = self.enter_stage(db, stage);
            let mut waited = BTreeSet::new();
            for key in &moved {
                let current = db.fact_fingerprint(key);
                waited.extend(db.reader_graph().stale_readers(key, |seen| seen == current));
            }
            let mut queued = 0;
            for reader in waited {
                if crate::profile::tracing_evaluations() {
                    self.trace(db, reader, "stage barrier queues", &[]);
                }
                match reader.kind {
                    DerivationKind::Diagnostics => {}
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
        for reader in std::mem::take(&mut self.waiting) {
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
        let mut touched = self.reset_outputs(db, &batch);
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
                    derivation::end()
                })
                .collect()
        });
        self.record(db, evaluations, &mut touched, || {
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
    fn reset_outputs(&self, db: &mut DbIndex, members: &[DerivationId]) -> FxHashSet<HashedKey> {
        let mut touched = FxHashSet::default();
        for member in members {
            touched.extend(db.reader_graph().outputs_of(*member).cloned());
        }
        let reset = derivation::capture_writes(|| {
            for key in touched.iter().filter(|key| key.is_primary()) {
                db.reset_fact(key);
            }
        });
        touched.extend(reset);
        self.verify_fingerprints(db, &touched, || format!("resetting {members:?}"));
        touched
    }

    /// Runs `f` with the reader graph's key table lent to this thread.
    /// `touched` moved before the table was lent: memos that read them are
    /// stale from here on.
    fn with_lent_keys(
        &self,
        db: &mut DbIndex,
        touched: &FxHashSet<HashedKey>,
        f: impl FnOnce(&mut DbIndex) -> Vec<derivation::Evaluation>,
    ) -> Vec<derivation::Evaluation> {
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let lent = scopeguard_lent_keys();
        derivation::note_written(
            touched
                .iter()
                .cloned()
                .chain(self.moved_rules.borrow_mut().drain(..)),
        );
        let evaluations = f(db);
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        std::mem::forget(lent);
        evaluations
    }

    /// Resets the group's primary outputs and re-runs it. Returns every key the
    /// group touched: its previous outputs and its new ones.
    fn evaluate_group(
        &self,
        db: &mut DbIndex,
        group: &[DerivationId],
        recipe: &dyn Recipe,
    ) -> FxHashSet<HashedKey> {
        let mut touched = self.reset_outputs(db, group);
        // Fingerprints are taken once the whole group has run: the group always
        // re-runs as a unit, so a member's read of what a later member writes is
        // internal to it, not a stale input.
        let evaluations = self.with_lent_keys(db, &touched, |db| {
            group
                .iter()
                .map(|member| {
                    derivation::begin(*member);
                    recipe.evaluate(db, *member);
                    derivation::end()
                })
                .collect()
        });
        crate::profile::phase("engine: record", || {
            self.record(db, evaluations, &mut touched, || {
                format!("evaluating {group:?}")
            })
        });
        touched
    }

    /// Replaces the records of `evaluations`, fingerprinting their reads as
    /// they stand now, and adds their outputs to `touched`.
    fn record(
        &self,
        db: &mut DbIndex,
        evaluations: Vec<derivation::Evaluation>,
        touched: &mut FxHashSet<HashedKey>,
        step: impl Fn() -> String,
    ) {
        for key in touched.iter().chain(
            evaluations
                .iter()
                .flat_map(|evaluation| evaluation.outputs().chain(evaluation.moved())),
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
        for evaluation in evaluations {
            touched.extend(evaluation.moved().cloned());
            let (id, mut slots, reads, outputs) = evaluation.into_recorded();
            slots.extend(reads.into_iter().map(|key| graph.intern_read(key)));
            // A resumed evaluation read some keys both before and after the
            // table was lent.
            slots.sort_unstable();
            slots.dedup();
            touched.extend(outputs.iter().cloned());
            graph.replace_slots(
                id,
                slots.into_iter().map(|slot| (slot, None)).collect(),
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
