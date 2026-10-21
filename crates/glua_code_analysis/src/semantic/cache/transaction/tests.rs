use super::*;
use crate::db_index::derivation::{self, DerivationId, DerivationKind, FactKey, KeyTable};
use crate::{FileId, FlowId, GmodRealm, LuaDeclId, VarRefId};
use glua_parser::LuaSyntaxKind;
use rowan::{TextRange, TextSize};

fn cache() -> LuaInferCache {
    LuaInferCache::new(FileId::new(1), Default::default())
}

fn syntax(at: u32) -> LuaSyntaxId {
    LuaSyntaxId::new(
        LuaSyntaxKind::NameExpr.into(),
        TextRange::new(at.into(), (at + 1).into()),
    )
}

fn begin() {
    derivation::lend_keys(KeyTable::default());
    next();
}

fn next() {
    let _recording = derivation::begin(DerivationId::workspace(DerivationKind::GmodLoad));
}

fn proof(cache: &LuaInferCache) -> Provenance {
    let frame = derivation::open_frame().unwrap();
    derivation::note_read(|| FactKey::GmodRealm(FileId::new(7)));
    cache.close_frame(Some(frame)).unwrap()
}

fn finish() {
    derivation::end();
    derivation::take_keys();
}

fn stored(cache: &LuaInferCache, key: LuaSyntaxId) -> Option<&LuaType> {
    match cache.expr_cache.peek(&key) {
        Some(CacheEntry::Cache(value)) => Some(value),
        _ => None,
    }
}

#[test]
fn parallel_cache_rollback_restores_overwritten_canonical_proof_and_removes_unfrozen_insert() {
    begin();
    let mut cache = cache();
    let old = proof(&cache);
    cache
        .expr_cache
        .insert(syntax(1), CacheEntry::Cache(LuaType::Integer), Some(old));
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    let provisional = proof(&cache);
    cache.expr_cache.insert(
        syntax(1),
        CacheEntry::Cache(LuaType::String),
        Some(provisional.clone()),
    );
    cache.expr_cache.insert(
        syntax(2),
        CacheEntry::Cache(LuaType::Boolean),
        Some(provisional),
    );
    drop(transaction);
    cache.rollback_transaction();
    assert_eq!(stored(&cache, syntax(1)), Some(&LuaType::Integer));
    assert!(cache.expr_cache.peek(&syntax(2)).is_none());
    derivation::end();
    next();
    let lookup = cache.lookup();
    assert!(matches!(
        cache.expr_cache.get(&syntax(1), lookup, &[]),
        Some(CacheEntry::Cache(LuaType::Integer))
    ));
    finish();
}

#[test]
fn parallel_cache_rollback_restores_stale_removal_retain_and_clear() {
    begin();
    let mut cache = cache();
    let old = proof(&cache);
    for at in 1..=3 {
        cache.expr_cache.insert(
            syntax(at),
            CacheEntry::Cache(LuaType::Integer),
            Some(old.clone()),
        );
    }
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    derivation::note_write(|| FactKey::GmodRealm(FileId::new(7)));
    let lookup = cache.lookup();
    assert!(cache.expr_cache.get(&syntax(1), lookup, &[]).is_none());
    cache.expr_cache.retain(|key, _| *key != syntax(2));
    cache.expr_cache.clear();
    drop(transaction);
    cache.rollback_transaction();
    for at in 1..=3 {
        assert_eq!(stored(&cache, syntax(at)), Some(&LuaType::Integer));
    }
    finish();
}

#[test]
fn parallel_cache_commit_retains_finalised_new_proof() {
    begin();
    let mut cache = cache();
    let transaction = cache.begin_transaction();
    let provisional = proof(&cache);
    cache.expr_cache.insert(
        syntax(1),
        CacheEntry::Cache(LuaType::Integer),
        Some(provisional),
    );
    drop(transaction);
    derivation::end();
    cache.commit_transaction();
    next();
    let lookup = cache.lookup();
    assert!(matches!(
        cache.expr_cache.get(&syntax(1), lookup, &[]),
        Some(CacheEntry::Cache(LuaType::Integer))
    ));
    finish();
}

#[test]
fn parallel_cache_rollback_preserves_raw_call_bucket_and_old_lookup_during_compute() {
    begin();
    let mut cache = cache();
    let old = proof(&cache);
    cache.insert_call_func_cache(
        syntax(1),
        None,
        LuaType::Integer,
        CacheEntry::Ready,
        Some(old),
    );
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    let provisional = proof(&cache);
    cache.insert_call_func_cache(
        syntax(1),
        None,
        LuaType::String,
        CacheEntry::Ready,
        Some(provisional.clone()),
    );
    assert!(matches!(
        cache.get_call_func_cache(syntax(1), None, &LuaType::Integer),
        Some(CacheEntry::Ready)
    ));
    cache.insert_call_func_cache(
        syntax(1),
        None,
        LuaType::Integer,
        CacheEntry::error(super::super::InferFailReason::FieldNotFound),
        Some(provisional),
    );
    cache.remove_call_func_cache(syntax(1), None, &LuaType::String);
    drop(transaction);
    cache.rollback_transaction();
    assert_eq!(cache.call_cache[&(syntax(1), None)].len(), 1);
    assert!(matches!(
        cache.get_call_func_cache(syntax(1), None, &LuaType::Integer),
        Some(CacheEntry::Ready)
    ));
    finish();
}

#[test]
fn parallel_cache_rollback_restores_discarded_flow_and_removes_new_outer_map() {
    begin();
    let mut cache = cache();
    let reference = VarRefId::VarRef(LuaDeclId::new(FileId::new(1), TextSize::from(1)));
    let key = VarRefCacheKey::from(&reference);
    let new_key = VarRefCacheKey::VarRef(LuaDeclId::new(FileId::new(1), TextSize::from(2)));
    let old = proof(&cache);
    cache.set_flow_cache_with_origin_by_key(
        &key,
        FlowId(1),
        GmodRealm::Server,
        super::super::FlowOrigin::Real,
        CacheEntry::Cache(Arc::new(LuaType::Integer)),
        Some(old),
    );
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    let provisional = proof(&cache);
    cache.set_flow_cache_with_origin_by_key(
        &key,
        FlowId(1),
        GmodRealm::Server,
        super::super::FlowOrigin::Real,
        CacheEntry::Cache(Arc::new(LuaType::String)),
        Some(provisional.clone()),
    );
    let discarded = cache.take_flow_cache_for_var_ref(&reference);
    assert!(discarded.is_some());
    cache.restore_flow_cache_for_var_ref(&reference, None);
    drop(discarded);
    cache.set_flow_cache_with_origin_by_key(
        &new_key,
        FlowId(1),
        GmodRealm::Server,
        super::super::FlowOrigin::Real,
        CacheEntry::Cache(Arc::new(LuaType::Boolean)),
        Some(provisional),
    );
    drop(transaction);
    cache.rollback_transaction();
    assert!(!cache.flow_node_cache.contains_key(&new_key));
    assert!(
        matches!(cache.flow_node_cache[&key].peek(&(FlowId(1), GmodRealm::Server, super::super::FlowOrigin::Real)), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::Integer)
    );
    finish();
}

#[test]
fn parallel_cache_rollback_restores_keyed_flow_locations_after_nested_swaps() {
    begin();
    let mut cache = cache();
    let reference = VarRefId::VarRef(LuaDeclId::new(FileId::new(1), TextSize::from(1)));
    let key = VarRefCacheKey::from(&reference);
    let inner = (FlowId(1), GmodRealm::Server, super::super::FlowOrigin::Real);
    let mut plain = FlowEntries::default();
    plain.insert(
        inner,
        CacheEntry::Cache(Arc::new(LuaType::Integer)),
        Some(proof(&cache)),
    );
    cache.flow_node_cache.insert(key.clone(), plain);
    let mut keyed = FlowEntries::default();
    keyed.insert(
        inner,
        CacheEntry::Cache(Arc::new(LuaType::String)),
        Some(proof(&cache)),
    );
    cache
        .keyed_flow_node_cache
        .insert((key.clone(), LuaType::String), keyed);
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    let previous = cache.key_flow_cache(&reference, &LuaType::String);
    let nested = cache.key_flow_cache(&reference, &LuaType::Boolean);
    cache.set_flow_cache_with_origin_by_key(
        &key,
        FlowId(1),
        GmodRealm::Server,
        super::super::FlowOrigin::Real,
        CacheEntry::Cache(Arc::new(LuaType::Boolean)),
        Some(proof(&cache)),
    );
    cache.unkey_flow_cache(&reference, LuaType::Boolean, nested);
    cache.unkey_flow_cache(&reference, LuaType::String, previous);
    drop(transaction);
    cache.rollback_transaction();
    assert!(
        matches!(cache.flow_node_cache[&key].peek(&inner), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::Integer)
    );
    assert!(
        matches!(cache.keyed_flow_node_cache[&(key.clone(), LuaType::String)].peek(&inner), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::String)
    );
    assert!(
        !cache
            .keyed_flow_node_cache
            .contains_key(&(key, LuaType::Boolean))
    );
    finish();
}

/// A walk repeated from the same origin finds the entries only this
/// evaluation may use, until the evaluation clears them.
#[test]
fn keyed_flow_keeps_local_entries_for_its_origin_until_cleared() {
    let mut cache = cache();
    let reference = VarRefId::VarRef(LuaDeclId::new(FileId::new(1), TextSize::from(1)));
    let key = VarRefCacheKey::from(&reference);
    let inner = (FlowId(1), GmodRealm::Server, super::super::FlowOrigin::Real);
    let local = |cache: &LuaInferCache| match cache.flow_node_cache.get(&key)?.peek(&inner) {
        Some(CacheEntry::Cache(value)) => Some(value.as_ref().clone()),
        _ => None,
    };

    let previous = cache.key_flow_cache(&reference, &LuaType::String);
    cache.set_flow_cache_with_origin_by_key(
        &key,
        FlowId(1),
        GmodRealm::Server,
        super::super::FlowOrigin::Real,
        CacheEntry::Cache(Arc::new(LuaType::String)),
        None,
    );
    cache.unkey_flow_cache(&reference, LuaType::String, previous);

    let previous = cache.key_flow_cache(&reference, &LuaType::Boolean);
    assert_eq!(local(&cache), None, "another origin does not see the entry");
    cache.unkey_flow_cache(&reference, LuaType::Boolean, previous);

    let previous = cache.key_flow_cache(&reference, &LuaType::String);
    assert_eq!(local(&cache), Some(LuaType::String), "the same origin does");
    cache.unkey_flow_cache(&reference, LuaType::String, previous);

    cache.clear();
    let previous = cache.key_flow_cache(&reference, &LuaType::String);
    assert_eq!(local(&cache), None, "clearing the evaluation drops it");
    cache.unkey_flow_cache(&reference, LuaType::String, previous);
}

#[test]
fn parallel_cache_clone_has_independent_undo_and_query_marker_is_detached() {
    begin();
    let mut cache = cache();
    cache.expr_cache.insert(
        syntax(1),
        CacheEntry::Cache(LuaType::Integer),
        Some(proof(&cache)),
    );
    let transaction = cache.begin_transaction();
    cache.expr_cache.insert(
        syntax(1),
        CacheEntry::Cache(LuaType::String),
        Some(proof(&cache)),
    );
    let mut cloned = cache.clone();
    {
        let detached = detach_transaction();
        assert!(current().is_none());
        let mut query = self::cache();
        query.expr_cache.insert(
            syntax(2),
            CacheEntry::Cache(LuaType::Boolean),
            Some(proof(&query)),
        );
        query.rollback_transaction();
        assert_eq!(stored(&query, syntax(2)), Some(&LuaType::Boolean));
        drop(detached);
    }
    assert!(current().is_some());
    drop(transaction);
    cache.rollback_transaction();
    cloned.commit_transaction();
    assert_eq!(stored(&cache, syntax(1)), Some(&LuaType::Integer));
    assert_eq!(stored(&cloned, syntax(1)), Some(&LuaType::String));
    finish();
}

#[derive(Clone, Debug)]
struct EqualKey(u32, &'static str);

impl PartialEq for EqualKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for EqualKey {}

impl std::hash::Hash for EqualKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&0u8, state);
    }
}

#[test]
fn parallel_cache_replacement_preserves_equal_keys_and_rolls_back_collisions() {
    begin();
    let mut cache = cache();
    let mut map = super::super::TrackedMap::default();
    map.insert(EqualKey(1, "original"), 10, Some(proof(&cache)));
    map.insert(EqualKey(2, "second"), 20, Some(proof(&cache)));
    let transaction = cache.begin_transaction();
    map.insert(EqualKey(1, "replacement"), 30, Some(proof(&cache)));
    map.retain(|key, _| {
        if key.0 == 1 {
            assert_eq!(key.1, "original");
        }
        key.0 == 1
    });
    drop(transaction);
    map.rollback_transaction();
    cache.rollback_transaction();
    assert_eq!(map.peek(&EqualKey(1, "lookup")), Some(&10));
    assert_eq!(map.peek(&EqualKey(2, "lookup")), Some(&20));
    finish();
}

fn flow_key(at: u32) -> VarRefCacheKey {
    VarRefCacheKey::VarRef(LuaDeclId::new(FileId::new(1), TextSize::from(at)))
}

fn put_flow(
    cache: &mut LuaInferCache,
    key: &VarRefCacheKey,
    flow: u32,
    entry: CacheEntry<Arc<LuaType>>,
    provenance: Provenance,
) {
    cache.set_flow_cache_with_origin_by_key(
        key,
        FlowId(flow),
        GmodRealm::Server,
        super::super::FlowOrigin::Real,
        entry,
        Some(provenance),
    );
}

#[test]
fn parallel_cache_flow_hits_and_elsewhere_misses_do_not_queue_finish() {
    begin();
    let mut cache = cache();
    let outer = derivation::open_frame().unwrap();
    let previous_realm = cache.set_query_realm(Some(GmodRealm::Server));
    let inner = derivation::open_frame().unwrap();
    assert_eq!(cache.query_realm(), Some(GmodRealm::Server));
    derivation::note_read(|| FactKey::GmodRealm(FileId::new(7)));
    let old = cache.close_frame(Some(inner)).unwrap();
    cache.restore_query_realm(previous_realm);
    cache.close_frame(Some(outer));
    for at in 1..=32 {
        put_flow(
            &mut cache,
            &flow_key(at),
            1,
            CacheEntry::Cache(Arc::new(LuaType::Integer)),
            old.clone(),
        );
    }
    derivation::end();
    next();
    let previous_realm = cache.set_query_realm(Some(GmodRealm::Server));
    let transaction = cache.begin_transaction();
    for at in 1..=32 {
        assert!(
            cache
                .get_flow_cache_with_origin_by_key(
                    &flow_key(at),
                    FlowId(1),
                    GmodRealm::Server,
                    super::super::FlowOrigin::Real
                )
                .is_some()
        );
        assert!(
            cache
                .get_flow_cache_with_origin_by_key(
                    &flow_key(at),
                    FlowId(2),
                    GmodRealm::Server,
                    super::super::FlowOrigin::Real
                )
                .is_none()
        );
    }
    cache.set_query_realm(Some(GmodRealm::Client));
    assert!(
        cache
            .get_flow_cache_with_origin_by_key(
                &flow_key(1),
                FlowId(1),
                GmodRealm::Server,
                super::super::FlowOrigin::Real
            )
            .is_none()
    );
    cache.set_query_realm(Some(GmodRealm::Server));
    assert!(
        cache
            .get_flow_cache_with_origin_by_key(
                &flow_key(1),
                FlowId(1),
                GmodRealm::Server,
                super::super::FlowOrigin::Real,
            )
            .is_some()
    );
    let undo = cache.transaction.as_ref().unwrap();
    assert!(undo.touched_flows.is_empty());
    assert!(undo.flows.is_empty());
    assert!(undo.keyed_flows.is_empty());
    drop(transaction);
    cache.commit_transaction();
    cache.restore_query_realm(previous_realm);
    assert_eq!(cache.flow_node_cache.len(), 32);
    finish();
}

#[test]
fn parallel_cache_sparse_finish_restores_flow_stale_removal() {
    begin();
    let mut cache = cache();
    let old = proof(&cache);
    put_flow(
        &mut cache,
        &flow_key(1),
        1,
        CacheEntry::Cache(Arc::new(LuaType::Integer)),
        old.clone(),
    );
    put_flow(
        &mut cache,
        &flow_key(2),
        1,
        CacheEntry::Cache(Arc::new(LuaType::String)),
        old,
    );
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    derivation::note_write(|| FactKey::GmodRealm(FileId::new(7)));
    assert!(
        cache
            .get_flow_cache_with_origin_by_key(
                &flow_key(1),
                FlowId(1),
                GmodRealm::Server,
                super::super::FlowOrigin::Real
            )
            .is_none()
    );
    let undo = cache.transaction.as_ref().unwrap();
    assert_eq!(undo.touched_flows.len(), 1);
    assert!(undo.touched_flows.contains(&flow_key(1)));
    assert!(undo.flows.is_empty());
    drop(transaction);
    cache.rollback_transaction();
    let inner = (FlowId(1), GmodRealm::Server, super::super::FlowOrigin::Real);
    assert!(
        matches!(cache.flow_node_cache[&flow_key(1)].peek(&inner), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::Integer)
    );
    assert!(
        matches!(cache.flow_node_cache[&flow_key(2)].peek(&inner), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::String)
    );
    finish();
}

#[test]
fn parallel_cache_sparse_commit_finishes_moved_flow_before_next_epoch() {
    begin();
    let mut cache = cache();
    let reference = VarRefId::VarRef(LuaDeclId::new(FileId::new(1), TextSize::from(1)));
    let key = flow_key(1);
    let old = proof(&cache);
    put_flow(
        &mut cache,
        &key,
        1,
        CacheEntry::Cache(Arc::new(LuaType::Integer)),
        old,
    );
    derivation::end();
    next();
    let transaction = cache.begin_transaction();
    let previous = cache.key_flow_cache(&reference, &LuaType::String);
    let provisional = proof(&cache);
    put_flow(
        &mut cache,
        &key,
        1,
        CacheEntry::Cache(Arc::new(LuaType::String)),
        provisional,
    );
    cache.unkey_flow_cache(&reference, LuaType::String, previous);
    drop(transaction);
    derivation::end();
    cache.commit_transaction();
    next();
    let transaction = cache.begin_transaction();
    let previous = cache.key_flow_cache(&reference, &LuaType::String);
    let provisional = proof(&cache);
    put_flow(
        &mut cache,
        &key,
        1,
        CacheEntry::Cache(Arc::new(LuaType::Boolean)),
        provisional,
    );
    cache.unkey_flow_cache(&reference, LuaType::String, previous);
    drop(transaction);
    cache.rollback_transaction();
    let inner = (FlowId(1), GmodRealm::Server, super::super::FlowOrigin::Real);
    assert!(
        matches!(cache.keyed_flow_node_cache[&(key.clone(), LuaType::String)].peek(&inner), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::String)
    );
    assert!(
        matches!(cache.flow_node_cache[&key].peek(&inner), Some(CacheEntry::Cache(value)) if value.as_ref() == &LuaType::Integer)
    );
    finish();
}

#[test]
fn parallel_cache_transaction_guard_tracks_nested_roots_and_detached_scopes() {
    let mut parent = cache();
    let parent_guard = parent.begin_transaction();
    let parent_id = current().unwrap();
    assert!(parent_guard.counted);
    assert!(ACTIVE.load(Ordering::Relaxed) >= 1);
    let detached = detach_transaction();
    assert!(!detached.counted);
    assert!(current().is_none());
    let mut child = cache();
    let child_guard = child.begin_transaction();
    let child_id = current().unwrap();
    assert!(child_guard.counted);
    assert_ne!(parent_id, child_id);
    assert!(ACTIVE.load(Ordering::Relaxed) >= 2);
    drop(child_guard);
    assert!(current().is_none());
    child.commit_transaction();
    drop(detached);
    assert_eq!(current(), Some(parent_id));
    drop(parent_guard);
    assert!(current().is_none());
    parent.rollback_transaction();
    let again = parent.begin_transaction();
    assert!(current().is_some());
    drop(again);
    parent.commit_transaction();
    assert!(current().is_none());
}
