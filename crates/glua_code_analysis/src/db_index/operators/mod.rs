mod lua_operator;
mod lua_operator_meta_method;

use std::sync::Arc;

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
    /// Each operator file's `Vfs::file_order_key`, which orders an owner's
    /// operators: file ids follow load history, and readers take the first.
    file_order: CowShards<FileId, Arc<str>>,
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
            file_order: Default::default(),
            holders: Default::default(),
            derived: Default::default(),
        }
    }

    /// Installs `operator`, reporting whether it was new. Re-settling an
    /// identical operator moves nothing observable. `file_order_key` is the
    /// operator file's `Vfs::file_order_key`.
    pub fn add_operator(&mut self, operator: LuaOperator, file_order_key: Arc<str>) -> bool {
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
        self.file_order.entry(id.file_id).or_insert(file_order_key);
        let file_order = &self.file_order;
        let ids = self
            .type_operators_map
            .entry(owner.clone())
            .or_default()
            .entry(op)
            .or_default();
        if let Err(at) = ids.binary_search_by(|existing| {
            operator_sort_key(file_order, existing).cmp(&operator_sort_key(file_order, &id))
        }) {
            ids.insert(at, id);
        }
        let ids = self.in_filed_operator_map.entry(id.file_id).or_default();
        if let Err(at) = ids.binary_search_by_key(&id.position, |existing| existing.position) {
            ids.insert(at, id);
        }
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
                self.file_order.remove(&id.file_id);
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

    /// Files each of `order_keys`' files under its new order key: its
    /// operators take the place in each owner's list that the key gives them.
    pub(crate) fn rekey_files(&mut self, order_keys: &[(FileId, Arc<str>)]) {
        for (file_id, order_key) in order_keys {
            if self
                .file_order
                .get(file_id)
                .is_none_or(|held| held == order_key)
            {
                continue;
            }
            self.file_order.insert(*file_id, order_key.clone());
            let mut owners = Vec::new();
            for operator in self
                .in_filed_operator_map
                .get(file_id)
                .into_iter()
                .flatten()
                .filter_map(|id| self.operators.get(id))
            {
                let owner = (operator.get_owner().clone(), operator.get_op());
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
            let file_order = &self.file_order;
            for (owner, op) in owners {
                let Some(ids) = self
                    .type_operators_map
                    .get_mut(&owner)
                    .and_then(|map| map.get_mut(&op))
                else {
                    continue;
                };
                ids.sort_by(|left, right| {
                    operator_sort_key(file_order, left).cmp(&operator_sort_key(file_order, right))
                });
                derivation::note_write(|| FactKey::OperatorOwner(owner.clone()));
            }
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

/// Canonical order for an owner's operators: source position, with the
/// file's path as the outer key.
fn operator_sort_key<'a>(
    file_order: &'a CowShards<FileId, Arc<str>>,
    id: &LuaOperatorId,
) -> (&'a str, u32, u32) {
    (
        file_order.get(&id.file_id).map_or("", |key| key),
        u32::from(id.position),
        id.file_id.id,
    )
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
        self.file_order.clear();
        self.holders.clear();
        self.derived.clear();
    }
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};

    use super::*;

    fn call_operator(file_id: FileId) -> LuaOperator {
        LuaOperator::new(
            LuaOperatorOwner::Type(crate::LuaTypeDeclId::global("Foo")),
            LuaOperatorMetaMethod::Call,
            file_id,
            TextRange::new(TextSize::new(4), TextSize::new(8)),
            OperatorFunction::DefaultClassCtor {
                id: crate::LuaSignatureId::new(file_id, TextSize::new(4)),
                strip_self: false,
                return_self: false,
            },
        )
    }

    fn first_call_file(index: &LuaOperatorIndex) -> Option<FileId> {
        index
            .get_operators(
                &LuaOperatorOwner::Type(crate::LuaTypeDeclId::global("Foo")),
                LuaOperatorMetaMethod::Call,
            )?
            .first()
            .map(|id| id.file_id)
    }

    /// An owner's operators are ordered by their files' paths, not by the
    /// ids load history gave the files, and follow a file that moves.
    #[test]
    fn an_owners_operators_follow_their_files_paths() {
        let (loaded_first, loaded_later) = (FileId::new(1), FileId::new(2));
        let mut index = LuaOperatorIndex::new();
        index.add_operator(call_operator(loaded_first), Arc::from("b.lua"));
        index.add_operator(call_operator(loaded_later), Arc::from("a.lua"));
        assert_eq!(first_call_file(&index), Some(loaded_later));

        index.rekey_files(&[(loaded_later, Arc::from("c.lua"))]);

        assert_eq!(first_call_file(&index), Some(loaded_first));
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaOperatorIndex {
    operators,
    type_operators_map,
    in_filed_operator_map,
    file_order,
    holders,
    derived,
});
