mod lua_operator;
mod lua_operator_meta_method;

use crate::db_index::cow_shards::CowShards;
use rustc_hash::FxHashMap;

use crate::FileId;
use crate::db_index::derivation::{self, DerivationId, FactKey, Fingerprint};

use super::traits::LuaIndex;
pub use lua_operator::{LuaOperator, LuaOperatorId, LuaOperatorOwner, OperatorFunction};
pub use lua_operator_meta_method::LuaOperatorMetaMethod;

#[derive(Debug, Clone)]
pub struct LuaOperatorIndex {
    operators: CowShards<LuaOperatorId, LuaOperator>,
    type_operators_map:
        CowShards<LuaOperatorOwner, FxHashMap<LuaOperatorMetaMethod, Vec<LuaOperatorId>>>,
    in_filed_operator_map: CowShards<FileId, Vec<LuaOperatorId>>,
    /// Who installed each operator: `None` for the source layer, else the
    /// derivation. An operator leaves with its last holder.
    holders: CowShards<LuaOperatorId, Vec<Option<DerivationId>>>,
    /// The operators each derivation installed, retracted before it re-runs.
    derived: CowShards<DerivationId, Vec<LuaOperatorId>>,
}

impl Default for LuaOperatorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaOperatorIndex {
    pub fn new() -> Self {
        Self {
            operators: Default::default(),
            type_operators_map: Default::default(),
            in_filed_operator_map: Default::default(),
            holders: Default::default(),
            derived: Default::default(),
        }
    }

    /// Installs `operator`, reporting whether it was new. Re-settling an
    /// identical operator moves nothing observable.
    pub fn add_operator(&mut self, operator: LuaOperator) -> bool {
        let id = operator.get_id();
        let holder = if derivation::writes_source() {
            None
        } else {
            derivation::current()
        };
        if let Some(derivation_id) = holder {
            derivation::note_write(|| FactKey::OperatorContrib(derivation_id));
            self.derived.entry(derivation_id).or_default().push(id);
        }
        self.holders.entry(id).or_default().push(holder);
        if self.operators.contains_key(&id) {
            return false;
        }
        let owner = operator.get_owner().clone();
        let op = operator.get_op();
        self.operators.insert(id, operator);
        insert_sorted(
            self.type_operators_map
                .entry(owner.clone())
                .or_default()
                .entry(op)
                .or_default(),
            id,
        );
        insert_sorted(
            self.in_filed_operator_map.entry(id.file_id).or_default(),
            id,
        );
        derivation::note_write(|| FactKey::OperatorOwner(owner.clone()));
        derivation::note_write(|| FactKey::OperatorFile(id.file_id));
        true
    }

    /// Withdraws the operators derivation `id` installed, before it runs
    /// again.
    pub(crate) fn retract_contributions(&mut self, id: &DerivationId) {
        for operator_id in self.derived.remove(id).unwrap_or_default() {
            self.release(operator_id, Some(*id));
        }
    }

    /// Drops one hold on an operator, removing it with its last holder.
    fn release(&mut self, id: LuaOperatorId, holder: Option<DerivationId>) {
        let Some(holders) = self.holders.get_mut(&id) else {
            return;
        };
        let Some(at) = holders.iter().position(|existing| *existing == holder) else {
            return;
        };
        holders.remove(at);
        if !holders.is_empty() {
            return;
        }
        self.holders.remove(&id);
        self.remove_operator(id);
    }

    fn remove_operator(&mut self, id: LuaOperatorId) {
        let Some(operator) = self.operators.remove(&id) else {
            return;
        };
        derivation::note_write(|| FactKey::OperatorFile(id.file_id));
        if let Some(ids) = self.in_filed_operator_map.get_mut(&id.file_id) {
            ids.retain(|existing| *existing != id);
            if ids.is_empty() {
                self.in_filed_operator_map.remove(&id.file_id);
            }
        }
        let owner = operator.get_owner();
        derivation::note_write(|| FactKey::OperatorOwner(owner.clone()));
        let Some(operators_map) = self.type_operators_map.get_mut(owner) else {
            return;
        };
        if let Some(operators) = operators_map.get_mut(&operator.get_op()) {
            operators.retain(|existing| *existing != id);
            if operators.is_empty() {
                operators_map.remove(&operator.get_op());
            }
        }
        if operators_map.is_empty() {
            self.type_operators_map.remove(owner);
        }
    }

    pub fn get_operators(
        &self,
        owner: &LuaOperatorOwner,
        meta_method: LuaOperatorMetaMethod,
    ) -> Option<&Vec<LuaOperatorId>> {
        derivation::note_read(|| FactKey::OperatorOwner(owner.clone()));
        self.type_operators_map
            .get(owner)
            .and_then(|map| map.get(&meta_method))
    }

    /// Every metamethod this file declares. Another file's inference reads
    /// them whenever it applies an operator to the owning type.
    pub fn operators_in_file(&self, file_id: FileId) -> Vec<&LuaOperator> {
        derivation::note_read(|| FactKey::OperatorFile(file_id));
        self.in_filed_operator_map
            .get(&file_id)
            .map(|ids| ids.iter().filter_map(|id| self.get_operator(id)).collect())
            .unwrap_or_default()
    }
    pub fn get_operator(&self, id: &LuaOperatorId) -> Option<&LuaOperator> {
        derivation::note_read(|| FactKey::OperatorFile(id.file_id));
        self.operators.get(id)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::OperatorOwner(owner) => {
                self.type_operators_map
                    .get(owner)
                    .map_or(derivation::ABSENT, |table| {
                        derivation::fingerprint_of(
                            &table
                                .iter()
                                .map(|entry| derivation::fingerprint_of(&entry))
                                .fold(0u64, u64::wrapping_add),
                        )
                    })
            }
            FactKey::OperatorContrib(id) => derivation::fingerprint_opt(self.derived.get(id)),
            FactKey::OperatorFile(file_id) => {
                self.in_filed_operator_map
                    .get(file_id)
                    .map_or(derivation::ABSENT, |ids| {
                        derivation::fingerprint_of(
                            &ids.iter()
                                .map(|id| self.operators.get(id))
                                .collect::<Vec<_>>(),
                        )
                    })
            }
            _ => {
                debug_assert!(false, "{key:?} is not an operator fact");
                derivation::ABSENT
            }
        }
    }
}

/// Files `id` in canonical order unless the list already holds it.
fn insert_sorted(ids: &mut Vec<LuaOperatorId>, id: LuaOperatorId) {
    let key = |id: &LuaOperatorId| (id.file_id, id.position);
    if let Err(at) = ids.binary_search_by_key(&key(&id), key) {
        ids.insert(at, id);
    }
}

impl LuaIndex for LuaOperatorIndex {
    fn remove(&mut self, file_id: FileId) {
        let operator_ids = self
            .in_filed_operator_map
            .get(&file_id)
            .cloned()
            .unwrap_or_default();
        for id in operator_ids {
            while self
                .holders
                .get(&id)
                .is_some_and(|holders| holders.contains(&None))
            {
                self.release(id, None);
            }
        }
    }

    fn clear(&mut self) {
        for file_id in self.in_filed_operator_map.keys() {
            derivation::note_write(|| FactKey::OperatorFile(*file_id));
        }
        for owner in self.type_operators_map.keys() {
            derivation::note_write(|| FactKey::OperatorOwner(owner.clone()));
        }
        self.operators.clear();
        self.type_operators_map.clear();
        self.in_filed_operator_map.clear();
        self.holders.clear();
        self.derived.clear();
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaOperatorIndex {
    operators,
    type_operators_map,
    in_filed_operator_map,
    holders,
    derived,
});
