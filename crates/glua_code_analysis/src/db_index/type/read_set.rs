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
//! armed flag is a plain `Cell<bool>` so a call outside the settled tail (the
//! walk asks for type caches constantly) pays one thread-local bool read.

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
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static READS: RefCell<FxHashSet<LuaTypeOwner>> = RefCell::new(FxHashSet::default());
    static SIGNATURES: RefCell<FxHashSet<LuaSignatureId>> = RefCell::new(FxHashSet::default());
    static MISSING_MEMBER_SLOTS: RefCell<FxHashSet<(LuaMemberOwner, LuaMemberKey)>> = RefCell::new(FxHashSet::default());
}

/// Starts collecting on this thread, discarding anything left over.
pub(crate) fn arm() {
    READS.with(|reads| reads.borrow_mut().clear());
    SIGNATURES.with(|signatures| signatures.borrow_mut().clear());
    MISSING_MEMBER_SLOTS.with(|slots| slots.borrow_mut().clear());
    ARMED.with(|armed| armed.set(true));
}

/// Stops collecting and hands back what was read since [`arm`].
pub(crate) fn disarm() -> InferenceReadSet {
    ARMED.with(|armed| armed.set(false));
    InferenceReadSet {
        type_owners: READS.with(|reads| std::mem::take(&mut *reads.borrow_mut())),
        signatures: SIGNATURES.with(|signatures| std::mem::take(&mut *signatures.borrow_mut())),
        missing_member_slots: MISSING_MEMBER_SLOTS
            .with(|slots| std::mem::take(&mut *slots.borrow_mut())),
    }
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

#[inline]
pub(crate) fn record_signature(signature_id: &LuaSignatureId) {
    if ARMED.with(Cell::get) {
        SIGNATURES.with(|signatures| {
            signatures.borrow_mut().insert(*signature_id);
        });
    }
}

#[inline]
pub(crate) fn record_missing_member_slot(owner: &LuaMemberOwner, key: &LuaMemberKey) {
    if ARMED.with(Cell::get) {
        MISSING_MEMBER_SLOTS.with(|slots| {
            slots.borrow_mut().insert((owner.clone(), key.clone()));
        });
    }
}
