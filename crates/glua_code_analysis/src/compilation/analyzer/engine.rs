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
//! work has run is checked once, not after each writer. A reader that comes
//! before the derivation that queued it in the round's order (a back edge)
//! waits for the next round, which re-checks it once however many later
//! producers moved it.
//!
//! A re-analysis settles the re-analysed files first. Until their derivations
//! settle through every stage, a reader in another file only waits: what it
//! would re-check is still being rebuilt, and most of it comes back as it was.
//! Once they settle, the readers whose reads still moved run at the stage the
//! run reached, like any other work.
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
//! once no source layer is left to rebuild, each loop a fact from outside
//! moved under since it last settled is cleared and settles again from the
//! first stage, until none is: what a loop holds is a function of what it
//! reads, not of the order it read it in. A loop downstream of another
//! through the kinds loops close through waits for it to settle; loops
//! joined only through other kinds are cleared together, and one that a
//! loop cleared as late or later reaches through what moved since settled
//! on what that one held on the way, so it is cleared again once a fact
//! from outside moved under it. A loop among the work a settled focus or
//! loop leaves waiting is cleared before that work is released when no
//! other waiting work lies upstream of it: run first, its members would only
//! settle on what it held, and so would everything downstream of it, before
//! it is cleared.
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

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::DbIndex;
use crate::db_index::derivation::{
    self, AnalysisStage, DerivationId, DerivationKind, FactKey, HashedKey, OnDrop, StageRule,
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

    /// Whether the run hands back between slices.
    fn yields(self) -> bool {
        !matches!(self, Self::Whole)
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

    /// The rank of the first queued entry.
    fn first_rank(&self) -> Option<u8> {
        self.order.first().map(|(rank, ..)| *rank)
    }

    /// Whether the entries at `rank` belong to more than one file besides
    /// the workspace's.
    fn spans_files(&self, rank: u8) -> bool {
        let mut first = None;
        for reason in [Reason::Changed, Reason::Read] {
            let mut from = 0;
            while let Some(&(entry_rank, entry_reason, file_key, id)) = self
                .order
                .range((rank, reason, from, DerivationId::floor())..)
                .next()
            {
                if entry_rank != rank || entry_reason != reason {
                    break;
                }
                if id.file_id != crate::FileId::VIRTUAL
                    && *first.get_or_insert(id.file_id) != id.file_id
                {
                    return true;
                }
                let Some(next) = file_key.checked_add(1) else {
                    break;
                };
                from = next;
            }
        }
        false
    }

    /// The entries queued at `rank` with reason `reason`, in order.
    fn at(
        &self,
        rank: u8,
        reason: Reason,
        file_key: u64,
    ) -> impl Iterator<Item = &(u8, Reason, u64, DerivationId)> {
        self.order
            .range((rank, reason, file_key, DerivationId::floor())..)
            .take_while(move |(entry_rank, entry_reason, ..)| {
                *entry_rank == rank && *entry_reason == reason
            })
    }

    /// Takes out every entry at `rank`, file by file in the order a round
    /// takes the files, each file's as [`Self::take_file_at`] takes them.
    fn take_rank(&mut self, rank: u8) -> Vec<(crate::FileId, Vec<(DerivationId, Reason)>)> {
        let mut taken = self
            .order
            .split_off(&(rank, Reason::Changed, 0, DerivationId::floor()));
        if let Some(next) = rank.checked_add(1) {
            let mut after = taken.split_off(&(next, Reason::Changed, 0, DerivationId::floor()));
            self.order.append(&mut after);
        }
        let mut by_file = BTreeMap::<_, Vec<_>>::new();
        for (_, reason, file_key, id) in taken {
            self.reasons.remove(&id);
            by_file
                .entry((file_key, id.file_id))
                .or_default()
                .push((id, reason));
        }
        by_file
            .into_iter()
            .map(|((_, file_id), entries)| (file_id, entries))
            .collect()
    }

    /// [`Self::take_rank`] bounded to the files, in order, whose entries come
    /// to `entries`, with the file that reaches it whole; the workspace's
    /// entries stay queued until the rank's last files are taken.
    fn take_rank_bounded(
        &mut self,
        rank: u8,
        entries: usize,
    ) -> Vec<(crate::FileId, Vec<(DerivationId, Reason)>)> {
        let Some(cut) = self.rank_cut(rank, entries) else {
            return self.take_rank(rank);
        };
        let mut taken = Vec::new();
        for reason in [Reason::Changed, Reason::Read] {
            let mut from = self
                .order
                .split_off(&(rank, reason, 0, DerivationId::floor()));
            let mut after = from.split_off(&(rank, reason, cut, DerivationId::floor()));
            self.order.append(&mut after);
            taken.extend(from);
        }
        let mut by_file = BTreeMap::<_, Vec<_>>::new();
        for (entry_rank, reason, file_key, id) in taken {
            if id.file_id == crate::FileId::VIRTUAL {
                self.order.insert((entry_rank, reason, file_key, id));
                continue;
            }
            self.reasons.remove(&id);
            by_file
                .entry((file_key, id.file_id))
                .or_default()
                .push((id, reason));
        }
        by_file
            .into_iter()
            .map(|((_, file_id), entries)| (file_id, entries))
            .collect()
    }

    /// The file key [`Self::take_rank_bounded`] stops before: that of the
    /// first file after those, in order, whose entries come to `entries`.
    /// `None` when the rank's files come to no more.
    fn rank_cut(&self, rank: u8, entries: usize) -> Option<u64> {
        let range = |reason| {
            self.order
                .range((rank, reason, 0, DerivationId::floor())..)
                .take_while(move |(entry_rank, entry_reason, ..)| {
                    *entry_rank == rank && *entry_reason == reason
                })
                .filter(|(.., id)| id.file_id != crate::FileId::VIRTUAL)
                .map(|(_, _, file_key, _)| *file_key)
                .peekable()
        };
        let (mut changed, mut read) = (range(Reason::Changed), range(Reason::Read));
        let mut count = 0;
        let mut current = None;
        loop {
            let file_key = match (changed.peek(), read.peek()) {
                (Some(a), Some(b)) if a <= b => changed.next(),
                (Some(_), Some(_)) | (None, Some(_)) => read.next(),
                (Some(_), None) => changed.next(),
                (None, None) => return None,
            }?;
            if current != Some(file_key) {
                if count >= entries {
                    return Some(file_key);
                }
                current = Some(file_key);
            }
            count += 1;
        }
    }

    /// Takes out every entry of `file_id` at `rank`.
    fn take_file_at(&mut self, rank: u8, file_id: crate::FileId) -> Vec<(DerivationId, Reason)> {
        let file_key = self.schedule.file_key(file_id);
        let mut taken = Vec::new();
        for reason in [Reason::Changed, Reason::Read] {
            let ids = self
                .at(rank, reason, file_key)
                .take_while(|(_, _, key, _)| *key == file_key)
                .filter(|(.., id)| id.file_id == file_id)
                .map(|(.., id)| *id)
                .collect::<Vec<_>>();
            for id in ids {
                self.remove(id);
                taken.push((id, reason));
            }
        }
        taken
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

    /// Finishes what the applies since the last call left to settle off the
    /// ordered publish.
    fn settle(&self) {}

    /// Drops a worker result [`Self::apply`] will not publish.
    fn discard(&self, _id: DerivationId, _computed: Box<dyn std::any::Any + Send>) {}
}

#[derive(Default)]
pub(crate) struct Engine {
    /// The order the run takes files in.
    schedule: Schedule,
    /// This round's work.
    worklist: Queue,
    /// Back edges: readers that come before the derivation that queued them.
    next_round: Queue,
    /// The derivation this round evaluated or decided last, and, for one
    /// evaluated on the index, why it was queued.
    cursor: Option<(DerivationId, Option<Reason>)>,
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
    /// Whether the run handed back once the focus settled, so the next
    /// quiescence releases the waiting work rather than handing back again.
    handed_back: bool,
    /// Whether the run handed back once a release queued work that has not
    /// run since: a focus put ahead again in between releases it again, and
    /// that release goes on into it.
    released_unrun: bool,
    /// Set from outside while an edit waits: a yielding slice stops between
    /// two evaluations.
    interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
    before_edits: std::sync::Mutex<FxHashMap<HashedKey, derivation::Fingerprint>>,
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
    /// The members of the loops this run cleared at quiescence for good, at
    /// the first check after them that cleared none (see
    /// [`Self::settled_loop_members`]). Settled again from nothing once the
    /// loops that reach it settled, a loop holds nothing from before the
    /// run, and what moves under it later it follows as any reader does. A
    /// loop through a pass that runs from the first stage, which loops leave
    /// out, could otherwise be cleared without end, each time from what the
    /// pass made of the last.
    cleared: FxHashSet<DerivationId>,
    /// The members of the loops this run cleared before the waiting work
    /// they belong to was released (see [`Self::clear_parked_loops`]), once
    /// each: should a fact from outside move under one after all, quiescence
    /// clears it once more.
    cleared_parked: FxHashSet<DerivationId>,
    /// The loops cleared since the last loop check at quiescence that
    /// cleared none, each as last cleared.
    pending_loops: Vec<ClearedLoop>,
    /// Since the first of `pending_loops` was cleared: every key whose
    /// readers were checked.
    pending_moved: FxHashSet<HashedKey>,
    /// The members of cleared loops that a loop cleared as late or later
    /// reaches: they settled on what it held on the way, so, as for a
    /// statement from before the run, every fact that moved under one counts.
    settled_early: FxHashSet<DerivationId>,
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
    /// How often each derivation ran since the run entered its stage.
    stage_runs: FxHashMap<DerivationId, StageRuns>,
    /// Whether a derivation that runs more often within a stage than settling
    /// can need panics rather than being left as its last run left it.
    unsettled_panics: bool,
    batch_slot_notifications: bool,
    /// How cold builds are driven; see [`superstep_mode`].
    superstep: SuperstepMode,
    pending_slot_notifications: FxHashSet<HashedKey>,
    /// In a superstep, the file whose superstep task is running:
    /// its batches take no other file's work.
    task: Option<crate::FileId>,
    /// In worker supersteps, the files whose work settles on the index, each
    /// to quiescence, since one of their derivations ran too often; and
    /// whether the stage settles on the serial loop instead.
    serial_files: FxHashSet<crate::FileId>,
    /// The members of a cycle whose outputs alternate: worker superstep
    /// tasks run them from one snapshot, and a round re-checks those it
    /// sent to the next round on what the others held in the last one, so
    /// each reads what the others held before and they can swap values every
    /// superstep or round. For the rest of the stage they run on the index,
    /// one after another as the serial loop runs them, each again in the
    /// round that moved it.
    cycle_members: FxHashSet<DerivationId>,
    /// The readers a round sent to the next one as back edges, until they run
    /// again: one whose outputs then alternate joins [`Self::cycle_members`].
    back_edges: FxHashSet<DerivationId>,
    /// For each member of a cycle a superstep closed, the fingerprints of
    /// its outputs after the last two supersteps that closed one through it.
    cycle_outputs: FxHashMap<DerivationId, [Option<derivation::Fingerprint>; 2]>,
    /// While a superstep's installed facts queue their readers: each reader
    /// queued, with the key that queued it.
    queued_while_installing: Option<Vec<(DerivationId, HashedKey)>>,
    /// How often each group, by its first member, waited a superstep for
    /// the queries it read to be computed since the run entered its stage.
    query_deferrals: FxHashMap<DerivationId, u8>,
    serial_stage: bool,
    /// What superstep tasks took, to start the costliest first.
    task_costs: TaskCosts,
    /// The groups a worker superstep that stopped left unrun, by file: the
    /// next superstep runs them as they were decided before it takes another
    /// entry.
    carried: Vec<TaskPlan>,
    /// Whether a slice that ends with a worker superstep due leaves its tasks
    /// to [`Superstep::run_tasks`] outside the run while readers share the
    /// index, and installs them when the run goes on.
    detach: bool,
    /// The worker superstep whose tasks run outside the run, and its files.
    detached: Option<Superstep>,
    detached_files: Vec<crate::FileId>,
}

/// The seconds a group of each file's superstep task took at each rank, by
/// its last task there, and their average at each rank: the estimates that
/// start a superstep's costliest tasks first.
#[derive(Default)]
struct TaskCosts {
    per_group: FxHashMap<(crate::FileId, u8), f64>,
    by_rank: FxHashMap<u8, (f64, usize)>,
}

impl TaskCosts {
    /// Notes that the task of `file_id` at `rank` ran `groups` groups in
    /// `elapsed`.
    fn observe(
        &mut self,
        rank: u8,
        file_id: crate::FileId,
        groups: usize,
        elapsed: std::time::Duration,
    ) {
        if groups == 0 {
            return;
        }
        let per_group = elapsed.as_secs_f64() / groups as f64;
        self.per_group.insert((file_id, rank), per_group);
        let (sum, count) = self.by_rank.entry(rank).or_default();
        *sum += per_group;
        *count += 1;
    }

    /// The estimated cost of each of `plans` at `rank`, a file with no task
    /// there yet taken at the average; `None` before any task ran at `rank`.
    fn estimates(&self, rank: u8, plans: &[TaskPlan]) -> Option<Vec<f64>> {
        let (sum, count) = self.by_rank.get(&rank)?;
        let average = sum / *count as f64;
        Some(
            plans
                .iter()
                .map(|plan| {
                    let per_group = self
                        .per_group
                        .get(&(plan.file_id, rank))
                        .copied()
                        .unwrap_or(average);
                    per_group * plan.groups.len() as f64
                })
                .collect(),
        )
    }
}

/// A loop the run cleared, as it was cleared last.
#[derive(Debug)]
struct ClearedLoop {
    members: Vec<DerivationId>,
    /// Whether it was cleared at quiescence rather than before the waiting
    /// work it belongs to was released.
    at_quiescence: bool,
    /// The wave it was cleared in, counted over the engine's runs.
    wave: usize,
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
    static TEST_SUPERSTEP: std::cell::Cell<Option<SuperstepMode>> = const { std::cell::Cell::new(None) };
}

/// Until the guard drops, engines this thread creates run the serial engine.
#[cfg(test)]
pub(crate) fn serial_engine() -> impl Drop {
    TEST_SUPERSTEP.with(|mode| mode.set(Some(SuperstepMode::Off)));
    OnDrop(|| TEST_SUPERSTEP.with(|mode| mode.set(None)))
}

impl Engine {
    pub(crate) fn new(edits: derivation::EditMaps, schedule: Schedule) -> Self {
        let batch_slot_notifications = true;
        #[cfg(test)]
        let batch_slot_notifications =
            TEST_SLOT_BATCH.with(|value| value.get().unwrap_or(batch_slot_notifications));
        let superstep = superstep_mode();
        #[cfg(test)]
        let superstep = TEST_SUPERSTEP.with(|mode| mode.get().unwrap_or(superstep));
        Self {
            schedule,
            batch_slot_notifications,
            superstep,
            worklist: Queue::new(schedule),
            next_round: Queue::new(schedule),
            to_follow: edits.clone(),
            edits,
            unsettled_panics: cfg!(debug_assertions) || check_convergence(),
            ..Self::default()
        }
    }

    /// Stops a yielding slice between two evaluations while `interrupt` is set.
    pub(crate) fn interrupt_with(
        &mut self,
        interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.interrupt = Some(interrupt);
    }

    /// Has a slice that ends with a worker superstep due leave its tasks to
    /// run outside the run; see [`Self::take_detached`].
    pub(crate) fn detach_supersteps(&mut self) {
        self.detach = true;
    }

    /// The superstep whose tasks the last slice left to run outside it, to
    /// run them on the index while readers share it and hand it back with
    /// [`Self::restore_detached`]. The next slice installs them, run or not.
    pub(crate) fn take_detached(&mut self) -> Option<Superstep> {
        self.detached.take()
    }

    pub(crate) fn restore_detached(&mut self, superstep: Superstep) {
        self.detached = Some(superstep);
    }

    pub(crate) fn has_detached(&self) -> bool {
        self.detached.is_some()
    }

    fn interrupted(&self) -> bool {
        self.interrupt
            .as_ref()
            .is_some_and(|interrupt| interrupt.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Whether the edits other files' references have yet to follow change
    /// `file_id`'s text.
    pub(crate) fn edits_text_of(&self, file_id: crate::FileId) -> bool {
        self.to_follow.edits(file_id)
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
        // Each key, and where `later` moved it.
        let followed = |keys: FxHashSet<HashedKey>| {
            keys.into_iter()
                .flat_map(|key| {
                    let moved = later
                        .map_key(&key)
                        .filter(|moved| moved != key.key())
                        .map(HashedKey::new);
                    std::iter::once(key).chain(moved)
                })
                .collect::<FxHashSet<_>>()
        };
        self.touched = followed(std::mem::take(&mut self.touched));
        self.pending_slot_notifications =
            followed(std::mem::take(&mut self.pending_slot_notifications));
        self.pending_moved = followed(std::mem::take(&mut self.pending_moved));
        self.to_follow = later;
        self.before_edits_mut().clear();
        self.since_search = None;
    }

    /// Whether reads of `key` that saw a fingerprint still hold: the value is
    /// the same, or the same once the edits' moves are undone.
    fn key_holds<'a>(
        &'a self,
        db: &'a DbIndex,
        key: &'a HashedKey,
    ) -> impl Fn(derivation::Fingerprint) -> bool + 'a {
        holds(db, &self.edits, &self.before_edits, key)
    }

    fn fingerprint_before_edits(&self, db: &DbIndex, key: &HashedKey) -> derivation::Fingerprint {
        fingerprint_before_edits(db, &self.edits, &self.before_edits, key)
    }

    fn before_edits_mut(&mut self) -> &mut FxHashMap<HashedKey, derivation::Fingerprint> {
        self.before_edits
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `key`'s current fingerprint, taken once per value. Every write inside
    /// an evaluation is one of its outputs, so a group's touched keys are the
    /// only ones it can move; writes outside `run` forget every fingerprint.
    fn fingerprint(&self, db: &DbIndex, key: &HashedKey) -> derivation::Fingerprint {
        current_fingerprint(db, key)
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

    /// Queues `id` as changed, or parks it while the focus settles if it
    /// lies outside the focus.
    pub(crate) fn changed(&mut self, id: DerivationId) {
        self.stats
            .requeue_cause(id, "changed: its file re-analysed or a producer removed");
        if self.waits(id) {
            self.wait(id, Reason::Changed);
        } else {
            self.queue(id, Reason::Changed);
        }
    }

    /// Queues `id` in this round unless it comes before the derivation being
    /// evaluated on the index in the round's order, or ranks before the one a
    /// superstep decided last, else in the next. A changed entry stays changed.
    fn queue(&mut self, id: DerivationId, reason: Reason) {
        let Some((cursor, cursor_reason)) = self.cursor else {
            self.worklist.insert(id, reason);
            return;
        };
        if self.worklist.schedule == Schedule::Deferred || id.kind.rank() < cursor.kind.rank() {
            self.next_round.insert(id, reason);
        } else if cursor_reason.is_some_and(|cursor_reason| {
            !self.cycle_members.contains(&id)
                && self.worklist.entry(id, reason) < self.worklist.entry(cursor, cursor_reason)
        }) {
            self.back_edges.insert(id);
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
        let keys = keys.into_iter().collect::<Vec<_>>();
        let found = self.stale_readers_of(db, &keys);
        let derived = self.derived_keys_to_check(db, &keys);
        for (key, readers) in keys.into_iter().zip(found) {
            self.queue_readers(db, key, readers);
        }
        let found = self.stale_readers_of(db, &derived.iter().collect::<Vec<_>>());
        for (key, readers) in derived.iter().zip(found) {
            self.queue_readers(db, key, readers);
        }
    }

    /// The keys read out of each base among `keys` whose readers have to be
    /// checked at the base's current fingerprint, taken on worker threads
    /// while nothing is being recorded.
    fn derived_keys_to_check(&self, db: &DbIndex, keys: &[&HashedKey]) -> Vec<HashedKey> {
        let graph = db.reader_graph();
        let bases = keys
            .iter()
            .filter(|key| key.is_base() && graph.has_derived_keys(key))
            .collect::<Vec<_>>();
        let now = if derivation::recording() {
            bases
                .iter()
                .map(|key| current_fingerprint(db, key))
                .collect::<Vec<_>>()
        } else {
            bases
                .par_iter()
                .with_min_len(KEYS_PER_JOB)
                .map(|key| current_fingerprint(db, key))
                .collect()
        };
        bases
            .into_iter()
            .zip(now)
            .flat_map(|(key, now)| graph.derived_keys_to_check(key, now))
            .collect()
    }

    /// The readers of each of `keys` whose recorded fingerprint holds neither
    /// as the key stands nor with the edits mapped back, found on worker
    /// threads while nothing is being recorded; `None` for a query output,
    /// whose fingerprint may compute it.
    fn stale_readers_of(
        &self,
        db: &DbIndex,
        keys: &[&HashedKey],
    ) -> Vec<Option<Vec<(DerivationId, derivation::Fingerprint)>>> {
        if derivation::recording() {
            return vec![None; keys.len()];
        }
        if !self.edits.is_empty() {
            // A key that moved fingerprints anew with the edits undone.
            let mut memo = lock(&self.before_edits);
            for key in keys {
                memo.remove(*key);
            }
        }
        let graph = db.reader_graph();
        let (edits, before_edits) = (&self.edits, &self.before_edits);
        keys.par_iter()
            .with_min_len(KEYS_PER_JOB)
            .map(|key| {
                (!matches!(
                    key.key(),
                    FactKey::ReceiverAuthority(_) | FactKey::CallbackParams(_)
                ))
                .then(|| {
                    graph
                        .stale_readers_seen(key, holds(db, edits, before_edits, key))
                        .collect()
                })
            })
            .collect()
    }

    /// Queues the readers of `key` whose recorded fingerprint differs from its
    /// current value: `readers`, when they were found already.
    fn queue_readers(
        &mut self,
        db: &DbIndex,
        key: &HashedKey,
        readers: Option<Vec<(DerivationId, derivation::Fingerprint)>>,
    ) {
        if self.batch_slot_notifications
            && self.loop_focus.is_empty()
            && matches!(
                key.key(),
                FactKey::MemberSlot(..) | FactKey::MemberOwnerMap(_)
            )
        {
            self.note_checked(key);
            if !self.edits.is_empty() {
                self.before_edits_mut().remove(key);
                self.touched.insert(key.clone());
            }
            let readers = readers.unwrap_or_else(|| self.stale_readers(db, key));
            for (reader, seen) in readers {
                self.remember_moved_read(db, key, reader, seen);
                if reader.kind.is_query() {
                    db.queries().set_stale(reader, true);
                }
            }
            self.pending_slot_notifications.insert(key.clone());
            return;
        }
        self.queue_readers_now(db, key, readers);
    }

    /// Notes that the readers of `key` are checked, for the next loop check.
    fn note_checked(&mut self, key: &HashedKey) {
        if let Some(keys) = &mut self.since_search {
            keys.insert(key.clone());
        }
        if !self.pending_loops.is_empty() {
            self.pending_moved.insert(key.clone());
        }
    }

    fn stale_readers(
        &self,
        db: &DbIndex,
        key: &HashedKey,
    ) -> Vec<(DerivationId, derivation::Fingerprint)> {
        db.reader_graph()
            .stale_readers_seen(key, self.key_holds(db, key))
            .collect()
    }

    pub(crate) fn flush_slot_notifications(&mut self, db: &DbIndex) {
        let pending = std::mem::take(&mut self.pending_slot_notifications)
            .into_iter()
            .collect::<Vec<_>>();
        let pending = crate::profile::phase("engine: sort slot notifications", || {
            let mut named = pending
                .into_par_iter()
                .map(|key| (format!("{:?}", key.key()), key))
                .collect::<Vec<_>>();
            named.sort_by(|(left, _), (right, _)| left.cmp(right));
            named.into_iter().map(|(_, key)| key).collect::<Vec<_>>()
        });
        let found = self.stale_readers_of(db, &pending.iter().collect::<Vec<_>>());
        for (key, readers) in pending.iter().zip(found) {
            self.queue_readers_now(db, key, readers);
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

    fn queue_readers_now(
        &mut self,
        db: &DbIndex,
        key: &HashedKey,
        readers: Option<Vec<(DerivationId, derivation::Fingerprint)>>,
    ) {
        self.note_checked(key);
        if !self.edits.is_empty() {
            self.before_edits_mut().remove(key);
            self.touched.insert(key.clone());
        }
        if let FactKey::Signature(signature_id) | FactKey::InferredGuard(signature_id) = key.key() {
            self.payload_holders.insert(signature_id.get_file_id());
        }
        let readers = readers.unwrap_or_else(|| self.stale_readers(db, key));
        let cursor = self.cursor.map(|(id, _)| id);
        for (reader, seen) in readers {
            if let Some(queued) = &mut self.queued_while_installing {
                queued.push((reader, key.clone()));
            }
            self.stats
                .requeued(key, cursor, reader, seen == derivation::ABSENT);
            self.remember_moved_read(db, key, reader, seen);
            if reader.kind.is_query() {
                db.queries().set_stale(reader, true);
            }
            if crate::profile::tracing_evaluations() {
                self.queued_by
                    .entry(reader)
                    .or_insert_with(|| (key.clone(), cursor));
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
        self.requeue_carried();
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
        // An edit's focus hands the run back once it settles.
        self.handed_back = false;
        self.requeue_carried();
        let files = file_ids.iter().copied().collect::<FxHashSet<_>>();
        let replaced = |id: DerivationId| files.contains(&id.file_id);
        self.worklist.take_where(replaced);
        self.next_round.take_where(replaced);
        self.waiting.retain(|id, _| !replaced(*id));
        self.held.retain(|id, _| !replaced(*id));
        self.stale_sources
            .retain(|file_id| !files.contains(file_id));
    }

    /// Notes that `file_id`'s derivations are removed and built again.
    pub(crate) fn rebuilding(&mut self, file_id: crate::FileId) {
        self.rebuilt.insert(file_id);
    }

    /// Whether `file_id` has work still to settle: queued, waiting, in the
    /// focus or a source layer to rebuild.
    pub(crate) fn is_settling(&self, file_id: crate::FileId) -> bool {
        self.focus.contains(&file_id) || self.has_work(file_id)
    }

    /// Whether `file_id` has work queued, waiting, held or on a worker, or a
    /// source layer to rebuild.
    fn has_work(&self, file_id: crate::FileId) -> bool {
        self.stale_sources.contains(&file_id)
            || self
                .worklist
                .files()
                .chain(self.next_round.files())
                .chain(self.waiting.keys().map(|id| id.file_id))
                .chain(self.held.keys().map(|id| id.file_id))
                .chain(self.carried.iter().flat_map(TaskPlan::files))
                .chain(self.detached_files.iter().copied())
                .any(|queued| queued == file_id)
    }

    /// Whether `file_id` is in the focus that is still settling and has work
    /// left in it: one whose own work settled answers while the rest of the
    /// focus settles.
    pub(crate) fn in_focus(&self, file_id: crate::FileId) -> bool {
        self.focus.contains(&file_id) && self.has_work(file_id)
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
        let stale = self
            .diagnostics
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .into_par_iter()
            .filter(|file_id| {
                graph.is_stale(
                    DerivationId::diagnostics(*file_id),
                    |key| db.fact_fingerprint(key),
                    |_, _| false,
                )
            })
            .collect::<Vec<_>>();
        if crate::profile::tracing_evaluations() && !stale.is_empty() {
            self.report_stale_diagnostics(db, &stale);
        }
        stale
    }

    /// Under `GLUALS_TRACE_EVALUATIONS`, prints which kinds of moved facts made
    /// `stale` files' diagnostics stale, most common first.
    fn report_stale_diagnostics(&self, db: &DbIndex, stale: &[crate::FileId]) {
        let mut kinds = FxHashMap::<String, usize>::default();
        for file_id in stale {
            let moved = self.moved_reads(db, DerivationId::diagnostics(*file_id));
            let file_kinds = moved
                .iter()
                .map(|key| key.split(['(', ' ']).next().unwrap_or_default().to_string())
                .collect::<FxHashSet<_>>();
            for kind in file_kinds {
                *kinds.entry(kind).or_default() += 1;
            }
        }
        let mut kinds = kinds.into_iter().collect::<Vec<_>>();
        kinds.sort_unstable_by(|left, right| {
            right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0))
        });
        kinds.truncate(12);
        eprintln!(
            "[engine] {} stale diagnostics, by moved fact kind (files): {kinds:?}",
            stale.len()
        );
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
        if let Some(mut superstep) = self.detached.take() {
            self.detached_files.clear();
            superstep.stop = StopSignal::new(slice, self.interrupt.as_ref());
            self.finish_superstep(db, recipe, slice, superstep);
        }
        loop {
            self.file_queries(db);
            let interrupted = slice.yields() && self.interrupted();
            let on_workers = self.superstep == SuperstepMode::Workers && !self.serial_stage;
            if !on_workers {
                self.requeue_carried();
            }
            let superstep_due = on_workers
                && (!self.carried.is_empty()
                    || self
                        .worklist
                        .first_rank()
                        .is_some_and(|rank| self.worklist.spans_files(rank)));
            if interrupted || slice.over(self.stats.total_evaluations() - started) {
                // The superstep due next runs while readers share the index,
                // and the next slice installs it.
                if superstep_due && !interrupted && self.detach && matches!(slice, Slice::Until(_))
                {
                    self.detach_superstep(db, slice);
                }
                self.flush_slot_notifications(db);
                self.cursor = None;
                return false;
            }
            if self.focus.is_empty() && (!self.worklist.is_empty() || !self.carried.is_empty()) {
                self.released_unrun = false;
            }
            if superstep_due {
                self.superstep_on_workers(db, recipe, slice);
                continue;
            }
            let Some((id, reason)) = self.worklist.pop_first() else {
                self.flush_slot_notifications(db);
                if !self.worklist.is_empty() {
                    continue;
                }
                if self.next_round.is_empty() {
                    if crate::profile::phase("engine: stage", || self.advance_stage(db)) {
                        continue;
                    }
                    if slice.yields()
                        && !self.handed_back
                        && self.loop_focus.is_empty()
                        && !self.focus.is_empty()
                    {
                        // The focus's files answer before the work waiting on it is checked.
                        self.focus.clear();
                        self.handed_back = true;
                        crate::profile::phase_report("focus");
                        self.cursor = None;
                        return false;
                    }
                    if crate::profile::phase("engine: release", || self.release_waiting(db)) {
                        // What the settled focus released runs in slices sized
                        // for it; loops cleared ahead of it settle first.
                        if slice.yields() && !self.released_unrun && self.loop_focus.is_empty() {
                            self.released_unrun = true;
                            self.cursor = None;
                            return false;
                        }
                        continue;
                    }
                    // A source layer left to rebuild moves what loops read.
                    if self.stale_sources.is_empty()
                        && crate::profile::phase("engine: loops", || self.clear_loops(db))
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
            let name = if !self.loop_focus.is_empty() {
                "serial: loop focus"
            } else if self.serial_stage {
                "serial: serial stage"
            } else if id.file_id == crate::FileId::VIRTUAL {
                "serial: workspace"
            } else {
                "serial: one file"
            };
            let _serial = crate::profile::PhaseGuard::new(name);
            self.step(db, recipe, id, reason);
        }
        self.cursor = None;
        crate::profile::phase("engine: rebase moved reads", || self.rebase_moved_reads(db));
        if check_settled() {
            self.report_unsettled(db);
        }
        true
    }

    /// Under `GLUALS_CHECK_SETTLED`, prints every derivation a settled run
    /// left with a read that no longer holds: a reader the run failed to
    /// queue.
    fn report_unsettled(&mut self, db: &mut DbIndex) {
        db.reader_graph_mut().forget_fingerprints();
        for id in db.reader_graph().recorded_derivations() {
            if matches!(
                id.kind,
                DerivationKind::Source | DerivationKind::Diagnostics
            ) {
                continue;
            }
            let moved = self.moved_reads(db, id);
            if !moved.is_empty() {
                eprintln!("[unsettled] {id:?} moved={moved:?}");
            }
        }
    }

    /// Files the queries filed since the last call and queues the readers of
    /// those that moved.
    fn file_queries(&mut self, db: &mut DbIndex) {
        let moved = file_queries(db);
        if !moved.is_empty() {
            self.readers_of(db, moved.iter());
        }
    }

    /// In worker supersteps: evaluates every entry queued at the first rank,
    /// each file's task on a worker thread on its own fork of the index as
    /// the superstep found it; work an evaluation queues for another file at
    /// the same rank waits for the next superstep. The root decides what each task runs, as
    /// [`Self::step`] does before it evaluates a group; a group that spans
    /// files runs in the task of the first file that queued one of its
    /// members. A group with a member in a serial file or the workspace, a
    /// query and workspace derivations run on the index after the barrier,
    /// the queries computed concurrently when the run is whole. A task
    /// evaluates its groups in order on its own fork of
    /// the index, recording against the reader graph as the superstep found
    /// it, and runs nothing more: what its writes move is found at the
    /// barrier, which installs the forks and files the records in file
    /// order before checking the readers of everything the tasks touched.
    ///
    /// Once a group ran, the superstep stops between two groups when
    /// [`StopSignal::reached`] says so. The groups it left unrun are carried:
    /// the next superstep runs them as decided, on the index as the groups
    /// that ran left it, before it takes the rank again, so a rank stopped
    /// often is decided once rather than once per stop. Deciding again could
    /// only drop a member decided stale whose reads moved back, which then
    /// runs once more than it had to and moves nothing, or add a co-producer
    /// a group gained, which the widened check after the group queues as for
    /// any group: the superstep is split, an order the run settles the same
    /// in. Entries queued meanwhile wait for the carried groups; a carried
    /// file that settles on the index since, and every carried group once the
    /// stage settles on the serial loop, is queued again instead.
    fn superstep_on_workers(&mut self, db: &mut DbIndex, recipe: &dyn Recipe, slice: Slice) {
        let Some(mut superstep) = self.plan_superstep(db, slice) else {
            return;
        };
        if !superstep.plans.is_empty() {
            self.start_tasks(db, &mut superstep, false);
            superstep.run_tasks(db, recipe);
        }
        self.finish_superstep(db, recipe, slice, superstep);
    }

    /// Plans the superstep due next and leaves it to run outside the run
    /// while readers share the index ([`Self::take_detached`]): its tasks,
    /// then, once the next slice installs them, the entries it left to the
    /// root list.
    fn detach_superstep(&mut self, db: &mut DbIndex, slice: Slice) {
        let Some(mut superstep) = self.plan_superstep(db, slice) else {
            return;
        };
        if !superstep.plans.is_empty() {
            self.start_tasks(db, &mut superstep, true);
        }
        self.detached_files = superstep
            .plans
            .iter()
            .flat_map(TaskPlan::files)
            .chain(superstep.on_root.iter().map(|(file_id, _)| *file_id))
            .chain(superstep.root.iter().map(|(id, _)| id.file_id))
            .collect();
        self.detached = Some(superstep);
    }

    /// Plans the next worker superstep: the groups the last one carried, or
    /// what each entry queued at the first rank comes to. `None` when there
    /// is nothing to run.
    fn plan_superstep(&mut self, db: &mut DbIndex, slice: Slice) -> Option<Superstep> {
        let stop = StopSignal::new(slice, self.interrupt.as_ref());
        if !self.carried.is_empty() {
            let carried = std::mem::take(&mut self.carried);
            let (plans, serial) = carried.into_iter().partition::<Vec<_>, _>(|plan| {
                !plan
                    .files()
                    .iter()
                    .any(|file_id| self.serial_files.contains(file_id))
            });
            self.requeue(serial);
            let rank = plans
                .first()
                .map(|plan| plan.groups[0].members[0].kind.rank())?;
            self.stats.supersteps += 1;
            return Some(Superstep {
                rank,
                plans,
                on_root: Vec::new(),
                root: Vec::new(),
                stop,
                tasks: None,
            });
        }
        let rank = self.worklist.first_rank()?;
        self.stats.supersteps += 1;
        let dispatching = crate::profile::PhaseGuard::new("superstep: dispatch");
        let mut plans = Vec::<TaskPlan>::new();
        let mut root = Vec::new();
        let mut on_root = Vec::new();
        let mut workspace = Vec::new();
        // Each member of a planned group, with the plan and position of its
        // group: the files of a group that spans several share one.
        let mut grouped = FxHashMap::<DerivationId, (usize, usize)>::default();
        let taken = self.worklist.take_rank_bounded(rank, SUPERSTEP_ENTRIES);
        let checked = self.check_dispatch(db, &taken);
        for ((file_id, entries), checked) in taken.into_iter().zip(checked) {
            if file_id == crate::FileId::VIRTUAL {
                workspace = entries;
                continue;
            }
            if self.serial_files.contains(&file_id) {
                on_root.push((file_id, entries));
                continue;
            }
            let mut groups = Vec::<PlannedGroup>::new();
            let plan = plans.len();
            for ((id, reason), checked) in entries.into_iter().zip(checked) {
                if let Some(&(at_plan, at)) = grouped.get(&id) {
                    let planned = if at_plan == plan {
                        &mut groups[at]
                    } else {
                        &mut plans[at_plan].groups[at]
                    };
                    planned.entries.push((id, reason));
                    continue;
                }
                match self.decide(db, id, reason, checked) {
                    Dispatched::Settled => {}
                    Dispatched::Root => root.push((id, reason)),
                    Dispatched::Task(members, rerun) => {
                        // A group of one is its own entry, queued once.
                        if members.len() > 1 {
                            grouped.extend(
                                members.iter().map(|member| (*member, (plan, groups.len()))),
                            );
                        }
                        groups.push(PlannedGroup {
                            members,
                            rerun,
                            entries: vec![(id, reason)],
                        });
                    }
                }
            }
            if !groups.is_empty() {
                plans.push(TaskPlan { file_id, groups });
            }
        }
        drop(dispatching);
        root.extend(workspace);
        Some(Superstep {
            rank,
            plans,
            on_root,
            root,
            stop,
            tasks: None,
        })
    }

    /// Installs what `superstep`'s tasks ran, then runs the entries it left
    /// to the root list until its stop signal is reached.
    fn finish_superstep(
        &mut self,
        db: &mut DbIndex,
        recipe: &dyn Recipe,
        slice: Slice,
        superstep: Superstep,
    ) {
        let Superstep {
            rank,
            plans,
            on_root,
            root,
            stop,
            tasks,
        } = superstep;
        if let Some(tasks) = tasks {
            self.install_tasks(db, recipe, rank, plans, tasks);
        }
        let _root = crate::profile::PhaseGuard::new("superstep: root list");
        for (file_id, entries) in on_root {
            if stop.reached() {
                for (id, reason) in entries {
                    self.worklist.insert(id, reason);
                }
                continue;
            }
            stop.progress();
            let deferred = self.worklist.take_file_at(rank, file_id);
            self.run_task(db, recipe, rank, file_id, entries);
            for (id, reason) in deferred {
                self.worklist.insert(id, reason);
            }
        }
        let mut root = root.into_iter().peekable();
        while let Some((id, reason)) = root.next() {
            if id.kind.is_query() {
                let mut queries = vec![(id, reason)];
                while let Some(query) = root.next_if(|(id, _)| id.kind.is_query()) {
                    queries.push(query);
                }
                let left = self.run_recorded_queries(db, recipe, slice, queries);
                self.run_root(db, recipe, &stop, left);
                continue;
            }
            self.run_root(db, recipe, &stop, vec![(id, reason)]);
        }
    }

    /// Evaluates `entries` of the root list one at a time on the index, until
    /// `stop` is reached; the rest wait for the next superstep.
    fn run_root(
        &mut self,
        db: &mut DbIndex,
        recipe: &dyn Recipe,
        stop: &StopSignal,
        entries: Vec<(DerivationId, Reason)>,
    ) {
        for (id, reason) in entries {
            if stop.reached() {
                self.worklist.insert(id, reason);
                continue;
            }
            stop.progress();
            self.file_queries(db);
            self.step(db, recipe, id, reason);
        }
    }

    /// Holds `id` until its kind's first stage when the run has not reached
    /// it. Returns whether it did.
    fn hold_for_stage(&mut self, db: &DbIndex, id: DerivationId, reason: Reason) -> bool {
        if db.analysis_stage() >= id.kind.first_stage() {
            return false;
        }
        let held = self.held.entry(id).or_insert(reason);
        *held = (*held).min(reason);
        true
    }

    /// Decides what queued `id` comes to in a worker superstep: for a group
    /// a task runs, [`Self::commit_group`] does the rest
    /// of what [`Self::step`] does before it evaluates one once the task ran
    /// it. Deciding that a group runs changes nothing but the cursor, so an
    /// entry queued again is decided afresh.
    fn decide(
        &mut self,
        db: &mut DbIndex,
        id: DerivationId,
        reason: Reason,
        checked: Option<Checked>,
    ) -> Dispatched {
        if self.hold_for_stage(db, id, reason) {
            return Dispatched::Settled;
        }
        self.cursor = Some((id, None));
        if id.kind.is_query() {
            return Dispatched::Root;
        }
        let group = match checked {
            Some(Checked::Settled(staleness)) => {
                self.settle_staleness(db, id, staleness);
                None
            }
            Some(Checked::Stale(group)) => Some(group),
            None => self
                .needs_evaluation(db, id, reason)
                .then(|| co_producers(db, id)),
        };
        let Some(group) = group else {
            self.moved_from.remove(&id);
            return Dispatched::Settled;
        };
        if group.iter().any(|member| {
            member.file_id == crate::FileId::VIRTUAL
                || self.serial_files.contains(&member.file_id)
                || self.cycle_members.contains(member)
        }) {
            return Dispatched::Root;
        }
        let rerun = self.stats.profiling()
            && group
                .iter()
                .all(|member| self.stats.distinct.contains(member));
        Dispatched::Task(group, rerun)
    }

    /// Does for `group`, which a task ran, what [`Self::step`] does before it
    /// evaluates one, in the order the groups were decided.
    fn commit_group(
        &mut self,
        db: &DbIndex,
        group: &PlannedGroup,
        moved_in_task: &[Vec<(HashedKey, derivation::Fingerprint)>],
    ) {
        let (id, reason) = group.entries[0];
        self.trace_evaluation(db, id, reason);
        let group = &group.members;
        let settled = db.analysis_stage() == AnalysisStage::Closed;
        for (member, moved_in_task) in group.iter().zip(moved_in_task) {
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
            let mut moved = self.moved_from.remove(member).unwrap_or_default();
            if member.kind.is_statement() {
                for (key, seen) in moved_in_task {
                    if moved.iter().all(|moved| moved.key != *key) {
                        moved.push(Moved {
                            key: key.clone(),
                            seen: *seen,
                            settled,
                        });
                    }
                }
                self.settled_on(*member, moved);
            }
        }
        self.evaluated
            .extend(group.iter().map(|member| member.file_id));
        if group
            .iter()
            .any(|member| member.file_id != group[0].file_id)
        {
            self.stats.cross_file_groups += 1;
        }
        for member in group {
            let runs = &mut self.stage_runs.entry(*member).or_default().in_tasks;
            *runs = runs.saturating_add(1);
            self.stats.note_runs(*member, *runs);
            // A loop that keeps moving under supersteps, where a task runs
            // only what was queued before it began, settles as the serial
            // loop settles it: its file's work runs to quiescence on the
            // index, and if that does not settle it, the stage's work does.
            if *runs > SUPERSTEP_RUN_LIMIT && self.serial_files.insert(member.file_id) {
                self.stats.serial_files += 1;
            }
            if *runs > 4 * SUPERSTEP_RUN_LIMIT && !self.serial_stage {
                self.serial_stage = true;
                self.stats.serial_stages += 1;
            }
        }
    }

    /// Whether each of the entries `taken` from files at a rank needs
    /// evaluating, and the group it runs with if it does, found on worker
    /// threads while nothing is being edited or recorded: [`Self::decide`]
    /// asks the same of the index as it stands, which deciding leaves as it
    /// is. `None` for an entry `decide` decides otherwise.
    fn check_dispatch(
        &self,
        db: &DbIndex,
        taken: &[(crate::FileId, Vec<(DerivationId, Reason)>)],
    ) -> Vec<Vec<Option<Checked>>> {
        if derivation::recording() {
            return taken
                .iter()
                .map(|(_, entries)| entries.iter().map(|_| None).collect())
                .collect();
        }
        let stage = db.analysis_stage();
        let (edits, before_edits) = (&self.edits, &self.before_edits);
        taken
            .par_iter()
            .map(|(_, entries)| {
                entries
                    .iter()
                    .map(|(id, reason)| {
                        if stage < id.kind.first_stage() || id.kind.is_query() {
                            return None;
                        }
                        Some(match staleness(db, edits, before_edits, *id, *reason) {
                            Staleness::Stale => Checked::Stale(co_producers(db, *id)),
                            settled => Checked::Settled(settled),
                        })
                    })
                    .collect()
            })
            .collect()
    }

    /// Snapshots the index and the reader graph for `superstep`'s tasks. Tasks
    /// that run detached do so while readers share the index, which is left
    /// first as a pause leaves it, so the pause writes nothing the snapshot
    /// shares; they stop only for an interrupt.
    fn start_tasks(&mut self, db: &mut DbIndex, superstep: &mut Superstep, detaching: bool) {
        let _snapshotting = crate::profile::PhaseGuard::new("superstep: snapshot");
        // Tasks lend no table, so stage rules that moved are marked here.
        let moved_rules = std::mem::take(self.moved_rules.get_mut());
        db.reader_graph_mut().mark_written(&moved_rules);
        if detaching {
            db.flush_inference_derived_state();
            superstep.stop = StopSignal::new(Slice::Whole, None);
            superstep.stop.interrupt = self.interrupt.clone();
        }
        let quarantine = db.reader_graph_mut().quarantine_releases();
        let snapshot = derivation::RecordingSnapshot::new(
            db.reader_graph_shared(),
            quarantine.started(),
            db.queries().ready_outputs(),
        );
        superstep.tasks = Some(StartedTasks {
            frozen: db.fork(),
            snapshot: Some(snapshot),
            quarantine,
            cost: self.task_costs.estimates(superstep.rank, &superstep.plans),
            profiling: self.stats.profiling(),
            outcomes: None,
        });
    }

    /// Installs the forks of the tasks that ran and files what they
    /// recorded, in file order. The groups left unrun are carried to the
    /// next superstep.
    fn install_tasks(
        &mut self,
        db: &mut DbIndex,
        recipe: &dyn Recipe,
        rank: u8,
        plans: Vec<TaskPlan>,
        tasks: StartedTasks,
    ) {
        let StartedTasks {
            mut frozen,
            snapshot,
            quarantine,
            profiling,
            outcomes,
            ..
        } = tasks;
        drop(snapshot);
        let outcomes = outcomes.unwrap_or_else(|| {
            plans
                .iter()
                .map(|_| (None, std::time::Duration::ZERO))
                .collect()
        });
        let outcomes = plans
            .iter()
            .zip(outcomes)
            .map(|(plan, (outcome, elapsed))| {
                if let Some(outcome) = &outcome {
                    self.task_costs
                        .observe(rank, plan.file_id, outcome.groups.len(), elapsed);
                }
                outcome
            })
            .collect::<Vec<_>>();
        let committing = crate::profile::PhaseGuard::new("superstep: commit");
        let mut carried = Vec::new();
        let mut started = Vec::with_capacity(outcomes.len());
        for (plan, outcome) in plans.into_iter().zip(outcomes) {
            let TaskPlan {
                file_id,
                mut groups,
            } = plan;
            let unrun =
                groups.split_off(outcome.as_ref().map_or(0, |outcome| outcome.groups.len()));
            let ran = outcome
                .as_ref()
                .map_or(&[][..], |outcome| &outcome.groups[..]);
            for (group, run) in groups.iter().zip(ran) {
                self.commit_group(db, group, &run.moved);
            }
            if !unrun.is_empty() {
                carried.push(TaskPlan {
                    file_id,
                    groups: unrun,
                });
            }
            started.extend(outcome.map(|outcome| (file_id, outcome)));
        }
        self.stats.superstep_tasks += started.len();
        drop(committing);
        let installing = crate::profile::PhaseGuard::new("superstep: install");
        let mut touched = Vec::new();
        let mut forks = Vec::with_capacity(started.len());
        let mut recordings = Vec::with_capacity(started.len());
        let mut outcomes = Vec::with_capacity(started.len());
        for (file_id, outcome) in started {
            let TaskOutcome {
                mut fork,
                recording,
                groups,
                touched,
                reruns,
            } = outcome;
            fork.release_reader_graph();
            forks.push(fork);
            recordings.push(recording);
            outcomes.push((file_id, groups, touched, reruns));
        }
        let written = recordings
            .iter_mut()
            .flat_map(|recording| recording.take_written())
            .collect::<Vec<_>>();
        // The forks install into the index while the reader graph interns the
        // keys the tasks read first: neither reads what the other writes.
        frozen.release_reader_graph();
        let mut table = db.reader_graph_mut().lend_keys();
        let (installed, new_slots) = rayon::join(
            || {
                let installed =
                    crate::profile::phase("install: tasks", || db.install_tasks(&frozen, forks));
                drop(frozen);
                crate::profile::phase("install: flatten", || db.flatten_installs());
                installed
            },
            || {
                crate::profile::phase("records: intern", || {
                    recordings
                        .iter_mut()
                        .map(|recording| recording.intern(&mut table))
                        .collect::<Vec<_>>()
                })
            },
        );
        let mut tasks = Vec::with_capacity(outcomes.len());
        for ((file_id, groups, task_touched, reruns), conflicts) in
            outcomes.into_iter().zip(installed)
        {
            if !conflicts.is_empty() {
                self.stats.conflicting_tasks += 1;
            }
            for field in &conflicts {
                *self.stats.task_conflicts.entry(field).or_default() += 1;
            }
            touched.extend(task_touched.iter().cloned());
            tasks.push((file_id, groups, task_touched, reruns, !conflicts.is_empty()));
        }
        drop(installing);
        let filing = crate::profile::PhaseGuard::new("superstep: file records");
        let canonicalising = crate::profile::PhaseGuard::new("records: canonicalise");
        let filed = recordings
            .into_par_iter()
            .zip(new_slots)
            .zip(tasks.par_iter_mut())
            .map(|((recording, new_slots), (_, groups, ..))| {
                file_task(&table, recording, &new_slots, groups)
            })
            .collect::<Vec<_>>();
        db.reader_graph_mut().restore_keys(table);
        drop(canonicalising);
        let mut released = Vec::new();
        let mut on_root = Vec::new();
        let mut records = Vec::with_capacity(filed.iter().map(|task| task.records.len()).sum());
        touched.reserve(filed.iter().map(|task| task.touched.len()).sum());
        for task in filed {
            touched.extend(task.touched);
            on_root.extend(task.on_root);
            records.extend(task.records);
        }
        let ran = tasks
            .iter()
            .flat_map(|(_, groups, ..)| groups.iter().flat_map(|group| &group.members))
            .copied()
            .collect::<FxHashSet<_>>();
        // What the derivations that ran produced before their records are
        // replaced: a fact one stops producing still came from it.
        let produced_before = {
            let graph = db.reader_graph();
            let mut produced = FxHashMap::<HashedKey, Vec<DerivationId>>::default();
            for member in &ran {
                for key in graph.outputs_of(*member) {
                    produced.entry(key.clone()).or_default().push(*member);
                }
            }
            produced
        };
        db.reader_graph_mut()
            .replace_all_kept(records, &mut released, derivation::new_stamp);
        if !self.removed_outputs.is_empty() {
            for member in tasks
                .iter()
                .flat_map(|(_, groups, ..)| groups.iter().flat_map(|group| &group.members))
            {
                if member.kind.is_statement() && self.rebuilt.contains(&member.file_id) {
                    self.settled_on_removed(db, *member);
                }
            }
        }
        for (_, groups, ..) in &tasks {
            for group in groups {
                if let Some(started) = group.elapsed.filter(|_| profiling) {
                    for member in &group.members {
                        self.stats
                            .timed_evaluation(*member, started / group.members.len() as u32);
                    }
                    self.stats
                        .timed(group.members[0].kind, started, usize::from(group.rerun), 1);
                }
            }
        }
        db.reader_graph_mut().release_keys(released);
        let mut table = db.reader_graph_mut().lend_keys();
        quarantine.finish(&mut table);
        db.reader_graph_mut().restore_keys(table);
        db.reader_graph_mut().mark_written(&written);
        drop(filing);
        let checking = crate::profile::PhaseGuard::new("superstep: readers of");
        derivation::dedup_keys(&mut touched);
        self.forget_installed(db, &touched, &written);
        db.reader_graph().uncheck_derived(&touched);
        self.queued_while_installing = Some(Vec::new());
        self.readers_of(db, touched.iter());
        let queued = self.queued_while_installing.take().unwrap_or_default();
        self.note_cycles(db, &ran, &produced_before, queued);
        drop(checking);
        let _after = crate::profile::PhaseGuard::new("superstep: after");
        let widened = tasks
            .par_iter()
            .map(|(_, groups, ..)| {
                groups
                    .iter()
                    .filter(|group| widened(db, &group.members))
                    .map(|group| group.members[0])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for ((_, _, _, reruns, _), widened) in tasks.iter().zip(widened) {
            for id in widened {
                self.stats.requeue_cause(id, "group widened");
                self.worklist.insert(id, Reason::Changed);
            }
            for id in reruns {
                self.stats
                    .requeue_cause(*id, "wrote a type under a fallback it may have read");
                self.worklist.insert(*id, Reason::Changed);
            }
        }
        // A group that read a query the snapshot had no answer for runs again
        // in the next superstep, once the queries it read are computed here;
        // one still lacking an answer after that runs here, where queries are
        // computed as they are read.
        for (members, queries) in on_root {
            let deferrals = self.query_deferrals.entry(members[0]).or_default();
            if !queries.is_empty() && *deferrals < QUERY_DEFERRALS {
                *deferrals += 1;
                for key in &queries {
                    if db.queries().ready_fingerprint(key.key()).is_none() {
                        self.file_queries(db);
                        let query = db.queries().id_of(key.key());
                        self.step(db, recipe, query, Reason::Changed);
                    }
                }
                self.stats
                    .requeue_cause(members[0], "read a query the snapshot had no answer for");
                self.worklist.insert(members[0], Reason::Changed);
                continue;
            }
            for id in members {
                self.file_queries(db);
                self.step(db, recipe, id, Reason::Changed);
            }
        }
        // A task installed over an earlier task's change left a value that
        // lacks what one of them wrote: every producer of a primary key both
        // touched runs again here, which resets the key and computes it from
        // all its producers.
        let _repairing = crate::profile::PhaseGuard::new("superstep: conflict repair");
        let mut touched_before = FxHashSet::default();
        let mut again = BTreeSet::new();
        for (_, _, touched, _, conflicted) in &tasks {
            if *conflicted {
                for key in touched
                    .intersection(&touched_before)
                    .filter(|key| key.is_primary())
                {
                    again.extend(
                        db.reader_graph()
                            .producers(key)
                            .filter(|producer| producer.kind != DerivationKind::Source),
                    );
                }
            }
            touched_before.extend(touched.iter().cloned());
        }
        let mut ran = FxHashSet::default();
        for id in again {
            if ran.contains(&id) {
                continue;
            }
            ran.extend(co_producers(db, id));
            self.file_queries(db);
            self.step(db, recipe, id, Reason::Changed);
        }
        self.carried = carried;
    }

    /// Queues again every entry the carried groups took.
    fn requeue_carried(&mut self) {
        let carried = std::mem::take(&mut self.carried);
        self.requeue(carried);
    }

    /// Queues again every entry `plans`' groups took.
    fn requeue(&mut self, plans: Vec<TaskPlan>) {
        for (id, reason) in plans
            .into_iter()
            .flat_map(|plan| plan.groups)
            .flat_map(|group| group.entries)
        {
            self.worklist.insert(id, reason);
        }
    }

    /// Notes the cycles among the derivations a superstep `ran`: a reader
    /// `queued` by a key one of them produces, or `produced_before` the
    /// superstep, reaches that producer. The members of a loop of reaches
    /// whose outputs alternate run on the index from then on (see
    /// [`Self::cycle_members`]).
    fn note_cycles(
        &mut self,
        db: &DbIndex,
        ran: &FxHashSet<DerivationId>,
        produced_before: &FxHashMap<HashedKey, Vec<DerivationId>>,
        queued: Vec<(DerivationId, HashedKey)>,
    ) {
        let graph = db.reader_graph();
        let mut reaches = FxHashMap::<DerivationId, Vec<DerivationId>>::default();
        for (reader, key) in queued {
            if !ran.contains(&reader) {
                continue;
            }
            let before = produced_before.get(&key).into_iter().flatten().copied();
            for producer in graph.producers(&key).chain(before) {
                if producer != reader && ran.contains(&producer) {
                    reaches.entry(reader).or_default().push(producer);
                }
            }
        }
        for component in loops_of(&reaches) {
            let alternates = component
                .iter()
                .any(|member| self.outputs_alternate(db, *member));
            if alternates {
                self.stats.cycle_members += component
                    .into_iter()
                    .filter(|member| self.cycle_members.insert(*member))
                    .count();
            }
        }
    }

    /// Records the fingerprint of what `member` produces now, and returns
    /// whether it is what it produced two cycles back but not one: its
    /// outputs alternate. A cycle that only grows its facts never returns to
    /// an earlier state.
    fn outputs_alternate(&mut self, db: &DbIndex, member: DerivationId) -> bool {
        let mut outputs = db
            .reader_graph()
            .outputs_of(member)
            .map(|key| {
                (
                    derivation::fingerprint_of(key.key()),
                    self.fingerprint(db, key),
                )
            })
            .collect::<Vec<_>>();
        outputs.sort_unstable();
        let now = derivation::fingerprint_of(&outputs);
        let history = self.cycle_outputs.entry(member).or_default();
        let alternates = history[0] == Some(now) && history[1] != Some(now);
        *history = [history[1], Some(now)];
        alternates
    }

    /// Forgets what was taken of the index before the tasks' forks were
    /// installed: inference, and the cached fingerprints of the keys the
    /// tasks `touched` and `written`, the only ones whose values they moved.
    fn forget_installed(&mut self, db: &DbIndex, touched: &[HashedKey], written: &[HashedKey]) {
        let graph = db.reader_graph();
        touched
            .par_iter()
            .chain(written.par_iter())
            .for_each(|key| graph.forget_fingerprint(key));
        derivation::advance_write_generation();
        let before_edits = self.before_edits_mut();
        for key in touched.iter().chain(written) {
            forget_before_edits(before_edits, graph, key);
        }
    }

    /// Evaluates `entries` of `file_id` and what they queue in the file at
    /// `rank`, to quiescence. Returns every derivation it took.
    fn run_task(
        &mut self,
        db: &mut DbIndex,
        recipe: &dyn Recipe,
        rank: u8,
        file_id: crate::FileId,
        entries: Vec<(DerivationId, Reason)>,
    ) -> Vec<DerivationId> {
        let mut local = Queue::new(self.worklist.schedule);
        for (id, reason) in entries {
            local.insert(id, reason);
        }
        let mut ran = Vec::new();
        self.task = Some(file_id);
        super::parallel::in_task(&[file_id], || {
            while let Some((id, reason)) = local.pop_first() {
                ran.push(id);
                self.file_queries(db);
                self.step(db, recipe, id, reason);
                for (id, reason) in self.worklist.take_file_at(rank, file_id) {
                    local.insert(id, reason);
                }
            }
        });
        self.task = None;
        ran
    }

    /// Evaluates the queued `id` if it is due and stale: the work of one pop of
    /// [`Self::run`].
    fn step(&mut self, db: &mut DbIndex, recipe: &dyn Recipe, id: DerivationId, reason: Reason) {
        if self.hold_for_stage(db, id, reason) {
            return;
        }
        self.cursor = Some((id, Some(reason)));
        let stale = crate::profile::phase("engine: stale check", || {
            self.needs_evaluation(db, id, reason)
        });
        if !stale || !self.admit_run(id) {
            self.moved_from.remove(&id);
            if id.kind.is_query() {
                db.queries().set_stale(id, false);
            }
            return;
        }
        self.trace_evaluation(db, id, reason);
        let group = crate::profile::phase("engine: co-producers", || co_producers(db, id));
        if group.len() == 1 && self.task.is_none() && recipe.splits(id.kind) {
            crate::profile::phase("engine: batch", || self.run_batch(db, id, recipe));
            self.queue_reruns();
            return;
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
            .filter(|member| member.kind.is_statement() && self.rebuilt.contains(&member.file_id))
            .collect::<Vec<_>>();
        let started = self.stats.profiling().then(std::time::Instant::now);
        let touched = crate::profile::phase("engine: evaluate", || {
            self.evaluate_group(db, &group, recipe)
        });
        for member in &group {
            if self.back_edges.remove(member)
                && self.outputs_alternate(db, *member)
                && self.cycle_members.insert(*member)
            {
                self.stats.cycle_members += 1;
            }
        }
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
        if widened(db, &group) {
            // Again in this round: nothing ran after it yet.
            self.stats.requeue_cause(id, "group widened");
            self.worklist.insert(id, Reason::Changed);
        }
        self.queue_reruns();
        crate::profile::phase("engine: readers of", || self.readers_of(db, touched.iter()));
        for member in rebuilt {
            self.settled_on_removed(db, member);
        }
    }

    /// Notes that `member`, a statement of a rebuilt file, settled before the
    /// run on what the removed derivations it reads wrote, unless its loop
    /// was cleared before release since: settled from nothing after the
    /// removal, it holds nothing the removed derivations supplied.
    fn settled_on_removed(&mut self, db: &DbIndex, member: DerivationId) {
        if self.removed_outputs.is_empty() || self.cleared_parked.contains(&member) {
            return;
        }
        let removed = db
            .reader_graph()
            .reads_of(member)
            .filter_map(|key| {
                let seen = *self.removed_outputs.get(key)?;
                Some(Moved {
                    key: key.clone(),
                    seen,
                    settled: true,
                })
            })
            .collect::<Vec<_>>();
        self.settled_on(member, removed);
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
            || self.settled_before_run(member))
            && !self.key_holds(db, &moved.key)(removed.unwrap_or(moved.seen))
    }

    /// Whether statement `member` holds what a run that settled before this
    /// one left: it ran in this run with a record from before it, or it is a
    /// reader that has not run in it yet; or what a loop cleared with its own
    /// held on the way.
    fn settled_before_run(&self, member: DerivationId) -> bool {
        self.from_before.contains(&member)
            || self.settled_early.contains(&member)
            || !self.ran.contains(&member)
    }

    /// Whether a fact `member` of `lp` reads from outside the loop now holds
    /// another value than the one `member` had settled on: one it settled on
    /// when it ran, or, when it is among the `parked` readers that have yet
    /// to run again, one that moved under it since.
    fn moved_from_outside(
        &self,
        db: &DbIndex,
        lp: &[DerivationId],
        member: DerivationId,
        parked: &FxHashSet<DerivationId>,
    ) -> bool {
        let graph = db.reader_graph();
        let moved_since = parked
            .contains(&member)
            .then(|| self.moved_from.get(&member))
            .flatten();
        self.settled_on
            .get(&member)
            .into_iter()
            .chain(moved_since)
            .flatten()
            .any(|moved| {
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
    }

    /// Whether a fact read from outside `lp` moved under a member of it or
    /// under a derivation that shares a key one writes: such a co-producer
    /// runs and is cleared with the loop, and what the loop holds can rest on
    /// what it supplied.
    fn loop_moved_from_outside(
        &self,
        db: &DbIndex,
        lp: &[DerivationId],
        parked: &FxHashSet<DerivationId>,
    ) -> bool {
        with_co_producers(db, lp.iter().copied())
            .into_iter()
            .any(|producer| self.moved_from_outside(db, lp, producer, parked))
    }

    /// At quiescence, once no source layer is left to rebuild, clears every
    /// loop of statements, closed directly or through the passes that run
    /// once statements settle (see [`DerivationKind::closes_loops`]), one of
    /// whose statements, or a derivation sharing a key one writes, reads from
    /// outside the loop a fact that moved since it settled on it, and that is not cleared already (see
    /// [`Self::settled_loop_members`]): each member's primary outputs, and
    /// those of whatever shares them, go back to the source layer, they run
    /// again, and the run starts over from the first stage, as a cold build
    /// settles a loop through them. Returns whether it cleared any; one that
    /// cleared none clears for good the loops cleared at quiescence since
    /// the last that did not.
    fn clear_loops(&mut self, db: &mut DbIndex) -> bool {
        let settled = self.settled_loop_members(db);
        // After a check that cleared loops, what a loop's members read and
        // write, and what their facts hold, moved only through the keys
        // checked since: a loop it left that the next one clears is
        // downstream of one.
        let since = crate::profile::phase("loops: since", || {
            self.since_search.take().map(|keys| {
                db.reader_graph()
                    .readers_downstream(keys.iter(), DerivationKind::closes_loops)
            })
        });
        let _roots = crate::profile::PhaseGuard::new("loops: roots");
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
        if crate::profile::tracing_evaluations() {
            for root in &roots {
                let moved = self.settled_on[root]
                    .iter()
                    .filter(|moved| self.net_moved(db, *root, moved))
                    .map(|moved| {
                        format!(
                            "{:?} seen={} settled={} removed={:?} from_before={}",
                            moved.key.key(),
                            moved.seen,
                            moved.settled,
                            self.removed_outputs.get(&moved.key),
                            self.from_before.contains(root)
                        )
                    })
                    .collect::<Vec<_>>();
                self.trace(db, *root, "loop root", &moved);
            }
        }
        if roots.is_empty() {
            self.commit_cleared_loops();
            self.settled_on.clear();
            return false;
        }
        // A loop downstream of another that is cleared waits for it to
        // settle: cleared together, it would settle from what the other held
        // on the way, not from what the other settles to.
        drop(_roots);
        let _search = crate::profile::PhaseGuard::new("loops: search");
        let parked = FxHashSet::default();
        let loops = db.reader_graph().read_loops(
            with_co_producers(db, roots),
            DerivationKind::closes_loops,
            |lp| {
                lp.iter()
                    .any(|member| !self.cleared.contains(member) && !settled.contains(member))
                    && self.loop_moved_from_outside(db, lp, &parked)
            },
        );
        if loops.is_empty() {
            self.commit_cleared_loops();
            self.settled_on.clear();
            return false;
        }
        drop(_search);
        self.since_search = Some(FxHashSet::default());
        self.clear(db, loops, true);
        true
    }

    /// Of the loops cleared at quiescence since the last check that cleared
    /// none, the members of those this check leaves: no other loop cleared
    /// since, as late or later, reaches one through what moved since, unless
    /// it reaches that one in turn. Each loop such a one reaches settled on
    /// what it held on the way, so every fact that moved under one of its
    /// members counts, and the check clears it again if one from outside did.
    fn settled_loop_members(&mut self, db: &DbIndex) -> FxHashSet<DerivationId> {
        let _order = crate::profile::PhaseGuard::new("loops: order");
        let loops = self
            .pending_loops
            .iter()
            .map(|lp| (lp.members.as_slice(), lp.wave))
            .collect::<Vec<_>>();
        let upstream =
            db.reader_graph()
                .latest_upstream_loops(&loops, &self.pending_moved, |kind| {
                    !matches!(kind, DerivationKind::Source | DerivationKind::Diagnostics)
                });
        let mut settled = FxHashSet::default();
        for (lp, upstream) in self.pending_loops.iter().zip(upstream) {
            if upstream.is_some_and(|wave| wave >= lp.wave) {
                self.settled_early.extend(lp.members.iter().copied());
            } else if lp.at_quiescence {
                settled.extend(lp.members.iter().copied());
            }
        }
        settled
    }

    /// Clears for good the loops cleared at quiescence since the last check
    /// that cleared none.
    fn commit_cleared_loops(&mut self) {
        for lp in std::mem::take(&mut self.pending_loops) {
            if lp.at_quiescence {
                self.cleared.extend(lp.members);
            }
        }
        self.pending_moved.clear();
    }

    /// Once a focus or a loop settled, before the waiting work `released` is
    /// queued: clears the loops [`Self::clear_loops`] would clear once that
    /// work ran that have a member among it, if none of that work, nor a
    /// source layer still to rebuild, lies upstream of them. Run first, their
    /// members would settle on what the loops held, and so would whatever
    /// reads them, all to run again once the loops are cleared. A loop cleared
    /// here that a fact from outside moves under after all is cleared once
    /// more at quiescence, so what it holds never rests on how far that work
    /// had settled. Returns whether it cleared any.
    fn clear_parked_loops(&mut self, db: &mut DbIndex, released: &[DerivationId]) -> bool {
        if released.is_empty() {
            return false;
        }
        let parked = released.iter().copied().collect::<FxHashSet<_>>();
        // Statements that settled on a moved fact root loops too, so a loop
        // downstream of one of theirs waits for it.
        let roots = released
            .iter()
            .filter_map(|reader| self.moved_from.get_key_value(reader))
            .chain(&self.settled_on)
            .filter(|(member, moved)| {
                moved
                    .iter()
                    .any(|moved| self.net_moved(db, **member, moved))
            })
            .map(|(member, _)| *member)
            .collect::<BTreeSet<_>>();
        if roots.is_empty() {
            return false;
        }
        // A loop cleared since the last check that cleared none settles at
        // quiescence.
        let pending = self
            .pending_loops
            .iter()
            .flat_map(|lp| &lp.members)
            .copied()
            .collect::<FxHashSet<_>>();
        let loops = db.reader_graph().read_loops(
            with_co_producers(db, roots),
            DerivationKind::closes_loops,
            |lp| {
                lp.iter().any(|member| {
                    !self.cleared.contains(member)
                        && !self.cleared_parked.contains(member)
                        && !pending.contains(member)
                }) && self.loop_moved_from_outside(db, lp, &parked)
            },
        );
        let loops = loops
            .into_iter()
            .filter(|lp| lp.iter().any(|member| parked.contains(member)))
            .collect::<Vec<_>>();
        if loops.is_empty() {
            return false;
        }
        // Waiting work outside the loops moves what they read when it runs,
        // through derivations of any kind.
        let downstream = {
            let graph = db.reader_graph();
            let members = loops
                .iter()
                .flatten()
                .flat_map(|statement| co_producers(db, *statement))
                .collect::<FxHashSet<_>>();
            let sources = self.stale_sources.iter().flat_map(|file_id| {
                graph
                    .derivations_of_file(*file_id)
                    .filter(|id| id.kind == DerivationKind::Source)
            });
            let upstream = released
                .iter()
                .copied()
                .filter(|reader| !members.contains(reader))
                .chain(sources)
                .collect::<Vec<_>>();
            let outputs = upstream.iter().flat_map(|id| graph.outputs_of(*id));
            graph.readers_downstream(outputs, |_| true)
        };
        let loops = loops
            .into_iter()
            .filter(|lp| !lp.iter().any(|member| downstream.contains(member)))
            .collect::<Vec<_>>();
        if loops.is_empty() {
            return false;
        }
        self.stats.loops_cleared_parked += loops.len();
        self.cleared_parked.extend(loops.iter().flatten().copied());
        self.clear(db, loops, false);
        true
    }

    /// Clears `loops`, at quiescence or before waiting work is released:
    /// each member's primary outputs, and those of whatever shares them, go
    /// back to the source layer, they run again before any other work, and
    /// the run starts over from the first stage. Checks at quiescence clear
    /// them again or leave them (see [`Self::settled_loop_members`]).
    fn clear(&mut self, db: &mut DbIndex, loops: Vec<Vec<DerivationId>>, at_quiescence: bool) {
        let _clear = crate::profile::PhaseGuard::new("loops: clear");
        if crate::profile::tracing_evaluations() {
            self.trace_loop_edges(db, &loops);
        }
        self.stats.loop_waves += 1;
        self.stats.loops_cleared += loops.len();
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
        self.pending_loops
            .retain(|lp| !lp.members.iter().any(|member| members.contains(member)));
        let wave = self.stats.loop_waves;
        self.pending_loops
            .extend(loops.into_iter().map(|lp| ClearedLoop {
                members: lp,
                at_quiescence,
                wave,
            }));
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
            forget_before_edits(self.before_edits_mut(), db.reader_graph(), key);
        }
        self.loop_focus = members.iter().copied().collect();
        // Settled from nothing, a loop whose rules leave it more than one
        // fixed point lands on the one the order it runs in reaches, so it
        // settles in file-id order whatever order the run takes files in.
        debug_assert!(self.worklist.is_empty() && self.next_round.is_empty());
        self.worklist = Queue::new(Schedule::Canonical);
        self.next_round = Queue::new(Schedule::Canonical);
        self.cursor = None;
        self.readers_of(db, touched.iter());
        for member in members {
            // The loop's own reset is not a fact from outside it.
            self.moved_from.remove(&member);
            self.settled_on.remove(&member);
            self.stats.requeue_cause(member, "loop cleared");
            self.queue(member, Reason::Changed);
        }
        self.enter_stage(db, AnalysisStage::Open);
    }

    /// Under `GLUALS_TRACE_EVALUATIONS`, prints each loop about to be cleared:
    /// its members by kind, its files, and the kinds of the facts that close
    /// it, read by one member and written by another.
    fn trace_loop_edges(&self, db: &DbIndex, loops: &[Vec<DerivationId>]) {
        let graph = db.reader_graph();
        for lp in loops {
            let members = lp.iter().copied().collect::<FxHashSet<_>>();
            let mut kinds = BTreeMap::<String, usize>::new();
            for member in lp {
                *kinds.entry(format!("{:?}", member.kind)).or_default() += 1;
            }
            let files = lp
                .iter()
                .map(|member| member.file_id)
                .collect::<FxHashSet<_>>()
                .len();
            let mut edges = FxHashMap::<String, usize>::default();
            for member in lp {
                for key in graph.reads_of(*member) {
                    let base = key.base().map(HashedKey::new);
                    let closes = graph
                        .producers(key)
                        .chain(base.iter().flat_map(|base| graph.producers(base)))
                        .any(|producer| producer != *member && members.contains(&producer));
                    if closes {
                        *edges.entry(variant_name(key)).or_default() += 1;
                    }
                }
            }
            let mut edges = edges.into_iter().collect::<Vec<_>>();
            edges.sort_unstable_by(|left, right| {
                right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0))
            });
            eprintln!(
                "[trace] loop of {} members in {files} files {kinds:?}, closed by {edges:?}",
                lp.len()
            );
        }
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

    /// Counts a run of `id` on the index and returns whether it may run. Past
    /// [`MAX_INDEX_RUNS`] within one stage the facts it reads keep moving, so
    /// some rule among them does not converge: it panics under
    /// `GLUALS_CHECK_CONVERGENCE` or debug assertions, and otherwise warns
    /// once and leaves `id` as its last run left it.
    fn admit_run(&mut self, id: DerivationId) -> bool {
        let runs = &mut self.stage_runs.entry(id).or_default().on_index;
        *runs = runs.saturating_add(1);
        self.stats.note_runs(id, *runs);
        if *runs <= MAX_INDEX_RUNS {
            return true;
        }
        assert!(
            !self.unsettled_panics,
            "{id:?} evaluated {runs} times without the run moving to another stage"
        );
        if *runs == MAX_INDEX_RUNS + 1 {
            log::warn!(
                "{id:?} ran {MAX_INDEX_RUNS} times without settling; it keeps what its last run left"
            );
        }
        false
    }

    /// Moves the run to `stage`. Returns the keys of the rules that moved.
    fn enter_stage(&mut self, db: &mut DbIndex, stage: AnalysisStage) -> Vec<HashedKey> {
        self.stage_runs.clear();
        self.serial_files.clear();
        self.cycle_members.clear();
        self.cycle_outputs.clear();
        self.back_edges.clear();
        self.query_deferrals.clear();
        self.serial_stage = false;
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
    /// moved at the stage the run reached, and marks the source layers that
    /// still read a moved fact; or, when it can clear loops among them first
    /// ([`Self::clear_parked_loops`]), leaves the rest waiting for those.
    /// Returns whether it queued any.
    fn release_waiting(&mut self, db: &mut DbIndex) -> bool {
        self.focus.clear();
        self.handed_back = false;
        if !self.loop_focus.is_empty() {
            let _held = crate::profile::PhaseGuard::new("release: held");
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
        let _warm = crate::profile::PhaseGuard::new("release: warm");
        db.reader_graph().warm_reads(
            self.waiting
                .iter()
                .filter(|(_, reason)| **reason == Reason::Read)
                .map(|(reader, _)| *reader),
            |key| db.fact_fingerprint(key),
        );
        let waiting = std::mem::take(&mut self.waiting)
            .into_iter()
            .collect::<Vec<_>>();
        drop(_warm);
        let _staleness = crate::profile::PhaseGuard::new("release: staleness");
        let checked = {
            let (db, edits, before_edits) = (&*db, &self.edits, &self.before_edits);
            waiting
                .par_iter()
                .map(|(reader, reason)| staleness(db, edits, before_edits, *reader, *reason))
                .collect::<Vec<_>>()
        };
        drop(_staleness);
        let _settle = crate::profile::PhaseGuard::new("release: settle");
        let mut released = Vec::new();
        for ((reader, reason), staleness) in waiting.into_iter().zip(checked) {
            if reason == Reason::Changed {
                released.push((reader, reason));
                continue;
            }
            let moved = if crate::profile::tracing_evaluations() {
                self.moved_reads(db, reader)
            } else {
                Vec::new()
            };
            if !self.settle_staleness(db, reader, staleness) {
                self.moved_from.remove(&reader);
                continue;
            }
            if crate::profile::tracing_evaluations() {
                self.trace(db, reader, "released", &moved);
            }
            if reader.kind == DerivationKind::Source {
                self.stale_sources.insert(reader.file_id);
            } else {
                released.push((reader, reason));
            }
        }
        drop(_settle);
        let readers = released
            .iter()
            .map(|(reader, _)| *reader)
            .collect::<Vec<_>>();
        if crate::profile::phase("release: loops", || self.clear_parked_loops(db, &readers)) {
            // The rest waits for the loops, then is checked again.
            for (reader, reason) in released {
                if !self.loop_focus.contains(&reader) {
                    self.wait(reader, reason);
                }
            }
            return true;
        }
        for (reader, _) in released {
            // Found to need evaluating, it is queued as changed rather than
            // checked again.
            self.stats.requeue_cause(reader, "released after the focus");
            self.worklist.insert(reader, Reason::Changed);
            queued += 1;
        }
        self.stats.focus_settled(queued);
        self.cursor = None;
        // Files prioritised while the loops settled become the focus now,
        // whether or not the release queued anything.
        let after_loops = std::mem::take(&mut self.after_loops);
        if !after_loops.is_empty() {
            self.prioritise(&after_loops);
            return true;
        }
        queued > 0
    }

    /// Whether `id`, queued for `reason`, has to run: a reader runs only if a
    /// fact it read moved. A stage rule it saw holds: only a stage barrier
    /// moves one, and the barrier queues the rule's readers itself.
    fn needs_evaluation(&mut self, db: &mut DbIndex, id: DerivationId, reason: Reason) -> bool {
        let staleness = staleness(db, &self.edits, &self.before_edits, id, reason);
        self.settle_staleness(db, id, staleness)
    }

    /// Applies what [`staleness`] found of `id`: whether it has to run.
    fn settle_staleness(
        &mut self,
        db: &mut DbIndex,
        id: DerivationId,
        staleness: Staleness,
    ) -> bool {
        match staleness {
            Staleness::Stale => true,
            Staleness::Holds => false,
            Staleness::Rebased(moved) => {
                db.reader_graph_mut().set_fingerprints(id, moved);
                false
            }
        }
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
                let _recording = derivation::begin(id);
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
                        let _recording = derivation::resume(evaluation);
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
        recipe.settle();
        for (member, elapsed) in times {
            self.stats.timed_evaluation(member, elapsed);
        }
        for (member, reads) in batch.iter().zip(computed_reads.iter().flatten()) {
            assert_batch_reads_held(db, *member, reads);
        }
        self.record(db, evaluations, &mut touched, &FxHashMap::default(), || {
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

    /// Evaluates the queries of a superstep's root list that have to run,
    /// computing them concurrently each on a pool worker. A query reads only the index, which publishing one leaves as it
    /// is, so each computes what it would have alone; one that read another
    /// query still being computed is evaluated again in order. Returns the
    /// entries left to evaluate one at a time: all of them unless the run is
    /// whole and enough have to run to pay for the workers.
    fn run_recorded_queries(
        &mut self,
        db: &mut DbIndex,
        recipe: &dyn Recipe,
        slice: Slice,
        queries: Vec<(DerivationId, Reason)>,
    ) -> Vec<(DerivationId, Reason)> {
        if !matches!(slice, Slice::Whole)
            || !super::parallel::records_on_workers()
            || super::parallel::runs_inline(queries.len())
        {
            return queries;
        }
        let mut batch = Vec::new();
        let mut popped = Vec::new();
        for (id, reason) in queries {
            self.file_queries(db);
            if self.hold_for_stage(db, id, reason) {
                continue;
            }
            self.cursor = Some((id, Some(reason)));
            if !self.needs_evaluation(db, id, reason) || !self.admit_run(id) {
                self.moved_from.remove(&id);
                db.queries().set_stale(id, false);
                continue;
            }
            let batched = co_producers(db, id).len() == 1;
            if batched {
                batch.push(id);
            }
            popped.push((id, reason, batched));
        }
        if super::parallel::runs_inline(batch.len()) {
            return popped
                .into_iter()
                .map(|(id, reason, _)| (id, reason))
                .collect();
        }
        self.stats.queries_on_workers += batch.len();
        self.publish_recorded_batch(db, batch[0].kind, batch, popped, recipe);
        Vec::new()
    }

    /// Computes `batch`, derivations of `kind` that each need evaluating
    /// alone, on pool workers against the canonical index and publishes them
    /// in order on this thread; `popped` holds every derivation the batch was
    /// gathered from, with whether it joined it. A result whose reads no
    /// longer hold, or that read a key another member's reset moved, is
    /// discarded and evaluated here instead.
    fn publish_recorded_batch(
        &mut self,
        db: &mut DbIndex,
        kind: DerivationKind,
        batch: Vec<DerivationId>,
        popped: Vec<(DerivationId, Reason, bool)>,
        recipe: &dyn Recipe,
    ) {
        let mut alone = Vec::new();
        for (id, reason, batched) in popped {
            self.trace_evaluation(db, id, reason);
            if !batched {
                alone.push(id);
            }
        }
        let mut owners = ResetOwners::default();
        for id in &batch {
            for key in db.reader_graph().outputs_of(*id) {
                owners.note(key.clone(), Some(*id));
            }
        }
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
            Some(&mut owners),
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
            super::parallel::map_derivations_collect(db, &batch, |db, id| {
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
                .all(|key| !reset.contains(key) || owners.owner(key) == Some(*id));
            if profiling {
                self.stats.parallel_attempts += 1;
            }
            let mut table = db.reader_graph_mut().lend_keys();
            let evaluation = if valid {
                let evaluation = result
                    .evaluation
                    .canonicalise(&mut table)
                    .expect("validated worker slots");
                derivation::lend_keys(table);
                let lent = scopeguard_lent_keys();
                let _recording = derivation::resume(evaluation);
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
                let _recording = derivation::begin(*id);
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
                &FxHashMap::default(),
                || format!("publishing {id:?}"),
            );
            touched.extend(published);
            self.stats.timed_evaluation(
                *id,
                computing + started.map(|started| started.elapsed()).unwrap_or_default(),
            );
        }
        recipe.settle();
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let lent = scopeguard_lent_keys();
        derivation::note_recomputed(|key| recomputing.get(key) == Some(&db.fact_fingerprint(key)));
        let mut table = derivation::take_keys();
        std::mem::forget(lent);
        quarantine.finish(&mut table);
        db.reader_graph_mut().restore_keys(table);
        if let Some(started) = started {
            self.stats
                .timed(kind, started.elapsed(), reruns, batch.len());
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
    /// them with what the reset wrote, which `written` also takes. `owners`,
    /// holding each output's producer, also takes for every key the reset
    /// wrote the producer of the output whose reset wrote it.
    fn reset_outputs(
        &self,
        db: &mut DbIndex,
        members: &[DerivationId],
        also: &FxHashSet<HashedKey>,
        deferred: &FxHashSet<HashedKey>,
        written: Option<&mut FxHashSet<HashedKey>>,
        owners: Option<&mut ResetOwners>,
    ) -> FxHashSet<HashedKey> {
        let mut touched = also.clone();
        for member in members {
            touched.extend(db.reader_graph().outputs_of(*member).cloned());
        }
        *self.recomputing.borrow_mut() = touched
            .iter()
            .map(|key| (key.clone(), self.fingerprint(db, key)))
            .collect();
        let resets = touched
            .iter()
            .filter(|key| key.is_primary() && !deferred.contains(*key));
        let reset = match owners {
            None => derivation::capture_writes(|| {
                for key in resets {
                    db.reset_fact(key);
                }
            }),
            Some(owners) => {
                let wrote = resets
                    .map(|key| {
                        let producer = owners.owner(key);
                        (producer, derivation::capture_writes(|| db.reset_fact(key)))
                    })
                    .collect::<Vec<_>>();
                let mut reset = FxHashSet::default();
                for (producer, keys) in wrote {
                    for key in keys {
                        owners.note(key.clone(), producer);
                        reset.insert(key);
                    }
                }
                reset
            }
        };
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
    /// Each key is reset only as the first member that writes it runs, so a
    /// member before that reads it as the group last left it and is recorded
    /// as having seen that: members settle as they would one by one, whatever
    /// grouped them. A key two members write is read from its reset on as
    /// the group leaves it: the group re-runs as a unit, so what a later
    /// member read of it is internal to the group, not a stale input. Which
    /// keys two members write comes from their records; a run that shows two
    /// members writing a key not reset before either ran is discarded, and
    /// the group runs again with it reset first, along with everything the
    /// discarded run wrote.
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
            let resets = group_resets(db, group, &shared, &discarded);
            let deferred = resets.deferred();
            let GroupResets {
                mut front,
                own,
                joint,
            } = resets;
            touched.extend(self.reset_outputs(
                db,
                group,
                &discarded,
                &deferred,
                (group.len() > 1).then_some(&mut front),
                None,
            ));
            let mut reset = front.into_iter().map(|key| (key, 0)).collect();
            let mut cleared = FxHashSet::default();
            let (evaluations, found) = self.with_lent_keys(db, &touched, |db| {
                run_group(
                    db,
                    group,
                    own,
                    joint,
                    &mut reset,
                    &mut cleared,
                    |db, member| {
                        let _recording = derivation::begin(member);
                        recipe.evaluate(db, member);
                        let evaluation = derivation::end();
                        let outputs = evaluation.outputs().cloned().collect();
                        (evaluation, outputs)
                    },
                )
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
    /// [`derivation::ReaderGraph::seen_by_member`]), `reset` holding the
    /// position in the group of each key reset as the group ran.
    fn record(
        &self,
        db: &mut DbIndex,
        evaluations: Vec<(derivation::Evaluation, Option<u64>)>,
        touched: &mut FxHashSet<HashedKey>,
        reset: &FxHashMap<HashedKey, usize>,
        step: impl Fn() -> String,
    ) {
        // The queries the evaluations computed are filed first, so the reads
        // below fingerprint what they moved; the slots they no longer keep
        // are freed with the evaluations' own, which may read them.
        let mut released = Vec::new();
        touched.extend(file_queries_into(db, &mut released));
        for key in touched.iter().chain(
            evaluations
                .iter()
                .flat_map(|(evaluation, _)| evaluation.outputs().chain(evaluation.moved())),
        ) {
            db.reader_graph().forget_fingerprint(key);
            forget_before_edits(&mut lock(&self.before_edits), db.reader_graph(), key);
        }
        self.verify_fingerprints(db, touched, step);
        // Out of the index while its records are replaced, so the reads can be
        // fingerprinted against it.
        let mut graph = std::mem::take(db.reader_graph_mut());
        for (position, (evaluation, ended)) in evaluations.into_iter().enumerate() {
            touched.extend(evaluation.moved().cloned());
            let (id, mut slots, reads, outputs) = evaluation.into_recorded();
            let now = derivation::new_stamp();
            slots.extend(reads.into_iter().map(|key| graph.intern_read_at(key, now)));
            // A resumed evaluation read some keys both before and after the
            // table was lent.
            slots.sort_unstable();
            slots.dedup();
            touched.extend(outputs.iter().cloned());
            let slots = match ended {
                Some(ended) => {
                    graph.seen_by_member(slots, ended, |key| reset_before(reset, key, position))
                }
                None => slots.into_iter().map(|slot| (slot, None)).collect(),
            };
            graph.replace_slots(
                id,
                slots,
                outputs,
                |key| db.fact_fingerprint(key),
                &mut released,
                now,
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
    /// Supersteps run, and file tasks across them.
    supersteps: usize,
    superstep_tasks: usize,
    /// Groups of derivations in several files that tasks ran, and queries the
    /// root list computed on workers.
    cross_file_groups: usize,
    queries_on_workers: usize,
    /// Tasks run again on the index, by the first value they conflicted on.
    conflicting_tasks: usize,
    task_conflicts: BTreeMap<&'static str, usize>,
    /// Files and stages a worker run settled on the index, and derivations
    /// of a cycle among a superstep's work it ran there.
    serial_files: usize,
    serial_stages: usize,
    cycle_members: usize,
    /// The most times one derivation ran within a stage, and which one.
    most_runs: Option<(u16, DerivationId)>,
    /// Loops of statements cleared, and the derivations they ran again.
    pub(crate) loops_cleared: usize,
    pub(crate) loop_members_cleared: usize,
    /// How often loops were cleared, each time settling together from the
    /// first stage, and how many of them were cleared before the waiting
    /// work they belong to was released.
    pub(crate) loop_waves: usize,
    pub(crate) loops_cleared_parked: usize,
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
            crate::profile::memory::checkpoint(&format!("stage {stage:?}"));
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

    fn note_runs(&mut self, id: DerivationId, runs: u16) {
        if self.most_runs.is_none_or(|(most, _)| runs > most) {
            self.most_runs = Some((runs, id));
        }
    }

    /// The most times one derivation ran within a stage.
    #[cfg(test)]
    pub(crate) fn most_runs(&self) -> u16 {
        self.most_runs.map_or(0, |(runs, _)| runs)
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

    /// How many files and stages a worker run settled on the index after
    /// their work kept moving.
    #[cfg(test)]
    pub(crate) fn serial_fallbacks(&self) -> usize {
        self.serial_files + self.serial_stages
    }

    /// How many derivations of a cycle among a superstep's work ran on the
    /// index.
    #[cfg(test)]
    pub(crate) fn cycle_members(&self) -> usize {
        self.cycle_members
    }

    /// Each stage the run entered, with how much it queued.
    #[cfg(test)]
    pub(crate) fn stages(&self) -> &[(AnalysisStage, usize)] {
        &self.stages
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
            "[engine] evaluations={} rounds={} rebuilt sources={} loops cleared={} ({} derivations, {} waves, {} before release) files={files} {}",
            self.total_evaluations(),
            self.rounds,
            self.rebuilt_sources,
            self.loops_cleared,
            self.loop_members_cleared,
            self.loop_waves,
            self.loops_cleared_parked,
            kinds.join(" ")
        );
        eprintln!(
            "[engine] parallel attempts={} fallbacks={}",
            self.parallel_attempts, self.parallel_fallbacks
        );
        if let Some((runs, id)) = self.most_runs {
            eprintln!("[engine] most runs in a stage={runs} by {id:?}");
        }
        if self.supersteps > 0 {
            eprintln!(
                "[engine] supersteps={} tasks={} conflicting={} serial files={} serial stages={} cycle members={} conflicts={:?}",
                self.supersteps,
                self.superstep_tasks,
                self.conflicting_tasks,
                self.serial_files,
                self.serial_stages,
                self.cycle_members,
                self.task_conflicts
            );
            eprintln!(
                "[engine] cross-file groups={} queries on workers={}",
                self.cross_file_groups, self.queries_on_workers
            );
        }
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
/// The groups one file's task runs in a worker superstep, in order.
struct TaskPlan {
    file_id: crate::FileId,
    groups: Vec<PlannedGroup>,
}

impl TaskPlan {
    /// The files the task covers: its own and those of its groups' members,
    /// in order.
    fn files(&self) -> Vec<crate::FileId> {
        let mut files = vec![self.file_id];
        files.extend(
            self.groups
                .iter()
                .flat_map(|group| group.members.iter().map(|member| member.file_id)),
        );
        files.sort_unstable();
        files.dedup();
        files
    }
}

/// A worker superstep from its plan to its install: the file tasks at its
/// rank, the entries it leaves to the root list, and, once started, the
/// snapshot the tasks run on.
pub(crate) struct Superstep {
    rank: u8,
    plans: Vec<TaskPlan>,
    on_root: Vec<(crate::FileId, Vec<(DerivationId, Reason)>)>,
    root: Vec<(DerivationId, Reason)>,
    stop: StopSignal,
    tasks: Option<StartedTasks>,
}

/// The snapshot a superstep's tasks run on, and what they ran once they did.
struct StartedTasks {
    frozen: DbIndex,
    snapshot: Option<derivation::RecordingSnapshot>,
    quarantine: derivation::ReleaseQuarantine,
    cost: Option<Vec<f64>>,
    profiling: bool,
    outcomes: Option<Vec<(Option<TaskOutcome>, std::time::Duration)>>,
}

impl Superstep {
    /// Runs the tasks on worker threads, each on its own fork of the index as
    /// the superstep found it, until the stop signal is reached, then lets go
    /// of the reader graph they shared. `db` only places them on the threads.
    pub(crate) fn run_tasks(&mut self, db: &DbIndex, recipe: &dyn Recipe) {
        let Some(tasks) = &mut self.tasks else {
            return;
        };
        let _running = crate::profile::PhaseGuard::new("superstep: tasks");
        let files = self
            .plans
            .iter()
            .map(|plan| plan.file_id)
            .collect::<Vec<_>>();
        let by_file = self
            .plans
            .iter()
            .map(|plan| (plan.file_id, plan))
            .collect::<FxHashMap<_, _>>();
        let longest = std::sync::Mutex::new((std::time::Duration::ZERO, crate::FileId::VIRTUAL));
        let cost = tasks.cost.take();
        let snapshot = tasks.snapshot.take().expect("a superstep's tasks run once");
        let (frozen, profiling, stop) = (&tasks.frozen, tasks.profiling, &self.stop);
        let mut outcomes = super::parallel::map_tasks_collect(db, &files, cost, |_, file_id| {
            let started = std::time::Instant::now();
            let outcome = run_task_on_worker(
                frozen,
                &snapshot,
                by_file[&file_id],
                recipe,
                profiling,
                stop,
            );
            let elapsed = started.elapsed();
            crate::profile::add_phase("superstep: task cpu", elapsed, 0);
            let mut longest = lock(&longest);
            if elapsed > longest.0 {
                *longest = (elapsed, file_id);
            }
            (outcome, elapsed)
        });
        let (longest, longest_file) = *lock(&longest);
        crate::profile::add_phase("superstep: longest task", longest, 0);
        if crate::profile::phase_enabled() && longest > std::time::Duration::from_millis(20) {
            let plan = by_file[&longest_file];
            let mut kinds = BTreeMap::<String, usize>::new();
            for group in &plan.groups {
                for member in &group.members {
                    *kinds.entry(format!("{:?}", member.kind)).or_default() += 1;
                }
            }
            eprintln!(
                "[superstep] longest {}ms {longest_file:?} groups={} of {} tasks {kinds:?}",
                longest.as_millis(),
                plan.groups.len(),
                files.len(),
            );
        }
        drop(snapshot);
        tasks.frozen.release_reader_graph();
        for outcome in outcomes
            .iter_mut()
            .filter_map(|(outcome, _)| outcome.as_mut())
        {
            outcome.fork.release_reader_graph();
        }
        tasks.outcomes = Some(outcomes);
    }
}

/// A group a file's task runs in a worker superstep.
struct PlannedGroup {
    members: Vec<DerivationId>,
    /// Whether every member ran before.
    rerun: bool,
    /// The queued entries it took: the one it was decided for, then the
    /// later ones of its members.
    entries: Vec<(DerivationId, Reason)>,
}

/// When a worker superstep stops between two groups: once one ran, at the
/// slice's deadline or, in a slice that yields, while an edit waits. Both only
/// turn on while the superstep holds the index. A slice of evaluations stops
/// only between supersteps, where [`Engine::run`] counts them: counted across
/// workers, which groups ran would depend on timing.
struct StopSignal {
    deadline: Option<std::time::Instant>,
    interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    progressed: std::sync::atomic::AtomicBool,
}

impl StopSignal {
    fn new(
        slice: Slice,
        interrupt: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        Self {
            deadline: match slice {
                Slice::Until(deadline) => Some(deadline),
                Slice::Whole | Slice::Evaluations(_) => None,
            },
            interrupt: interrupt.filter(|_| slice.yields()).cloned(),
            progressed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Notes that a group is about to run.
    fn progress(&self) {
        self.progressed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the groups not yet started wait for the next superstep.
    fn reached(&self) -> bool {
        self.progressed.load(std::sync::atomic::Ordering::Relaxed)
            && (self
                .deadline
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                || self
                    .interrupt
                    .as_ref()
                    .is_some_and(|interrupt| interrupt.load(std::sync::atomic::Ordering::Acquire)))
    }
}

/// What dispatching a queued entry to a worker superstep decided.
enum Dispatched {
    /// Nothing runs: it waits for its stage, or what it read holds.
    Settled,
    /// A task runs the group; whether every member ran before.
    Task(Vec<DerivationId>, bool),
    /// It runs on the index after the barrier: a query, a group with a
    /// member in the workspace or in a file that settles on the index.
    Root,
}

/// What a task ran for one group: each evaluation it files, by its index
/// among the task's, with what it saw of each of its reads.
struct GroupRun {
    members: Vec<DerivationId>,
    records: Vec<(usize, Vec<derivation::Fingerprint>)>,
    /// For each member, the reads of its record from before the superstep
    /// that an earlier group of the task moved, with what it had seen.
    moved: Vec<Vec<(HashedKey, derivation::Fingerprint)>>,
    rerun: bool,
    elapsed: Option<std::time::Duration>,
}

/// What canonicalising a task's recording leaves to file: the records of the
/// evaluations to keep, in order, the groups that read a query the snapshot
/// had no answer for, each with the query outputs it read, and the keys its
/// kept evaluations moved or produced.
/// The loops of `reaches`: its strongly connected components of more than
/// one node.
fn loops_of(reaches: &FxHashMap<DerivationId, Vec<DerivationId>>) -> Vec<Vec<DerivationId>> {
    // Tarjan's algorithm, iterative: the graphs are a superstep's re-queues.
    #[derive(Clone, Copy)]
    struct Visit {
        index: usize,
        low: usize,
        on_stack: bool,
    }
    let mut visits = FxHashMap::<DerivationId, Visit>::default();
    let mut stack = Vec::new();
    let mut loops = Vec::new();
    let mut next_index = 0;
    let empty = Vec::new();
    for &start in reaches.keys() {
        if visits.contains_key(&start) {
            continue;
        }
        let mut path = vec![(start, 0usize)];
        visits.insert(
            start,
            Visit {
                index: next_index,
                low: next_index,
                on_stack: true,
            },
        );
        next_index += 1;
        stack.push(start);
        while let Some(&mut (node, ref mut next)) = path.last_mut() {
            let edges = reaches.get(&node).unwrap_or(&empty);
            if let Some(&target) = edges.get(*next) {
                *next += 1;
                match visits.get(&target).copied() {
                    None => {
                        visits.insert(
                            target,
                            Visit {
                                index: next_index,
                                low: next_index,
                                on_stack: true,
                            },
                        );
                        next_index += 1;
                        stack.push(target);
                        path.push((target, 0));
                    }
                    Some(visit) if visit.on_stack => {
                        let low = visits[&node].low.min(visit.index);
                        visits.get_mut(&node).expect("visited").low = low;
                    }
                    Some(_) => {}
                }
                continue;
            }
            path.pop();
            let visit = visits[&node];
            if let Some(&(parent, _)) = path.last() {
                let low = visits[&parent].low.min(visit.low);
                visits.get_mut(&parent).expect("visited").low = low;
            }
            if visit.low == visit.index {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    visits.get_mut(&member).expect("visited").on_stack = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                if component.len() > 1 {
                    loops.push(component);
                }
            }
        }
    }
    loops
}

struct FiledTask {
    records: Vec<derivation::KeptRecord>,
    on_root: Vec<(Vec<DerivationId>, Vec<HashedKey>)>,
    touched: Vec<HashedKey>,
}

/// Canonicalises a task's `recording` into `table`, its new keys interned at
/// `new_slots`, and keeps the record of each evaluation `groups` file.
fn file_task(
    table: &derivation::KeyTable,
    recording: derivation::TaskRecording,
    new_slots: &[derivation::KeyIdx],
    groups: &mut [GroupRun],
) -> FiledTask {
    let evaluations = recording
        .canonicalise_interned(new_slots, table)
        .expect("no slot is released while tasks run");
    let mut evaluations = evaluations.into_iter().map(Some).collect::<Vec<_>>();
    let mut filed = FiledTask {
        records: Vec::new(),
        on_root: Vec::new(),
        touched: Vec::new(),
    };
    let mut kept = Vec::new();
    for group in groups {
        if group.records.iter().any(|(at, _)| {
            evaluations[*at]
                .as_ref()
                .is_some_and(|(_, canonical)| *canonical)
        }) {
            let queries = group
                .records
                .iter()
                .filter_map(|(at, _)| evaluations[*at].as_ref())
                .flat_map(|(evaluation, _)| table.read_queries(evaluation))
                .collect();
            filed.on_root.push((group.members.clone(), queries));
            continue;
        }
        for (at, seen) in &mut group.records {
            let (evaluation, _) = evaluations[*at].take().expect("a record is filed once");
            filed.touched.extend(evaluation.moved().cloned());
            kept.push((evaluation, std::mem::take(seen)));
        }
    }
    let kept = kept
        .into_par_iter()
        .map(|(evaluation, seen)| table.keep_record(evaluation, seen))
        .collect::<Vec<_>>();
    filed.records.reserve(kept.len());
    for (record, written) in kept {
        filed.records.push(record);
        filed.touched.extend(written);
    }
    filed
}

/// What [`Engine::check_dispatch`] found of a queued entry.
enum Checked {
    /// It holds, as [`staleness`] found.
    Settled(Staleness),
    Stale(Vec<DerivationId>),
}

/// Whether a queued derivation has to run.
enum Staleness {
    Stale,
    Holds,
    /// It holds once the reads the edits only moved are recorded as they
    /// stand.
    Rebased(Vec<(derivation::KeyIdx, derivation::Fingerprint)>),
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `key`'s current fingerprint, taken once per value.
fn current_fingerprint(db: &DbIndex, key: &HashedKey) -> derivation::Fingerprint {
    db.reader_graph()
        .current_fingerprint(key, |key| db.fact_fingerprint(key))
}

/// Whether reads of `key` that saw a fingerprint still hold: the value is the
/// same, or the same once `edits`' moves are undone.
fn holds<'a>(
    db: &'a DbIndex,
    edits: &'a derivation::EditMaps,
    before_edits: &'a std::sync::Mutex<FxHashMap<HashedKey, derivation::Fingerprint>>,
    key: &'a HashedKey,
) -> impl Fn(derivation::Fingerprint) -> bool + 'a {
    let current = std::cell::OnceCell::new();
    let before = std::cell::OnceCell::new();
    move |seen| {
        seen == *current.get_or_init(|| current_fingerprint(db, key))
            || (!edits.is_empty()
                && seen
                    == *before
                        .get_or_init(|| fingerprint_before_edits(db, edits, before_edits, key)))
    }
}

/// `key`'s fingerprint with `edits` undone, taken once per value into `memo`.
fn fingerprint_before_edits(
    db: &DbIndex,
    edits: &derivation::EditMaps,
    memo: &std::sync::Mutex<FxHashMap<HashedKey, derivation::Fingerprint>>,
    key: &HashedKey,
) -> derivation::Fingerprint {
    if let Some(fingerprint) = lock(memo).get(key) {
        return *fingerprint;
    }
    let fingerprint = derivation::fingerprint_before_edits(edits, || db.fact_fingerprint(key));
    lock(memo).insert(key.clone(), fingerprint);
    fingerprint
}

/// Whether `id`, queued for `reason`, has to run, without changing anything:
/// a reader runs only if a fact it read moved. A stage rule it saw holds:
/// only a stage barrier moves one, and the barrier queues the rule's readers
/// itself.
///
/// A read that held only with `edits` mapped back saw positions an edit
/// moved. An output made from them is still right if the edit's remap
/// rewrote it, or if it names none of the positions the edit moved: with the
/// edit mapped back it fingerprints as it does now. A fold of every writer's
/// share is judged by this writer's share, which it also wrote: the fold
/// holds the edited files' shares too. Only a derivation that holds is
/// rebased onto the values as they stand: one that runs is recorded again,
/// and one checked again before it runs must still be found to need it.
fn staleness(
    db: &DbIndex,
    edits: &derivation::EditMaps,
    before_edits: &std::sync::Mutex<FxHashMap<HashedKey, derivation::Fingerprint>>,
    id: DerivationId,
    reason: Reason,
) -> Staleness {
    let before = |key: &HashedKey| fingerprint_before_edits(db, edits, before_edits, key);
    if reason == Reason::Changed
        || db.reader_graph().is_stale(
            id,
            |key| db.fact_fingerprint(key),
            |key, seen| key.is_stage_rule() || (!edits.is_empty() && seen == before(key)),
        )
    {
        return Staleness::Stale;
    }
    if edits.is_empty() {
        return Staleness::Holds;
    }
    let fingerprints = db
        .reader_graph()
        .reads_of(id)
        .map(|key| current_fingerprint(db, key))
        .collect();
    let moved = db.reader_graph().moved_fingerprints(id, fingerprints);
    if moved.is_empty() {
        return Staleness::Holds;
    }
    let outputs = db.reader_graph().outputs_of(id).collect::<Vec<_>>();
    let holds = outputs.iter().all(|key| {
        key.follows_edits()
            || key
                .writer_share(id)
                .is_some_and(|share| outputs.iter().any(|output| *output.key() == share))
            || current_fingerprint(db, key) == before(key)
    });
    if holds {
        Staleness::Rebased(moved)
    } else {
        Staleness::Stale
    }
}

/// What a task left for the barrier.
struct TaskOutcome {
    fork: DbIndex,
    recording: derivation::TaskRecording,
    groups: Vec<GroupRun>,
    touched: FxHashSet<HashedKey>,
    reruns: Vec<DerivationId>,
}

/// Runs `plan`'s groups on a fork of `frozen`, recording against `snapshot`,
/// until `stop` is reached: the groups that ran are the first of the plan's.
/// `None` for a task that stopped before its first group.
fn run_task_on_worker(
    frozen: &DbIndex,
    snapshot: &derivation::RecordingSnapshot,
    plan: &TaskPlan,
    recipe: &dyn Recipe,
    profiling: bool,
    stop: &StopSignal,
) -> Option<TaskOutcome> {
    if stop.reached() {
        return None;
    }
    let mut db = frozen.task_fork();
    let recorder = derivation::TaskRecorder::start(snapshot);
    let mut touched = FxHashSet::default();
    let groups = super::parallel::in_task(&plan.files(), || {
        plan.groups
            .iter()
            .enumerate()
            .take_while(|(at, _)| *at == 0 || !stop.reached())
            .map(|(_, group)| {
                stop.progress();
                let started = profiling.then(std::time::Instant::now);
                let moved = group
                    .members
                    .iter()
                    .map(|member| moved_in_task(&db, frozen, &touched, *member))
                    .collect();
                let (records, group_touched) =
                    evaluate_on_worker(&mut db, &recorder, &group.members, recipe);
                touched.extend(group_touched);
                GroupRun {
                    members: group.members.clone(),
                    records: records
                        .into_iter()
                        .map(|(at, _, seen)| (at, seen))
                        .collect(),
                    moved,
                    rerun: group.rerun,
                    elapsed: started.map(|started| started.elapsed()),
                }
            })
            .collect()
    });
    let reruns = derivation::take_reruns();
    Some(TaskOutcome {
        fork: db,
        recording: recorder.finish(),
        groups,
        touched,
        reruns,
    })
}

/// [`Engine::evaluate_group`] in a task: resets the group's outputs on the
/// task's fork and runs it with the task's recorder, then fingerprints what
/// each evaluation read as the fork holds it. Returns the evaluations to file
/// with what each saw of its reads, and adds every key the group touched to
/// `touched`.
#[allow(clippy::type_complexity)]
fn evaluate_on_worker(
    db: &mut DbIndex,
    recorder: &derivation::TaskRecorder,
    group: &[DerivationId],
    recipe: &dyn Recipe,
) -> (
    Vec<(usize, DerivationId, Vec<derivation::Fingerprint>)>,
    FxHashSet<HashedKey>,
) {
    let mut touched = FxHashSet::default();
    let mut shared = FxHashSet::default();
    let mut discarded = FxHashSet::default();
    loop {
        let resets = group_resets(db, group, &shared, &discarded);
        let deferred = resets.deferred();
        let GroupResets { front, own, joint } = resets;
        let mut reset = FxHashMap::default();
        let mut outputs = discarded.clone();
        for member in group {
            outputs.extend(db.reader_graph().outputs_of(*member).cloned());
        }
        let recomputing = outputs
            .iter()
            .map(|key| (key.clone(), task_fingerprint(db, recorder, key)))
            .collect::<FxHashMap<_, _>>();
        let written = derivation::capture_writes(|| {
            for key in outputs
                .iter()
                .filter(|key| key.is_primary() && !deferred.contains(*key))
            {
                db.reset_fact(key);
            }
        });
        if group.len() > 1 {
            reset.extend(front.iter().chain(&written).map(|key| (key.clone(), 0)));
        }
        touched.extend(outputs);
        touched.extend(written);
        derivation::note_recomputing(recomputing.keys().cloned());
        derivation::note_written(
            touched
                .iter()
                .filter(|key| !recomputing.contains_key(*key))
                .cloned(),
        );
        let mut cleared = FxHashSet::default();
        let (evaluations, found) = run_group(
            db,
            group,
            own,
            joint,
            &mut reset,
            &mut cleared,
            |db, member| {
                recorder.begin(member);
                recipe.evaluate(db, member);
                let at = recorder.end();
                let outputs = recorder
                    .with_ended(at, |evaluation, _| evaluation.outputs().cloned().collect());
                (at, outputs)
            },
        );
        derivation::note_recomputed(|key| recomputing.get(key) == Some(&db.fact_fingerprint(key)));
        touched.extend(cleared);
        for (at, _) in &evaluations {
            recorder.with_ended(*at, |evaluation, _| {
                touched.extend(evaluation.outputs().chain(evaluation.moved()).cloned());
            });
        }
        if found.is_empty() {
            let records = evaluations
                .into_iter()
                .enumerate()
                .map(|(position, (at, ended))| {
                    let id = recorder.with_ended(at, |evaluation, _| evaluation.id);
                    let seen = recorder
                        .read_states(at, ended, |key| reset_before(&reset, key, position))
                        .into_iter()
                        .map(|state| match state {
                            derivation::ReadState::Unseen => derivation::UNSEEN,
                            derivation::ReadState::Query(seen) => seen,
                            derivation::ReadState::Snapshot(slot) => db
                                .reader_graph()
                                .slot_fingerprint(slot, |key| db.fact_fingerprint(key)),
                            derivation::ReadState::Fact(key) => db.fact_fingerprint(&key),
                        })
                        .collect();
                    (at, id, seen)
                })
                .collect();
            return (records, touched);
        }
        for (at, _) in &evaluations {
            recorder.with_ended(*at, |evaluation, _| {
                discarded.extend(evaluation.outputs().chain(evaluation.moved()).cloned());
            });
        }
        touched.extend(discarded.iter().cloned());
        shared.extend(found);
    }
}

/// The reads of `member`'s record from before the superstep that the groups
/// its task ran before it moved, with what `member` had seen of each: what the
/// serial loop notes of a reader when it evaluates those groups first.
fn moved_in_task(
    db: &DbIndex,
    frozen: &DbIndex,
    written: &FxHashSet<HashedKey>,
    member: DerivationId,
) -> Vec<(HashedKey, derivation::Fingerprint)> {
    if !member.kind.is_statement() || written.is_empty() {
        return Vec::new();
    }
    let graph = frozen.reader_graph();
    graph
        .reads_of(member)
        .zip(graph.recorded_slots(member))
        .filter(|(key, _)| {
            !key.is_stage_rule()
                && (written.contains(*key)
                    || key
                        .base()
                        .is_some_and(|base| written.contains(&HashedKey::new(base))))
        })
        .filter(|(key, (_, seen))| db.fact_fingerprint(key) != *seen)
        .map(|(key, (_, seen))| (key.clone(), seen))
        .collect()
}

/// `key`'s fingerprint as a task's fork holds it: through the reader graph's
/// cache when the task wrote neither it nor a key it is read out of, which
/// leaves it as the superstep found it.
fn task_fingerprint(
    db: &DbIndex,
    recorder: &derivation::TaskRecorder,
    key: &HashedKey,
) -> derivation::Fingerprint {
    if recorder.written_here(key) {
        db.fact_fingerprint(key)
    } else {
        db.reader_graph()
            .current_fingerprint(key, |key| db.fact_fingerprint(key))
    }
}

/// When a group run resets each primary key its members wrote.
struct GroupResets {
    /// Reset before the group runs: the first member's keys, and the keys a
    /// discarded run wrote or showed two members writing.
    front: FxHashSet<HashedKey>,
    /// Each member's own keys, which no other derivation writes, reset as it
    /// runs.
    own: Vec<Vec<HashedKey>>,
    /// The keys each member is the first of several writers of, reset as it
    /// runs.
    joint: Vec<Vec<HashedKey>>,
}

impl GroupResets {
    /// The keys reset only as a member runs.
    fn deferred(&self) -> FxHashSet<HashedKey> {
        self.own
            .iter()
            .chain(&self.joint)
            .flatten()
            .cloned()
            .collect()
    }
}

fn group_resets(
    db: &DbIndex,
    group: &[DerivationId],
    shared: &FxHashSet<HashedKey>,
    discarded: &FxHashSet<HashedKey>,
) -> GroupResets {
    let graph = db.reader_graph();
    let mut resets = GroupResets {
        front: FxHashSet::default(),
        own: vec![Vec::new(); group.len()],
        joint: vec![Vec::new(); group.len()],
    };
    if group.len() == 1 {
        return resets;
    }
    let mut placed = FxHashSet::default();
    for (position, member) in group.iter().enumerate() {
        for key in graph.outputs_of(*member).filter(|key| key.is_primary()) {
            if !placed.insert(key) {
                continue;
            }
            if position == 0 || shared.contains(key) || discarded.contains(key) {
                resets.front.insert(key.clone());
            } else if graph
                .producers(key)
                .all(|producer| producer == *member || producer.kind == DerivationKind::Source)
            {
                resets.own[position].push(key.clone());
            } else {
                resets.joint[position].push(key.clone());
            }
        }
    }
    resets
        .front
        .extend(discarded.iter().filter(|key| key.is_primary()).cloned());
    resets
}

/// Whether `key` was reset as the group ran, at or before the member at
/// `position`, so the member read it as the group leaves it.
fn reset_before(reset: &FxHashMap<HashedKey, usize>, key: &HashedKey, position: usize) -> bool {
    reset.get(key).is_some_and(|from| *from <= position)
}

/// Runs `group`'s members in order, each resetting first the keys `resets`
/// defers to it, and notes in `reset` the position at which each joint key
/// and what its reset wrote was reset. Stops at the first sign that two
/// members write a primary key not reset before either ran: a key reset as a
/// member runs that an earlier one wrote, or a key two members wrote. Returns
/// the evaluations, each but the last with the stamp it ended at, and the
/// keys that stopped it (none if it ran through).
fn run_group<E>(
    db: &mut DbIndex,
    group: &[DerivationId],
    own: Vec<Vec<HashedKey>>,
    joint: Vec<Vec<HashedKey>>,
    reset: &mut FxHashMap<HashedKey, usize>,
    cleared: &mut FxHashSet<HashedKey>,
    mut evaluate: impl FnMut(&mut DbIndex, DerivationId) -> (E, Vec<HashedKey>),
) -> (Vec<(E, Option<u64>)>, FxHashSet<HashedKey>) {
    let mut evaluations = Vec::with_capacity(group.len());
    // Each primary key written before it was reset, with the position of the
    // member that wrote it first.
    let mut writers = FxHashMap::<HashedKey, usize>::default();
    let steps = group.iter().zip(own.into_iter().zip(joint));
    for (position, (member, (own, joint))) in steps.enumerate() {
        let taken = own
            .iter()
            .chain(&joint)
            .filter(|key| writers.contains_key(*key))
            .cloned()
            .collect::<FxHashSet<_>>();
        if !taken.is_empty() {
            return (evaluations, taken);
        }
        let mut moved = derivation::capture_writes(|| {
            for key in &own {
                db.reset_fact(key);
            }
        });
        let joint_moved = derivation::capture_writes(|| {
            for key in &joint {
                db.reset_fact(key);
            }
        });
        for key in joint.into_iter().chain(joint_moved.iter().cloned()) {
            reset.entry(key).or_insert(position);
        }
        moved.extend(joint_moved);
        if !moved.is_empty() {
            derivation::note_written(moved.iter().cloned());
            cleared.extend(moved);
        }
        let (evaluation, outputs) = evaluate(db, *member);
        let mut found = FxHashSet::default();
        for key in outputs
            .iter()
            .filter(|key| key.is_primary() && !reset.contains_key(*key))
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
    let mut released = Vec::new();
    let moved = file_queries_into(db, &mut released);
    db.reader_graph_mut().release_keys(released);
    moved
}

/// [`file_queries`], the slots the filed records no longer keep added to
/// `released` for the caller to free once the evaluations it holds, which may
/// read them, are filed too.
fn file_queries_into(db: &mut DbIndex, released: &mut Vec<derivation::KeyIdx>) -> Vec<HashedKey> {
    if !db.queries().has_pending() {
        return Vec::new();
    }
    let filings = db.queries().take_pending();
    let graph = db.reader_graph_mut();
    let mut moved = Vec::new();
    let mut records = Vec::with_capacity(filings.len());
    for filing in filings {
        if let Some(key) = filing.moved {
            graph.forget_fingerprint(&key);
            moved.push(key);
        }
        records.push((filing.id, derivation::ViewedReads::from_keys(filing.reads)));
    }
    graph.file_viewed_into(records, released);
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

/// The batch member each key a batch reset moved belongs to: the producer of
/// the output whose reset wrote it, or of the output itself. A key two
/// members' outputs or resets share, or that a reset outside the members
/// wrote, belongs to none.
#[derive(Default)]
struct ResetOwners(FxHashMap<HashedKey, Option<DerivationId>>);

impl ResetOwners {
    fn note(&mut self, key: HashedKey, producer: Option<DerivationId>) {
        let owner = self.0.entry(key).or_insert(producer);
        if *owner != producer {
            *owner = None;
        }
    }

    fn owner(&self, key: &HashedKey) -> Option<DerivationId> {
        self.0.get(key).copied().flatten()
    }
}

/// Whether a member of `group` now shares a primary output with a derivation
/// outside it, which it ran without.
fn widened(db: &DbIndex, group: &[DerivationId]) -> bool {
    group.iter().any(|member| {
        db.reader_graph().outputs_of(*member).any(|key| {
            key.is_primary()
                && db.reader_graph().producers(key).any(|producer| {
                    producer.kind != DerivationKind::Source && !group.contains(&producer)
                })
        })
    })
}

/// `ids` and the derivations that share a key one of them writes: a loop
/// through a co-producer of one holds what that one supplied with it.
fn with_co_producers(
    db: &DbIndex,
    ids: impl IntoIterator<Item = DerivationId>,
) -> BTreeSet<DerivationId> {
    ids.into_iter()
        .flat_map(|id| co_producers(db, id))
        .collect()
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

/// Forgets the lent key table if evaluating a group unwinds.
fn scopeguard_lent_keys() -> impl Drop {
    OnDrop(derivation::forget_lent_keys)
}

/// How often one derivation may run on the index within one stage before
/// [`Engine::admit_run`] takes it that the facts it reads do not settle.
const MAX_INDEX_RUNS: u16 = 16;

/// The queued entries one worker superstep takes at most, short of a file's:
/// its tasks' forks and recordings live until it installs them, so this
/// bounds what it holds.
const SUPERSTEP_ENTRIES: usize = 32_768;

/// How often a worker superstep runs one derivation within a stage before the
/// stage settles on the serial loop.
const SUPERSTEP_RUN_LIMIT: u16 = 16;

/// The fewest keys one worker job checks the readers of: an evaluation's
/// handful runs on its own thread rather than waking the pool.
const KEYS_PER_JOB: usize = 64;

/// How many supersteps a group waits for the queries it read before it runs
/// on the index instead.
const QUERY_DEFERRALS: u8 = 2;

/// How often one derivation ran within a stage: in worker superstep tasks,
/// and on the index.
#[derive(Debug, Clone, Copy, Default)]
struct StageRuns {
    in_tasks: u16,
    on_index: u16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SuperstepMode {
    /// The serial engine, one derivation at a time.
    Off,
    /// Each file's task on a worker thread.
    #[default]
    Workers,
}

/// `GLUALS_SUPERSTEP`: `off` runs the serial engine; unset or any other value
/// runs each rank whose work spans files a superstep at a time with the tasks
/// on worker threads.
fn superstep_mode() -> SuperstepMode {
    static MODE: std::sync::OnceLock<SuperstepMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("GLUALS_SUPERSTEP").as_deref() {
        Ok("off") => SuperstepMode::Off,
        _ => SuperstepMode::Workers,
    })
}

fn check_settled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_SETTLED").is_some())
}

fn check_convergence() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_CONVERGENCE").is_some())
}

/// `GLUALS_CHECK_FINGERPRINTS`: recompute every cached fingerprint and panic on
/// a key that moved without being written.
fn check_fingerprints() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_FINGERPRINTS").is_some())
}

/// Under tests, debug assertions or `GLUALS_CHECK_FINGERPRINTS`: whether a
/// batch checks that no member computed from a fact another member moved.
fn check_batch_reads() -> bool {
    cfg!(any(test, debug_assertions)) || check_fingerprints()
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
    fn call_targets_match_across_analysis_threads_cold_and_after_an_edit() {
        let edited = INVENTORY.replace(
            "function Inventory.Format(value) return tostring(value) end",
            "function Inventory.Format(value) return value * 2 end
            function Inventory.Clear(inv) inv.items = {} end",
        );
        let shops = inventory_shops();
        let files = inventory_files(&shops);
        let edits = [("lua/autorun/sh_inventory.lua", edited.as_str())];
        let _serial = serial_engine();
        let analyse = |threads: usize| {
            parallel::set_analysis_threads(threads);
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files.clone());
            let cold = recorded_digest(&mut ws);
            for (path, text) in edits {
                ws.def_file(path, text);
            }
            let warm = recorded_digest(&mut ws);
            (cold, warm)
        };
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let serial = analyse(1);
        let threaded = analyse(4);
        parallel::set_analysis_threads(0);
        drop(threads);
        assert_eq!(serial, threaded);
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
                id.kind == DerivationKind::CallTarget
                    && graph.reads_of(*id).any(|key| *key.key() == read)
            })
            .map(|id| id.file_id)
            .collect::<FxHashSet<_>>()
            .len();
        assert_eq!(readers, shops.len());
    }

    /// Readers of `REGISTRY[...]` on both sides of the file whose dynamic
    /// fields make `REGISTRY` a wildcard owner: each reader's dynamic fields
    /// infer the entry through that wildcard state, which the registry file's
    /// dynamic fields write in the same batch.
    #[test]
    fn dynamic_fields_reading_a_peers_wildcard_owner_match_across_threads_and_warm() {
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
        let _serial = serial_engine();
        let analyse = |threads: usize| {
            parallel::set_analysis_threads(threads);
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files.clone());
            (ws.settled_digest(), recorded_digest(&mut ws))
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
        assert_eq!(serial.1, recorded.1);
        assert_eq!(recorded.0, warm);
    }

    /// Local fallbacks re-run together after their classes change: each file's
    /// scan reads the declared types its own reset restored from under its
    /// fallbacks, which no other member's reset touches.
    #[test]
    fn local_fallbacks_reading_their_own_reset_match_across_threads_and_cold() {
        let _serial = serial_engine();
        let classes = r#"
            ---@class Entity
            ---@class Radio: Entity
            ---@field Tune fun(self: Radio)
        "#;
        let edited = r#"
            ---@class Entity
            ---@class Radio: Entity
            ---@field Tune fun(self: Radio)
            ---@field Mute fun(self: Radio)
        "#;
        let tuners = (0..10)
            .map(|index| {
                (
                    format!("lua/autorun/client/cl_tuner_{index}.lua"),
                    format!(
                        "---@type Entity
local tuned{index}
tuned{index}:Tune()
print(tuned{index})"
                    ),
                )
            })
            .collect::<Vec<_>>();
        let classes_path = "lua/autorun/sh_radio_classes.lua";
        let mut files = vec![(classes_path, classes)];
        files.extend(
            tuners
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_str())),
        );
        let analyse = |threads: usize| {
            parallel::set_analysis_threads(threads);
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files.clone());
            let uri = ws.virtual_url_generator.new_uri(classes_path);
            ws.def_file(classes_path, edited);
            ws.analysis.remove_file_by_uri(&uri);
            ws.def_file(classes_path, edited);
            (recorded_digest(&mut ws), ws.semantic_digest())
        };
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let serial = analyse(1);
        let recorded = analyse(4);
        let mut final_files = files.clone();
        final_files[0].1 = edited;
        let mut cold = VirtualWorkspace::new();
        cold.def_files(final_files);
        let cold = cold.semantic_digest();
        parallel::set_analysis_threads(0);
        drop(threads);
        let differ = |left: &[String], right: &[String]| {
            let left_set = left.iter().collect::<std::collections::BTreeSet<_>>();
            let right_set = right.iter().collect::<std::collections::BTreeSet<_>>();
            let mut lines = left_set
                .difference(&right_set)
                .map(|line| format!("- {line}"))
                .chain(
                    right_set
                        .difference(&left_set)
                        .map(|line| format!("+ {line}")),
                )
                .collect::<Vec<_>>();
            lines.truncate(20);
            lines.join("\n")
        };
        assert!(
            serial.0 == recorded.0,
            "serial vs recorded records:\n{}",
            differ(&serial.0, &recorded.0)
        );
        assert!(
            recorded.1 == cold,
            "warm vs cold settled:\n{}",
            differ(&recorded.1, &cold)
        );
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

    /// Two statements that never settle: the reader copies `WRITTEN` plus one
    /// into `COPIED`, and the writer copies `COPIED` back into `WRITTEN`.
    struct Feedback;

    fn integer(db: &DbIndex, position: u32) -> i64 {
        match db
            .get_type_index()
            .get_type_cache(&owner(position))
            .map(|cache| cache.as_type().clone())
        {
            Some(LuaType::IntegerConst(value)) => value,
            _ => 0,
        }
    }

    impl Recipe for Feedback {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            if id == statement(READER) {
                let next = integer(db, WRITTEN) + 1;
                bind(db, COPIED, LuaType::IntegerConst(next));
            } else {
                let copied = integer(db, COPIED);
                bind(db, WRITTEN, LuaType::IntegerConst(copied));
            }
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("feedback does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("feedback does not split")
        }
    }

    #[test]
    fn a_loop_that_never_settles_stops_where_its_runs_run_out() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.unsettled_panics = false;
        for start in [READER, WRITER] {
            engine.changed(statement(start));
        }
        engine.open(db);

        let settled = engine.run(db, &Feedback, Slice::Whole);

        let runs = i64::from(MAX_INDEX_RUNS);
        assert_eq!(
            (settled, integer(db, COPIED), integer(db, WRITTEN)),
            (true, runs, runs)
        );
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

    /// Three statements that all write `SHARED`, so they run as one group.
    /// The first copies `WRITTEN`, which the other two both write.
    struct Trio {
        written: Mutex<LuaType>,
    }

    const THIRD: u32 = 20;

    impl Recipe for Trio {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            if id == statement(READER) {
                let seen = db
                    .get_type_index()
                    .get_type_cache(&owner(WRITTEN))
                    .map_or(LuaType::Unknown, |cache| cache.as_type().clone());
                bind(db, COPIED, seen);
            } else {
                let written = self.written.lock().unwrap().clone();
                bind(db, WRITTEN, written);
            }
            bind(db, SHARED, LuaType::Nil);
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("a trio does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("a trio does not split")
        }
    }

    #[test]
    fn a_group_member_reads_a_fact_two_later_members_write_as_they_leave_it() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let trio = Trio {
            written: Mutex::new(LuaType::Integer),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);

        let settled = settle(db, &mut engine, &trio, &[READER, WRITER, THIRD]);

        assert_eq!((settled, copied(db)), (true, Some(LuaType::Integer)));
    }

    #[test]
    fn a_group_member_reads_again_a_fact_two_later_members_move() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let trio = Trio {
            written: Mutex::new(LuaType::Integer),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        settle(db, &mut engine, &trio, &[READER, WRITER, THIRD]);

        *trio.written.lock().unwrap() = LuaType::String;
        let settled = settle(db, &mut engine, &trio, &[WRITER, THIRD]);

        assert_eq!((settled, copied(db)), (true, Some(LuaType::String)));
    }

    #[test]
    fn changed_work_outside_the_focus_waits_for_it() {
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.prioritise(&[FileId::new(FILE)]);

        engine.wait(in_file(FILE + 2), Reason::Read);
        engine.changed(in_file(FILE));
        engine.changed(in_file(FILE + 1));
        engine.changed(in_file(FILE + 2));

        let queued = engine.worklist.files().collect::<Vec<_>>();
        assert_eq!(queued, [FileId::new(FILE)]);
        for parked in [FILE + 1, FILE + 2] {
            let reason = engine.waiting.get(&in_file(parked));
            assert_eq!(reason, Some(&Reason::Changed));
        }
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
        let _serial = serial_engine();
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.changed(in_file(FILE));
        engine.changed(in_file(FILE + 1));
        engine.open(db);
        engine.run(db, &PeerRead, Slice::Evaluations(100));
    }

    /// The reader copies the size of a member slot two writers, at 10 and
    /// 20, contribute to.
    struct SlotContributors {
        reader: u32,
        second: Mutex<bool>,
    }

    /// A reader between the two contributors: a round reaches it after the
    /// first and before the second.
    const BETWEEN_CONTRIBUTORS: u32 = 15;

    fn joined_owner() -> crate::LuaMemberOwner {
        crate::LuaMemberOwner::Type(crate::LuaTypeDeclId::global("RoundJoined"))
    }

    impl Recipe for SlotContributors {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let joined = joined_owner();
            if id == statement(self.reader) {
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
        let _serial = serial_engine();
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let recipe = SlotContributors {
            reader: BETWEEN_CONTRIBUTORS,
            second: Mutex::new(true),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.batch_slot_notifications = batch;
        bind(db, 110, LuaType::IntegerConst(1));
        bind(db, 120, LuaType::IntegerConst(1));
        assert!(settle(
            db,
            &mut engine,
            &recipe,
            &[BETWEEN_CONTRIBUTORS, WRITER, 20]
        ));
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
            reader: READER,
            second: Mutex::new(true),
        };
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.batch_slot_notifications = true;
        bind(db, 110, LuaType::IntegerConst(1));
        engine.changed(statement(READER));
        engine.open(db);
        assert!(!engine.run(db, &recipe, Slice::Evaluations(1)));
        let _recording = derivation::begin(statement(WRITER));
        recipe.evaluate(db, statement(WRITER));
        let evaluation = derivation::end();
        let mut touched = evaluation.outputs().cloned().collect::<FxHashSet<_>>();
        engine.record(
            db,
            vec![(evaluation, None)],
            &mut touched,
            &FxHashMap::default(),
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
    fn a_focus_file_whose_work_settled_leaves_the_focus_ahead_of_the_rest() {
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        let settled = FileId::new(FILE + 1);
        engine.prioritise(&[settled, FileId::new(FILE)]);
        engine.changed(statement(1));

        assert!(engine.in_focus(FileId::new(FILE)));
        assert!(!engine.in_focus(settled));
        assert!(engine.is_settling(settled));
    }

    #[test]
    fn member_round_notifications_are_flushed_before_a_slice_returns() {
        let _serial = serial_engine();
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let recipe = SlotContributors {
            reader: READER,
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

    /// A query the evaluations computed is filed with them: a key only its
    /// previous record and an unfiled evaluation read stays live for the
    /// evaluation's record.
    #[test]
    fn a_query_filed_with_an_evaluation_keeps_the_keys_the_evaluation_read() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();
        let read = HashedKey::new(FactKey::TypeCache(owner(WRITTEN)));
        let query = DerivationId::new(
            DerivationKind::ReceiverAuthority,
            FileId::new(FILE),
            LuaSyntaxId::new(
                LuaSyntaxKind::ClosureExpr.into(),
                TextRange::at(TextSize::new(WRITTEN), TextSize::new(1)),
            ),
        );
        db.reader_graph_mut().file_viewed(vec![(
            query,
            derivation::ViewedReads::from_keys(vec![(read.clone(), 1)]),
        )]);
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let lent = scopeguard_lent_keys();
        let recording = derivation::begin(statement(READER));
        let _ = db.get_type_index().get_type_cache(&owner(WRITTEN));
        drop(recording);
        let evaluation = derivation::end();
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        std::mem::forget(lent);
        db.queries()
            .receiver_authority
            .queue_filing(query, Vec::new());

        let engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.record(
            db,
            vec![(evaluation, None)],
            &mut FxHashSet::default(),
            &FxHashMap::default(),
            String::new,
        );

        assert!(
            db.reader_graph()
                .reads_of(statement(READER))
                .any(|key| *key == read)
        );
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

    const RELAY: u32 = 1000;
    const RELAY_PAIRED: u32 = 1010;
    const RELAY_SHARED: u32 = 2000;

    fn relay_owner(file_id: FileId, position: u32) -> LuaTypeOwner {
        LuaTypeOwner::Decl(LuaDeclId::new(file_id, TextSize::new(position)))
    }

    fn relay_statement(file_id: FileId, start: u32) -> DerivationId {
        DerivationId::new(
            DerivationKind::LocalStat,
            file_id,
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(start), TextSize::new(1)),
            ),
        )
    }

    fn bind_relay(db: &mut DbIndex, owner: LuaTypeOwner, typ: LuaType) {
        db.get_type_index_mut()
            .force_bind_type(owner, LuaTypeCache::InferType(typ));
    }

    /// Statements of one rank, one per file but two in the third: the first
    /// file's binds `value`, every other copies it, and each file's bind one
    /// fact they share.
    struct Relay {
        source: FileId,
        value: LuaType,
    }

    impl Recipe for Relay {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let source = relay_owner(self.source, RELAY);
            if id.file_id == self.source {
                bind_relay(db, source, self.value.clone());
                return;
            }
            let seen = db
                .get_type_index()
                .get_type_cache(&source)
                .map_or(LuaType::Unknown, |cache| cache.as_type().clone());
            let start = u32::from(id.syntax_id().get_range().start());
            bind_relay(db, relay_owner(id.file_id, start), seen);
            bind_relay(db, relay_owner(id.file_id, RELAY_SHARED), LuaType::Nil);
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("a relay does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("a relay does not split")
        }
    }

    /// A workspace of an addon's source file and five readers, with the
    /// [`Relay`] statements in them; the source's file comes first.
    fn relay_workspace() -> (VirtualWorkspace, Relay, Vec<FileId>, Vec<DerivationId>) {
        let mut ws = VirtualWorkspace::new();
        let mut files = vec![ws.def_file("lua/autorun/sh_relay_source.lua", "local relay = 1")];
        files.extend((1..=5).map(|index| {
            ws.def_file(
                &format!("lua/autorun/server/sv_relay_{index}.lua"),
                "local relay = 1",
            )
        }));
        assert!(files.is_sorted(), "the source's file comes first");
        let mut statements = files
            .iter()
            .map(|file_id| relay_statement(*file_id, RELAY))
            .collect::<Vec<_>>();
        statements.push(relay_statement(files[2], RELAY_PAIRED));
        let recipe = Relay {
            source: files[0],
            value: LuaType::IntegerConst(7),
        };
        (ws, recipe, files, statements)
    }

    /// [`recorded_digest`] of a workspace the relays settled in.
    fn relay_digest(ws: &mut VirtualWorkspace) -> Vec<String> {
        ws.get_db_mut().flush_inference_derived_state();
        recorded_digest(ws)
    }

    fn queued_reasons(engine: &Engine) -> Vec<(DerivationId, Reason)> {
        let mut queued = engine
            .worklist
            .reasons
            .iter()
            .map(|(id, reason)| (*id, *reason))
            .collect::<Vec<_>>();
        queued.sort();
        queued
    }

    /// Every entry the carried groups took.
    fn carried_reasons(engine: &Engine) -> Vec<(DerivationId, Reason)> {
        let mut carried = engine
            .carried
            .iter()
            .flat_map(|plan| &plan.groups)
            .flat_map(|group| group.entries.iter().copied())
            .collect::<Vec<_>>();
        carried.sort();
        carried
    }

    /// What a cold build of the relays did in its first worker superstep, run
    /// in `slice`, and once every later one, each in `slice` too, and a whole
    /// run settled it.
    struct RelayRun {
        evaluated_first: usize,
        queued_after_first: Vec<(DerivationId, Reason)>,
        carried_after_first: Vec<(DerivationId, Reason)>,
        supersteps: usize,
        digest: Vec<String>,
    }

    fn relay_run(slice: Slice, interrupted: bool) -> RelayRun {
        let (mut ws, recipe, _, statements) = relay_workspace();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.interrupt_with(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            interrupted,
        )));
        for id in &statements {
            engine.changed(*id);
        }
        engine.open(db);
        engine.superstep_on_workers(db, &recipe, slice);
        let evaluated_first = engine.stats.evaluations(DerivationKind::LocalStat);
        let queued_after_first = queued_reasons(&engine);
        let carried_after_first = carried_reasons(&engine);
        while !engine.worklist.is_empty() || !engine.carried.is_empty() {
            engine.superstep_on_workers(db, &recipe, slice);
        }
        assert!(engine.run(db, &recipe, Slice::Whole));
        RelayRun {
            evaluated_first,
            queued_after_first,
            carried_after_first,
            supersteps: engine.stats.supersteps,
            digest: relay_digest(&mut ws),
        }
    }

    /// The relay statements outside the source's file, queued as changed.
    fn relays_changed() -> Vec<(DerivationId, Reason)> {
        let (_, _, files, statements) = relay_workspace();
        let mut changed = statements
            .into_iter()
            .filter(|id| id.file_id != files[0])
            .map(|id| (id, Reason::Changed))
            .collect::<Vec<_>>();
        changed.sort();
        changed
    }

    /// Runs `f` with each superstep's tasks run one after another, in file
    /// order.
    fn with_one_thread<T>(f: impl FnOnce() -> T) -> T {
        let _threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        parallel::set_analysis_threads(1);
        let _restore = OnDrop(|| parallel::set_analysis_threads(0));
        f()
    }

    #[test]
    fn a_worker_superstep_past_its_deadline_stops_after_its_first_group_and_settles_as_a_whole_one()
    {
        let (whole, stopped) = with_one_thread(|| {
            (
                relay_run(Slice::Whole, false),
                relay_run(Slice::Until(std::time::Instant::now()), false),
            )
        });

        assert_eq!(whole.evaluated_first, 7);
        assert_eq!(stopped.evaluated_first, 1);
        assert_eq!(stopped.queued_after_first, []);
        assert_eq!(stopped.carried_after_first, relays_changed());
        assert!(
            stopped.supersteps > whole.supersteps,
            "stopped {} whole {}",
            stopped.supersteps,
            whole.supersteps
        );
        assert_eq!(stopped.digest, whole.digest);
    }

    /// Settles the relays in slices that end at once, each detaching the
    /// worker superstep due, the next slice installing it, and work with no
    /// superstep due running a slice of one evaluation; the first detached
    /// superstep's tasks are left unrun unless `run_first`. Returns how many
    /// supersteps were detached, and the settled digest.
    fn relay_detached(run_first: bool) -> (usize, Vec<String>) {
        let (mut ws, recipe, _, statements) = relay_workspace();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        engine.interrupt_with(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));
        engine.detach_supersteps();
        for id in &statements {
            engine.changed(*id);
        }
        engine.open(db);
        let mut detached = 0;
        while !engine.run(db, &recipe, Slice::Until(std::time::Instant::now())) {
            let Some(mut superstep) = engine.take_detached() else {
                if engine.run(db, &recipe, Slice::Evaluations(1)) {
                    break;
                }
                continue;
            };
            if run_first || detached > 0 {
                superstep.run_tasks(db, &recipe);
            }
            engine.restore_detached(superstep);
            detached += 1;
        }
        (detached, relay_digest(&mut ws))
    }

    #[test]
    fn detached_supersteps_settle_as_inline_ones_whether_their_tasks_ran_or_not() {
        let (whole, (ran, ran_digest), (skipped, skipped_digest)) = with_one_thread(|| {
            (
                relay_run(Slice::Whole, false),
                relay_detached(true),
                relay_detached(false),
            )
        });

        assert!(ran > 0, "no superstep was detached");
        assert_eq!(skipped, ran + 1);
        assert_eq!(ran_digest, whole.digest);
        assert_eq!(skipped_digest, whole.digest);
    }

    #[test]
    fn an_edit_waiting_stops_a_yielding_worker_superstep_but_not_a_whole_one() {
        let far = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let (whole, interrupted_whole, interrupted) = with_one_thread(|| {
            (
                relay_run(Slice::Whole, false),
                relay_run(Slice::Whole, true),
                relay_run(Slice::Until(far), true),
            )
        });

        assert_eq!(interrupted_whole.evaluated_first, 7);
        assert_eq!(interrupted.evaluated_first, 1);
        assert_eq!(interrupted.carried_after_first, relays_changed());
        assert_eq!(interrupted_whole.digest, whole.digest);
        assert_eq!(interrupted.digest, whole.digest);
    }

    /// Settles the relays, moves the source's fact from outside so every
    /// reader is queued to check it, marks two of them changed, one of them
    /// grouped with a reader in its file, and runs the first superstep in
    /// `slice`. Returns what that left queued and carried, how many
    /// supersteps settling it took and the settled digest.
    fn relay_rerun(slice: Slice) -> RelayRerun {
        let (mut ws, recipe, files, statements) = relay_workspace();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        for id in &statements {
            engine.changed(*id);
        }
        engine.open(db);
        assert!(engine.run(db, &recipe, Slice::Whole));
        let settled = engine.stats.supersteps;
        let source = relay_owner(files[0], RELAY);
        bind_relay(db, source.clone(), LuaType::String);
        let moved = HashedKey::new(FactKey::TypeCache(source));
        db.reader_graph().forget_fingerprint(&moved);
        engine.readers_of(db, [&moved]);
        engine.changed(relay_statement(files[2], RELAY_PAIRED));
        engine.changed(relay_statement(files[3], RELAY));
        engine.open(db);
        engine.superstep_on_workers(db, &recipe, slice);
        let queued = queued_reasons(&engine);
        let carried = carried_reasons(&engine);
        let settling = files
            .iter()
            .filter(|file_id| engine.is_settling(**file_id))
            .copied()
            .collect();
        assert!(engine.run(db, &recipe, Slice::Whole));
        RelayRerun {
            queued,
            carried,
            settling,
            supersteps: engine.stats.supersteps - settled,
            digest: relay_digest(&mut ws),
        }
    }

    struct RelayRerun {
        queued: Vec<(DerivationId, Reason)>,
        carried: Vec<(DerivationId, Reason)>,
        settling: Vec<FileId>,
        supersteps: usize,
        digest: Vec<String>,
    }

    /// The groups the first superstep left unrun, with the entries each took.
    fn relay_unrun(files: &[FileId]) -> Vec<(DerivationId, Reason)> {
        let mut unrun = vec![
            (relay_statement(files[2], RELAY), Reason::Read),
            (relay_statement(files[2], RELAY_PAIRED), Reason::Changed),
            (relay_statement(files[3], RELAY), Reason::Changed),
            (relay_statement(files[4], RELAY), Reason::Read),
            (relay_statement(files[5], RELAY), Reason::Read),
        ];
        unrun.sort();
        unrun
    }

    #[test]
    fn a_stopped_worker_superstep_carries_its_unrun_groups_and_settles_as_a_whole_one() {
        let (whole, stopped) = with_one_thread(|| {
            (
                relay_rerun(Slice::Whole),
                relay_rerun(Slice::Until(std::time::Instant::now())),
            )
        });
        let (_, _, files, _) = relay_workspace();

        assert_eq!((whole.queued, whole.carried), (vec![], vec![]));
        assert_eq!(stopped.queued, []);
        assert_eq!(stopped.carried, relay_unrun(&files));
        assert_eq!(stopped.settling, files[2..].to_vec());
        assert!(
            stopped.supersteps > whole.supersteps,
            "stopped {} whole {}",
            stopped.supersteps,
            whole.supersteps
        );
        assert_eq!(stopped.digest, whole.digest);
    }

    /// Stops the relays' first superstep after its first group, then does
    /// `between` before settling them whole. Returns what was queued and how
    /// many files had groups carried after `between`, and the settled digest.
    fn relay_carried_then(
        between: impl FnOnce(&mut Engine, &[FileId]),
    ) -> (Vec<(DerivationId, Reason)>, usize, Vec<String>) {
        let (mut ws, recipe, files, statements) = relay_workspace();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        for id in &statements {
            engine.changed(*id);
        }
        engine.open(db);
        engine.superstep_on_workers(db, &recipe, Slice::Until(std::time::Instant::now()));
        assert_eq!(carried_reasons(&engine), relays_changed());
        between(&mut engine, &files);
        let queued = queued_reasons(&engine);
        let carried = engine.carried.len();
        assert!(engine.run(db, &recipe, Slice::Whole));
        assert!(engine.carried.is_empty());
        (queued, carried, relay_digest(&mut ws))
    }

    #[test]
    fn carried_groups_are_queued_again_when_the_focus_or_the_files_change_or_the_stage_goes_serial()
    {
        let (whole, prioritised, replaced, serial) = with_one_thread(|| {
            (
                relay_run(Slice::Whole, false).digest,
                relay_carried_then(|engine, files| engine.prioritise(&files[1..2])),
                relay_carried_then(|engine, files| engine.replacing(&files[5..])),
                relay_carried_then(|engine, _| engine.serial_stage = true),
            )
        });
        let (_, _, files, _) = relay_workspace();
        let changed_in = |keep: &dyn Fn(FileId) -> bool| {
            relays_changed()
                .into_iter()
                .filter(|(id, _)| keep(id.file_id))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            (prioritised.0, prioritised.1),
            (changed_in(&|file_id| file_id == files[1]), 0)
        );
        assert_eq!(prioritised.2, whole);
        assert_eq!(
            (replaced.0, replaced.1),
            (changed_in(&|file_id| file_id != files[5]), 0)
        );
        assert_eq!((serial.0, serial.1), (vec![], 5));
        assert_eq!(serial.2, whole);
    }

    /// Statements that all bind one fact in the source's file, so those of
    /// different files run as one group; every one but the source's copies
    /// the source's value into a fact of its own.
    struct Spanning {
        source: FileId,
        value: Mutex<LuaType>,
    }

    impl Recipe for Spanning {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let source = relay_owner(self.source, RELAY);
            if id.file_id == self.source {
                bind_relay(db, source, self.value.lock().unwrap().clone());
            } else {
                let seen = db
                    .get_type_index()
                    .get_type_cache(&source)
                    .map_or(LuaType::Unknown, |cache| cache.as_type().clone());
                let start = u32::from(id.syntax_id().get_range().start());
                bind_relay(db, relay_owner(id.file_id, start), seen);
            }
            bind_relay(db, relay_owner(self.source, RELAY_SHARED), LuaType::Nil);
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("a spanning statement does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("a spanning statement does not split")
        }
    }

    /// What each reader file's copy settles to after the spanning statements
    /// are built and then changed, and how many groups across files ran in
    /// tasks.
    fn spanning_run(serial: bool) -> (Vec<Option<LuaType>>, usize) {
        let _serial = serial.then(serial_engine);
        let (mut ws, _, files, statements) = relay_workspace();
        let recipe = Spanning {
            source: files[0],
            value: Mutex::new(LuaType::IntegerConst(7)),
        };
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        for value in [7, 8] {
            *recipe.value.lock().unwrap() = LuaType::IntegerConst(value);
            for id in &statements {
                engine.changed(*id);
            }
            engine.open(db);
            assert!(engine.run(db, &recipe, Slice::Whole));
        }
        let copies = files[1..]
            .iter()
            .map(|file_id| {
                db.get_type_index()
                    .get_type_cache(&relay_owner(*file_id, RELAY))
                    .map(|cache| cache.as_type().clone())
            })
            .collect();
        (copies, engine.stats.cross_file_groups)
    }

    #[test]
    fn a_group_spanning_files_runs_in_a_task_and_settles_as_the_serial_engine_does() {
        let serial = spanning_run(true);
        let workers = spanning_run(false);

        assert_eq!(serial.1, 0);
        assert!(workers.1 > 0, "no group across files ran in a task");
        assert_eq!(workers.0, serial.0);
        assert!(
            workers
                .0
                .iter()
                .all(|copy| *copy == Some(LuaType::IntegerConst(8))),
            "{:?}",
            workers.0
        );
    }

    /// Statements of three files that all bind one fact in the first file,
    /// so they run as one group: the first file's copies a fact both others
    /// bind `value` into.
    struct SpanningTrio {
        reader: FileId,
        value: Mutex<LuaType>,
    }

    impl Recipe for SpanningTrio {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let written = relay_owner(self.reader, RELAY);
            if id.file_id == self.reader {
                let seen = db
                    .get_type_index()
                    .get_type_cache(&written)
                    .map_or(LuaType::Unknown, |cache| cache.as_type().clone());
                bind_relay(db, relay_owner(self.reader, RELAY_PAIRED), seen);
            } else {
                bind_relay(db, written, self.value.lock().unwrap().clone());
            }
            bind_relay(db, relay_owner(self.reader, RELAY_SHARED), LuaType::Nil);
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("a spanning trio does not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("a spanning trio does not split")
        }
    }

    /// What the reader's copy settles to after each of `values` is written,
    /// and how many groups across files ran in tasks.
    fn spanning_trio_run(serial: bool, values: &[LuaType]) -> (Vec<Option<LuaType>>, usize) {
        let _serial = serial.then(serial_engine);
        let (mut ws, _, files, _) = relay_workspace();
        let recipe = SpanningTrio {
            reader: files[0],
            value: Mutex::new(values[0].clone()),
        };
        let statements = files[..3]
            .iter()
            .map(|file_id| relay_statement(*file_id, RELAY))
            .collect::<Vec<_>>();
        let db = ws.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        let mut copies = Vec::new();
        for (round, value) in values.iter().enumerate() {
            *recipe.value.lock().unwrap() = value.clone();
            let changed = if round == 0 {
                &statements[..]
            } else {
                &statements[1..]
            };
            for id in changed {
                engine.changed(*id);
            }
            engine.open(db);
            assert!(engine.run(db, &recipe, Slice::Whole));
            copies.push(
                db.get_type_index()
                    .get_type_cache(&relay_owner(files[0], RELAY_PAIRED))
                    .map(|cache| cache.as_type().clone()),
            );
        }
        (copies, engine.stats.cross_file_groups)
    }

    #[test]
    fn a_group_in_a_task_reads_a_fact_two_later_members_write_as_they_leave_it() {
        let values = [LuaType::Integer, LuaType::String];
        let expected = values.iter().cloned().map(Some).collect::<Vec<_>>();

        let serial = spanning_trio_run(true, &values);
        let workers = spanning_trio_run(false, &values);

        assert!(workers.1 > 0, "no group across files ran in a task");
        assert_eq!((serial.0, workers.0), (expected.clone(), expected));
    }

    /// Twelve runners, each passing its callers a table, and a caller of
    /// each.
    fn runner_files(ready: &str) -> Vec<(String, String)> {
        let runners = (0..12).map(|index| {
            (
                format!("lua/autorun/sh_runner_{index}.lua"),
                format!("function RunWith{index}(cb)\n    cb({{ ready = {ready} }})\nend\n"),
            )
        });
        let callers = (0..12).map(|index| {
            (
                format!("lua/autorun/sh_caller_{index}.lua"),
                format!("RunWith{index}(function(state)\n    local seen = state.ready\nend)\n"),
            )
        });
        runners.chain(callers).collect()
    }

    /// How many queries the root list computed on workers when every runner
    /// changed what it passes, and the records everything settled to, with
    /// `threads` analysis threads.
    fn runners_edited(threads: usize) -> (usize, Vec<String>) {
        let _threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        parallel::set_analysis_threads(threads);
        let _restore = OnDrop(|| parallel::set_analysis_threads(0));
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        for ready in ["true", "7"] {
            let files = runner_files(ready);
            ws.def_files(
                files
                    .iter()
                    .map(|(path, text)| (path.as_str(), text.as_str()))
                    .collect(),
            );
        }
        let on_workers = ws.analysis.compilation.last_stats().queries_on_workers;
        (on_workers, recorded_digest(&mut ws))
    }

    #[test]
    fn queries_a_superstep_leaves_to_the_root_list_compute_on_workers_and_settle_as_serially() {
        let serial = runners_edited(1);
        let workers = runners_edited(4);

        assert_eq!(serial.0, 0);
        assert!(workers.0 > 0, "no query was computed on a worker");
        assert!(
            serial.1 == workers.1,
            "serial and worker records differ: {:?}",
            serial
                .1
                .iter()
                .filter(|line| !workers.1.contains(line))
                .take(10)
                .collect::<Vec<_>>()
        );
    }

    const INPUT: u32 = 500;

    /// Statements, each at its start with the positions it reads: each binds
    /// at its start the union of what is bound where it reads, so a loop of
    /// them keeps every value it ever saw until it is cleared. At the
    /// positions in `exports`, a module export does the same: a kind no loop
    /// closes through. Each hundred positions lie in a file of their own.
    struct Unions {
        reads: Vec<(u32, Vec<u32>)>,
        exports: Vec<u32>,
    }

    impl Unions {
        fn derivation(&self, start: u32) -> DerivationId {
            let (kind, syntax) = if self.exports.contains(&start) {
                (DerivationKind::ChunkReturn, LuaSyntaxKind::Chunk)
            } else {
                (DerivationKind::LocalStat, LuaSyntaxKind::LocalStat)
            };
            DerivationId::new(
                kind,
                FileId::new(FILE + start / 100),
                LuaSyntaxId::new(
                    syntax.into(),
                    TextRange::at(TextSize::new(start), TextSize::new(1)),
                ),
            )
        }

        /// What each of the derivations binds, in order.
        fn bound(&self, db: &DbIndex) -> Vec<Option<LuaType>> {
            self.reads
                .iter()
                .map(|(start, _)| {
                    db.get_type_index()
                        .get_type_cache(&owner(*start))
                        .map(|cache| cache.as_type().clone())
                })
                .collect()
        }

        /// A workspace the derivations settled in from nothing, `INPUT`
        /// holding `value`.
        fn settled_from(&self, value: &str) -> VirtualWorkspace {
            let mut ws = VirtualWorkspace::new();
            let db = ws.get_db_mut();
            bind(db, INPUT, text(value));
            let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
            for (start, _) in &self.reads {
                engine.changed(self.derivation(*start));
            }
            engine.open(db);
            assert!(engine.run(db, self, Slice::Whole));
            ws
        }
    }

    /// Turns `INPUT` to `value` under the derivations that read it, and has
    /// `engine` check them.
    fn move_input(db: &mut DbIndex, engine: &mut Engine, value: &str) {
        bind(db, INPUT, text(value));
        let input = HashedKey::new(FactKey::TypeCache(owner(INPUT)));
        db.reader_graph().forget_fingerprint(&input);
        engine.readers_of(db, [&input]);
    }

    impl Recipe for Unions {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let start = u32::from(id.syntax_id().get_range().start());
            let (_, reads) = self
                .reads
                .iter()
                .find(|(at, _)| *at == start)
                .expect("a statement of the recipe");
            let mut value = None::<LuaType>;
            for position in reads {
                let Some(read) = db
                    .get_type_index()
                    .get_type_cache(&owner(*position))
                    .map(|cache| cache.as_type().clone())
                else {
                    continue;
                };
                value = Some(match value {
                    Some(value) => TypeOps::Union.apply(db, &value, &read),
                    None => read,
                });
            }
            if let Some(value) = value {
                bind(db, start, value);
            }
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("unions do not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("unions do not split")
        }
    }

    /// What `recipe`'s derivations bind, in order, once `INPUT` turns from
    /// "old" to "new" under them in a run that takes files in `schedule`'s
    /// order, on the serial engine if `serial`, with the stats of that run;
    /// and what they bind in a cold build from "new".
    fn unions_after_edit(
        recipe: &Unions,
        schedule: Schedule,
        serial: bool,
    ) -> (Vec<Option<LuaType>>, EngineStats, Vec<Option<LuaType>>) {
        let _serial = serial.then(serial_engine);
        let mut warm = recipe.settled_from("old");
        let db = warm.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), schedule);
        move_input(db, &mut engine, "new");
        engine.open(db);
        assert!(engine.run(db, recipe, Slice::Whole));
        let warm_bound = recipe.bound(db);
        let mut cold = recipe.settled_from("new");
        let cold_bound = recipe.bound(cold.get_db_mut());
        (warm_bound, engine.stats().clone(), cold_bound)
    }

    #[test]
    fn independent_loops_an_edit_moves_clear_in_one_wave_and_settle_as_a_cold_build() {
        let recipe = Unions {
            reads: vec![
                (10, vec![INPUT, 20]),
                (20, vec![10]),
                (30, vec![INPUT, 40]),
                (40, vec![30]),
            ],
            exports: Vec::new(),
        };

        let (warm, stats, cold) = unions_after_edit(&recipe, Schedule::Canonical, true);

        assert_eq!(warm, cold);
        assert_eq!((stats.loops_cleared, stats.loop_waves), (2, 1));
    }

    #[test]
    fn a_loop_downstream_of_a_cleared_one_clears_before_its_waiting_members_run() {
        let recipe = Unions {
            reads: vec![
                (10, vec![INPUT, 20]),
                (20, vec![10]),
                (30, vec![10, 40]),
                (40, vec![30]),
            ],
            exports: Vec::new(),
        };

        let (warm, stats, cold) = unions_after_edit(&recipe, Schedule::Canonical, true);

        assert_eq!(warm, cold);
        assert_eq!(
            (
                stats.loops_cleared,
                stats.loop_waves,
                stats.loops_cleared_parked
            ),
            (2, 2, 1)
        );
    }

    #[test]
    fn a_loop_with_waiting_work_upstream_of_it_waits_for_that_work_to_settle() {
        let recipe = Unions {
            reads: vec![
                (10, vec![INPUT, 20]),
                (20, vec![10]),
                (30, vec![10, 50, 40]),
                (40, vec![30]),
                (50, vec![10]),
            ],
            exports: Vec::new(),
        };

        let (warm, stats, cold) = unions_after_edit(&recipe, Schedule::Canonical, true);

        assert_eq!(warm, cold);
        assert_eq!(
            (
                stats.loops_cleared,
                stats.loop_waves,
                stats.loops_cleared_parked
            ),
            (2, 2, 0)
        );
    }

    #[test]
    fn a_loop_downstream_of_another_only_through_a_pass_loops_leave_out_waits_for_it() {
        let recipe = Unions {
            reads: vec![
                (10, vec![INPUT, 20]),
                (20, vec![10]),
                (25, vec![10]),
                (30, vec![25, 40]),
                (40, vec![30]),
            ],
            exports: vec![25],
        };

        for schedule in [
            Schedule::Canonical,
            Schedule::Reversed,
            Schedule::Shuffled(7),
            Schedule::Deferred,
        ] {
            let (warm, stats, cold) = unions_after_edit(&recipe, schedule, true);

            assert_eq!(warm, cold, "{schedule:?}");
            // Cleared with the loop upstream of it, then again once that loop settled.
            assert_eq!(
                (
                    stats.loops_cleared,
                    stats.loop_waves,
                    stats.loops_cleared_parked
                ),
                (3, 2, 0),
                "{schedule:?}"
            );
        }
    }

    #[test]
    fn a_chain_of_loops_joined_through_passes_settles_as_a_cold_build_in_any_order() {
        let recipe = Unions {
            reads: vec![
                (110, vec![INPUT, 120]),
                (120, vec![110]),
                (125, vec![110]),
                (210, vec![125, 220]),
                (220, vec![210]),
                (225, vec![210]),
                (310, vec![225, 320]),
                (320, vec![310]),
            ],
            exports: vec![125, 225],
        };

        for serial in [true, false] {
            for schedule in [
                Schedule::Canonical,
                Schedule::Reversed,
                Schedule::Shuffled(7),
                Schedule::Deferred,
            ] {
                let (warm, stats, cold) = unions_after_edit(&recipe, schedule, serial);

                assert_eq!(warm, cold, "{schedule:?} serial={serial}");
                assert_eq!(stats.loop_waves, 3, "{schedule:?} serial={serial}");
            }
        }
    }

    #[test]
    fn a_loop_clears_once_no_source_layer_is_left_to_rebuild() {
        let _serial = serial_engine();
        let recipe = Unions {
            reads: vec![(10, vec![INPUT, 20]), (20, vec![10])],
            exports: Vec::new(),
        };
        let mut warm = recipe.settled_from("old");
        let db = warm.get_db_mut();
        let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
        move_input(db, &mut engine, "mid");
        engine.source_stale(FileId::new(FILE));
        engine.open(db);
        assert!(engine.run(db, &recipe, Slice::Whole));
        let cleared_while_pending = engine.stats().loops_cleared;

        // The rebuilt source layer moves what the loop reads once more.
        assert_eq!(
            engine.take_stale_sources(),
            BTreeSet::from([FileId::new(FILE)])
        );
        move_input(db, &mut engine, "new");
        engine.open(db);
        assert!(engine.run(db, &recipe, Slice::Whole));

        assert_eq!(
            (cleared_while_pending, engine.stats().loops_cleared),
            (0, 1)
        );
        assert_eq!(
            recipe.bound(db),
            recipe.bound(recipe.settled_from("new").get_db_mut())
        );
    }

    /// Statements, each at its start with the position it writes and the
    /// positions it reads: each binds there the union of what it reads and
    /// what the group it runs in wrote there before it, as statements assigning
    /// one local add to its type. An unknown adds nothing to a known value.
    struct Supplies {
        writes: Vec<(u32, u32, Vec<u32>)>,
    }

    impl Supplies {
        fn bound(&self, db: &DbIndex) -> Vec<Option<LuaType>> {
            self.writes
                .iter()
                .map(|(_, written, _)| {
                    db.get_type_index()
                        .get_type_cache(&owner(*written))
                        .map(|cache| cache.as_type().clone())
                })
                .collect()
        }

        /// A workspace the statements settled in from nothing, `INPUT`
        /// holding `value`.
        fn settled_from(&self, value: LuaType) -> VirtualWorkspace {
            let mut ws = VirtualWorkspace::new();
            let db = ws.get_db_mut();
            bind(db, INPUT, value);
            let mut engine = Engine::new(derivation::EditMaps::default(), Schedule::Canonical);
            for (start, _, _) in &self.writes {
                engine.changed(statement(*start));
            }
            engine.open(db);
            assert!(engine.run(db, self, Slice::Whole));
            ws
        }
    }

    impl Recipe for Supplies {
        fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
            let start = u32::from(id.syntax_id().get_range().start());
            let (_, written, reads) = self
                .writes
                .iter()
                .find(|(at, _, _)| *at == start)
                .expect("a statement of the recipe");
            // What is there is merged into, not read, as a declaration's
            // writes merge into its type.
            let mut value = derivation::untracked(|| {
                db.get_type_index()
                    .get_type_cache(&owner(*written))
                    .map(|cache| cache.as_type().clone())
            });
            for position in reads {
                let Some(read) = db
                    .get_type_index()
                    .get_type_cache(&owner(*position))
                    .map(|cache| cache.as_type().clone())
                else {
                    continue;
                };
                value = Some(match value {
                    None => read,
                    Some(value) if read.is_unknown() => value,
                    Some(value) if value.is_unknown() => read,
                    Some(value) => TypeOps::Union.apply(db, &value, &read),
                });
            }
            if let Some(value) = value {
                bind(db, *written, value);
            }
        }

        fn splits(&self, _: DerivationKind) -> bool {
            false
        }

        fn compute(&self, _: &DbIndex, _: DerivationId) -> Box<dyn std::any::Any + Send> {
            unreachable!("supplies do not split")
        }

        fn apply(&self, _: &mut DbIndex, _: DerivationId, _: Box<dyn std::any::Any + Send>) {
            unreachable!("supplies do not split")
        }
    }

    #[test]
    fn a_loop_whose_only_outside_supply_goes_unknown_settles_as_a_cold_build() {
        // `local x = INPUT`, then `y = x`, `z = y` and `x = x + z`: the loop
        // through `x` keeps what `INPUT` gave it unless it is cleared, and
        // only the first statement, outside it, reads `INPUT`.
        let recipe = Supplies {
            writes: vec![
                (10, 10, vec![INPUT]),
                (20, 20, vec![10]),
                (30, 30, vec![20]),
                (40, 10, vec![10, 30]),
            ],
        };

        for serial in [true, false] {
            for schedule in [
                Schedule::Canonical,
                Schedule::Reversed,
                Schedule::Shuffled(7),
                Schedule::Deferred,
            ] {
                let _serial = serial.then(serial_engine);
                let mut warm = recipe.settled_from(text("old"));
                let db = warm.get_db_mut();
                let mut engine = Engine::new(derivation::EditMaps::default(), schedule);
                bind(db, INPUT, LuaType::Unknown);
                let input = HashedKey::new(FactKey::TypeCache(owner(INPUT)));
                db.reader_graph().forget_fingerprint(&input);
                engine.readers_of(db, [&input]);
                engine.open(db);
                assert!(engine.run(db, &recipe, Slice::Whole));

                assert_eq!(
                    recipe.bound(db),
                    recipe.bound(recipe.settled_from(LuaType::Unknown).get_db_mut()),
                    "{schedule:?} serial={serial}"
                );
            }
        }
    }
}
