use rustc_hash::FxHashMap;

use crate::{
    DbIndex, Emmyrc, FileId, InferFailReason, LuaMemberId, LuaMemberIndexItem, LuaType, LuaTypeFact,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeCheckCheckLevel {
    Normal,
    GenericConditional,
}

#[derive(Debug, Clone)]
pub struct TypeCheckContext<'db> {
    pub detail: bool,
    pub db: &'db DbIndex,
    /// The config the check reads its strictness from: the checked file's, or
    /// else the shared one.
    pub emmyrc: &'db Emmyrc,
    /// The file the check is for, when known.
    pub file_id: Option<FileId>,
    pub level: TypeCheckCheckLevel,
    member_facts: FxHashMap<LuaMemberId, LuaTypeFact>,
}

impl<'db> TypeCheckContext<'db> {
    pub fn new(db: &'db DbIndex, detail: bool, level: TypeCheckCheckLevel) -> Self {
        Self {
            detail,
            db,
            emmyrc: db.get_emmyrc(),
            file_id: None,
            level,
            member_facts: FxHashMap::default(),
        }
    }

    /// Checks with the config of `file_id`.
    pub fn in_file(mut self, file_id: FileId) -> Self {
        self.emmyrc = self.db.get_file_emmyrc(file_id);
        self.file_id = Some(file_id);
        self
    }

    pub fn with_member_facts(mut self, member_facts: FxHashMap<LuaMemberId, LuaTypeFact>) -> Self {
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
