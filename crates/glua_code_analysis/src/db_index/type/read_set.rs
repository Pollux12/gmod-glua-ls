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

use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{Cell, RefCell};

use super::type_owner::LuaTypeOwner;
use crate::{DynamicFieldOwner, FileId, LuaMemberKey, LuaMemberOwner, LuaSignatureId};
use rowan::TextSize;
use smol_str::SmolStr;

#[derive(Debug, Default, Clone)]
pub(crate) struct InferenceReadSet {
    pub type_owners: FxHashSet<LuaTypeOwner>,
    pub member_maps: FxHashMap<LuaMemberOwner, (u64, u64, usize)>,
    pub signatures: FxHashSet<LuaSignatureId>,
    pub params: FxHashSet<LuaSignatureId>,
    pub modules: FxHashSet<FileId>,
    /// Special-call materializations read through the flow effects of one
    /// call-site, keyed by that site. See `LuaFlowIndex::special_writes`.
    pub special_calls: FxHashSet<(FileId, TextSize)>,
    /// Call-site-inferred parameter facts, keyed by the callee slot that
    /// holds them. See `CallSiteParamIndex::contrib_writes`.
    pub contribs: FxHashSet<(LuaSignatureId, usize)>,
    /// Dynamic-field definitions consulted, keyed by owner and field name
    /// (`None` is the wildcard bucket). See `DynamicFieldIndex::dynamic_writes`.
    pub dynamic_fields: FxHashSet<(DynamicFieldOwner, Option<SmolStr>)>,
    pub missing_member_slots: FxHashSet<(LuaMemberOwner, LuaMemberKey)>,
}

impl InferenceReadSet {
    pub(crate) fn extend(&mut self, other: Self) {
        self.type_owners.extend(other.type_owners);
        for (owner, version) in other.member_maps {
            self.member_maps.entry(owner).or_insert(version);
        }
        self.signatures.extend(other.signatures);
        self.params.extend(other.params);
        self.modules.extend(other.modules);
        self.special_calls.extend(other.special_calls);
        self.contribs.extend(other.contribs);
        self.dynamic_fields.extend(other.dynamic_fields);
        self.missing_member_slots.extend(other.missing_member_slots);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InferredReturnReadRecord {
    pub reads: InferenceReadSet,
    pub type_epoch: u64,
    pub return_epoch: u64,
    pub param_epoch: u64,
    pub module_epoch: u64,
    pub special_epoch: u64,
    pub contrib_epoch: u64,
    pub dynamic_epoch: u64,
}

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static TRACK_MEMBER_MAPS: Cell<bool> = const { Cell::new(false) };
    static MEMBER_MAPS: RefCell<FxHashMap<LuaMemberOwner, (u64, u64, usize)>> = RefCell::new(FxHashMap::default());
    static READS: RefCell<FxHashSet<LuaTypeOwner>> = RefCell::new(FxHashSet::default());
    static SIGNATURES: RefCell<FxHashSet<LuaSignatureId>> = RefCell::new(FxHashSet::default());
    static PARAMS: RefCell<FxHashSet<LuaSignatureId>> = RefCell::new(FxHashSet::default());
    static MODULES: RefCell<FxHashSet<FileId>> = RefCell::new(FxHashSet::default());
    static SPECIAL_CALLS: RefCell<FxHashSet<(FileId, TextSize)>> = RefCell::new(FxHashSet::default());
    static CONTRIBS: RefCell<FxHashSet<(LuaSignatureId, usize)>> = RefCell::new(FxHashSet::default());
    static DYNAMIC_FIELDS: RefCell<FxHashSet<(DynamicFieldOwner, Option<SmolStr>)>> = RefCell::new(FxHashSet::default());
    static MISSING_MEMBER_SLOTS: RefCell<FxHashSet<(LuaMemberOwner, LuaMemberKey)>> = RefCell::new(FxHashSet::default());
}

/// Starts collecting reads, sharing the collection with any enclosing scope.
pub(crate) fn arm() {
    if DEPTH.with(Cell::get) == 0 {
        TRACK_MEMBER_MAPS.with(|track| track.set(false));
        READS.with(|reads| reads.borrow_mut().clear());
        MEMBER_MAPS.with(|maps| maps.borrow_mut().clear());
        SIGNATURES.with(|signatures| signatures.borrow_mut().clear());
        PARAMS.with(|params| params.borrow_mut().clear());
        MODULES.with(|modules| modules.borrow_mut().clear());
        SPECIAL_CALLS.with(|sites| sites.borrow_mut().clear());
        CONTRIBS.with(|slots| slots.borrow_mut().clear());
        DYNAMIC_FIELDS.with(|fields| fields.borrow_mut().clear());
        MISSING_MEMBER_SLOTS.with(|slots| slots.borrow_mut().clear());
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
}

/// Includes member-map shape reads for settled passes that validate them.
pub(crate) fn arm_with_member_maps() {
    arm();
    TRACK_MEMBER_MAPS.with(|track| track.set(true));
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
            member_maps: MEMBER_MAPS.with(|maps| maps.borrow().clone()),
            signatures: SIGNATURES.with(|signatures| signatures.borrow().clone()),
            params: PARAMS.with(|params| params.borrow().clone()),
            modules: MODULES.with(|modules| modules.borrow().clone()),
            special_calls: SPECIAL_CALLS.with(|sites| sites.borrow().clone()),
            contribs: CONTRIBS.with(|slots| slots.borrow().clone()),
            dynamic_fields: DYNAMIC_FIELDS.with(|fields| fields.borrow().clone()),
            missing_member_slots: MISSING_MEMBER_SLOTS.with(|slots| slots.borrow().clone()),
        }
    } else {
        InferenceReadSet {
            type_owners: READS.with(|reads| std::mem::take(&mut *reads.borrow_mut())),
            member_maps: MEMBER_MAPS.with(|maps| std::mem::take(&mut *maps.borrow_mut())),
            signatures: SIGNATURES.with(|signatures| std::mem::take(&mut *signatures.borrow_mut())),
            params: PARAMS.with(|params| std::mem::take(&mut *params.borrow_mut())),
            modules: MODULES.with(|modules| std::mem::take(&mut *modules.borrow_mut())),
            special_calls: SPECIAL_CALLS.with(|sites| std::mem::take(&mut *sites.borrow_mut())),
            contribs: CONTRIBS.with(|slots| std::mem::take(&mut *slots.borrow_mut())),
            dynamic_fields: DYNAMIC_FIELDS.with(|fields| std::mem::take(&mut *fields.borrow_mut())),
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

/// Records a read of a signature's parameter contract (`param_docs`).
///
/// `LuaSignature::get_param_info_by_id/by_name` cannot record: the signature
/// does not know its own id. Callers that hold the id record at the shared
/// inference boundaries instead (`infer_param`, closure/special-call
/// resolution), which covers every read an armed scope can make.
#[inline]
pub(crate) fn record_param(signature_id: &LuaSignatureId) {
    if DEPTH.with(Cell::get) > 0 {
        PARAMS.with(|params| {
            params.borrow_mut().insert(*signature_id);
        });
    }
}

/// Records a read of a module's exported type, keyed by the exporting file.
#[inline]
pub(crate) fn record_module(file_id: &FileId) {
    if DEPTH.with(Cell::get) > 0 {
        MODULES.with(|modules| {
            modules.borrow_mut().insert(*file_id);
        });
    }
}

/// Records a read of one call-site's special-call materializations, keyed by
/// the calling file and the call's start offset.
#[inline]
pub(crate) fn record_special_call(file_id: &FileId, position: TextSize) {
    if DEPTH.with(Cell::get) > 0 {
        SPECIAL_CALLS.with(|sites| {
            sites.borrow_mut().insert((*file_id, position));
        });
    }
}

/// Records a read of one call-site-inferred parameter fact, keyed by the
/// callee slot that holds it.
#[inline]
pub(crate) fn record_contrib(signature_id: &LuaSignatureId, param_idx: usize) {
    if DEPTH.with(Cell::get) > 0 {
        CONTRIBS.with(|slots| {
            slots.borrow_mut().insert((*signature_id, param_idx));
        });
    }
}

/// Records a read of one dynamic field's definitions, keyed by owner and
/// field name (`None` is the wildcard bucket).
#[inline]
pub(crate) fn record_dynamic_field(owner: &DynamicFieldOwner, field_name: Option<&str>) {
    if DEPTH.with(Cell::get) > 0 {
        DYNAMIC_FIELDS.with(|fields| {
            fields
                .borrow_mut()
                .insert((owner.clone(), field_name.map(SmolStr::new)));
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

#[inline]
pub(crate) fn record_member_map(index: &crate::LuaMemberIndex, owner: &LuaMemberOwner) {
    if DEPTH.with(Cell::get) > 0 && TRACK_MEMBER_MAPS.with(Cell::get) {
        MEMBER_MAPS.with(|maps| {
            maps.borrow_mut()
                .entry(owner.clone())
                .or_insert_with(|| index.read_version(owner));
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InferenceReadVersion {
    type_epoch: u64,
    return_epoch: u64,
    param_epoch: u64,
    module_epoch: u64,
    special_epoch: u64,
    contrib_epoch: u64,
    dynamic_epoch: u64,
}

impl InferenceReadVersion {
    pub(crate) fn capture(db: &crate::DbIndex) -> Self {
        Self {
            type_epoch: db.get_type_index().type_writes(),
            return_epoch: db.get_signature_index().return_writes(),
            param_epoch: db.get_signature_index().param_writes(),
            module_epoch: db.get_module_index().module_writes(),
            special_epoch: db.get_flow_index().special_call_writes(),
            contrib_epoch: db.get_call_site_param_index().contrib_writes(),
            dynamic_epoch: db.get_dynamic_field_index().dynamic_writes(),
        }
    }

    pub(crate) fn is_stale(&self, db: &crate::DbIndex, reads: &InferenceReadSet) -> bool {
        self.value_reads_changed(db, reads)
            || reads
                .member_maps
                .iter()
                .any(|(owner, version)| db.get_member_index().read_version(owner) != *version)
    }

    fn value_reads_changed(&self, db: &crate::DbIndex, reads: &InferenceReadSet) -> bool {
        reads.type_owners.iter().any(|key| db.get_type_index().type_write_version(key) > self.type_epoch)
            || reads.signatures.iter().any(|key| db.get_signature_index().return_write_version(key) > self.return_epoch)
            || reads.params.iter().any(|key| db.get_signature_index().param_write_version(key) > self.param_epoch)
            || reads.modules.iter().any(|key| db.get_module_index().module_write_version(key) > self.module_epoch)
            || reads.special_calls.iter().any(|(file, pos)| db.get_flow_index().special_call_write_version(file, pos) > self.special_epoch)
            || reads.contribs.iter().any(|(sig, param)| db.get_call_site_param_index().contrib_write_version(sig, *param) > self.contrib_epoch)
            || reads.dynamic_fields.iter().any(|key| db.get_dynamic_field_index().dynamic_write_version(key) > self.dynamic_epoch)
            || reads.missing_member_slots.iter().any(|(owner, key)| {
                !db.get_member_index().get_members_for_owner_key(owner, key).is_empty()
                    || matches!(key, LuaMemberKey::Name(name) if crate::dynamic_field_owner_of(db, owner).is_some_and(|dynamic_owner| {
                        !db.get_dynamic_field_index().field_definitions(&dynamic_owner, name.as_str()).is_empty()
                    }))
            })
    }
}

impl InferredReturnReadRecord {
    pub(crate) fn is_stale(&self, db: &crate::DbIndex) -> bool {
        InferenceReadVersion {
            type_epoch: self.type_epoch,
            return_epoch: self.return_epoch,
            param_epoch: self.param_epoch,
            module_epoch: self.module_epoch,
            special_epoch: self.special_epoch,
            contrib_epoch: self.contrib_epoch,
            dynamic_epoch: self.dynamic_epoch,
        }
        .value_reads_changed(db, &self.reads)
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use crate::{
        DbIndex, LuaDeclId, LuaIndex, LuaMember, LuaMemberFeature, LuaMemberId, LuaType,
        LuaTypeCache,
    };
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::TextRange;

    #[test]
    fn recorded_types_ignore_unrelated_writes() {
        let mut db = DbIndex::new();
        let owner = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), 1.into()));
        let other = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), 2.into()));
        let version = InferenceReadVersion::capture(&db);
        arm();
        record(&owner);
        let reads = disarm();
        db.get_type_index_mut()
            .bind_type(other, LuaTypeCache::InferType(LuaType::String));
        assert!(!version.is_stale(&db, &reads));
        db.get_type_index_mut()
            .bind_type(owner, LuaTypeCache::InferType(LuaType::Integer));
        assert!(version.is_stale(&db, &reads));
    }

    #[test]
    fn member_reads_invalidate_on_add_rekey_delete_and_reopen() {
        let mut db = DbIndex::new();
        let file = FileId::new(1);
        let owner = LuaMemberOwner::Element(crate::InFiled::new(
            file,
            TextRange::new(0.into(), 10.into()),
        ));
        let id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(1.into(), 2.into()),
            ),
            file,
        );
        let version = InferenceReadVersion::capture(&db);
        let collect = |db: &DbIndex| {
            arm_with_member_maps();
            let _ = db.get_member_index().get_members(&owner);
            disarm()
        };
        let reads = collect(&db);
        let member = || {
            LuaMember::new(
                id,
                LuaMemberKey::Name("x".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            )
        };
        db.get_member_index_mut()
            .add_member(owner.clone(), member());
        assert!(version.is_stale(&db, &reads));
        let reads = collect(&db);
        assert!(!version.is_stale(&db, &reads));
        db.get_member_index_mut()
            .rekey_member(id, LuaMemberKey::Name("y".into()));
        assert!(version.is_stale(&db, &reads));
        let reads = collect(&db);
        db.get_member_index_mut().remove_files(&[file]);
        assert!(version.is_stale(&db, &reads));
        let reads = collect(&db);
        db.get_member_index_mut().add_member(owner, member());
        assert!(version.is_stale(&db, &reads));
    }
}
