mod lua_operator;
mod lua_operator_meta_method;

use rustc_hash::FxHashMap;

use crate::FileId;

use super::traits::LuaIndex;
pub use lua_operator::{LuaOperator, LuaOperatorId, LuaOperatorOwner, OperatorFunction};
pub use lua_operator_meta_method::LuaOperatorMetaMethod;

#[derive(Debug)]
pub struct LuaOperatorIndex {
    operators: FxHashMap<LuaOperatorId, LuaOperator>,
    type_operators_map:
        FxHashMap<LuaOperatorOwner, FxHashMap<LuaOperatorMetaMethod, Vec<LuaOperatorId>>>,
    in_filed_operator_map: FxHashMap<FileId, Vec<LuaOperatorId>>,
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
        }
    }

    pub fn add_operator(&mut self, operator: LuaOperator) {
        let id = operator.get_id();
        let owner = operator.get_owner().clone();
        let op = operator.get_op();
        self.operators.insert(id, operator);
        self.type_operators_map
            .entry(owner)
            .or_default()
            .entry(op)
            .or_default()
            .push(id);
        self.in_filed_operator_map
            .entry(id.file_id)
            .or_default()
            .push(id);
    }

    pub fn get_operators(
        &self,
        owner: &LuaOperatorOwner,
        meta_method: LuaOperatorMetaMethod,
    ) -> Option<&Vec<LuaOperatorId>> {
        self.type_operators_map
            .get(owner)
            .and_then(|map| map.get(&meta_method))
    }

    pub fn get_operator(&self, id: &LuaOperatorId) -> Option<&LuaOperator> {
        self.operators.get(id)
    }
}

impl LuaIndex for LuaOperatorIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(operator_ids) = self.in_filed_operator_map.remove(&file_id) {
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
                }
            }
        }
    }

    fn clear(&mut self) {
        self.operators.clear();
        self.type_operators_map.clear();
        self.in_filed_operator_map.clear();
    }
}
