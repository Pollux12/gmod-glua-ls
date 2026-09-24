pub(crate) mod analyzer;
mod test;

pub use analyzer::gmod::get_scripted_class_info_for_file;
pub(crate) use analyzer::gmod::get_scripted_class_type_decl_id;
pub use analyzer::is_member_assignment_in_conditional_branch;
pub use analyzer::unresolve::extract_hook_name;
pub use analyzer::unresolve::resolve_gmod_hook_add_callback_doc_function;
pub use analyzer::unresolve::resolve_gmod_hook_callback_doc_function;

use std::sync::{Arc, Mutex};

use crate::{
    Emmyrc, FileId, LuaIndex, LuaInferCache,
    db_index::{
        DbIndex,
        derivation::{self, DerivationId, DerivationKind, DiagnosticReads},
    },
    semantic::SemanticModel,
};

#[derive(Debug)]
pub struct LuaCompilation {
    db: DbIndex,
    emmyrc: Arc<Emmyrc>,
    /// Every `update_index` batch since [`Self::start_batch_log`], for the
    /// determinism harness's replay.
    batch_log: Option<Vec<Vec<FileId>>>,
    /// What each file's last diagnostics read. Diagnostics run behind a
    /// shared borrow, possibly on several threads.
    diagnostic_reads: Mutex<DiagnosticReads>,
    /// The files the last `update_index` re-analysed, for locality checks.
    last_evaluated_files: Vec<FileId>,
}

impl LuaCompilation {
    pub fn new(emmyrc: Arc<Emmyrc>) -> Self {
        let mut compilation = Self {
            db: DbIndex::new(),
            emmyrc: emmyrc.clone(),
            batch_log: None,
            diagnostic_reads: Mutex::default(),
            last_evaluated_files: Vec::new(),
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

    /// Re-analyses `file_ids` from their current text; see
    /// [`analyzer::reanalyze`]. Returns the files whose last diagnostics read
    /// a fact that moved.
    pub fn update_index(&mut self, file_ids: Vec<FileId>) -> Vec<FileId> {
        if let Some(log) = &mut self.batch_log {
            log.push(file_ids.clone());
        }
        let edits = crate::db_index::derivation::EditMaps::new(
            self.db.get_vfs_mut().take_edit_maps(&file_ids),
        );
        let reanalysis = analyzer::reanalyze(&mut self.db, file_ids.clone(), edits.clone());
        self.last_evaluated_files = reanalysis.evaluated_files;
        let reads = self
            .diagnostic_reads
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for file_id in &file_ids {
            if self.db.get_vfs().get_syntax_tree(file_id).is_none() {
                reads.forget(*file_id);
            }
        }
        reads.apply_edits(&edits);
        let db = &self.db;
        let edits = &edits;
        reads.stale_files(&reanalysis.touched, |key| {
            let current = db.fact_fingerprint(key);
            let key = key.clone();
            let before_edits = std::cell::OnceCell::new();
            move |seen| {
                seen == current
                    || (!edits.is_empty()
                        && seen
                            == *before_edits.get_or_init(|| {
                                crate::db_index::derivation::fingerprint_before_edits(edits, || {
                                    db.fact_fingerprint(&key)
                                })
                            }))
            }
        })
    }

    /// The files the last [`Self::update_index`] re-analysed: those whose
    /// source layer was rebuilt or whose derivations were evaluated.
    pub fn last_evaluated_files(&self) -> &[FileId] {
        &self.last_evaluated_files
    }

    /// Whether `file_id`'s facts were built from its current text.
    pub fn is_indexed(&self, file_id: FileId) -> bool {
        self.db
            .reader_graph()
            .derivations_of_file(file_id)
            .next()
            .is_some()
    }

    /// Runs `diagnose` recording what it reads as `file_id`'s diagnostics
    /// (`None`: the data every file's diagnostics share).
    #[cfg(test)]
    pub(crate) fn diagnostic_read_keys(&self) -> Vec<derivation::FactKey> {
        self.diagnostic_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .read_keys()
            .cloned()
            .collect()
    }

    pub(crate) fn record_diagnostic_reads<T>(
        &self,
        file_id: Option<FileId>,
        diagnose: impl FnOnce() -> T,
    ) -> T {
        if derivation::recording() {
            return diagnose();
        }
        let id = match file_id {
            Some(file_id) => {
                let Some(tree) = self.db.get_vfs().get_syntax_tree(&file_id) else {
                    return diagnose();
                };
                DerivationId::new(
                    DerivationKind::Diagnostics,
                    file_id,
                    glua_parser::LuaSyntaxId::from_node(&tree.get_red_root()),
                )
            }
            None => DerivationId::workspace(DerivationKind::Diagnostics),
        };
        derivation::begin(id);
        let result = diagnose();
        let (_, reads, _) = derivation::end().into_parts(|key| self.db.fact_fingerprint(key));
        let mut store = self
            .diagnostic_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match file_id {
            Some(file_id) => store.record(file_id, reads),
            None => store.record_shared(reads),
        }
        result
    }

    /// An independent copy-on-write copy; see [`DbIndex::fork_independent`].
    pub fn fork(&self) -> Self {
        let reads = self
            .diagnostic_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Self {
            db: self.db.fork_independent(),
            emmyrc: self.emmyrc.clone(),
            batch_log: None,
            diagnostic_reads: Mutex::new(reads),
            last_evaluated_files: Vec::new(),
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
        *self
            .diagnostic_reads
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = DiagnosticReads::default();
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
