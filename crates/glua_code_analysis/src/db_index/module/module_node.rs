use rustc_hash::FxHashMap;

use crate::FileId;

#[derive(Debug, Default)]
pub struct ModuleNode {
    pub parent: Option<ModuleNodeId>,
    pub children: FxHashMap<String, ModuleNodeId>,
    pub file_ids: Vec<FileId>,
}

#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub struct ModuleNodeId {
    pub id: u32,
}
