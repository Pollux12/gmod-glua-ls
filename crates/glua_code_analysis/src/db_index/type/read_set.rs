//! Which cached facts an inference actually consulted, recorded as it runs.
//!
//! The settled tail runs to a fixpoint, and re-deriving every candidate every
//! round costs the same as the first round however little moved. A candidate
//! only needs re-deriving when a cache it *read* has since changed, and the
//! read set is free to collect.
//!
//! Stored types, signature returns, and failed member lookups record here at
//! their shared read boundaries. A read that bypassed those boundaries would
//! leave a candidate looking unaffected by a fact it actually depends on, and
//! the fixpoint would settle on the stale answer. Over-recording is harmless —
//! it costs one re-derivation — so a new reader belongs at its shared boundary.
//!
//! Worker-local by construction — each file is re-derived on one thread, and
//! the collector is a thread local armed around that file's inference. The
//! armed flag is a plain `Cell<u32>` depth so a call outside the settled tail
//! (the walk asks for type caches constantly) pays one thread-local read.
//!
//! The depth makes arming nest: the fixpoint arms around each attempt while a
//! return derivation arms around itself inside that attempt. Only the outermost
//! `arm` clears, and only the outermost `disarm` takes — a nested `disarm`
//! hands back a snapshot and leaves the outer collection in place, so the
//! outer read set keeps what the inner scope read. The inner record then
//! over-approximates by the outer scope's earlier reads, which only costs a
//! re-derivation.

use rustc_hash::FxHashSet;
use std::cell::{Cell, RefCell};

use super::type_owner::LuaTypeOwner;
use crate::{LuaMemberKey, LuaMemberOwner, LuaSignatureId};

#[derive(Debug, Default, Clone)]
pub(crate) struct InferenceReadSet {
    pub type_owners: FxHashSet<LuaTypeOwner>,
    pub signatures: FxHashSet<LuaSignatureId>,
    pub missing_member_slots: FxHashSet<(LuaMemberOwner, LuaMemberKey)>,
}

impl InferenceReadSet {
    pub(crate) fn extend(&mut self, other: Self) {
        self.type_owners.extend(other.type_owners);
        self.signatures.extend(other.signatures);
        self.missing_member_slots.extend(other.missing_member_slots);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InferredReturnReadRecord {
    pub reads: InferenceReadSet,
    pub type_epoch: u64,
    pub return_epoch: u64,
}

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static READS: RefCell<FxHashSet<LuaTypeOwner>> = RefCell::new(FxHashSet::default());
    static SIGNATURES: RefCell<FxHashSet<LuaSignatureId>> = RefCell::new(FxHashSet::default());
    static MISSING_MEMBER_SLOTS: RefCell<FxHashSet<(LuaMemberOwner, LuaMemberKey)>> = RefCell::new(FxHashSet::default());
}

/// Starts collecting on this thread, discarding anything left over.
///
/// Nests: only the outermost `arm` clears, so an inner scope (a return
/// derivation inside a fixpoint attempt) accumulates into the same collection
/// its outer scope reads back on `disarm`.
pub(crate) fn arm() {
    if DEPTH.with(Cell::get) == 0 {
        READS.with(|reads| reads.borrow_mut().clear());
        SIGNATURES.with(|signatures| signatures.borrow_mut().clear());
        MISSING_MEMBER_SLOTS.with(|slots| slots.borrow_mut().clear());
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
}

/// Stops collecting and hands back what was read since [`arm`].
///
/// A nested `disarm` returns a snapshot and leaves the outer collection in
/// place; only the outermost `disarm` takes. Unbalanced calls saturate rather
/// than underflow; every `arm` in the codebase is paired, so the saturated
/// path only takes whatever is left over.
pub(crate) fn disarm() -> InferenceReadSet {
    let depth = DEPTH.with(Cell::get);
    DEPTH.with(|armed| armed.set(depth.saturating_sub(1)));
    if depth > 1 {
        InferenceReadSet {
            type_owners: READS.with(|reads| reads.borrow().clone()),
            signatures: SIGNATURES.with(|signatures| signatures.borrow().clone()),
            missing_member_slots: MISSING_MEMBER_SLOTS.with(|slots| slots.borrow().clone()),
        }
    } else {
        InferenceReadSet {
            type_owners: READS.with(|reads| std::mem::take(&mut *reads.borrow_mut())),
            signatures: SIGNATURES.with(|signatures| std::mem::take(&mut *signatures.borrow_mut())),
            missing_member_slots: MISSING_MEMBER_SLOTS
                .with(|slots| std::mem::take(&mut *slots.borrow_mut())),
        }
    }
}

#[inline]
pub(crate) fn record(owner: &LuaTypeOwner) {
    if DEPTH.with(Cell::get) == 0 {
        return;
    }
    READS.with(|reads| {
        reads.borrow_mut().insert(owner.clone());
    });
}

#[inline]
pub(crate) fn record_signature(signature_id: &LuaSignatureId) {
    if DEPTH.with(Cell::get) > 0 {
        SIGNATURES.with(|signatures| {
            signatures.borrow_mut().insert(*signature_id);
        });
    }
}

#[inline]
pub(crate) fn record_missing_member_slot(owner: &LuaMemberOwner, key: &LuaMemberKey) {
    if DEPTH.with(Cell::get) > 0 {
        MISSING_MEMBER_SLOTS.with(|slots| {
            slots.borrow_mut().insert((owner.clone(), key.clone()));
        });
    }
}
