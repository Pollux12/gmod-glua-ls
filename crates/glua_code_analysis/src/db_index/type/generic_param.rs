use smol_str::SmolStr;

use crate::{GenericTplId, LuaAttributeUse, LuaType};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GenericParam {
    pub name: SmolStr,
    pub type_constraint: Option<LuaType>,
    pub attributes: Option<Vec<LuaAttributeUse>>,
    pub tpl_id: Option<GenericTplId>,
}

impl GenericParam {
    pub fn new(
        name: SmolStr,
        type_constraint: Option<LuaType>,
        attributes: Option<Vec<LuaAttributeUse>>,
    ) -> Self {
        Self {
            name,
            type_constraint,
            attributes,
            tpl_id: None,
        }
    }

    pub fn with_tpl_id(mut self, tpl_id: GenericTplId) -> Self {
        self.tpl_id = Some(tpl_id);
        self
    }
}
