use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hashbrown::HashTable;
use rustc_hash::FxHashMap;

use super::{Evaluation, HashedKey, KeyIdx, KeyTable, ReaderGraph, frames};

mod worker;
pub(crate) use worker::{
    RecordingSnapshot, compute_recorded, note_observed_query_read, query_requires_canonical,
    require_canonical_query, worker_query_readiness,
};
pub(super) use worker::{SavedRecording, active, report_worker_replays, with_keys, with_snapshot};

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
    new_lookup: HashTable<u32>,
    visitation: Vec<u64>,
    flags: Vec<u8>,
}

impl WorkerKeys {
    fn push(&mut self, key: WorkerKey, snapshot: &ReaderGraph) -> u32 {
        let local = u32::try_from(self.entries.len()).expect("fewer than 2^32 fact keys");
        self.entries.push(key);
        self.visitation.push(0);
        let query = match key {
            WorkerKey::Known(slot) => snapshot.keys().is_query(slot),
            WorkerKey::New(index) => matches!(
                self.new_keys[index as usize].key(),
                super::FactKey::ReceiverAuthority(_) | super::FactKey::CallbackParams(_)
            ),
        };
        let flags = u8::from(self.snapshot_taints(local, snapshot)) | (u8::from(query) << 1);
        self.flags.push(flags);
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
        if let Some(slot) = snapshot.find_slot(&key) {
            return self.known(slot, snapshot);
        }
        if let Some(local) = self.new_lookup.find(key.hash, |local| {
            let WorkerKey::New(index) = self.entries[*local as usize] else {
                unreachable!()
            };
            self.new_keys[index as usize] == key
        }) {
            return *local;
        }
        let index = u32::try_from(self.new_keys.len()).expect("fewer than 2^32 fact keys");
        let hash = key.hash;
        self.new_keys.push(key);
        let local = self.push(WorkerKey::New(index), snapshot);
        let entries = &self.entries;
        let new_keys = &self.new_keys;
        self.new_lookup.insert_unique(hash, local, |local| {
            let WorkerKey::New(index) = entries[*local as usize] else {
                unreachable!()
            };
            new_keys[index as usize].hash
        });
        local
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
        if self.flags[local as usize] & 1 != 0 {
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

    pub(crate) fn canonical_slots(self, table: &mut KeyTable, stamp: u64) -> Option<Vec<KeyIdx>> {
        assert!(
            table.quarantines(stamp),
            "worker publication outside its release epoch"
        );
        if self
            .entries
            .iter()
            .any(|key| matches!(key, WorkerKey::Known(slot) if table.live_key(*slot).is_none()))
        {
            return None;
        }
        let new_slots = self
            .new_keys
            .into_iter()
            .map(|key| table.intern_worker_read(key))
            .collect::<Vec<_>>();
        Some(
            self.entries
                .into_iter()
                .map(|key| match key {
                    WorkerKey::Known(slot) => slot,
                    WorkerKey::New(index) => new_slots[index as usize],
                })
                .collect(),
        )
    }

    pub(crate) fn canonicalise(
        self,
        mut evaluation: Evaluation,
        table: &mut KeyTable,
    ) -> Option<Evaluation> {
        assert!(
            evaluation.frames.is_empty(),
            "worker publication with open frames"
        );
        assert!(
            evaluation.reads.is_empty() && evaluation.viewed.is_empty(),
            "worker reads bypassed its registry"
        );
        let slots = self.canonical_slots(table, evaluation.stamp)?;
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
        Some(evaluation)
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
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
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
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
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
            derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
            let frame = derivation::open_frame().unwrap();
            let mut evaluation = derivation::suspend();
            let table = derivation::take_keys();
            let mut keys = WorkerKeys::default();
            let child = HashedKey::new(FactKey::GmodRealmAt(FileId::new(1), TextSize::new(5)));
            keys.record(&mut evaluation, child, &graph);
            assert_eq!(keys.new_keys.len(), 1);
            derivation::lend_keys(table);
            derivation::resume(evaluation);
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
        derivation::begin(DerivationId::workspace(DerivationKind::ValidGuards));
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
        derivation::resume(evaluation);
        assert_eq!(
            derivation::reuse(&inner_proof, 0, &[], None),
            derivation::Reuse::Held
        );
        let late = derivation::open_frame().unwrap();
        derivation::note_read(|| key(3).key().clone());
        let late_proof = derivation::close_frame(late, 0).unwrap();
        derivation::end();
        derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
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
