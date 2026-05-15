mod flow_node;
mod flow_tree;
mod signature_cast;

use std::collections::HashMap;

use rowan::TextSize;

use crate::{FileId, LuaSignatureId, LuaType, VarRefId};
pub use flow_node::*;
pub use flow_tree::{BranchLabelInfo, FlowTree};
use glua_parser::{LuaAstPtr, LuaDocOpType};
pub use signature_cast::LuaSignatureCast;

use super::traits::LuaIndex;

#[derive(Debug)]
pub struct LuaFlowIndex {
    file_flow_tree: HashMap<FileId, FlowTree>,
    signature_cast_cache: HashMap<FileId, HashMap<LuaSignatureId, LuaSignatureCast>>,
    special_call_effects: HashMap<FileId, HashMap<TextSize, Vec<LuaSpecialCallEffect>>>,
}

impl Default for LuaFlowIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaFlowIndex {
    pub fn new() -> Self {
        Self {
            file_flow_tree: HashMap::new(),
            signature_cast_cache: HashMap::new(),
            special_call_effects: HashMap::new(),
        }
    }

    pub fn add_flow_tree(&mut self, file_id: FileId, flow_tree: FlowTree) {
        self.file_flow_tree.insert(file_id, flow_tree);
    }

    pub fn get_flow_tree(&self, file_id: &FileId) -> Option<&FlowTree> {
        self.file_flow_tree.get(file_id)
    }

    pub fn get_signature_cast(&self, signature_id: &LuaSignatureId) -> Option<&LuaSignatureCast> {
        self.signature_cast_cache
            .get(&signature_id.get_file_id())?
            .get(signature_id)
    }

    pub fn add_signature_cast(
        &mut self,
        file_id: FileId,
        signature_id: LuaSignatureId,
        name: String,
        cast: LuaAstPtr<LuaDocOpType>,
        fallback_cast: Option<LuaAstPtr<LuaDocOpType>>,
    ) {
        self.signature_cast_cache
            .entry(file_id)
            .or_default()
            .insert(
                signature_id,
                LuaSignatureCast {
                    name,
                    cast,
                    fallback_cast,
                },
            );
    }

    pub fn add_special_call_effect(
        &mut self,
        file_id: FileId,
        position: TextSize,
        target: VarRefId,
        type_ref: LuaType,
    ) {
        self.special_call_effects
            .entry(file_id)
            .or_default()
            .entry(position)
            .or_default()
            .push(LuaSpecialCallEffect { target, type_ref });
    }

    pub fn get_special_call_effects(
        &self,
        file_id: &FileId,
        position: TextSize,
    ) -> Option<&[LuaSpecialCallEffect]> {
        self.special_call_effects
            .get(file_id)?
            .get(&position)
            .map(|effects| effects.as_slice())
    }
}

impl LuaIndex for LuaFlowIndex {
    fn remove(&mut self, file_id: FileId) {
        self.file_flow_tree.remove(&file_id);
        self.signature_cast_cache.remove(&file_id);
        self.special_call_effects.remove(&file_id);
    }

    fn clear(&mut self) {
        self.file_flow_tree.clear();
        self.signature_cast_cache.clear();
        self.special_call_effects.clear();
    }
}

#[derive(Debug, Clone)]
pub struct LuaSpecialCallEffect {
    pub target: VarRefId,
    pub type_ref: LuaType,
}
