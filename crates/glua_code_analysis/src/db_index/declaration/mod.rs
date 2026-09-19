mod decl;
mod decl_id;
mod decl_tree;
mod scope;

pub use decl::LuaDeclExtra;
pub use decl::{LocalAttribute, LuaDecl, LuaDeclInitializer};
pub use decl_id::LuaDeclId;
pub use decl_tree::{LuaDeclOrMemberId, LuaDeclarationTree};
use rustc_hash::FxHashMap;
pub use scope::{LuaScope, LuaScopeId, LuaScopeKind, ScopeOrDeclId};

use crate::FileId;
use crate::db_index::op_log::{self, MutRef};
use crate::db_index::read_set::WriteKey;

use super::traits::LuaIndex;

#[derive(Debug, Clone)]
pub struct LuaDeclIndex {
    decl_trees: FxHashMap<FileId, LuaDeclarationTree>,
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
        }
    }

    pub fn add_decl_tree(&mut self, tree: LuaDeclarationTree) {
        if op_log::recording() {
            let file_id = tree.file_id();
            let replay = tree.clone();
            op_log::push(
                op_log::Domain::Decl,
                Box::new([WriteKey::FileScoped(file_id)]),
                move |db| db.get_decl_index_mut().add_decl_tree(replay),
            );
        }
        self.decl_trees.insert(tree.file_id(), tree);
    }

    pub fn get_decl_tree(&self, file_id: &FileId) -> Option<&LuaDeclarationTree> {
        self.decl_trees.get(file_id)
    }

    pub fn get_decl_tree_mut(&mut self, file_id: &FileId) -> Option<&mut LuaDeclarationTree> {
        op_log::note_mut_ref(MutRef::DeclTree(*file_id));
        crate::db_index::read_set::record_file_scoped(file_id);
        self.decl_trees.get_mut(file_id)
    }

    pub fn get_decl(&self, decl_id: &LuaDeclId) -> Option<&LuaDecl> {
        let tree = self.decl_trees.get(&decl_id.file_id)?;
        tree.get_decl(decl_id)
    }

    pub fn get_decl_mut(&mut self, decl_id: &LuaDeclId) -> Option<&mut LuaDecl> {
        op_log::note_mut_ref(MutRef::Decl(*decl_id));
        crate::db_index::read_set::record_decl_misc(decl_id);
        let tree = self.decl_trees.get_mut(&decl_id.file_id)?;
        tree.get_decl_mut(*decl_id)
    }

    /// Boundary replace for the parallel lua walk: installs the fork's final
    /// `LuaDecl` under the same id through `add_decl`, so module-scoped name
    /// bookkeeping stays consistent. Runs only on the merge thread.
    pub(crate) fn replace_decl_object(&mut self, decl_id: &LuaDeclId, decl: LuaDecl) {
        if let Some(tree) = self.decl_trees.get_mut(&decl_id.file_id) {
            tree.add_decl(decl);
        }
    }
}

impl LuaIndex for LuaDeclIndex {
    fn remove(&mut self, file_id: FileId) {
        self.decl_trees.remove(&file_id);
    }

    fn clear(&mut self) {
        self.decl_trees.clear();
    }
}
