mod lua_operator;
mod lua_operator_meta_method;

use rustc_hash::FxHashMap;

use crate::FileId;
use crate::db_index::op_log;
use crate::db_index::read_set::{KeyedEpochs, WriteKey};

use super::traits::LuaIndex;
pub use lua_operator::{LuaOperator, LuaOperatorId, LuaOperatorOwner, OperatorFunction};
pub use lua_operator_meta_method::LuaOperatorMetaMethod;

#[derive(Debug, Clone)]
pub struct LuaOperatorIndex {
    operators: FxHashMap<LuaOperatorId, LuaOperator>,
    type_operators_map:
        FxHashMap<LuaOperatorOwner, FxHashMap<LuaOperatorMetaMethod, Vec<LuaOperatorId>>>,
    in_filed_operator_map: FxHashMap<FileId, Vec<LuaOperatorId>>,
    /// Write epochs for `in_filed_operator_map`/`operators` —
    /// `FileScopedKind::OperatorFile`.
    pub(crate) file_write_epochs: KeyedEpochs<FileId>,
    /// Write epochs for the operator tables, stamped per operand owner so a
    /// write under one owner stales only readers of that owner.
    pub(crate) operator_epochs: KeyedEpochs<LuaOperatorOwner>,
}

impl Default for LuaOperatorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaOperatorIndex {
    pub fn new() -> Self {
        Self {
            operators: FxHashMap::default(),
            type_operators_map: FxHashMap::default(),
            in_filed_operator_map: FxHashMap::default(),
            file_write_epochs: KeyedEpochs::default(),
            operator_epochs: KeyedEpochs::default(),
        }
    }

    /// Installs `operator`, reporting whether it was new. Re-settling an
    /// identical operator moves nothing observable: only a novel install may
    /// bump a write epoch, or the fixpoint's staleness filter degrades to a
    /// full requeue.
    pub fn add_operator(&mut self, operator: LuaOperator) -> bool {
        if op_log::recording() {
            let operator2 = operator.clone();
            op_log::push(
                op_log::Domain::Operator,
                Box::new([
                    WriteKey::OperatorOwner(operator.get_owner().clone()),
                    WriteKey::FileScoped(operator.get_id().file_id),
                ]),
                move |db| {
                    db.get_operator_index_mut().add_operator(operator2);
                },
            );
        }
        let id = operator.get_id();
        if self.operators.contains_key(&id) {
            return false;
        }
        let owner = operator.get_owner().clone();
        let op = operator.get_op();
        self.operators.insert(id, operator);
        self.type_operators_map
            .entry(owner.clone())
            .or_default()
            .entry(op)
            .or_default()
            .push(id);
        self.in_filed_operator_map
            .entry(id.file_id)
            .or_default()
            .push(id);
        self.operator_epochs.note(owner);
        self.file_write_epochs.note(id.file_id);
        true
    }

    pub fn get_operators(
        &self,
        owner: &LuaOperatorOwner,
        meta_method: LuaOperatorMetaMethod,
    ) -> Option<&Vec<LuaOperatorId>> {
        crate::db_index::read_set::record_operator_owner(&self.operator_epochs, owner);
        self.type_operators_map
            .get(owner)
            .and_then(|map| map.get(&meta_method))
    }

    /// Every metamethod this file declares. Another file's inference reads
    /// them whenever it applies an operator to the owning type.
    pub fn operators_in_file(&self, file_id: FileId) -> Vec<&LuaOperator> {
        crate::db_index::read_set::record_file_scoped(
            &self.file_write_epochs,
            &file_id,
            crate::db_index::read_set::FileScopedKind::OperatorFile,
        );
        self.in_filed_operator_map
            .get(&file_id)
            .map(|ids| ids.iter().filter_map(|id| self.get_operator(id)).collect())
            .unwrap_or_default()
    }
    pub fn get_operator(&self, id: &LuaOperatorId) -> Option<&LuaOperator> {
        crate::db_index::read_set::record_file_scoped(
            &self.file_write_epochs,
            &id.file_id,
            crate::db_index::read_set::FileScopedKind::OperatorFile,
        );
        self.operators.get(id)
    }
}

impl LuaIndex for LuaOperatorIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(operator_ids) = self.in_filed_operator_map.remove(&file_id) {
            self.file_write_epochs.note(file_id);
            for id in operator_ids {
                if let Some(operator) = self.operators.remove(&id) {
                    let owner = operator.get_owner();
                    let op = operator.get_op();
                    let operators_map = match self.type_operators_map.get_mut(owner) {
                        Some(map) => map,
                        None => continue,
                    };
                    let operators = match operators_map.get_mut(&op) {
                        Some(operators) => operators,
                        None => continue,
                    };
                    operators.retain(|x| x != &id);
                    if operators.is_empty() {
                        operators_map.remove(&op);
                    }

                    if operators_map.is_empty() {
                        self.type_operators_map.remove(owner);
                    }
                    self.operator_epochs.note(owner.clone());
                }
            }
        }
    }

    fn clear(&mut self) {
        for file_id in self.in_filed_operator_map.keys() {
            self.file_write_epochs.note(*file_id);
        }
        self.operator_epochs.clear();
        self.operators.clear();
        self.type_operators_map.clear();
        self.in_filed_operator_map.clear();
    }
}
