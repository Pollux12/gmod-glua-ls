//! Which type caches an inference actually consulted, recorded as it runs.
//!
//! The settled tail runs to a fixpoint, and re-deriving every candidate every
//! round costs the same as the first round however little moved. A candidate
//! only needs re-deriving when a cache it *read* has since changed, and the
//! read set is free to collect.
//!
//! What makes that safe is that every read of a stored type goes through
//! `LuaTypeIndex::read_type_cache`, which records here. A read that bypassed it
//! would leave a candidate looking unaffected by a cache it actually depends
//! on, and the fixpoint would settle on the stale answer. Over-recording is
//! harmless — it costs one re-derivation — so a new reader belongs on that
//! accessor rather than on `types` directly.
//!
//! Worker-local by construction — each file is re-derived on one thread, and
//! the collector is a thread local armed around that file's inference. The
//! armed flag is a plain `Cell<bool>` so a call outside the settled tail (the
//! walk asks for type caches constantly) pays one thread-local bool read.

use rustc_hash::FxHashSet;
use std::cell::{Cell, RefCell};

use super::type_owner::LuaTypeOwner;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static READS: RefCell<FxHashSet<LuaTypeOwner>> = RefCell::new(FxHashSet::default());
}

/// Starts collecting on this thread, discarding anything left over.
pub(crate) fn arm() {
    READS.with(|reads| reads.borrow_mut().clear());
    ARMED.with(|armed| armed.set(true));
}

/// Stops collecting and hands back what was read since [`arm`].
pub(crate) fn disarm() -> FxHashSet<LuaTypeOwner> {
    ARMED.with(|armed| armed.set(false));
    READS.with(|reads| std::mem::take(&mut *reads.borrow_mut()))
}

#[inline]
pub(crate) fn record(owner: &LuaTypeOwner) {
    if !ARMED.with(Cell::get) {
        return;
    }
    READS.with(|reads| {
        reads.borrow_mut().insert(owner.clone());
    });
}
