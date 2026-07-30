use rowan::TextRange;
use std::collections::HashMap;

use crate::db_index::LuaDeclId;
use glua_parser::LuaSyntaxId;

#[derive(Debug)]
pub struct FileReference {
    decl_references: HashMap<LuaDeclId, DeclReference>,
    references_to_decl: HashMap<TextRange, LuaDeclId>,
    direct_index_uses: HashMap<LuaDeclId, Vec<DeclIndexUse>>,
}

impl Default for FileReference {
    fn default() -> Self {
        Self::new()
    }
}

impl FileReference {
    pub fn new() -> Self {
        Self {
            decl_references: HashMap::new(),
            references_to_decl: HashMap::new(),
            direct_index_uses: HashMap::new(),
        }
    }

    pub fn add_decl_reference(
        &mut self,
        decl_id: LuaDeclId,
        range: TextRange,
        is_write: bool,
    ) -> bool {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.references_to_decl.entry(range)
        {
            entry.insert(decl_id);
        } else {
            return false;
        }

        let decl_ref = DeclReferenceCell { range, is_write };

        self.decl_references
            .entry(decl_id)
            .or_default()
            .add_cell(decl_ref);
        true
    }

    pub fn get_decl_references(&self, decl_id: &LuaDeclId) -> Option<&DeclReference> {
        self.decl_references.get(decl_id)
    }

    pub fn get_decl_references_map(&self) -> &HashMap<LuaDeclId, DeclReference> {
        &self.decl_references
    }

    pub fn get_decl_id(&self, range: &TextRange) -> Option<LuaDeclId> {
        self.references_to_decl.get(range).copied()
    }

    pub fn add_direct_index_use(&mut self, decl_id: LuaDeclId, use_site: DeclIndexUse) {
        self.direct_index_uses
            .entry(decl_id)
            .or_default()
            .push(use_site);
    }

    pub fn get_direct_index_uses(&self) -> &HashMap<LuaDeclId, Vec<DeclIndexUse>> {
        &self.direct_index_uses
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct DeclIndexUse {
    pub index_expr_id: LuaSyntaxId,
    pub is_inside_return: bool,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct DeclReferenceCell {
    pub range: TextRange,
    pub is_write: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeclReference {
    pub cells: Vec<DeclReferenceCell>,
    pub mutable: bool,
}

impl Default for DeclReference {
    fn default() -> Self {
        Self::new()
    }
}

impl DeclReference {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            mutable: false,
        }
    }

    pub fn add_cell(&mut self, cell: DeclReferenceCell) {
        if cell.is_write {
            self.mutable = true;
        }

        self.cells.push(cell);
    }
}
