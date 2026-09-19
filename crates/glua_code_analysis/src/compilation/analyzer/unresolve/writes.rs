//! Deferred index writes for speculative wave resolution.
//!
//! Wave attempts mutate the shared indexes inline, which is what keeps the
//! drain serial: item *i*+1 must observe item *i*'s writes. To run the
//! compute half on workers, each attempt instead records its writes as
//! closures here; the caller then replays them in the original item order
//! against the real index.
//!
//! Replaying the real write path (`bind_type`, `add_member`, …) at apply
//! time preserves the read-modify-write semantics of the serial drain — a
//! deferred `bind_type` merges against whatever the slot holds when it
//! lands, exactly as it would have inline. The only difference is *when*
//! the closure runs, so an item whose reads were invalidated by an
//! earlier-applied write must be recomputed rather than applied.
//!
//! One hazard is intra-item ordering: an attempt that reads a slot *after*
//! queueing its own write to it sees the pre-write state on a worker while
//! the serial path reads its own write. Reads and pushes share one ordinal
//! clock (`read_set::SEQ`), so each push stamps its key with its position
//! and a later read of the same key marks the attempt for serial recompute.
//! A read *before* the push is safe — it matches the serial attempt's own
//! pre-write read, and the staleness check already re-runs the item if an
//! earlier-applied write moved the slot in between.

use std::collections::VecDeque;

use crate::{
    DbIndex,
    compilation::analyzer::infer_cache_manager::InferCacheManager,
    db_index::read_set::{self, InferenceReadSet},
};

pub(crate) use crate::db_index::read_set::WriteKey;

/// One index mutation deferred from a speculative attempt. The boxed closure
/// replays the same call the serial path would have made, in the same order,
/// against the live index and the real per-file infer caches.
type DeferredWrite = Box<dyn FnOnce(&mut DbIndex, &mut InferCacheManager) + Send>;

/// Accumulates an attempt's index writes in call order.
///
/// A queue rather than a map: two writes to the same slot must land in the
/// order the attempt emitted them.
#[derive(Default)]
pub struct ResolveWrites {
    writes: VecDeque<DeferredWrite>,
    /// The ordinal of the first queued write to each slot. Only the first
    /// matters for the intra-item check — any read stamped later may have
    /// needed that write's value. Most attempts touch a handful of slots, so
    /// a linear scan stays cheaper than a hash map's build and teardown.
    first_push: Vec<(WriteKey, u64)>,
}

impl ResolveWrites {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn note_key(&mut self, key: WriteKey) {
        if self.first_push.iter().any(|(existing, _)| *existing == key) {
            return;
        }
        self.first_push.push((key, read_set::next_write_seq()));
    }

    /// Defers one index mutation against `key`'s slot. The closure runs at
    /// apply time against the live index, in push order.
    #[inline]
    pub fn push(
        &mut self,
        key: WriteKey,
        write: impl FnOnce(&mut DbIndex, &mut InferCacheManager) + Send + 'static,
    ) {
        self.note_key(key);
        self.writes.push_back(Box::new(write));
    }

    /// Defers one index mutation that lands on several slots at once — the
    /// closure is replayed once while every slot it touches is recorded for
    /// the intra-item read/write ordering check.
    #[inline]
    pub fn push_multi(
        &mut self,
        keys: impl IntoIterator<Item = WriteKey>,
        write: impl FnOnce(&mut DbIndex, &mut InferCacheManager) + Send + 'static,
    ) {
        for key in keys {
            self.note_key(key);
        }
        self.writes.push_back(Box::new(write));
    }

    /// Whether the attempt read a slot *after* queueing its own write to it.
    /// Such a read saw pre-write state where the serial attempt read its own
    /// write, so the plan must be discarded and the attempt rerun live.
    /// Reads stamped before the push are safe — they match the serial
    /// attempt's own pre-write reads.
    pub fn self_conflicted(&self, reads: &InferenceReadSet) -> bool {
        self.first_push.iter().any(|(key, push_seq)| {
            reads
                .read_seqs
                .get(key)
                .is_some_and(|read_seq| *read_seq > *push_seq)
        })
    }

    /// Replays every deferred write in push order.
    pub fn apply(self, db: &mut DbIndex, infer_manager: &mut InferCacheManager) {
        for write in self.writes {
            write(db, infer_manager);
        }
    }
}
