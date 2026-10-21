//! Index writes an unresolve attempt defers until it has finished computing.
//!
//! An attempt computes against `&DbIndex` and records each write as a
//! closure here; the caller replays them, in push order, right after the
//! attempt returns and before the next item is attempted.
//!
//! Replaying the real write path (`bind_type`, `add_member`, …) keeps its
//! read-modify-write semantics: a deferred `bind_type` merges against
//! whatever the slot holds when it lands.

use std::collections::VecDeque;

use crate::{DbIndex, compilation::analyzer::infer_cache_manager::InferCacheManager};

/// One index mutation an attempt deferred. The boxed closure makes the call
/// the attempt would have made, against the live index and the real per-file
/// infer caches.
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
