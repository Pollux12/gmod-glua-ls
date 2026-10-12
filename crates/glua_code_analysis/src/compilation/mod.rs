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

use std::sync::atomic::AtomicBool;
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
    /// Set while an edit waits to be taken in: a slice in progress stops
    /// between two evaluations.
    interrupt: Arc<AtomicBool>,
    /// Bumped whenever the index may change; see [`Self::index_revision`].
    index_revision: u64,
    /// The scripted scopes, lent to each re-analysis.
    scopes: analyzer::ScopesCache,
    /// Each file's inference memo from the last settled re-analysis, which
    /// the file's next diagnostics take; the next re-analysis drops the rest.
    memos: Mutex<rustc_hash::FxHashMap<FileId, LuaInferCache>>,
    /// Whether re-analyses leave worker supersteps' tasks to
    /// [`Self::run_detached_tasks`]; see [`Self::set_detached_tasks`].
    detach_tasks: bool,
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
        // Raw parallel iterators reach rayon's global pool too, so it is sized
        // before anything can run on it.
        analyzer::parallel::init_pool();
        let mut compilation = Self {
            db: DbIndex::new(),
            emmyrc: emmyrc.clone(),
            batch_log: None,
            diagnosed: Mutex::default(),
            last_evaluated_files: Vec::new(),
            last_stats: analyzer::EngineStats::default(),
            schedule: Schedule::default(),
            reanalysis: Mutex::default(),
            interrupt: Arc::default(),
            index_revision: 0,
            scopes: analyzer::ScopesCache::default(),
            memos: Mutex::default(),
            detach_tasks: false,
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
    /// read a fact that moved; see [`analyzer::Reanalysis`]. Every other file
    /// whose text changed since a re-analysis took it is taken too, so the
    /// index is never read against text it was not built from. A re-analysis
    /// in progress takes them in and settles them first. Advance it with
    /// [`Self::advance_update`].
    pub fn begin_update(&mut self, mut file_ids: Vec<FileId>) {
        file_ids.extend(self.db.get_vfs().files_changed_since_taken());
        file_ids.sort_unstable();
        file_ids.dedup();
        self.file_diagnostic_reads();
        self.install_detached();
        self.index_revision += 1;
        if let Some(log) = &mut self.batch_log {
            log.push(file_ids.clone());
        }
        let memos = std::mem::take(
            self.memos
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        // Freeing every file's memo takes long enough to hold up the take.
        if !memos.is_empty() {
            std::thread::spawn(move || drop(memos));
        }
        let edits = derivation::EditMaps::new(self.db.get_vfs_mut().take_staged(&file_ids));
        let reanalysis = self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match reanalysis {
            Some(reanalysis) => reanalysis.take(&mut self.db, file_ids, edits),
            None => {
                let mut begun = analyzer::Reanalysis::begin(
                    &mut self.db,
                    file_ids,
                    edits,
                    self.schedule,
                    std::mem::take(&mut self.scopes),
                    self.interrupt.clone(),
                );
                if self.detach_tasks {
                    begun.detach_supersteps();
                }
                *reanalysis = Some(begun);
            }
        }
    }

    /// Advances the re-analysis in progress by `slice`.
    pub fn advance_update(&mut self, slice: Slice) -> UpdateProgress {
        self.file_diagnostic_reads();
        let session = self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(reanalysis) = session else {
            return UpdateProgress::Settled(Vec::new());
        };
        self.index_revision += 1;
        if self.db.get_vfs().holds_untaken_text() {
            reanalysis.install_detached(&mut self.db);
            return UpdateProgress::TextChanged;
        }
        if !reanalysis.wrapped() {
            if !reanalysis.run(&mut self.db, slice) {
                return UpdateProgress::Settling;
            }
            reanalysis.wrap_up(&mut self.db);
            // Which diagnostics went stale is checked while readers share
            // the index.
            if self.detach_tasks && matches!(slice, Slice::Until(_)) {
                return UpdateProgress::Settling;
            }
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

    /// Has a re-analysis slice that ends with a worker superstep due leave its
    /// tasks to [`Self::run_detached_tasks`], which runs them while readers
    /// share the compilation. The next `&mut` access installs them first.
    pub fn set_detached_tasks(&mut self, detach: bool) {
        self.detach_tasks = detach;
    }

    /// Whether the last slice left work to [`Self::run_detached_tasks`].
    pub fn has_detached_tasks(&self) -> bool {
        self.session()
            .as_ref()
            .is_some_and(|reanalysis| reanalysis.has_detached() || reanalysis.needs_stale_check())
    }

    /// Runs what the last slice left to run outside it while readers share
    /// the compilation: a superstep's tasks, each on its own fork of the
    /// index, or the check of which diagnostics a settled run left stale.
    /// The next slice installs the tasks or reports the check.
    pub fn run_detached_tasks(&self) {
        let detached = {
            let mut session = self.session();
            let Some(reanalysis) = session.as_mut() else {
                return;
            };
            if reanalysis.needs_stale_check() {
                reanalysis.check_stale(&self.db);
                return;
            }
            reanalysis.take_detached()
        };
        let Some((mut superstep, recipes)) = detached else {
            return;
        };
        superstep.run_tasks(&self.db, &*recipes);
        if let Some(reanalysis) = self.session().as_mut() {
            reanalysis.restore_detached(superstep);
        }
    }

    /// Installs the tasks a slice left to run outside it, whether they ran or
    /// not, so the index can be written: what diagnostics read is filed
    /// against the reader graph the tasks ran on first.
    fn install_detached(&mut self) {
        if let Some(reanalysis) = self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            reanalysis.install_detached(&mut self.db);
        }
    }

    /// The flag that stops a slice in progress between two evaluations, so
    /// an edit waiting to be taken in is not held up by it.
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        self.interrupt.clone()
    }

    /// Whether a re-analysis is in progress.
    pub fn is_updating(&self) -> bool {
        self.session().is_some()
    }

    /// Settles `file_ids` before the rest of the re-analysis in progress.
    pub fn prioritise_update(&mut self, file_ids: &[FileId]) {
        self.file_diagnostic_reads();
        self.install_detached();
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

    /// Whether a re-analysis has taken in `file_id`'s current text.
    pub fn has_taken_text(&self, file_id: FileId) -> bool {
        self.db.get_vfs().get_file_revision(&file_id).is_some()
            && !self.db.get_vfs().has_untaken_edit(&file_id)
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

    /// Runs `diagnose` for `file_id`, recording what it reads. Only a report
    /// replaces the reads of the last one: a run cancelled before it finished
    /// leaves the last report standing, and with it what that report read.
    pub(crate) fn record_diagnostic_reads<T>(
        &self,
        file_id: FileId,
        diagnose: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        if derivation::recording() {
            return diagnose();
        }
        if self.db.get_vfs().get_syntax_tree(&file_id).is_none() {
            return diagnose();
        }
        let _recording = derivation::begin(DerivationId::diagnostics(file_id));
        // Diagnostics are shown at the file's path, so a move makes them stale.
        derivation::note_file_fact_read(derivation::FileFact::FilePath, file_id);
        let viewed = derivation::view_graph(self.db.reader_graph_shared());
        let result = diagnose();
        drop(viewed);
        // Fingerprints the engine took stand while nothing writes.
        let recorded = derivation::end();
        if result.is_some() {
            let reads = recorded
                .into_viewed_reads(self.db.reader_graph(), |key| self.db.fact_fingerprint(key));
            self.diagnosed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(file_id, reads);
        }
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
        let _filing = crate::profile::PhaseGuard::new("diagnostic reads");
        let mut diagnosed = diagnosed
            .into_iter()
            .map(|(file_id, reads)| (DerivationId::diagnostics(file_id), reads))
            .collect::<Vec<_>>();
        diagnosed.sort_unstable_by_key(|(id, _)| id.file_id);
        self.db.reader_graph_mut().file_viewed(diagnosed);
    }

    /// Files what the diagnostics of up to `files` files read, as
    /// [`Self::file_diagnostic_reads`] does for every file, so a quiet moment
    /// files them a part at a time instead of the next change all at once.
    /// Returns whether any are left to file.
    pub fn file_some_diagnostic_reads(&mut self, files: usize) -> bool {
        let diagnosed = self
            .diagnosed
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut file_ids = diagnosed.keys().copied().collect::<Vec<_>>();
        file_ids.sort_unstable();
        let part = file_ids
            .iter()
            .take(files)
            .filter_map(|file_id| {
                let reads = diagnosed.remove(file_id)?;
                Some((DerivationId::diagnostics(*file_id), reads))
            })
            .collect::<Vec<_>>();
        if part.is_empty() {
            return false;
        }
        let _filing = crate::profile::PhaseGuard::new("diagnostic reads part");
        let more = !diagnosed.is_empty();
        let graph = self.db.reader_graph_mut();
        if more {
            graph.file_viewed_part(part);
        } else {
            graph.file_viewed(part);
        }
        more
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
            interrupt: Arc::default(),
            index_revision: self.index_revision,
            scopes: analyzer::ScopesCache::default(),
            memos: Mutex::default(),
            detach_tasks: self.detach_tasks,
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
        self.install_detached();
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

    /// Drops the compilation part by part, calling `dropped` with each part's
    /// name once it is gone, for a census of what each part holds.
    pub fn drop_by_part(&mut self, mut dropped: impl FnMut(&'static str)) {
        drop(
            self.reanalysis
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(),
        );
        dropped("reanalysis");
        drop(std::mem::take(
            self.memos
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ));
        dropped("inference memos");
        drop(std::mem::take(
            self.diagnosed
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ));
        dropped("diagnostic reads");
        self.scopes = analyzer::ScopesCache::default();
        dropped("scopes");
        self.db.drop_by_part(dropped);
    }

    /// The reader graph's census, for the profile.
    pub fn memory_census(&self) -> String {
        let diagnosed = self
            .diagnosed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let diagnosed_bytes = diagnosed
            .values()
            .map(derivation::ViewedReads::heap_bytes)
            .sum::<usize>();
        format!(
            "reader graph: {}; unfiled diagnostic reads: {} files, MB {}",
            self.db.reader_graph().census(),
            diagnosed.len(),
            crate::profile::megabytes(diagnosed_bytes)
        )
    }

    pub fn get_db(&self) -> &DbIndex {
        &self.db
    }

    pub fn get_db_mut(&mut self) -> &mut DbIndex {
        self.file_diagnostic_reads();
        self.install_detached();
        self.index_revision += 1;
        &mut self.db
    }

    /// Changes whenever the index may have changed since it was last read:
    /// every path that writes it bumps the revision first.
    pub fn index_revision(&self) -> u64 {
        self.index_revision
    }

    /// Applies `config`; `affects_index` says whether facts derived under the
    /// previous one may no longer hold.
    pub fn update_config(&mut self, config: Arc<Emmyrc>, affects_index: bool) {
        self.file_diagnostic_reads();
        self.install_detached();
        if affects_index {
            self.index_revision += 1;
            self.scopes = analyzer::ScopesCache::default();
        }
        self.emmyrc = config.clone();
        self.db.update_config(config);
    }
}
