use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hashbrown::HashTable;
use rustc_hash::FxHashMap;

use super::graph::Freshness;
use super::{Evaluation, HashedKey, KeyIdx, KeyTable, ReaderGraph, frames};

mod worker;
pub(crate) use worker::{
    ReadState, RecordingSnapshot, TaskRecorder, TaskRecording, compute_recorded,
    note_observed_query_read, query_requires_canonical, require_canonical_query,
    worker_query_readiness,
};
pub(super) use worker::{SavedRecording, WorkerContext, active, report_worker_replays, with_keys};

#[must_use]
pub(crate) struct ReleaseQuarantine {
    active: Arc<AtomicBool>,
    started: u64,
}

impl ReleaseQuarantine {
    pub(super) fn new(active: Arc<AtomicBool>, started: u64) -> Self {
        Self { active, started }
    }

    pub(crate) fn started(&self) -> u64 {
        self.started
    }

    pub(crate) fn finish(self, table: &mut KeyTable) {
        table.finish_quarantine(&self.active);
    }
}

impl Drop for ReleaseQuarantine {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerKey {
    Known(KeyIdx),
    New(u32),
}

#[derive(Debug, Default)]
pub(crate) struct WorkerKeys {
    entries: Vec<WorkerKey>,
    new_keys: Vec<HashedKey>,
    known: FxHashMap<KeyIdx, u32>,
    /// Every entry by its key's hash, as `hashes` holds them.
    lookup: HashTable<u32>,
    hashes: Vec<u64>,
    visitation: Vec<u64>,
    flags: Vec<u8>,
    /// The stamp each entry was last written at here (0: not written here,
    /// as the snapshot holds it), as [`KeyTable`] keeps them for its slots.
    written: Vec<u64>,
    /// Whether each entry is being computed again here.
    windowed: Vec<bool>,
    /// The entries an evaluation here computes again, each with the stamp it
    /// was written at before; see [`KeyTable::open_window`].
    windows: FxHashMap<u32, u64>,
    /// The entries of the keys read out of each entry's key, and those whose
    /// base has no entry yet, by the base.
    derived: FxHashMap<u32, Vec<u32>>,
    awaiting_base: FxHashMap<HashedKey, Vec<u32>>,
    /// The stamp of the latest write here.
    last_written: u64,
    /// Keys written here that nothing had read, here or in the snapshot: a
    /// task that read one meanwhile has it stamped at the barrier.
    unread_writes: rustc_hash::FxHashSet<HashedKey>,
}

impl WorkerKeys {
    fn push(&mut self, key: WorkerKey, snapshot: &ReaderGraph) -> u32 {
        let local = u32::try_from(self.entries.len()).expect("fewer than 2^32 fact keys");
        self.entries.push(key);
        self.visitation.push(0);
        self.written.push(0);
        self.windowed.push(false);
        let (stored, query) = match key {
            WorkerKey::Known(slot) => (snapshot.key(slot), snapshot.keys().is_query(slot)),
            WorkerKey::New(index) => {
                let stored = &self.new_keys[index as usize];
                let query = matches!(
                    stored.key(),
                    super::FactKey::ReceiverAuthority(_) | super::FactKey::CallbackParams(_)
                );
                (stored, query)
            }
        };
        let flags = u8::from(self.snapshot_taints(local, snapshot)) | (u8::from(query) << 1);
        self.flags.push(flags);
        self.hashes.push(stored.hash);
        let hashes = &self.hashes;
        self.lookup
            .insert_unique(stored.hash, local, |local| hashes[*local as usize]);
        if !self.awaiting_base.is_empty()
            && let Some(children) = self.awaiting_base.remove(stored)
        {
            self.derived.entry(local).or_default().extend(children);
        }
        if let Some(base) = stored.base() {
            let base = HashedKey::new(base);
            match self.find(&base, snapshot) {
                Some(base) => {
                    self.derived.entry(base).or_default().push(local);
                    if self.windowed[base as usize] {
                        self.windowed[local as usize] = true;
                        self.windows.entry(local).or_insert(0);
                    }
                }
                None => self.awaiting_base.entry(base).or_default().push(local),
            }
        }
        local
    }

    fn known(&mut self, slot: KeyIdx, snapshot: &ReaderGraph) -> u32 {
        if let Some(local) = self.known.get(&slot) {
            return *local;
        }
        let local = self.push(WorkerKey::Known(slot), snapshot);
        self.known.insert(slot, local);
        local
    }

    fn intern(&mut self, key: HashedKey, snapshot: &ReaderGraph) -> u32 {
        if let Some(local) = self.find(&key, snapshot) {
            return local;
        }
        if let Some(slot) = snapshot.find_slot(&key) {
            return self.known(slot, snapshot);
        }
        let index = u32::try_from(self.new_keys.len()).expect("fewer than 2^32 fact keys");
        self.new_keys.push(key);
        self.push(WorkerKey::New(index), snapshot)
    }

    /// The entry `key` has here, without adding one.
    fn find(&self, key: &HashedKey, snapshot: &ReaderGraph) -> Option<u32> {
        self.lookup
            .find(key.hash, |local| self.key(*local, snapshot) == key)
            .copied()
    }

    /// The entry a write of `key` marks: one if anything read the key, here
    /// or in the snapshot, as [`KeyTable::mark_written`] marks only slots.
    /// Its derived keys the snapshot has slots for get entries too, so they
    /// are marked with it.
    fn written_entry(&mut self, key: &HashedKey, snapshot: &ReaderGraph) -> Option<u32> {
        let slot = match self.find(key, snapshot) {
            Some(local) => match self.entries[local as usize] {
                WorkerKey::Known(slot) => Some(slot),
                WorkerKey::New(_) => return Some(local),
            },
            None => snapshot.find_slot(key),
        };
        let slot = slot?;
        let local = self.known(slot, snapshot);
        for derived in snapshot.keys().derived_slots(slot) {
            self.known(*derived, snapshot);
        }
        Some(local)
    }

    /// The entry `local` and the entries of the keys read out of it.
    fn with_derived(&self, local: u32) -> Vec<u32> {
        std::iter::once(local)
            .chain(self.derived.get(&local).into_iter().flatten().copied())
            .collect()
    }

    fn stamp(&mut self, local: u32, now: u64) {
        self.last_written = self.last_written.max(now);
        for local in self.with_derived(local) {
            self.written[local as usize] = now;
        }
    }

    /// Marks `key` written at stamp `now`; whether an evaluation that began
    /// at stamp `since` logged it, as [`KeyTable::mark_written_logged`].
    pub(crate) fn mark_written_logged(
        &mut self,
        key: &HashedKey,
        since: u64,
        now: u64,
        snapshot: &ReaderGraph,
    ) -> bool {
        let Some(local) = self.written_entry(key, snapshot) else {
            self.unread_writes.insert(key.clone());
            return false;
        };
        self.stamp(local, now);
        self.visitation[local as usize] >= since
    }

    /// Marks `key` written at stamp `now`, as [`KeyTable::mark_written`].
    pub(crate) fn mark_written(&mut self, key: &HashedKey, now: u64, snapshot: &ReaderGraph) {
        match self.written_entry(key, snapshot) {
            Some(local) => self.stamp(local, now),
            None => {
                self.unread_writes.insert(key.clone());
            }
        }
    }

    /// Marks `key` written at `now` by an evaluation that computes it again,
    /// as [`KeyTable::open_window`].
    pub(crate) fn open_window(&mut self, key: &HashedKey, now: u64, snapshot: &ReaderGraph) {
        let Some(local) = self.written_entry(key, snapshot) else {
            self.unread_writes.insert(key.clone());
            return;
        };
        self.last_written = self.last_written.max(now);
        for local in self.with_derived(local) {
            let written = &mut self.written[local as usize];
            self.windows.entry(local).or_insert(*written);
            *written = now;
            self.windowed[local as usize] = true;
        }
    }

    /// Closes every window: a key `unchanged` accepts, and each key read out
    /// of it, is written as it was before its window opened, as
    /// [`KeyTable::close_windows`].
    pub(crate) fn close_windows(
        &mut self,
        snapshot: &ReaderGraph,
        unchanged: impl Fn(&HashedKey) -> bool,
    ) {
        let windows = std::mem::take(&mut self.windows);
        for (local, before) in &windows {
            self.windowed[*local as usize] = false;
            let key = self.key(*local, snapshot);
            if key.base().is_some() || !unchanged(key) {
                continue;
            }
            self.written[*local as usize] = *before;
            for derived in self.derived.get(local).into_iter().flatten() {
                if let Some(before) = windows.get(derived) {
                    self.written[*derived as usize] = *before;
                }
            }
        }
    }

    /// Whether `key` is computed again here.
    pub(crate) fn windowed(&self, key: &HashedKey, snapshot: &ReaderGraph) -> bool {
        !self.windows.is_empty()
            && self
                .find(key, snapshot)
                .is_some_and(|local| self.windowed[local as usize])
    }

    /// How the entry `local` was written here after stamp `since`, as
    /// [`KeyTable::written_since`] answers for a slot.
    fn entry_freshness(&self, local: u32, since: u64) -> Freshness {
        if self.written[local as usize] <= since {
            return Freshness::Unwritten;
        }
        match self.windows.get(&local) {
            Some(before) if *before <= since => Freshness::Recomputing,
            _ => Freshness::Written,
        }
    }

    /// Whether any of the entries `locals` was written here after `since`.
    pub(crate) fn freshness(&self, locals: &[u32], since: u64) -> Freshness {
        if self.last_written <= since {
            return Freshness::Unwritten;
        }
        locals
            .iter()
            .fold(Freshness::Unwritten, |freshness, local| {
                freshness.and(self.entry_freshness(*local, since))
            })
    }

    /// Whether any of the snapshot's `slots` was written here after `since`.
    pub(crate) fn snapshot_freshness(&self, slots: &[KeyIdx], since: u64) -> Freshness {
        if self.last_written <= since {
            return Freshness::Unwritten;
        }
        slots.iter().fold(Freshness::Unwritten, |freshness, slot| {
            match self.known.get(slot) {
                Some(local) => freshness.and(self.entry_freshness(*local, since)),
                None => freshness,
            }
        })
    }

    /// Whether `key` was written here.
    pub(crate) fn written_here(&self, key: &HashedKey, snapshot: &ReaderGraph) -> bool {
        self.last_written != 0
            && self
                .find(key, snapshot)
                .is_some_and(|local| self.written[local as usize] != 0)
    }

    /// The snapshot slot of the entry `local`, unless its key is new here.
    pub(crate) fn snapshot_slot(&self, local: u32) -> Option<KeyIdx> {
        match self.entries[local as usize] {
            WorkerKey::Known(slot) => Some(slot),
            WorkerKey::New(_) => None,
        }
    }

    /// Whether the entry `local` was written here.
    pub(crate) fn written_here_local(&self, local: u32) -> bool {
        self.written[local as usize] != 0
    }

    /// Whether the entry `local` was written here after stamp `stamp`.
    pub(crate) fn written_after(&self, local: u32, stamp: u64) -> bool {
        self.written[local as usize] > stamp
    }

    /// Every key written here.
    pub(crate) fn written_keys<'a>(
        &'a self,
        snapshot: &'a ReaderGraph,
    ) -> impl Iterator<Item = &'a HashedKey> {
        (0..self.entries.len() as u32)
            .filter(|local| self.written[*local as usize] != 0)
            .map(|local| self.key(local, snapshot))
            .chain(&self.unread_writes)
    }

    pub(crate) fn key<'a>(&'a self, local: u32, snapshot: &'a ReaderGraph) -> &'a HashedKey {
        match self.entries[local as usize] {
            WorkerKey::Known(slot) => snapshot.key(slot),
            WorkerKey::New(index) => &self.new_keys[index as usize],
        }
    }

    fn snapshot_taints(&self, local: u32, snapshot: &ReaderGraph) -> bool {
        let key = self.key(local, snapshot);
        if let WorkerKey::Known(slot) = self.entries[local as usize]
            && snapshot.keys().taints(slot)
        {
            return true;
        }
        let mut base = key.base();
        while let Some(key) = base {
            let key = HashedKey::new(key);
            if snapshot
                .find_slot(&key)
                .is_some_and(|slot| snapshot.keys().taints(slot))
            {
                return true;
            }
            base = key.base();
        }
        false
    }

    fn log(&mut self, evaluation: &mut Evaluation, local: u32, _snapshot: &ReaderGraph) {
        if self.is_query(local)
            && let Some(frame) = evaluation.frames.last_mut()
        {
            frame.has_query = true;
        }
        let stamp = &mut self.visitation[local as usize];
        if *stamp < evaluation.frame_stamp() {
            let first = *stamp < evaluation.stamp;
            *stamp = evaluation.frame_stamp();
            evaluation.log.push(local);
            if first {
                evaluation.read.push(local);
            }
        }
        if self.flags[local as usize] & 1 != 0 || self.windowed[local as usize] {
            frames::taint_evaluation(evaluation);
        }
    }

    fn is_query(&self, local: u32) -> bool {
        self.flags[local as usize] & 2 != 0
    }

    pub(crate) fn record(
        &mut self,
        evaluation: &mut Evaluation,
        key: HashedKey,
        snapshot: &ReaderGraph,
    ) -> u32 {
        let local = self.intern(key, snapshot);
        self.log(evaluation, local, snapshot);
        local
    }

    pub(crate) fn record_known(
        &mut self,
        evaluation: &mut Evaluation,
        slot: KeyIdx,
        snapshot: &ReaderGraph,
    ) -> Option<u32> {
        snapshot.keys().live_key(slot)?;
        let local = self.known(slot, snapshot);
        self.log(evaluation, local, snapshot);
        Some(local)
    }

    pub(crate) fn canonical_slots(
        mut self,
        table: &mut KeyTable,
        stamp: u64,
    ) -> Option<Vec<KeyIdx>> {
        if !self.known_live(table) {
            return None;
        }
        let new_slots = self.intern_new(table, stamp);
        Some(self.slots(&new_slots))
    }

    /// Whether every snapshot slot named here is still live in `table`.
    pub(crate) fn known_live(&self, table: &KeyTable) -> bool {
        self.entries
            .iter()
            .all(|key| !matches!(key, WorkerKey::Known(slot) if table.live_key(*slot).is_none()))
    }

    /// Interns the keys new here in `table`, in the order they were first
    /// read, and returns their slots.
    pub(crate) fn intern_new(&mut self, table: &mut KeyTable, stamp: u64) -> Vec<KeyIdx> {
        assert!(
            table.quarantines(stamp),
            "worker publication outside its release epoch"
        );
        std::mem::take(&mut self.new_keys)
            .into_iter()
            .map(|key| table.intern_worker_read(key))
            .collect()
    }

    /// Each entry's slot, the new keys at `new_slots`.
    pub(crate) fn slots(&self, new_slots: &[KeyIdx]) -> Vec<KeyIdx> {
        self.entries
            .iter()
            .map(|key| match key {
                WorkerKey::Known(slot) => *slot,
                WorkerKey::New(index) => new_slots[*index as usize],
            })
            .collect()
    }

    pub(crate) fn canonicalise(
        self,
        mut evaluation: Evaluation,
        table: &mut KeyTable,
    ) -> Option<Evaluation> {
        let slots = self.canonical_slots(table, evaluation.stamp)?;
        remap(&mut evaluation, &slots);
        Some(evaluation)
    }
}

/// Rewrites the entries `evaluation` recorded as the slots they canonicalise
/// to.
fn remap(evaluation: &mut Evaluation, slots: &[KeyIdx]) {
    assert!(
        evaluation.frames.is_empty(),
        "worker publication with open frames"
    );
    assert!(
        evaluation.reads.is_empty() && evaluation.viewed.is_empty(),
        "worker reads bypassed its registry"
    );
    for slot in &mut evaluation.log {
        *slot = slots[*slot as usize];
    }
    for slot in &mut evaluation.read {
        *slot = slots[*slot as usize];
    }
    for slot in evaluation.read_identities.values_mut() {
        *slot = slots[*slot as usize];
    }
    for read in evaluation.named_reads.iter_mut() {
        read.slot = slots[read.slot as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::super::{self as derivation, DerivationId, DerivationKind, FactKey, new_stamp};
    use super::*;
    use crate::FileId;
    use rowan::TextSize;

    fn key(file: u32) -> HashedKey {
        HashedKey::new(FactKey::GmodRealm(FileId::new(file)))
    }

    fn evaluation() -> Evaluation {
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        derivation::suspend()
    }

    #[test]
    fn parallel_transport_frame_visitation_matches_canonical_logging_with_unrelated_stamps() {
        let mut graph = ReaderGraph::default();
        for file in 1..=3 {
            graph.intern_read(key(file));
        }
        let mut reference = graph.keys().clone();
        let mut keys = WorkerKeys::default();
        let mut reference_log = Vec::new();
        let mut reference_reads = Vec::new();
        derivation::lend_keys(KeyTable::default());
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let mut read = |file| {
            derivation::CURRENT.with(|current| {
                let mut current = current.borrow_mut();
                let evaluation = current.as_mut().unwrap();
                let local = keys.record(evaluation, key(file), &graph);
                let slot = reference.find(&key(file)).unwrap();
                if let (Some(first), _) =
                    reference.log(slot, evaluation.frame_stamp(), evaluation.stamp)
                {
                    reference_log.push(local);
                    if first {
                        reference_reads.push(local);
                    }
                }
            });
        };
        read(1);
        read(1);
        let outer = derivation::open_frame().unwrap();
        read(1);
        std::thread::spawn(|| {
            for _ in 0..32 {
                new_stamp();
            }
        })
        .join()
        .unwrap();
        read(1);
        let child = derivation::open_frame().unwrap();
        read(1);
        read(2);
        read(2);
        derivation::close_frame(child, 0).unwrap();
        read(1);
        read(2);
        let sibling = derivation::open_frame().unwrap();
        read(1);
        read(2);
        derivation::close_frame(sibling, 0).unwrap();
        derivation::close_frame(outer, 0).unwrap();
        read(1);
        read(2);
        read(3);
        read(3);
        let evaluation = derivation::end();
        derivation::take_keys();
        assert_eq!(evaluation.log, reference_log);
        assert_eq!(evaluation.read, reference_reads);
        assert_eq!(evaluation.log, vec![0, 0, 0, 1, 0, 1, 2]);
        assert_eq!(evaluation.read, vec![0, 1, 2]);
    }

    #[test]
    fn parallel_transport_quarantine_preserves_old_free_slots_and_retirement_stamps() {
        let mut graph = ReaderGraph::default();
        let old = graph.intern_read(key(1));
        let available = graph.intern_read(key(2));
        graph.release_keys(vec![available]);
        let guard = graph.quarantine_releases();
        let worker_stamp = new_stamp();
        assert!(worker_stamp > guard.started());
        graph.release_keys(vec![old]);
        assert!(graph.keys().live_key(old).is_none());
        assert_eq!(graph.intern_read(key(3)), available);
        assert_ne!(graph.intern_read(key(4)), old);
        let mut table = graph.lend_keys();
        guard.finish(&mut table);
        let reused = table.intern_read(key(5), new_stamp());
        assert_eq!(reused, old);
        assert!(table.written_after(worker_stamp));
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_quarantine_releases_on_unwind_after_table_lending() {
        let mut graph = ReaderGraph::default();
        let slot = graph.intern_read(key(1));
        let guard = graph.quarantine_releases();
        graph.release_keys(vec![slot]);
        let mut table = graph.lend_keys();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = guard;
            panic!("abort epoch");
        }));
        assert!(result.is_err());
        assert_eq!(table.intern_read(key(2), new_stamp()), slot);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_registry_is_sparse_and_retired_known_reads_fail() {
        assert_eq!(size_of::<WorkerKey>(), 8);
        let mut graph = ReaderGraph::default();
        let global = graph.intern_read(key(1));
        let guard = graph.quarantine_releases();
        let mut evaluation = evaluation();
        let mut keys = WorkerKeys::default();
        let known = keys.record(&mut evaluation, key(1), &graph);
        let new = keys.record(&mut evaluation, key(9), &graph);
        assert_eq!(keys.record(&mut evaluation, key(9), &graph), new);
        assert_eq!(keys.entries.len(), 2);
        assert_eq!(keys.new_keys.len(), 1);
        assert_eq!(keys.visitation.len(), 2);
        assert_eq!(keys.entries[known as usize], WorkerKey::Known(global));
        assert_eq!(evaluation.log, vec![known, new]);
        graph.release_keys(vec![global]);
        assert_ne!(graph.intern_read(key(2)), global);
        let mut table = graph.lend_keys();
        assert!(keys.canonicalise(evaluation, &mut table).is_none());
        guard.finish(&mut table);
        graph.restore_keys(table);
    }

    #[test]
    fn parallel_transport_new_projected_reads_taint_for_held_and_windowed_bases() {
        for held in [false, true] {
            let mut graph = ReaderGraph::default();
            graph.intern_read(key(1));
            if held {
                graph.hold_windows([&key(1)]);
            } else {
                let mut table = graph.lend_keys();
                table.open_window(&key(1), new_stamp());
                graph.restore_keys(table);
            }
            derivation::lend_keys(KeyTable::default());
            let _recording =
                derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
            let frame = derivation::open_frame().unwrap();
            let mut evaluation = derivation::suspend();
            let table = derivation::take_keys();
            let mut keys = WorkerKeys::default();
            let child = HashedKey::new(FactKey::GmodRealmAt(FileId::new(1), TextSize::new(5)));
            keys.record(&mut evaluation, child, &graph);
            assert_eq!(keys.new_keys.len(), 1);
            derivation::lend_keys(table);
            let _recording = derivation::resume(evaluation);
            assert!(derivation::close_frame(frame, 0).is_none());
            derivation::end();
            derivation::take_keys();
        }
    }

    #[test]
    fn parallel_transport_cached_taint_covers_known_new_and_inherited_flags() {
        for held in [false, true] {
            for known in [false, true] {
                for inherited in [false, true] {
                    let mut graph = ReaderGraph::default();
                    let child =
                        HashedKey::new(FactKey::GmodRealmAt(FileId::new(1), TextSize::new(5)));
                    let flagged = if inherited { key(1) } else { child.clone() };
                    graph.intern_read(flagged.clone());
                    if held {
                        graph.hold_windows([&flagged]);
                    } else {
                        let mut table = graph.lend_keys();
                        table.open_window(&flagged, new_stamp());
                        graph.restore_keys(table);
                    }
                    if known {
                        graph.intern_read(child.clone());
                    }
                    let guard = graph.quarantine_releases();
                    derivation::lend_keys(KeyTable::default());
                    let _recording =
                        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
                    let mut keys = WorkerKeys::default();
                    let frame = derivation::open_frame().unwrap();
                    let local = derivation::CURRENT.with(|current| {
                        keys.record(
                            current.borrow_mut().as_mut().unwrap(),
                            child.clone(),
                            &graph,
                        )
                    });
                    assert_eq!(keys.flags, vec![1]);
                    assert!(derivation::close_frame(frame, 0).is_none());
                    let frame = derivation::open_frame().unwrap();
                    derivation::CURRENT.with(|current| {
                        let mut current = current.borrow_mut();
                        let evaluation = current.as_mut().unwrap();
                        keys.log(evaluation, local, &graph);
                        assert_eq!(keys.record(evaluation, child.clone(), &graph), local);
                    });
                    assert!(derivation::close_frame(frame, 0).is_none());
                    assert_eq!(keys.entries.len(), 1);
                    assert_eq!(keys.flags, vec![1]);
                    let evaluation = derivation::suspend();
                    assert_eq!(evaluation.log, vec![local, local]);
                    derivation::take_keys();
                    let mut table = graph.lend_keys();
                    guard.finish(&mut table);
                    graph.restore_keys(table);
                }
            }
        }
        let graph = ReaderGraph::default();
        let mut evaluation = evaluation();
        let mut keys = WorkerKeys::default();
        let local = keys.record(&mut evaluation, key(1), &graph);
        keys.log(&mut evaluation, local, &graph);
        assert_eq!(keys.flags, vec![0]);
        assert_eq!(evaluation.log, vec![local]);
    }

    #[test]
    fn parallel_transport_canonicalisation_preserves_nested_ranges_and_late_apply_reads() {
        let mut graph = ReaderGraph::default();
        graph.intern_read(key(99));
        let global = graph.intern_read(key(1));
        let guard = graph.quarantine_releases();
        derivation::lend_keys(KeyTable::default());
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        let outer = derivation::open_frame().unwrap();
        let mut keys = WorkerKeys::default();
        let known = derivation::CURRENT
            .with(|current| keys.record(current.borrow_mut().as_mut().unwrap(), key(1), &graph));
        let inner = derivation::open_frame().unwrap();
        let new = derivation::CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            let evaluation = current.as_mut().unwrap();
            keys.record_known(evaluation, global, &graph).unwrap();
            keys.record(evaluation, key(2), &graph)
        });
        let inner_proof = derivation::close_frame(inner, 0).unwrap();
        let outer_proof = derivation::close_frame(outer, 0).unwrap();
        let mut evaluation = derivation::suspend();
        assert_eq!(evaluation.log, vec![known, known, new]);
        evaluation.read_identities.insert((1, 7), known);
        evaluation.named_reads.insert_unique(
            0,
            derivation::NamedRead {
                tag: 1,
                owner: 7,
                name: None,
                slot: new,
            },
            |_| 0,
        );
        derivation::take_keys();
        let mut table = graph.lend_keys();
        let evaluation = keys.canonicalise(evaluation, &mut table).unwrap();
        let new_global = table.find(&key(2)).unwrap();
        assert_ne!(known, global);
        assert_eq!(evaluation.log, vec![global, global, new_global]);
        assert_eq!(evaluation.read_identities[&(1, 7)], global);
        assert_eq!(
            evaluation.named_reads.iter().next().unwrap().slot,
            new_global
        );
        derivation::lend_keys(table);
        let _recording = derivation::resume(evaluation);
        assert_eq!(
            derivation::reuse(&inner_proof, 0, &[], None),
            derivation::Reuse::Held
        );
        let late = derivation::open_frame().unwrap();
        derivation::note_read(|| key(3).key().clone());
        let late_proof = derivation::close_frame(late, 0).unwrap();
        derivation::end();
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
        assert_eq!(
            derivation::reuse(&outer_proof, 0, &[], None),
            derivation::Reuse::Held
        );
        assert_eq!(
            derivation::reuse(&late_proof, 0, &[], None),
            derivation::Reuse::Held
        );
        let reads = derivation::end().into_recorded().1;
        let mut table = derivation::take_keys();
        let late_global = table.find(&key(3)).unwrap();
        assert_eq!(reads, vec![global, new_global, late_global]);
        guard.finish(&mut table);
        graph.restore_keys(table);
    }
}
