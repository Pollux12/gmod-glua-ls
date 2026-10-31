//! Under `GLUALS_PROFILE`: what stored inference memos hold, by map.

use std::sync::Arc;

use glua_parser::LuaSyntaxId;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{CacheEntry, FlowEntries, LuaInferCache, TrackedMap, VarRefCacheKey};
use crate::db_index::derivation::Provenance;
use crate::{LuaFunctionType, LuaType};

type CallMemo = (LuaType, CacheEntry<Arc<LuaFunctionType>>, Provenance);

/// The bytes a hash table of `capacity` entries of `T` takes.
fn table_bytes<T>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = match capacity {
        0..4 => 4,
        4..8 => 8,
        _ => (capacity * 8 / 7).next_power_of_two(),
    };
    buckets * std::mem::size_of::<T>() + buckets + 16
}

/// What a set of memos holds: entries and table bytes per map, and the
/// distinct provenances and logs their entries keep alive.
#[derive(Default)]
pub struct MemoFootprint {
    maps: FxHashMap<&'static str, (usize, usize)>,
    computed: FxHashSet<usize>,
    logs: FxHashSet<usize>,
    computed_bytes: usize,
    log_bytes: usize,
}

impl MemoFootprint {
    fn table(&mut self, name: &'static str, entries: usize, bytes: usize) {
        let tally = self.maps.entry(name).or_default();
        tally.0 += entries;
        tally.1 += bytes;
    }

    fn provenance(&mut self, provenance: &Provenance) {
        let (computed, computed_bytes, log, log_bytes) = provenance.footprint();
        if self.computed.insert(computed) {
            self.computed_bytes += computed_bytes;
        }
        if self.logs.insert(log) {
            self.log_bytes += log_bytes;
        }
    }

    fn tracked<K, V>(&mut self, name: &'static str, map: &TrackedMap<K, V>) {
        let (shared, local) = map.tables();
        self.table(
            name,
            shared.len() + local.len(),
            table_bytes::<(K, (V, Provenance))>(shared.capacity())
                + table_bytes::<(K, V)>(local.capacity()),
        );
        for (_, provenance) in shared.values() {
            self.provenance(provenance);
        }
    }

    /// Adds what `cache` holds.
    pub fn add(&mut self, cache: &LuaInferCache) {
        macro_rules! tracked {
            ($($field:ident),+ $(,)?) => {
                $(self.tracked(stringify!($field), &cache.$field);)+
            };
        }
        tracked!(
            expr_cache,
            member_query_cache,
            call_arg_types_cache,
            condition_flow_cache,
            flow_node_realm_cache,
            numeric_table_index_query_cache,
            branch_special_call_effect_cache,
            constant_expr_cache,
            expr_var_ref_id_cache,
            self_type_cache,
            dynamic_field_resolution_cache,
            exact_table_member_cache,
            local_class_table_member_ids_cache,
            dynamic_field_type_cache,
            dynamic_field_definition_syntax_cache,
            call_returns_never_cache,
            owner_raw_member_type_cache,
            gamemode_class_cache,
        );
        let call_cache = &cache.call_cache;
        self.table(
            "call_cache",
            call_cache.values().map(Vec::len).sum(),
            table_bytes::<((LuaSyntaxId, Option<usize>), Vec<CallMemo>)>(call_cache.capacity())
                + call_cache
                    .values()
                    .map(|bucket| bucket.capacity() * std::mem::size_of::<CallMemo>())
                    .sum::<usize>(),
        );
        for (_, _, provenance) in call_cache.values().flatten() {
            self.provenance(provenance);
        }
        self.table(
            "flow_node_cache (outer)",
            cache.flow_node_cache.len(),
            table_bytes::<(VarRefCacheKey, FlowEntries)>(cache.flow_node_cache.capacity()),
        );
        for entries in cache.flow_node_cache.values() {
            self.tracked("flow_node_cache (entries)", entries);
        }
        self.table(
            "keyed_flow_node_cache (outer)",
            cache.keyed_flow_node_cache.len(),
            table_bytes::<((VarRefCacheKey, LuaType), FlowEntries)>(
                cache.keyed_flow_node_cache.capacity(),
            ),
        );
        for entries in cache.keyed_flow_node_cache.values() {
            self.tracked("keyed_flow_node_cache (entries)", entries);
        }
    }

    /// Prints the tally, largest map first.
    pub fn report(&self, label: &str) {
        let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        let mut maps = self.maps.iter().collect::<Vec<_>>();
        maps.sort_by_key(|(name, (_, bytes))| (std::cmp::Reverse(*bytes), **name));
        let tables = maps.iter().map(|(_, (_, bytes))| bytes).sum::<usize>();
        eprintln!(
            "[mem] {label}: tables {:.1} MiB, {} provenances {:.1} MiB, {} logs {:.1} MiB",
            mib(tables),
            self.computed.len(),
            mib(self.computed_bytes),
            self.logs.len(),
            mib(self.log_bytes),
        );
        for (name, (entries, bytes)) in maps {
            eprintln!("[mem]   {name}: {entries} entries, {:.1} MiB", mib(*bytes));
        }
    }
}
