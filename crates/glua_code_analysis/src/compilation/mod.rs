pub(crate) mod analyzer;
mod test;

pub use analyzer::gmod::get_scripted_class_info_for_file;
pub(crate) use analyzer::gmod::get_scripted_class_type_decl_id;
pub use analyzer::is_member_assignment_in_conditional_branch;
pub use analyzer::parallel::set_analysis_threads;
pub use analyzer::unresolve::extract_hook_name;
pub use analyzer::unresolve::resolve_gmod_hook_add_callback_doc_function;
pub use analyzer::unresolve::resolve_gmod_hook_callback_doc_function;
pub(crate) use analyzer::{CallbackArg, CallbackParamsQuery, ReceiverAuthorityQuery};
pub use analyzer::{Schedule, Slice};

use std::sync::{Arc, Mutex};

use crate::{
    Emmyrc, FileId, LuaIndex, LuaInferCache,
    db_index::{
        DbIndex,
        derivation::{self, DerivationId, DerivationKind},
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
    /// What each file's diagnostics read since the reader graph last changed,
    /// to file as their `Diagnostics` records before it next does.
    /// Diagnostics run behind a shared borrow, possibly on several threads.
    diagnosed: Mutex<rustc_hash::FxHashMap<FileId, derivation::ViewedReads>>,
    /// The files the last `update_index` re-analysed, for locality checks.
    last_evaluated_files: Vec<FileId>,
    last_stats: analyzer::EngineStats,
    schedule: Schedule,
    /// The re-analysis in progress, between its slices. Only `&mut` access
    /// advances it; readers sharing the compilation only ask what it has left.
    reanalysis: Mutex<Option<analyzer::Reanalysis>>,
    /// The VFS content revision the re-analysis in progress took its text at.
    taken_revision: u64,
    /// Bumped whenever the index may change; see [`Self::index_revision`].
    index_revision: u64,
    /// The scripted scopes, lent to each re-analysis.
    scopes: analyzer::ScopesCache,
    /// Each file's inference memo from the last settled re-analysis, which
    /// the file's next diagnostics take; the next re-analysis drops the rest.
    memos: Mutex<rustc_hash::FxHashMap<FileId, LuaInferCache>>,
}

/// Where the re-analysis in progress stands after
/// [`LuaCompilation::advance_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateProgress {
    /// It has work left.
    Settling,
    /// It settled: these files' last diagnostics read a fact that moved.
    Settled(Vec<FileId>),
    /// A file's text changed after it took its files: it evaluates nothing
    /// until it takes that file in ([`LuaCompilation::begin_update`]).
    TextChanged,
}

impl LuaCompilation {
    pub fn new(emmyrc: Arc<Emmyrc>) -> Self {
        let mut compilation = Self {
            db: DbIndex::new(),
            emmyrc: emmyrc.clone(),
            batch_log: None,
            diagnosed: Mutex::default(),
            last_evaluated_files: Vec::new(),
            last_stats: analyzer::EngineStats::default(),
            schedule: Schedule::default(),
            reanalysis: Mutex::default(),
            taken_revision: 0,
            index_revision: 0,
            scopes: analyzer::ScopesCache::default(),
            memos: Mutex::default(),
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

    /// [`Self::get_semantic_model`] for `file_id`'s diagnostics, starting from
    /// the memo the last settled re-analysis left for the file.
    pub(crate) fn get_diagnostic_semantic_model(
        &'_ self,
        file_id: FileId,
    ) -> Option<SemanticModel<'_>> {
        let memo = self
            .memos
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&file_id);
        let cache = match memo {
            Some(mut cache) => {
                *cache.config_mut() = Default::default();
                cache
            }
            None => LuaInferCache::new(file_id, Default::default()),
        };
        let tree = self.db.get_vfs().get_syntax_tree(&file_id)?;
        Some(SemanticModel::new(
            file_id,
            &self.db,
            cache,
            self.emmyrc.clone(),
            tree.get_chunk_node(),
        ))
    }

    /// Re-analyses `file_ids` from their current text, settling any
    /// re-analysis in progress with them; see [`Self::begin_update`]. Returns
    /// the files whose last diagnostics read a fact that moved.
    pub fn update_index(&mut self, file_ids: Vec<FileId>) -> Vec<FileId> {
        self.begin_update(file_ids);
        match self.advance_update(Slice::Whole) {
            UpdateProgress::Settled(stale) => stale,
            progress => unreachable!("a whole slice settles, not {progress:?}"),
        }
    }

    /// Starts re-analysing `file_ids` from their current text, and whatever
    /// read a fact that moved; see [`analyzer::Reanalysis`]. A re-analysis in
    /// progress takes them in and settles them first. Advance it with
    /// [`Self::advance_update`].
    pub fn begin_update(&mut self, file_ids: Vec<FileId>) {
        self.file_diagnostic_reads();
        self.index_revision += 1;
        if let Some(log) = &mut self.batch_log {
            log.push(file_ids.clone());
        }
        self.memos
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let edits = derivation::EditMaps::new(self.db.get_vfs_mut().take_edit_maps(&file_ids));
        let reanalysis = self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match reanalysis {
            Some(reanalysis) => reanalysis.take(&mut self.db, file_ids, edits),
            None => {
                *reanalysis = Some(analyzer::Reanalysis::begin(
                    &mut self.db,
                    file_ids,
                    edits,
                    self.schedule,
                    std::mem::take(&mut self.scopes),
                ));
            }
        }
        self.taken_revision = self.db.get_vfs().content_revision();
    }

    /// Advances the re-analysis in progress by `slice`.
    pub fn advance_update(&mut self, slice: Slice) -> UpdateProgress {
        self.file_diagnostic_reads();
        self.index_revision += 1;
        let session = self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(reanalysis) = session else {
            return UpdateProgress::Settled(Vec::new());
        };
        if self.db.get_vfs().content_revision() != self.taken_revision {
            return UpdateProgress::TextChanged;
        }
        if !reanalysis.run(&mut self.db, slice) {
            return UpdateProgress::Settling;
        }
        let reanalysis = session.take().expect("a re-analysis in progress");
        let reanalysed = reanalysis.finish(&mut self.db);
        self.scopes = reanalysed.scopes;
        *self
            .memos
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reanalysed.memos;
        self.last_evaluated_files = reanalysed.evaluated_files;
        self.last_stats = reanalysed.stats;
        UpdateProgress::Settled(reanalysed.stale_diagnostics)
    }

    /// Whether a re-analysis is in progress.
    pub fn is_updating(&self) -> bool {
        self.session().is_some()
    }

    /// Settles `file_ids` before the rest of the re-analysis in progress.
    pub fn prioritise_update(&mut self, file_ids: &[FileId]) {
        if let Some(reanalysis) = self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            reanalysis.prioritise(file_ids);
        }
    }

    /// Whether the re-analysis in progress still has work in `file_id`.
    pub fn is_settling(&self, file_id: FileId) -> bool {
        self.session()
            .as_ref()
            .is_some_and(|reanalysis| reanalysis.is_settling(file_id))
    }

    /// Whether `file_id` still settles ahead of the rest of the re-analysis
    /// in progress.
    pub fn settles_first(&self, file_id: FileId) -> bool {
        self.session()
            .as_ref()
            .is_some_and(|reanalysis| reanalysis.in_focus(file_id))
    }

    fn session(&self) -> std::sync::MutexGuard<'_, Option<analyzer::Reanalysis>> {
        self.reanalysis
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sets the order re-analyses take files in; see [`Schedule`].
    pub fn set_schedule(&mut self, schedule: Schedule) {
        self.schedule = schedule;
    }

    /// The files the last re-analysis re-analysed: those whose source layer
    /// was rebuilt or whose derivations were evaluated.
    pub fn last_evaluated_files(&self) -> &[FileId] {
        &self.last_evaluated_files
    }

    /// What the last re-analysis evaluated.
    #[cfg(test)]
    pub(crate) fn last_stats(&self) -> &analyzer::EngineStats {
        &self.last_stats
    }

    /// Whether `file_id`'s facts were built from its current text.
    pub fn is_indexed(&self, file_id: FileId) -> bool {
        self.db
            .reader_graph()
            .derivations_of_file(file_id)
            .any(|id| id.kind != DerivationKind::Diagnostics)
    }

    /// Every key some file's diagnostics read, filed or not.
    #[cfg(test)]
    pub(crate) fn diagnostic_read_keys(&self) -> Vec<derivation::FactKey> {
        let graph = self.db.reader_graph();
        let mut keys = graph
            .derivations()
            .filter(|id| id.kind == DerivationKind::Diagnostics)
            .flat_map(|id| graph.reads_of(id).map(|key| key.key().clone()))
            .collect::<Vec<_>>();
        let diagnosed = self
            .diagnosed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for reads in diagnosed.values() {
            keys.extend(reads.keys(graph).cloned());
        }
        keys
    }

    pub(crate) fn record_diagnostic_reads<T>(
        &self,
        file_id: FileId,
        diagnose: impl FnOnce() -> T,
    ) -> T {
        if derivation::recording() {
            return diagnose();
        }
        if self.db.get_vfs().get_syntax_tree(&file_id).is_none() {
            return diagnose();
        }
        derivation::begin(DerivationId::diagnostics(file_id));
        let viewed = derivation::view_graph(self.db.reader_graph_shared());
        let result = diagnose();
        drop(viewed);
        // Fingerprints the engine took stand while nothing writes.
        let reads = derivation::end()
            .into_viewed_reads(self.db.reader_graph(), |key| self.db.fact_fingerprint(key));
        self.diagnosed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(file_id, reads);
        result
    }

    /// Files what diagnostics read since the reader graph last changed, which
    /// names its slots as they stood, before anything changes it.
    fn file_diagnostic_reads(&mut self) {
        let diagnosed = std::mem::take(
            self.diagnosed
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if diagnosed.is_empty() {
            return;
        }
        let mut diagnosed = diagnosed.into_iter().collect::<Vec<_>>();
        diagnosed.sort_unstable_by_key(|(file_id, _)| *file_id);
        let graph = self.db.reader_graph_mut();
        let mut released = Vec::new();
        for (file_id, reads) in diagnosed {
            graph.file_viewed(DerivationId::diagnostics(file_id), reads, &mut released);
        }
        graph.release_keys(released);
    }

    /// An independent copy-on-write copy; see [`DbIndex::fork_independent`].
    pub fn fork(&self) -> Self {
        let diagnosed = self
            .diagnosed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        debug_assert!(self.session().is_none(), "forking mid re-analysis");
        Self {
            db: self.db.fork_independent(),
            emmyrc: self.emmyrc.clone(),
            batch_log: None,
            diagnosed: Mutex::new(diagnosed),
            last_evaluated_files: Vec::new(),
            last_stats: analyzer::EngineStats::default(),
            schedule: self.schedule,
            reanalysis: Mutex::default(),
            taken_revision: self.taken_revision,
            index_revision: self.index_revision,
            scopes: analyzer::ScopesCache::default(),
            memos: Mutex::default(),
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
        self.file_diagnostic_reads();
        self.index_revision += 1;
        self.db.remove_index(file_ids);
    }

    pub fn clear_index(&mut self) {
        self.index_revision += 1;
        *self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.db.clear();
        self.diagnosed
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.memos
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// The reader graph's census, for the profile.
    pub fn memory_census(&self) -> String {
        format!("reader graph: {}", self.db.reader_graph().census())
    }

    pub fn get_db(&self) -> &DbIndex {
        &self.db
    }

    pub fn get_db_mut(&mut self) -> &mut DbIndex {
        self.file_diagnostic_reads();
        self.index_revision += 1;
        &mut self.db
    }

    /// Changes whenever the index may have changed since it was last read:
    /// every path that writes it bumps the revision first.
    pub fn index_revision(&self) -> u64 {
        self.index_revision
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        self.file_diagnostic_reads();
        self.index_revision += 1;
        self.scopes = analyzer::ScopesCache::default();
        self.emmyrc = config.clone();
        self.db.update_config(config);
    }
}
