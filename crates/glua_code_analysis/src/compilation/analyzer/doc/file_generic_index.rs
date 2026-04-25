use std::collections::HashMap;

use rowan::{TextRange, TextSize};

use crate::{GenericParam, GenericTplId, LuaType};

#[derive(Debug, Clone)]
pub struct FileGenericIndex {
    generic_params: Vec<TagGenericParams>,
    root_node_ids: Vec<GenericEffectId>,
    effect_nodes: Vec<GenericEffectRangeNode>,
}

impl FileGenericIndex {
    pub fn new() -> Self {
        Self {
            generic_params: Vec::new(),
            root_node_ids: Vec::new(),
            effect_nodes: Vec::new(),
        }
    }

    pub fn add_generic_scope(&mut self, ranges: Vec<TextRange>, is_func: bool) -> GenericParamId {
        let params_index = self.generic_params.len();
        self.generic_params
            .push(TagGenericParams::new(is_func, params_index as u32));
        let params_id = GenericParamId::new(params_index);
        let root_node_ids: Vec<_> = self.root_node_ids.clone();
        for range in ranges {
            let mut added = false;
            for effect_id in root_node_ids.iter() {
                if self.try_add_range_to_effect_node(range, params_id, *effect_id) {
                    added = true;
                }
            }

            if !added {
                let child_node = GenericEffectRangeNode {
                    range,
                    params_id,
                    children: Vec::new(),
                };

                let child_node_id = self.effect_nodes.len();
                self.effect_nodes.push(child_node);
                self.root_node_ids.push(GenericEffectId::new(child_node_id));
            }
        }

        params_id
    }

    pub fn append_generic_param(
        &mut self,
        scope_id: GenericParamId,
        param: GenericParam,
    ) -> Option<GenericParam> {
        if let Some(scope) = self.generic_params.get_mut(scope_id.id) {
            return Some(scope.insert_param(param));
        }
        None
    }

    pub fn append_generic_params(
        &mut self,
        scope_id: GenericParamId,
        params: Vec<GenericParam>,
    ) -> Vec<GenericParam> {
        let mut assigned_params = Vec::new();
        for param in params {
            if let Some(param) = self.append_generic_param(scope_id, param) {
                assigned_params.push(param);
            }
        }
        assigned_params
    }

    fn try_add_range_to_effect_node(
        &mut self,
        range: TextRange,
        id: GenericParamId,
        effect_id: GenericEffectId,
    ) -> bool {
        let effect_node = match self.effect_nodes.get(effect_id.id) {
            Some(node) => node,
            None => return false,
        };

        if effect_node.range.contains_range(range) {
            let children = effect_node.children.clone();
            for child_effect_id in children {
                if self.try_add_range_to_effect_node(range, id, child_effect_id) {
                    return true;
                }
            }

            let child_node = GenericEffectRangeNode {
                range,
                params_id: id,
                children: Vec::new(),
            };

            let child_node_id = self.effect_nodes.len();
            self.effect_nodes.push(child_node);
            let effect_node = match self.effect_nodes.get_mut(effect_id.id) {
                Some(node) => node,
                None => return false,
            };
            effect_node
                .children
                .push(GenericEffectId::new(child_node_id));
            return true;
        }

        false
    }

    /// Find generic parameter by position and name.
    /// return (GenericTplId, constraint)
    pub fn find_generic(
        &self,
        position: TextSize,
        name: &str,
    ) -> Option<(GenericTplId, Option<LuaType>)> {
        let params_ids = self.find_generic_params(position)?;

        for params_id in params_ids.iter().rev() {
            if let Some(params) = self.generic_params.get(*params_id)
                && let Some((id, param)) = params.params.get(name)
            {
                let tpl_id = param.tpl_id.unwrap_or_else(|| params.tpl_id_for_idx(*id));
                return Some((tpl_id, param.type_constraint.clone()));
            }
        }

        None
    }

    fn find_generic_params(&self, position: TextSize) -> Option<Vec<usize>> {
        for effect_id in self.root_node_ids.iter() {
            if self
                .effect_nodes
                .get(effect_id.id)?
                .range
                .contains(position)
            {
                let mut result = Vec::new();
                self.try_find_generic_params(position, *effect_id, &mut result);
                return Some(result);
            }
        }

        None
    }

    fn try_find_generic_params(
        &self,
        position: TextSize,
        effect_id: GenericEffectId,
        result: &mut Vec<usize>,
    ) -> Option<()> {
        let effect_node = self.effect_nodes.get(effect_id.id)?;
        result.push(effect_node.params_id.id);
        for child_effect_id in effect_node.children.iter() {
            let child_effect_node = self.effect_nodes.get(child_effect_id.id)?;
            if child_effect_node.range.contains(position) {
                self.try_find_generic_params(position, *child_effect_id, result);
            }
        }

        Some(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub struct GenericParamId {
    pub id: usize,
}

impl GenericParamId {
    fn new(id: usize) -> Self {
        Self { id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GenericEffectRangeNode {
    range: TextRange,
    params_id: GenericParamId,
    children: Vec<GenericEffectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
struct GenericEffectId {
    id: usize,
}

impl GenericEffectId {
    fn new(id: usize) -> Self {
        Self { id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagGenericParams {
    params: HashMap<String, (usize, GenericParam)>,
    is_func: bool,
    scope_id: u32,
    next_index: usize,
}

impl TagGenericParams {
    pub fn new(is_func: bool, scope_id: u32) -> Self {
        Self {
            params: HashMap::new(),
            is_func,
            scope_id,
            next_index: 0,
        }
    }

    fn insert_param(&mut self, param: GenericParam) -> GenericParam {
        let current_index = self.next_index;
        self.next_index += 1;
        let param = param.with_tpl_id(self.tpl_id_for_idx(current_index));
        self.params
            .insert(param.name.to_string(), (current_index, param.clone()));
        param
    }

    fn tpl_id_for_idx(&self, idx: usize) -> GenericTplId {
        if self.is_func {
            GenericTplId::Func(idx as u32)
        } else {
            GenericTplId::scoped_type(self.scope_id, idx as u32)
        }
    }
}
