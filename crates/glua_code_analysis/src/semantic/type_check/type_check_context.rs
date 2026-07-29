use std::collections::HashMap;

use crate::{DbIndex, InferFailReason, LuaMemberId, LuaMemberIndexItem, LuaType, LuaTypeFact};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeCheckCheckLevel {
    Normal,
    GenericConditional,
}

#[derive(Debug, Clone)]
pub struct TypeCheckContext<'db> {
    pub detail: bool,
    pub db: &'db DbIndex,
    pub level: TypeCheckCheckLevel,
    member_facts: HashMap<LuaMemberId, LuaTypeFact>,
}

impl<'db> TypeCheckContext<'db> {
    pub fn new(db: &'db DbIndex, detail: bool, level: TypeCheckCheckLevel) -> Self {
        Self {
            detail,
            db,
            level,
            member_facts: HashMap::new(),
        }
    }

    pub fn with_member_facts(mut self, member_facts: HashMap<LuaMemberId, LuaTypeFact>) -> Self {
        self.member_facts = member_facts;
        self
    }

    pub fn member_type(&self, member_id: LuaMemberId) -> Option<LuaType> {
        self.member_facts
            .get(&member_id)
            .cloned()
            .or_else(|| self.db.get_type_index().get_type_fact(&member_id.into()))
            .map(|fact| fact.typ().clone())
    }

    pub fn resolve_member_item_type(
        &self,
        member_item: &LuaMemberIndexItem,
    ) -> Result<LuaType, InferFailReason> {
        if let LuaMemberIndexItem::One(member_id) = member_item
            && let Some(fact) = self.member_facts.get(member_id)
        {
            return Ok(fact.typ().clone());
        }

        member_item.resolve_type(self.db)
    }
}
