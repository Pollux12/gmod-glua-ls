pub(crate) mod analyzer;
mod test;

pub use analyzer::gmod::get_scripted_class_info_for_file;
pub(crate) use analyzer::gmod::get_scripted_class_type_decl_id;
pub use analyzer::is_member_assignment_in_conditional_branch;
pub use analyzer::parallel::set_analysis_threads;
pub use analyzer::unresolve::extract_hook_name;
pub use analyzer::unresolve::resolve_gmod_hook_add_callback_doc_function;
pub use analyzer::unresolve::resolve_gmod_hook_callback_doc_function;
pub use analyzer::{Schedule, Slice};

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
    last_stats: analyzer::EngineStats,
    schedule: Schedule,
    /// The re-analysis in progress, between its slices. Only `&mut` access
    /// advances it; readers sharing the compilation only ask what it has left.
    reanalysis: Mutex<Option<analyzer::Reanalysis>>,
    /// The VFS content revision the re-analysis in progress took its text at.
    taken_revision: u64,
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
            diagnostic_reads: Mutex::default(),
            last_evaluated_files: Vec::new(),
            last_stats: analyzer::EngineStats::default(),
            schedule: Schedule::default(),
            reanalysis: Mutex::default(),
            taken_revision: 0,
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
        if let Some(log) = &mut self.batch_log {
            log.push(file_ids.clone());
        }
        let edits = derivation::EditMaps::new(self.db.get_vfs_mut().take_edit_maps(&file_ids));
        let reads = self
            .diagnostic_reads
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for file_id in &file_ids {
            if self.db.get_vfs().get_syntax_tree(file_id).is_none() {
                reads.forget(*file_id);
            }
        }
        // Diagnostics recorded from here on read the new positions.
        crate::profile::phase("reanalyze: diagnostic reads follow edits", || {
            reads.apply_edits(&edits)
        });
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
                ));
            }
        }
        self.taken_revision = self.db.get_vfs().content_revision();
    }

    /// Advances the re-analysis in progress by `slice`.
    pub fn advance_update(&mut self, slice: Slice) -> UpdateProgress {
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
        let edits = reanalysis.edits().clone();
        let reanalysed = reanalysis.finish(&mut self.db);
        self.last_evaluated_files = reanalysed.evaluated_files;
        self.last_stats = reanalysed.stats;
        let reads = self
            .diagnostic_reads
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        UpdateProgress::Settled(crate::profile::phase(
            "reanalyze: stale diagnostics",
            || stale_diagnostic_files(reads, &self.db, &reanalysed.touched, &edits),
        ))
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
        file_id: FileId,
        diagnose: impl FnOnce() -> T,
    ) -> T {
        if derivation::recording() {
            return diagnose();
        }
        let Some(tree) = self.db.get_vfs().get_syntax_tree(&file_id) else {
            return diagnose();
        };
        derivation::begin(DerivationId::new(
            DerivationKind::Diagnostics,
            file_id,
            glua_parser::LuaSyntaxId::from_node(&tree.get_red_root()),
        ));
        let result = diagnose();
        let (_, reads, _) = derivation::end().into_parts(|key| self.db.fact_fingerprint(key));
        let reads = reads
            .into_iter()
            .map(|(key, seen)| (key.into_key(), seen))
            .collect();
        self.diagnostic_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(file_id, reads);
        result
    }

    /// An independent copy-on-write copy; see [`DbIndex::fork_independent`].
    pub fn fork(&self) -> Self {
        let reads = self
            .diagnostic_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        debug_assert!(self.session().is_none(), "forking mid re-analysis");
        Self {
            db: self.db.fork_independent(),
            emmyrc: self.emmyrc.clone(),
            batch_log: None,
            diagnostic_reads: Mutex::new(reads),
            last_evaluated_files: Vec::new(),
            last_stats: analyzer::EngineStats::default(),
            schedule: self.schedule,
            reanalysis: Mutex::default(),
            taken_revision: self.taken_revision,
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
        *self
            .reanalysis
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.db.clear();
        *self
            .diagnostic_reads
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = DiagnosticReads::default();
    }

    /// The reader graph's and the diagnostic reads' census, for the profile.
    pub fn memory_census(&self) -> String {
        let reads = self
            .diagnostic_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .census();
        format!(
            "reader graph: {}\ndiagnostic reads: {reads}",
            self.db.reader_graph().census()
        )
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

/// The files whose last diagnostics read one of `touched` and no longer hold,
/// even with `edits` mapped back. Reads that held only through the edits are
/// re-recorded as reads of the value as it stands.
fn stale_diagnostic_files(
    reads: &mut DiagnosticReads,
    db: &DbIndex,
    touched: &rustc_hash::FxHashSet<derivation::HashedKey>,
    edits: &derivation::EditMaps,
) -> Vec<FileId> {
    reads.stale_files(
        touched.iter().map(|key| key.key()),
        |key| db.fact_fingerprint(key),
        |key| {
            (!edits.is_empty())
                .then(|| derivation::fingerprint_before_edits(edits, || db.fact_fingerprint(key)))
        },
    )
}
