mod decl;
mod decl_id;
mod decl_tree;
mod scope;

pub use decl::LuaDeclExtra;
pub use decl::{LocalAttribute, LuaDecl, LuaDeclInitializer};
pub use decl_id::LuaDeclId;
pub use decl_tree::{LuaDeclOrMemberId, LuaDeclarationTree};
use rowan::TextRange;
pub use scope::{LuaScope, LuaScopeId, LuaScopeKind, ScopeOrDeclId};
use rustc_hash::FxHashMap;

use crate::{FileId, LuaMemberId};

use super::traits::LuaIndex;

#[derive(Debug)]
pub struct LuaDeclIndex {
    decl_trees: FxHashMap<FileId, LuaDeclarationTree>,
    /// The table literal a global declaration is written with — the `{}` of
    /// `X = {}` or of the GLua-idiomatic `X = X or {}`.
    global_initializer_tables: FxHashMap<LuaDeclId, TextRange>,
    /// The same fact for a *nested* global path: the `{}` of `X.k = {}` or
    /// of `X.k = X.k or {}`, keyed by the member that declares it.
    global_member_initializer_tables: FxHashMap<LuaMemberId, TextRange>,
}

impl Default for LuaDeclIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaDeclIndex {
    pub fn new() -> Self {
        Self {
            decl_trees: FxHashMap::default(),
            global_initializer_tables: FxHashMap::default(),
            global_member_initializer_tables: FxHashMap::default(),
        }
    }

    pub fn set_global_initializer_table(&mut self, decl_id: LuaDeclId, range: TextRange) {
        self.global_initializer_tables.insert(decl_id, range);
    }

    pub fn get_global_initializer_table(&self, decl_id: &LuaDeclId) -> Option<TextRange> {
        self.global_initializer_tables.get(decl_id).copied()
    }

    pub fn set_global_member_initializer_table(
        &mut self,
        member_id: LuaMemberId,
        range: TextRange,
    ) {
        self.global_member_initializer_tables
            .insert(member_id, range);
    }

    pub fn get_global_member_initializer_table(
        &self,
        member_id: &LuaMemberId,
    ) -> Option<TextRange> {
        self.global_member_initializer_tables
            .get(member_id)
            .copied()
    }

    pub fn add_decl_tree(&mut self, tree: LuaDeclarationTree) {
        self.decl_trees.insert(tree.file_id(), tree);
    }

    pub fn get_decl_tree(&self, file_id: &FileId) -> Option<&LuaDeclarationTree> {
        self.decl_trees.get(file_id)
    }

    pub fn get_decl_tree_mut(&mut self, file_id: &FileId) -> Option<&mut LuaDeclarationTree> {
        self.decl_trees.get_mut(file_id)
    }

    pub fn get_decl(&self, decl_id: &LuaDeclId) -> Option<&LuaDecl> {
        let tree = self.decl_trees.get(&decl_id.file_id)?;
        tree.get_decl(decl_id)
    }

    pub fn get_decl_mut(&mut self, decl_id: &LuaDeclId) -> Option<&mut LuaDecl> {
        let tree = self.decl_trees.get_mut(&decl_id.file_id)?;
        tree.get_decl_mut(*decl_id)
    }
}

impl LuaIndex for LuaDeclIndex {
    fn remove(&mut self, file_id: FileId) {
        self.decl_trees.remove(&file_id);
        self.global_initializer_tables
            .retain(|decl_id, _| decl_id.file_id != file_id);
        self.global_member_initializer_tables
            .retain(|member_id, _| member_id.file_id != file_id);
    }

    fn clear(&mut self) {
        self.decl_trees.clear();
        self.global_initializer_tables.clear();
        self.global_member_initializer_tables.clear();
    }
}
