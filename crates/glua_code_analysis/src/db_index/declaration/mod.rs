mod decl;
mod decl_id;
mod decl_tree;
mod scope;

use crate::db_index::cow_shards::{CowCell, CowShards};
pub use decl::LuaDeclExtra;
pub use decl::{LocalAttribute, LuaDecl, LuaDeclInitializer};
pub use decl_id::LuaDeclId;
pub use decl_tree::{LuaDeclOrMemberId, LuaDeclarationTree};
use rustc_hash::FxHashMap;
pub use scope::{LuaScope, LuaScopeId, LuaScopeKind, ScopeOrDeclId};

use crate::FileId;
use crate::db_index::derivation::{self, DerivationId, FactKey, Fingerprint};

use super::traits::LuaIndex;

#[derive(Debug, Clone)]
pub struct LuaDeclIndex {
    decl_trees: CowCell<FxHashMap<FileId, LuaDeclarationTree>>,
    /// Declarations derivations synthesize (the implicit `PANEL` of a vgui
    /// file, a registered panel's global), outside the trees the source
    /// layer builds, with the derivations holding each. A synthesized
    /// declaration leaves with its last holder.
    derived_decls: CowShards<LuaDeclId, (LuaDecl, Vec<DerivationId>)>,
    /// The synthesized declarations each derivation holds.
    derived_decl_ids: CowShards<DerivationId, Vec<LuaDeclId>>,
}

/// Whether `call_expr` calls the `error` intrinsic: the parser marks it as an `error(...)` call
/// and no local `error` is in scope there.
pub fn calls_intrinsic_error(
    db: &crate::DbIndex,
    file_id: FileId,
    call_expr: &glua_parser::LuaCallExpr,
) -> bool {
    use glua_parser::LuaAstNode;
    call_expr.is_error()
        && db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .is_none_or(|tree| {
                tree.find_local_decl("error", call_expr.get_position())
                    .is_none()
            })
}

impl Default for LuaDeclIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaDeclIndex {
    pub fn new() -> Self {
        Self {
            decl_trees: Default::default(),
            derived_decls: Default::default(),
            derived_decl_ids: Default::default(),
        }
    }

    /// Adds a declaration the current derivation synthesized.
    pub(crate) fn add_derived_decl(&mut self, decl: LuaDecl) {
        let Some(holder) = derivation::current() else {
            if let Some(tree) = self.decl_trees.get_mut(&decl.get_file_id()) {
                tree.add_decl(decl);
            }
            return;
        };
        let decl_id = decl.get_id();
        derivation::note_write(|| FactKey::Decl(decl_id));
        derivation::note_write(|| FactKey::DeclContrib(holder));
        self.derived_decl_ids
            .entry(holder)
            .or_default()
            .push(decl_id);
        self.derived_decls
            .entry(decl_id)
            .or_insert_with(|| (decl, Vec::new()))
            .1
            .push(holder);
    }

    /// Withdraws the declarations derivation `id` synthesized, returning the
    /// ones no derivation holds any more.
    pub(crate) fn retract_derived_decls(&mut self, id: &DerivationId) -> Vec<LuaDecl> {
        let mut removed = Vec::new();
        for decl_id in self.derived_decl_ids.remove(id).unwrap_or_default() {
            let Some((_, holders)) = self.derived_decls.get_mut(&decl_id) else {
                continue;
            };
            if let Some(at) = holders.iter().position(|holder| holder == id) {
                holders.remove(at);
            }
            if holders.is_empty() {
                derivation::note_write(|| FactKey::Decl(decl_id));
                if let Some((decl, _)) = self.derived_decls.remove(&decl_id) {
                    removed.push(decl);
                }
            }
        }
        removed
    }

    pub fn add_decl_tree(&mut self, tree: LuaDeclarationTree) {
        let file_id = tree.file_id();
        note_tree_write(file_id, &tree);
        if let Some(old) = self.decl_trees.insert(file_id, tree) {
            note_tree_write(file_id, &old);
        }
    }

    pub fn get_decl_tree(&self, file_id: &FileId) -> Option<&LuaDeclarationTree> {
        derivation::note_file_fact_read(derivation::FileFact::DeclTree, *file_id);
        self.decl_trees.get(file_id)
    }

    pub fn get_decl_tree_mut(&mut self, file_id: &FileId) -> Option<&mut LuaDeclarationTree> {
        derivation::note_file_fact_read(derivation::FileFact::DeclTree, *file_id);
        if self.decl_trees.contains_key(file_id) {
            derivation::note_write(|| FactKey::DeclTree(*file_id));
        }
        self.decl_trees.get_mut(file_id)
    }

    pub fn get_decl(&self, decl_id: &LuaDeclId) -> Option<&LuaDecl> {
        derivation::note_decl_read(*decl_id);
        self.decl_trees
            .get(&decl_id.file_id)
            .and_then(|tree| tree.get_decl(decl_id))
            .or_else(|| self.derived_decls.get(decl_id).map(|(decl, _)| decl))
    }

    pub fn get_decl_mut(&mut self, decl_id: &LuaDeclId) -> Option<&mut LuaDecl> {
        derivation::note_decl_read(*decl_id);
        if let Some(tree) = self.decl_trees.get_mut(&decl_id.file_id)
            && let Some(decl) = tree.get_decl_mut(*decl_id)
        {
            return Some(decl);
        }
        self.derived_decls.get_mut(decl_id).map(|(decl, _)| decl)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Decl(decl_id) => derivation::fingerprint_opt(
                self.decl_trees
                    .get(&decl_id.file_id)
                    .and_then(|tree| tree.get_decl(decl_id))
                    .or_else(|| self.derived_decls.get(decl_id).map(|(decl, _)| decl)),
            ),
            FactKey::DeclContrib(id) => derivation::fingerprint_opt(self.derived_decl_ids.get(id)),
            FactKey::DeclTree(file_id) => self
                .decl_trees
                .get(file_id)
                .map_or(derivation::ABSENT, LuaDeclarationTree::fingerprint),
            _ => {
                debug_assert!(false, "{key:?} is not a declaration fact");
                derivation::ABSENT
            }
        }
    }
}

/// Notes that the tree of `file_id`, and every declaration of `tree`, changed.
fn note_tree_write(file_id: FileId, tree: &LuaDeclarationTree) {
    if !derivation::recording() {
        return;
    }
    derivation::note_write(|| FactKey::DeclTree(file_id));
    for decl_id in tree.decl_ids() {
        derivation::note_write(|| FactKey::Decl(*decl_id));
    }
}

impl LuaIndex for LuaDeclIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(tree) = self.decl_trees.remove(&file_id) {
            note_tree_write(file_id, &tree);
        }
    }

    fn clear(&mut self) {
        for (file_id, tree) in &self.decl_trees {
            note_tree_write(*file_id, tree);
        }
        self.decl_trees.clear();
        self.derived_decls.clear();
        self.derived_decl_ids.clear();
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaDeclIndex {
    decl_trees,
    derived_decls,
    derived_decl_ids,
});
