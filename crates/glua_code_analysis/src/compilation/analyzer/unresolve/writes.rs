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

use std::collections::VecDeque;

use crate::{DbIndex, compilation::analyzer::infer_cache_manager::InferCacheManager};

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
}

impl ResolveWrites {
    pub fn new() -> Self {
        Self::default()
    }

    /// Defers one index mutation. The closure runs at apply time against the
    /// live index, in push order.
    #[inline]
    pub fn push(
        &mut self,
        write: impl FnOnce(&mut DbIndex, &mut InferCacheManager) + Send + 'static,
    ) {
        self.writes.push_back(Box::new(write));
    }

    /// Replays every deferred write in push order.
    pub fn apply(self, db: &mut DbIndex, infer_manager: &mut InferCacheManager) {
        for write in self.writes {
            write(db, infer_manager);
        }
    }
}
