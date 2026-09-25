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

use std::collections::{BTreeMap, BTreeSet};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::DbIndex;
use crate::db_index::derivation::{self, DerivationId, DerivationKind, FactKey, HashedKey};

/// Why a derivation is on the worklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// Its site is new or its outputs were dropped: always evaluated.
    Changed,
    /// A fact it read may have changed: evaluated only if one did.
    Read,
}

/// Queued derivations in the order a round takes them.
#[derive(Default)]
struct Queue {
    order: BTreeSet<(u8, Reason, DerivationId)>,
    reasons: FxHashMap<DerivationId, Reason>,
}

impl Queue {
    /// Queues `id`; a changed entry stays changed.
    fn insert(&mut self, id: DerivationId, reason: Reason) {
        if let Some(queued) = self.reasons.get(&id) {
            if *queued <= reason {
                return;
            }
            self.order.remove(&(id.kind.rank(), *queued, id));
        }
        self.reasons.insert(id, reason);
        self.order.insert((id.kind.rank(), reason, id));
    }

    fn remove(&mut self, id: DerivationId) {
        if let Some(reason) = self.reasons.remove(&id) {
            self.order.remove(&(id.kind.rank(), reason, id));
        }
    }

    fn pop_first(&mut self) -> Option<(DerivationId, Reason)> {
        let (_, reason, id) = self.order.pop_first()?;
        self.reasons.remove(&id);
        Some((id, reason))
    }

    /// Pops the first entry if it is of `kind`.
    fn pop_first_of(&mut self, kind: DerivationKind) -> Option<(DerivationId, Reason)> {
        let (_, _, id) = self.order.first()?;
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
}

impl Engine {
    pub(crate) fn with_edits(edits: derivation::EditMaps) -> Self {
        Self {
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
            assert!(
                except.contains(key) || cached == db.fact_fingerprint(key),
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

    /// Queues the readers of `keys` whose recorded fingerprint differs from
    /// the current value.
    pub(crate) fn readers_of<'a>(
        &mut self,
        db: &DbIndex,
        keys: impl IntoIterator<Item = &'a HashedKey>,
    ) {
        for key in keys {
            self.before_edits.get_mut().remove(key);
            self.touched.insert(key.clone());
            let readers = db
                .reader_graph()
                .stale_readers(key, self.key_holds(db, key))
                .collect::<Vec<_>>();
            for reader in readers {
                self.stats.requeued(key, self.cursor, reader);
                match reader.kind {
                    DerivationKind::Diagnostics => {}
                    DerivationKind::Source => {
                        self.stale_sources.insert(reader.file_id);
                    }
                    _ => self.queue(reader, Reason::Read),
                }
            }
        }
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
        std::mem::take(&mut self.stale_sources)
    }

    /// Evaluates the worklist to quiescence.
    pub(crate) fn run(&mut self, db: &mut DbIndex, recipe: &dyn Recipe) {
        crate::semantic::keep_access_paths();
        let _paths = OnDrop(crate::semantic::stop_keeping_access_paths);
        loop {
            let Some((id, reason)) = self.worklist.pop_first() else {
                if self.next_round.is_empty() {
                    break;
                }
                self.worklist = std::mem::take(&mut self.next_round);
                self.cursor = None;
                self.stats.rounds += 1;
                continue;
            };
            self.cursor = Some(id);
            let stale = crate::profile::phase("engine: stale check", || {
                self.needs_evaluation(db, id, reason)
            });
            if !stale {
                continue;
            }
            let group = crate::profile::phase("engine: co-producers", || co_producers(db, id));
            if group.len() == 1 && recipe.splits(id.kind) {
                self.run_batch(db, id, recipe);
                continue;
            }
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
                *self.stats.time.entry(id.kind).or_default() += started.elapsed();
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
    }

    /// Whether `id`, queued for `reason`, has to run: a reader runs only if a
    /// fact it read moved.
    fn needs_evaluation(&mut self, db: &mut DbIndex, id: DerivationId, reason: Reason) -> bool {
        if reason == Reason::Changed
            || db.reader_graph().is_stale(
                id,
                |key| db.fact_fingerprint(key),
                |key, seen| {
                    !self.edits.is_empty() && seen == self.fingerprint_before_edits(db, key)
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
        // edit moved; outputs made from them are right only if the edit's
        // remap rewrites them too.
        let moved = db.reader_graph_mut().set_fingerprints(id, fingerprints);
        moved
            && !db
                .reader_graph()
                .outputs_of(id)
                .all(|key| key.follows_edits())
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
            if co_producers(db, id).len() == 1 {
                batch.push(id);
            } else {
                alone.push(id);
            }
        }
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
            *self.stats.time.entry(first.kind).or_default() += started.elapsed();
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
        derivation::note_written(touched.iter().cloned());
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
        for key in touched
            .iter()
            .chain(evaluations.iter().flat_map(derivation::Evaluation::outputs))
        {
            db.reader_graph().forget_fingerprint(key);
            self.before_edits.borrow_mut().remove(key);
        }
        self.verify_fingerprints(db, touched, step);
        // Out of the index while its records are replaced, so the reads can be
        // fingerprinted against it.
        let mut graph = std::mem::take(db.reader_graph_mut());
        let mut released = Vec::new();
        for evaluation in evaluations {
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
    /// Under `GLUALS_PROFILE`: time per kind, and how often a key's move
    /// queued a reader, by key variant and the writer's and reader's kinds.
    time: BTreeMap<DerivationKind, std::time::Duration>,
    requeues: BTreeMap<(String, Option<DerivationKind>, DerivationKind), usize>,
    /// Recorded reads by key variant.
    reads: BTreeMap<String, usize>,
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
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE").is_some())
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

    /// Prints the counts under `GLUALS_PROFILE`.
    pub(crate) fn report(&self) {
        if std::env::var_os("GLUALS_PROFILE").is_none() || self.evaluations.is_empty() {
            return;
        }
        let kinds = self
            .evaluations
            .iter()
            .map(|(kind, count)| format!("{kind:?}={count}"))
            .collect::<Vec<_>>();
        eprintln!(
            "[engine] evaluations={} rounds={} {}",
            self.total_evaluations(),
            self.rounds,
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
            eprintln!("[engine] time {kind:?} {time:?}");
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
