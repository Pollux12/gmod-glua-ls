use std::{
    cell::Cell,
    hash::Hash,
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    CacheEntry, FlowEntries, LuaFunctionType, LuaInferCache, LuaSyntaxId, LuaType, Provenance,
    VarRefCacheKey,
};
use std::sync::Arc;

type CallKey = (LuaSyntaxId, Option<usize>);
type CallBucket = Vec<(LuaType, CacheEntry<Arc<LuaFunctionType>>, Provenance)>;
type KeyedFlowKey = (VarRefCacheKey, LuaType);

thread_local! {
    static CURRENT: Cell<Option<u64>> = const { Cell::new(None) };
}

pub(crate) struct CacheTransaction {
    previous: Option<u64>,
    marker: PhantomData<Rc<()>>,
}

impl Drop for CacheTransaction {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.previous));
    }
}

pub(crate) fn detach_transaction() -> CacheTransaction {
    CacheTransaction {
        previous: CURRENT.with(|current| current.replace(None)),
        marker: PhantomData,
    }
}

pub(super) fn current() -> Option<u64> {
    CURRENT.with(Cell::get)
}

#[derive(Debug, Clone)]
pub(super) struct CacheUndo {
    id: u64,
    calls: FxHashMap<CallKey, Option<CallBucket>>,
    flows: FxHashMap<VarRefCacheKey, Option<FlowEntries>>,
    keyed_flows: FxHashMap<KeyedFlowKey, Option<FlowEntries>>,
    touched_flows: FxHashSet<VarRefCacheKey>,
}

impl CacheUndo {
    pub(super) fn touch_flow(&mut self, key: &VarRefCacheKey) {
        if Some(self.id) == current() {
            self.touched_flows.insert(key.clone());
        }
    }
}

fn capture<K: Eq + Hash + Clone, V: Clone>(
    map: &mut FxHashMap<K, V>,
    undo: &mut FxHashMap<K, Option<V>>,
    key: &K,
    prepare: impl FnOnce(&mut V),
) {
    if undo.contains_key(key) {
        return;
    }
    match map.remove_entry(key) {
        Some((key, mut old)) => {
            map.insert(key.clone(), old.clone());
            prepare(&mut old);
            undo.insert(key, Some(old));
        }
        None => {
            undo.insert(key.clone(), None);
        }
    }
}

fn restore<K: Eq + Hash, V>(map: &mut FxHashMap<K, V>, undo: FxHashMap<K, Option<V>>) {
    for (key, old) in undo {
        map.remove(&key);
        if let Some(old) = old {
            map.insert(key, old);
        }
    }
}

impl LuaInferCache {
    pub(crate) fn begin_transaction(&mut self) -> CacheTransaction {
        assert!(self.transaction.is_none());
        assert!(current().is_none());
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        self.transaction = Some(Box::new(CacheUndo {
            id,
            calls: FxHashMap::default(),
            flows: FxHashMap::default(),
            keyed_flows: FxHashMap::default(),
            touched_flows: FxHashSet::default(),
        }));
        CacheTransaction {
            previous: CURRENT.with(|current| current.replace(Some(id))),
            marker: PhantomData,
        }
    }

    pub(super) fn active_transaction(&self) -> bool {
        self.transaction
            .as_ref()
            .is_some_and(|undo| Some(undo.id) == current())
    }

    pub(super) fn capture_call(&mut self, key: &CallKey) {
        if self.active_transaction() {
            capture(
                &mut self.call_cache,
                &mut self.transaction.as_mut().unwrap().calls,
                key,
                |_| {},
            );
        }
    }

    pub(super) fn clear_call_cache(&mut self) {
        if self.active_transaction() {
            for key in self.call_cache.keys().copied().collect::<Vec<_>>() {
                self.capture_call(&key);
            }
        }
        self.call_cache.clear();
    }

    pub(super) fn capture_flow(&mut self, key: &VarRefCacheKey) {
        if self.active_transaction() {
            capture(
                &mut self.flow_node_cache,
                &mut self.transaction.as_mut().unwrap().flows,
                key,
                FlowEntries::rollback_transaction,
            );
        }
    }

    pub(super) fn touch_flow(&mut self, key: &VarRefCacheKey) {
        if let Some(undo) = &mut self.transaction {
            undo.touch_flow(key);
        }
    }

    pub(super) fn capture_new_flow(&mut self, key: &VarRefCacheKey) {
        if self.active_transaction() && !self.flow_node_cache.contains_key(key) {
            self.capture_flow(key);
        }
    }

    pub(super) fn capture_keyed_flow(&mut self, key: &KeyedFlowKey) {
        if self.active_transaction() {
            capture(
                &mut self.keyed_flow_node_cache,
                &mut self.transaction.as_mut().unwrap().keyed_flows,
                key,
                FlowEntries::rollback_transaction,
            );
        }
    }

    pub(super) fn clear_flow_cache(&mut self) {
        if self.active_transaction() {
            for key in self.flow_node_cache.keys().cloned().collect::<Vec<_>>() {
                self.capture_flow(&key);
            }
        }
        self.flow_node_cache.clear();
    }

    fn finish_tracked_transaction(&mut self, rollback: bool) {
        macro_rules! finish {
            ($($field:ident),+ $(,)?) => {
                $(if rollback {
                    self.$field.rollback_transaction();
                } else {
                    self.$field.commit_transaction();
                })+
            };
        }
        finish!(
            expr_cache,
            member_query_cache,
            call_arg_types_cache,
            condition_flow_cache,
            flow_node_realm_cache,
            numeric_table_index_query_cache,
            branch_special_call_effect_cache,
            expr_var_ref_id_cache,
            self_type_cache,
            dynamic_field_resolution_cache,
            local_class_table_member_ids_cache,
            dynamic_field_type_cache,
            dynamic_field_definition_syntax_cache,
            call_returns_never_cache
        );
        if let Some(undo) = &self.transaction {
            for key in undo.touched_flows.iter().chain(undo.flows.keys()) {
                if let Some(entries) = self.flow_node_cache.get_mut(key) {
                    if rollback {
                        entries.rollback_transaction();
                    } else {
                        entries.commit_transaction();
                    }
                }
            }
            for key in undo.keyed_flows.keys() {
                if let Some(entries) = self.keyed_flow_node_cache.get_mut(key) {
                    if rollback {
                        entries.rollback_transaction();
                    } else {
                        entries.commit_transaction();
                    }
                }
            }
        }
    }

    pub(crate) fn commit_transaction(&mut self) {
        assert!(current().is_none());
        self.finish_tracked_transaction(false);
        self.transaction = None;
    }

    pub(crate) fn rollback_transaction(&mut self) {
        assert!(current().is_none());
        self.finish_tracked_transaction(true);
        if let Some(undo) = self.transaction.take() {
            restore(&mut self.call_cache, undo.calls);
            restore(&mut self.flow_node_cache, undo.flows);
            restore(&mut self.keyed_flow_node_cache, undo.keyed_flows);
        }
        self.clear();
    }
}

#[cfg(test)]
mod tests;
