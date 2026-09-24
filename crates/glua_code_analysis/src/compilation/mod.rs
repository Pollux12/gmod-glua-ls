pub(crate) mod analyzer;
mod test;

pub use analyzer::gmod::get_scripted_class_info_for_file;
pub(crate) use analyzer::gmod::get_scripted_class_type_decl_id;
pub use analyzer::is_member_assignment_in_conditional_branch;
pub use analyzer::unresolve::extract_hook_name;
pub use analyzer::unresolve::resolve_gmod_hook_add_callback_doc_function;
pub use analyzer::unresolve::resolve_gmod_hook_callback_doc_function;

use std::sync::Arc;

use crate::{
    Emmyrc, FileId, InFiled, LuaIndex, LuaInferCache, db_index::DbIndex, semantic::SemanticModel,
};

#[derive(Debug)]
pub struct LuaCompilation {
    db: DbIndex,
    emmyrc: Arc<Emmyrc>,
    /// Every `update_index` batch since [`Self::start_batch_log`], for the
    /// determinism harness's replay.
    batch_log: Option<Vec<Vec<FileId>>>,
}

impl LuaCompilation {
    pub fn new(emmyrc: Arc<Emmyrc>) -> Self {
        let mut compilation = Self {
            db: DbIndex::new(),
            emmyrc: emmyrc.clone(),
            batch_log: None,
        };

        compilation.db.update_config(emmyrc.clone());
        compilation
    }

    pub fn get_semantic_model(&'_ self, file_id: FileId) -> Option<SemanticModel<'_>> {
        let cache = LuaInferCache::new(file_id, Default::default());
        let tree = self.db.get_vfs().get_syntax_tree(&file_id)?;
        Some(SemanticModel::new(
            file_id,
            &self.db,
            cache,
            self.emmyrc.clone(),
            tree.get_chunk_node(),
        ))
    }

    /// Updates the index for `file_ids`.
    ///
    /// The returned stabilization-candidate list is retained for API
    /// compatibility. Late facts are now stabilized inside the analyzer, so
    /// external callers no longer need to schedule another indexing pass and
    /// this list is always empty.
    pub fn update_index(&mut self, file_ids: Vec<FileId>) -> Vec<FileId> {
        if let Some(log) = &mut self.batch_log {
            log.push(file_ids.clone());
        }
        let mut need_analyzed_files = vec![];
        let mut analyzed_file_ids = vec![];
        for file_id in file_ids {
            let tree = match self.db.get_vfs().get_syntax_tree(&file_id) {
                Some(tree) => tree,
                None => {
                    log::warn!("file_id {:?} not found in vfs", file_id);
                    continue;
                }
            };
            analyzed_file_ids.push(file_id);
            need_analyzed_files.push(InFiled {
                file_id,
                value: tree.get_chunk_node(),
            });
        }

        // Removing a file unresolved every dependency site that named it.
        // Now that it is back, those sites point at it again. Done here rather
        // than on the edit path so every caller of `update_index` is symmetric
        // with `remove_index`, not just the one that prompted it.
        for file_id in &analyzed_file_ids {
            self.db
                .get_file_dependencies_index_mut()
                .relink_unresolved_target(*file_id);
        }

        analyzer::analyze(&mut self.db, need_analyzed_files);
        Vec::new()
    }

    /// An independent copy-on-write copy; see [`DbIndex::fork_independent`].
    pub fn fork(&self) -> Self {
        Self {
            db: self.db.fork_independent(),
            emmyrc: self.emmyrc.clone(),
            batch_log: None,
        }
    }

    /// Starts recording every `update_index` batch, discarding any earlier log.
    pub fn start_batch_log(&mut self) {
        self.batch_log = Some(Vec::new());
    }

    /// Stops recording and returns the batches recorded since
    /// [`Self::start_batch_log`].
    pub fn take_batch_log(&mut self) -> Vec<Vec<FileId>> {
        self.batch_log.take().unwrap_or_default()
    }

    pub fn remove_index(&mut self, file_ids: Vec<FileId>) {
        self.db.remove_index(file_ids);
    }

    pub fn clear_index(&mut self) {
        self.db.clear();
    }

    pub fn get_db(&self) -> &DbIndex {
        &self.db
    }

    pub fn get_db_mut(&mut self) -> &mut DbIndex {
        &mut self.db
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        self.emmyrc = config.clone();
        self.db.update_config(config);
    }
}
