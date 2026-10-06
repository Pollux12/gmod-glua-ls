mod call_site_params;
pub(crate) mod call_target;
mod common;
mod decl;
mod derive;
mod doc;
mod dynamic_field;
mod engine;
mod flow;
mod function_summary;
pub(crate) mod gmod;
mod infer_cache_manager;
mod local_inference;
mod lua;
pub(crate) mod parallel;
mod setmetatable_factory;
pub(crate) mod unresolve;

pub(crate) use call_site_params::{CallbackArg, CallbackParamsQuery, ReceiverAuthorityQuery};
pub(crate) use common::{union_widens_cached_type, widen_mutable_decl_literal};
pub(crate) use dynamic_field::is_provably_builtin_iterator_call;
pub(crate) use dynamic_field::{
    copies_table_into_itself, for_range_pairs_source_for_var, writes_own_pairs_key,
};
pub(crate) use engine::EngineStats;
#[cfg(test)]
pub(crate) use engine::serial_engine;
pub use engine::{Schedule, Slice};
pub use lua::is_member_assignment_in_conditional_branch;
pub(crate) use lua::{
    dominating_guarded_table_bootstrap_range, infer_for_range_iter_expr_func,
    is_guarded_table_definition_site,
};

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::Arc;

use crate::db_index::derivation;
use crate::{
    AnalyzeError, AsyncState, FileId, FlowTree, GmodScopedClassInfo, InFiled, InferFailReason,
    LuaDeclId, LuaFunctionType, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaSignatureId, LuaType, LuaTypeCache, WorkspaceId,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{DbIndex, LuaMemberOwner},
};
use glua_parser::{LuaAstNode, LuaChunk, LuaClosureExpr, LuaExpr, LuaSyntaxId, LuaSyntaxNode};
use infer_cache_manager::InferCacheManager;
use unresolve::UnResolve;

pub(crate) fn infer_closure_body_function_type(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    closure: &LuaClosureExpr,
) -> Option<LuaType> {
    let signature_id = LuaSignatureId::from_closure(cache.get_file_id(), closure);
    let signature = db.get_signature_index().get(&signature_id)?;
    if signature.resolve_return != crate::SignatureReturnStatus::DocResolve
        || !signature.overloads.is_empty()
    {
        return None;
    }

    let return_points = lua::func_body::analyze_func_body_returns(closure.get_block()?);
    let return_type = lua::analyze_return_point(db, cache, &return_points)
        .ok()?
        .into_iter()
        .next()?
        .type_ref;
    let function_type = LuaFunctionType::new(
        signature.async_state,
        signature.is_colon_define,
        signature.is_vararg,
        signature.get_type_params(),
        return_type,
    )
    .with_optional_params(signature.get_param_optional_flags());

    Some(LuaType::DocFunction(function_type.into()))
}

/// Which files are scripted-class scopes, and their class info: a function of
/// the live files' paths, the main workspaces and the config.
#[derive(Clone)]
pub(crate) struct ScriptedScopes {
    files: Arc<HashSet<FileId>>,
    infos: Arc<HashMap<FileId, GmodScopedClassInfo>>,
}

impl ScriptedScopes {
    fn new(db: &DbIndex) -> Self {
        let mut context = AnalyzeContext::new();
        for file_id in db.get_vfs().get_all_local_file_ids() {
            if let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) {
                context.add_tree_chunk(InFiled::new(file_id, tree.get_chunk_node()));
            }
        }
        Self {
            files: context.get_or_compute_scripted_scope_files(db),
            infos: context.get_or_compute_scripted_scope_infos(db),
        }
    }
}

/// What [`ScriptedScopes`] are computed from, besides the config.
#[derive(PartialEq, Eq)]
struct ScopesKey {
    files: Vec<FileId>,
    workspaces: Vec<(WorkspaceId, Option<std::path::PathBuf>)>,
}

impl ScopesKey {
    fn of(db: &DbIndex) -> Self {
        let vfs = db.get_vfs();
        let mut files = vfs.get_all_local_file_ids();
        files.retain(|file_id| vfs.get_syntax_tree(file_id).is_some());
        files.sort_unstable();
        let module_index = db.get_module_index();
        let mut workspaces = module_index
            .get_main_workspace_ids()
            .into_iter()
            .map(|id| (id, module_index.get_workspace_root(id).map(Into::into)))
            .collect::<Vec<_>>();
        workspaces.sort_by_key(|(id, _)| id.id);
        Self { files, workspaces }
    }
}

/// The last [`ScriptedScopes`], reused while what they were computed from
/// stands. A new config clears it.
#[derive(Default)]
pub(crate) struct ScopesCache(Option<(ScopesKey, ScriptedScopes)>);

impl std::fmt::Debug for ScopesCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopesCache")
            .field("filled", &self.0.is_some())
            .finish()
    }
}

impl ScopesCache {
    fn get(&mut self, db: &DbIndex) -> ScriptedScopes {
        let key = ScopesKey::of(db);
        if let Some((cached, scopes)) = &self.0
            && *cached == key
        {
            return scopes.clone();
        }
        let scopes = ScriptedScopes::new(db);
        self.0 = Some((key, scopes.clone()));
        scopes
    }
}

/// What a finished [`Reanalysis`] did.
pub(crate) struct Reanalysed {
    /// The files whose diagnostics read a fact that moved, sorted.
    pub stale_diagnostics: Vec<FileId>,
    /// The files whose source layer was rebuilt or whose derivations were
    /// evaluated, sorted.
    pub evaluated_files: Vec<FileId>,
    pub(crate) stats: engine::EngineStats,
    /// The scripted scopes, to lend the next re-analysis.
    pub(crate) scopes: ScopesCache,
    /// Each evaluated file's inference memo, for its diagnostics.
    pub(crate) memos: HashMap<FileId, crate::LuaInferCache>,
}

/// A re-analysis in progress. It re-analyses files from their current text:
/// forgets what their previous analysis produced, rebuilds each live file's
/// source layer, and evaluates every derivation that is new or read a fact
/// that changed until nothing is stale. A file without a syntax tree is
/// treated as deleted.
///
/// It advances in slices ([`Self::run`]). Between two, no evaluation is open
/// and the index reads as it does outside any run, so readers can use it.
pub(crate) struct Reanalysis {
    engine: engine::Engine,
    /// The current pass; `None` once no pass is left.
    pass: Option<Pass>,
    /// The access paths the current pass built: text is fixed for a pass.
    access_paths: crate::semantic::AccessPaths,
    /// The stage the run is at while it waits for its next slice.
    stage: derivation::AnalysisStage,
    scopes: ScopesCache,
    /// The inference memos of the passes that settled; a later pass's
    /// replaces an earlier one's.
    memos: HashMap<FileId, crate::LuaInferCache>,
}

impl Reanalysis {
    /// Starts re-analysing `file_ids`: rebuilds their source layers and
    /// queues their derivations and whatever read a fact that moved.
    pub(crate) fn begin(
        db: &mut DbIndex,
        file_ids: Vec<FileId>,
        edits: derivation::EditMaps,
        schedule: engine::Schedule,
        mut scopes: ScopesCache,
        interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let mut engine = engine::Engine::new(edits, schedule);
        engine.interrupt_with(interrupt);
        let pass = start_pass(db, &mut engine, file_ids, &mut scopes);
        let mut reanalysis = Self {
            engine,
            pass,
            access_paths: crate::semantic::AccessPaths::default(),
            stage: derivation::AnalysisStage::Closed,
            scopes,
            memos: HashMap::default(),
        };
        reanalysis.pause(db);
        reanalysis
    }

    /// Takes `file_ids`' new text in, `edits` saying where it moved their
    /// positions since this re-analysis took them last: they settle before
    /// the work still queued.
    pub(crate) fn take(
        &mut self,
        db: &mut DbIndex,
        file_ids: Vec<FileId>,
        edits: derivation::EditMaps,
    ) {
        // What a pending pass's rebuild moved is queued under the edits it
        // was taken with; source layers are rebuilt at the stage outside a run.
        self.open_pass(db);
        db.set_analysis_stage(derivation::AnalysisStage::Closed);
        refresh_payload_refs(db, self.engine.payload_holders());
        self.engine.take_edits(edits);
        if let Some(pass) = start_pass(db, &mut self.engine, file_ids, &mut self.scopes) {
            self.pass = Some(pass);
            self.access_paths = crate::semantic::AccessPaths::default();
        }
        self.pause(db);
    }

    /// Evaluates until the re-analysis settles or `slice` is over. Returns
    /// whether it settled.
    pub(crate) fn run(&mut self, db: &mut DbIndex, slice: engine::Slice) -> bool {
        db.set_analysis_stage(self.stage);
        crate::semantic::keep_access_paths(std::mem::take(&mut self.access_paths));
        let unwinding = Unwinding;
        let evaluated = self.engine.stats().total_evaluations();
        let settled = loop {
            self.open_pass(db);
            let Some(Pass::Open(recipes)) = &self.pass else {
                break true;
            };
            let slice = slice.remaining(self.engine.stats().total_evaluations() - evaluated);
            let settled = crate::profile::phase("reanalyze: engine run", || {
                self.engine.run(db, recipes, slice)
            });
            if !settled {
                break false;
            }
            crate::profile::memory::checkpoint("after the engine run");
            if let Some(Pass::Open(recipes)) = self.pass.take() {
                self.memos.extend(recipes.into_memos());
            }
            let stale = self.engine.take_stale_sources();
            crate::semantic::stop_keeping_access_paths();
            self.pass = start_pass(
                db,
                &mut self.engine,
                stale.into_iter().collect(),
                &mut self.scopes,
            );
            crate::semantic::keep_access_paths(crate::semantic::AccessPaths::default());
        };
        std::mem::forget(unwinding);
        self.access_paths = crate::semantic::stop_keeping_access_paths();
        self.pause(db);
        settled
    }

    /// Queues the readers of what the pass's rebuild moved and opens the run.
    fn open_pass(&mut self, db: &mut DbIndex) {
        let (touched, scopes) = match self.pass.take() {
            Some(Pass::Replaced { touched, scopes }) => (touched, scopes),
            pass => {
                self.pass = pass;
                return;
            }
        };
        crate::profile::phase("reanalyze: readers of touched", || {
            self.engine.readers_of(db, touched.iter())
        });
        let recipes =
            crate::profile::phase("reanalyze: recipes", || derive::Recipes::new(db, scopes));
        self.engine.open(db);
        self.pass = Some(Pass::Open(recipes));
    }

    /// Leaves the index as it reads outside any run until the next slice.
    fn pause(&mut self, db: &mut DbIndex) {
        self.engine.flush_slot_notifications(db);
        self.stage = db.analysis_stage();
        db.set_analysis_stage(derivation::AnalysisStage::Closed);
        crate::profile::phase("reanalyze: flush", || db.flush_inference_derived_state());
    }

    /// Settles `file_ids` before the other work still queued.
    pub(crate) fn prioritise(&mut self, file_ids: &[FileId]) {
        self.engine.prioritise(file_ids);
    }

    /// Whether `file_id` has work still to settle. Until a pass opens, what
    /// its rebuild moved has not queued its readers, so every file may.
    pub(crate) fn is_settling(&self, file_id: FileId) -> bool {
        matches!(self.pass, Some(Pass::Replaced { .. })) || self.engine.is_settling(file_id)
    }

    /// Whether `file_id` still settles ahead of the rest.
    pub(crate) fn in_focus(&self, file_id: FileId) -> bool {
        self.engine.in_focus(file_id)
    }

    /// Wraps up a settled re-analysis.
    pub(crate) fn finish(self, db: &mut DbIndex) -> Reanalysed {
        debug_assert!(self.pass.is_none(), "finishing an unsettled re-analysis");
        crate::profile::phase("reanalyze: payload refs", || {
            refresh_payload_refs(db, self.engine.payload_holders())
        });
        call_target::check_settled_call_targets(db);
        crate::profile::phase_report("reanalyze");
        let stats = self.engine.stats().clone();
        let evaluated_files = self.engine.evaluated_files();
        stats.report(evaluated_files.len());
        if crate::profile::profiling() {
            for (kind, query_stats) in db.queries().stats() {
                eprintln!("[query] {kind:?} {query_stats:?}");
            }
        }
        crate::profile::memory::checkpoint("end of re-analysis");
        let stale_diagnostics = crate::profile::phase("reanalyze: stale diagnostics", || {
            self.engine.stale_diagnostics(db)
        });
        Reanalysed {
            stale_diagnostics,
            evaluated_files,
            stats,
            scopes: self.scopes,
            memos: self.memos,
        }
    }
}

impl std::fmt::Debug for Reanalysis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reanalysis")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

/// A pass of a re-analysis: some files' source layers rebuilt, then their
/// derivations and whatever read what the rebuild moved evaluated.
enum Pass {
    /// The source layers are rebuilt; the readers of `touched` are queued
    /// when the pass opens, at its first slice.
    Replaced {
        touched: HashSet<derivation::HashedKey>,
        scopes: ScriptedScopes,
    },
    Open(derive::Recipes),
}

/// Stops keeping access paths if a slice unwinds.
struct Unwinding;

impl Drop for Unwinding {
    fn drop(&mut self) {
        crate::semantic::stop_keeping_access_paths();
    }
}

/// Rebuilds `file_ids`' source layers and queues their derivations ahead of
/// the work already queued, alongside any files still settling first; `None`
/// when there is nothing to re-analyse.
fn start_pass(
    db: &mut DbIndex,
    engine: &mut engine::Engine,
    mut file_ids: Vec<FileId>,
    scopes: &mut ScopesCache,
) -> Option<Pass> {
    file_ids.sort();
    file_ids.dedup();
    if file_ids.is_empty() {
        return None;
    }
    let scopes = crate::profile::phase("reanalyze: scopes", || scopes.get(db));
    // Queries computed since the last run read the index as it stood before
    // the rebuild moves it.
    let moved = engine::file_queries(db);
    engine.replacing(&file_ids);
    engine.note_rebuilt(&file_ids);
    engine.prioritise(&file_ids);
    crate::profile::memory::checkpoint("before the source layer");
    let mut touched = crate::profile::phase("reanalyze: replace files", || {
        replace_files(db, engine, &file_ids, &scopes)
    });
    touched.extend(moved);
    crate::profile::memory::checkpoint("after the source layer");
    db.reader_graph_mut().forget_fingerprints();
    Some(Pass::Replaced { touched, scopes })
}

/// Forgets `file_ids`' derivations and facts, rebuilds the live ones' source
/// layer and queues their derivations. Returns every key whose value may
/// have changed.
fn replace_files(
    db: &mut DbIndex,
    engine: &mut engine::Engine,
    file_ids: &[FileId],
    scopes: &ScriptedScopes,
) -> HashSet<derivation::HashedKey> {
    let mut touched = HashSet::default();
    touched.insert(derivation::HashedKey::new(derivation::FactKey::FileSet));
    for file_id in file_ids {
        touched.insert(derivation::HashedKey::new(derivation::FactKey::FileText(
            *file_id,
        )));
        let live = db.get_vfs().get_syntax_tree(file_id).is_some();
        for producer in db.reader_graph().foreign_producers(*file_id) {
            if file_ids.contains(&producer.file_id) {
                continue;
            }
            // What it stored in a file that is re-indexed, not deleted, the
            // indexes keep, and the edit remap carries to its new place.
            if live
                && db
                    .reader_graph()
                    .outputs_of(producer)
                    .filter(|key| key.owning_file() == Some(*file_id))
                    .all(|key| key.keeps_foreign_contributions())
            {
                continue;
            }
            if crate::profile::tracing_evaluations() {
                let kept = db
                    .reader_graph()
                    .outputs_of(producer)
                    .filter(|key| {
                        key.owning_file() == Some(*file_id) && !key.keeps_foreign_contributions()
                    })
                    .map(|key| format!("{:?}", key.key()))
                    .collect::<Vec<_>>();
                eprintln!("[trace] foreign producer {producer:?} of {file_id:?} for {kept:?}");
            }
            match producer.kind {
                derivation::DerivationKind::Source => engine.source_stale(producer.file_id),
                _ => engine.changed(producer),
            }
        }
        if db
            .reader_graph()
            .derivations_of_file(*file_id)
            .next()
            .is_some()
        {
            engine.rebuilding(*file_id);
        }
        let outputs = db
            .reader_graph()
            .derivations_of_file(*file_id)
            .flat_map(|id| {
                db.reader_graph()
                    .outputs_of(id)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect::<HashSet<_>>();
        for key in outputs {
            let fingerprint = db.fact_fingerprint(&key);
            engine.note_removed_output(key, fingerprint);
        }
        let removed = db.reader_graph_mut().remove_file(*file_id);
        for (_, outputs) in removed {
            for key in outputs.iter() {
                if key.is_primary() && key.owning_file() != Some(*file_id) {
                    touched.extend(derivation::capture_writes(|| db.reset_fact(key)));
                    for producer in db.reader_graph().producers(key).collect::<Vec<_>>() {
                        if producer.kind != derivation::DerivationKind::Source {
                            engine.changed(producer);
                        }
                    }
                }
            }
            touched.extend(outputs.into_vec());
        }
    }
    // What removal takes out, read off the indexes as they drop it: a
    // recorded output can name a slot the index has since re-homed.
    derivation::begin(derivation::DerivationId::workspace(
        derivation::DerivationKind::Source,
    ));
    db.remove_index(file_ids.to_vec());
    let removal = derivation::end();
    touched.extend(removal.moved().cloned());
    let (_, _, removed) = removal.into_parts(|_| derivation::ABSENT);
    touched.extend(removed);

    let live = file_ids
        .iter()
        .copied()
        .filter(|file_id| db.get_vfs().get_syntax_tree(file_id).is_some())
        .collect::<Vec<_>>();
    for file_id in &live {
        db.get_file_dependencies_index_mut()
            .relink_unresolved_target(*file_id);
    }
    // Every file registers its module before any file declares, so a
    // dependency call resolves against the whole workspace's modules, and
    // declares before any file documents, so doc annotations resolve against
    // the whole workspace's declarations, whatever the order.
    let registered = live
        .iter()
        .map(|file_id| (*file_id, register_source(db, *file_id)))
        .collect::<Vec<_>>();
    // Each batch's declarations are walked together on the pool, outside
    // every recording evaluation, then written file by file in canonical
    // order, each inside its file's source evaluation.
    let batch = parallel::analysis_threads().saturating_mul(SOURCE_TAILS_PER_THREAD);
    let mut declared = Vec::with_capacity(registered.len());
    let mut registered = registered.into_iter();
    loop {
        let declaring = registered.by_ref().take(batch).collect::<Vec<_>>();
        if declaring.is_empty() {
            break;
        }
        let file_ids = declaring
            .iter()
            .map(|(file_id, _)| *file_id)
            .collect::<Vec<_>>();
        debug_assert!(!derivation::recording(), "declarations walk unrecorded");
        let walks = crate::profile::phase("declaration walks", || {
            parallel::map_files_collect(db, &file_ids, |db, file_id| {
                decl::walk_file(db, file_id, scopes)
            })
        });
        for ((file_id, evaluation), walk) in declaring.into_iter().zip(walks) {
            declared.push((
                file_id,
                declare_source(db, file_id, evaluation, scopes, walk),
            ));
        }
    }
    // Each batch's tails are computed together on the pool, outside every
    // recording evaluation, then published file by file in canonical order.
    let mut declared = declared.into_iter();
    loop {
        let documented = declared.by_ref().take(batch).collect::<Vec<_>>();
        if documented.is_empty() {
            break;
        }
        let file_ids = documented
            .iter()
            .map(|(file_id, _)| *file_id)
            .collect::<Vec<_>>();
        debug_assert!(!derivation::recording(), "source tails compute unrecorded");
        let tails = crate::profile::phase("source tails", || {
            parallel::map_files_collect(db, &file_ids, source_tail)
        });
        for ((file_id, (evaluation, context)), tail) in documented.into_iter().zip(tails) {
            let (outputs, derivations) = document_source(db, file_id, evaluation, context, tail);
            touched.extend(outputs);
            for id in derivations {
                engine.changed(id);
            }
        }
    }
    if let Some(edits) = engine.edits_to_apply() {
        engine.follow_removed_outputs(&edits);
        touched = crate::profile::phase("reanalyze: follow edits", || {
            follow_edits(db, engine, &edits, touched)
        });
    }
    for kind in derive::WORKSPACE_DERIVATIONS {
        let id = derivation::DerivationId::workspace(kind);
        if !db.reader_graph().contains(id) {
            engine.changed(id);
        }
    }
    touched
}

/// Carries every other file's reference to a position the edits moved to
/// its new place: stored facts through the index remaps, recorded reads
/// through the reader graph. Whatever names a position an edit destroyed is
/// evaluated again. Returns `touched` with each key also at its new position.
fn follow_edits(
    db: &mut DbIndex,
    engine: &mut engine::Engine,
    edits: &derivation::EditMaps,
    touched: HashSet<derivation::HashedKey>,
) -> HashSet<derivation::HashedKey> {
    let mut files = edits.files().collect::<Vec<_>>();
    files.sort();
    for file_id in files {
        let Some(map) = edits.map(file_id) else {
            continue;
        };
        let remap = crate::profile::phase("follow edits: file facts", || {
            crate::FileRemap::validated(db, file_id, map)
        });
        // A key naming a position the edit destroyed is taken again by the
        // derivation that wrote the member.
        for member_id in db.get_member_index_mut().remap_expr_keys(&remap) {
            let key = derivation::HashedKey::new(derivation::FactKey::Member(member_id));
            for producer in db.reader_graph().producers(&key).collect::<Vec<_>>() {
                rerun(engine, producer);
            }
        }
        let mut lost = crate::profile::phase("follow edits: remap indexes", || {
            crate::remap_into_file(db, &remap)
        })
        .into_iter()
        .collect::<Vec<_>>();
        lost.sort();
        if crate::profile::tracing_evaluations() {
            eprintln!("[trace] remap of {file_id:?} lost positions in {lost:?}");
        }
        for lost_file in lost {
            let ids = db
                .reader_graph()
                .derivations_of_file(lost_file)
                .collect::<Vec<_>>();
            for id in ids {
                rerun(engine, id);
            }
        }
    }
    let applied = crate::profile::phase("follow edits: rename reads", || {
        db.reader_graph_mut().apply_edits(edits)
    });
    for reader in applied.lost {
        if crate::profile::tracing_evaluations() {
            eprintln!("[trace] a read of {reader:?} names a destroyed position");
        }
        rerun(engine, reader);
    }
    // A moved read holds what it saw only with the edits mapped back; as a
    // touched key the run checks it and re-records it as the value now. No
    // derivation writes some of them, such as a statement's syntax.
    touched
        .into_iter()
        .flat_map(|key| {
            let moved = edits
                .map_key(&key)
                .filter(|moved| moved != key.key())
                .map(derivation::HashedKey::new);
            std::iter::once(key).chain(moved)
        })
        .chain(applied.moved)
        .collect()
}

/// Re-takes which files the signatures of every file whose signatures moved
/// name, so the next edit's remap finds them.
fn refresh_payload_refs(db: &mut DbIndex, holders: impl Iterator<Item = FileId>) {
    for holder in holders {
        db.get_signature_index_mut().snapshot_payload_refs(holder);
    }
}

fn rerun(engine: &mut engine::Engine, id: derivation::DerivationId) {
    match id.kind {
        derivation::DerivationKind::Source => engine.source_stale(id.file_id),
        derivation::DerivationKind::Diagnostics => engine.check_diagnostics(id.file_id),
        _ => engine.changed(id),
    }
}

/// The first stage of `file_id`'s source layer: module registration.
fn register_source(db: &mut DbIndex, file_id: FileId) -> derivation::Evaluation {
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .expect("a live file has a syntax tree")
        .get_red_root();
    derivation::begin(derivation::DerivationId::new(
        derivation::DerivationKind::Source,
        file_id,
        LuaSyntaxId::from_node(&root),
    ));
    if let Some(path) = db.get_vfs().get_file_path(&file_id).cloned()
        && let Some(path) = path.to_str()
    {
        db.get_module_index_mut().add_module_by_path(file_id, path);
    }
    let mut evaluation = derivation::end();
    evaluation.fingerprint_stage(|key| db.fact_fingerprint(key));
    evaluation
}

/// The second stage of a file's source layer: declarations.
fn declare_source(
    db: &mut DbIndex,
    file_id: FileId,
    evaluation: derivation::Evaluation,
    scopes: &ScriptedScopes,
    walk: decl::DeclWalk,
) -> (derivation::Evaluation, AnalyzeContext) {
    derivation::resume(evaluation);
    let mut context = AnalyzeContext::for_files(db, vec![file_id], scopes);
    crate::profile::phase("declaration writes", || walk.apply(db, &mut context));
    let mut evaluation = derivation::end();
    evaluation.fingerprint_stage(|key| db.fact_fingerprint(key));
    (evaluation, context)
}

/// How many files' declaration walks and source tails [`replace_files`]
/// computes per analysis thread at once: enough to keep the pool busy, few
/// enough that the batch's results are published before the next batch's are
/// built.
const SOURCE_TAILS_PER_THREAD: usize = 16;

/// The part of a file's source layer that its syntax and its own references
/// and declarations decide, which docs leave as they are: computed for many
/// files at once off the canonical thread, published by [`document_source`].
struct SourceTail {
    flow_tree: FlowTree,
    errors: Vec<AnalyzeError>,
    summaries: Option<(
        Arc<function_summary::FileFunctionSummaries>,
        derivation::Fingerprint,
    )>,
    role_mentions: Option<Arc<[smol_str::SmolStr]>>,
    syntax_derivations: Vec<derivation::DerivationId>,
}

/// `file_id`'s source tail, once every file has declared. Reads only facts of
/// `file_id`'s own source layer, which its source evaluation does not record.
fn source_tail(db: &DbIndex, file_id: FileId) -> SourceTail {
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .expect("a live file has a syntax tree")
        .get_chunk_node();
    let (flow_tree, errors) = flow::bind_flow_tree_for_file(db, file_id, root.clone());
    SourceTail {
        flow_tree,
        errors,
        summaries: function_summary::collect_function_summaries(db, file_id, &root),
        role_mentions: gmod::collect_role_mentions(db, file_id, &root),
        syntax_derivations: derive::syntax_derivations(file_id, &root),
    }
}

/// The last stage of a file's source layer: docs, callable signatures,
/// early member owners, guard candidates, then its source tail. Returns what
/// the whole source layer wrote and the file's other derivations.
fn document_source(
    db: &mut DbIndex,
    file_id: FileId,
    evaluation: derivation::Evaluation,
    mut context: AnalyzeContext,
    tail: SourceTail,
) -> (Vec<derivation::HashedKey>, Vec<derivation::DerivationId>) {
    derivation::resume(evaluation);
    run_analysis::<doc::DocAnalysisPipeline>(db, &mut context);
    publish_callable_signatures(db, &context);
    resolve_early_member_owners(db, &mut context);
    local_inference::prepare_inferred_positive_guards(db, &context);
    db.get_flow_index_mut()
        .add_flow_tree(file_id, tail.flow_tree);
    if !tail.errors.is_empty() {
        let diagnostic_index = db.get_diagnostic_index_mut();
        for error in tail.errors {
            diagnostic_index.add_diagnostic(file_id, error);
        }
    }
    if let Some((summaries, fingerprint)) = tail.summaries {
        db.set_function_summaries(file_id, summaries, fingerprint);
    }
    if let Some(mentions) = tail.role_mentions {
        db.set_role_mentions(file_id, mentions);
    }
    let evaluation = derivation::end();
    let moved = evaluation.moved().cloned().collect::<Vec<_>>();
    let (id, reads, outputs) = evaluation.into_parts(|key| db.fact_fingerprint(key));
    let reads = reads
        .into_iter()
        .map(|(key, seen)| (key, Some(seen)))
        .collect();
    db.reader_graph_mut()
        .replace(id, reads, outputs.clone(), |_| derivation::ABSENT);
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .expect("a live file has a syntax tree")
        .get_chunk_node();
    let derivations = derive::file_derivations(
        file_id,
        &root,
        tail.syntax_derivations,
        &context.inferred_guard_candidates,
    );
    (outputs.into_iter().chain(moved).collect(), derivations)
}

fn assert_send<T: Send>() {}

const _: fn() = assert_send::<crate::LuaInferCache>;

fn publish_callable_signatures(db: &mut DbIndex, context: &AnalyzeContext) -> usize {
    let mut published = 0;
    for (type_owner, signature_id) in &context.early_callable_signatures {
        write_type_cache(
            db,
            type_owner.clone(),
            LuaTypeCache::InferType(LuaType::Signature(*signature_id)),
            TypeCacheWriteMode::InsertOnly,
        );
        published += 1;
    }
    published
}

fn resolve_early_member_owners(db: &mut DbIndex, context: &mut AnalyzeContext) -> usize {
    let mut resolved = 0;
    for (member_id, decl_id) in std::mem::take(&mut context.early_member_owner_candidates) {
        let Some(type_cache) = db.get_type_index().get_type_cache(&decl_id.into()) else {
            continue;
        };
        if !type_cache.is_doc() {
            continue;
        }
        let type_id = match type_cache.as_type() {
            LuaType::Def(type_id) | LuaType::Ref(type_id) => type_id.clone(),
            _ => continue,
        };
        common::add_member(db, LuaMemberOwner::Type(type_id), member_id);
        resolved += 1;
    }
    resolved
}

/// Whether an initializer's type is decided by a call or index read.
///
/// `or`, `and` and parentheses take their type from an operand, so they inherit
/// exactly the same sensitivity to what the batch has indexed so far while
/// hiding it behind a different syntax node.
pub(crate) fn initializer_reads_through_call_or_index(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::CallExpr(_) | LuaExpr::IndexExpr(_) => true,
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|inner| initializer_reads_through_call_or_index(&inner)),
        // `CT + self.a / self.b` reads through the index reads it combines
        // exactly as `self.a or self.b` does: the operator adds no fact of its
        // own, so the answer is a placeholder until those reads settle.
        LuaExpr::BinaryExpr(binary) => binary.get_exprs().is_some_and(|(left, right)| {
            initializer_reads_through_call_or_index(&left)
                || initializer_reads_through_call_or_index(&right)
        }),
        LuaExpr::UnaryExpr(unary) => unary
            .get_expr()
            .is_some_and(|inner| initializer_reads_through_call_or_index(&inner)),
        _ => false,
    }
}

/// Whether an initializer's uninformative result may still improve once the
/// unresolve pass settles what it reads.
///
/// An operator expression contributes no type of its own: `w - 1` is `unknown`
/// only while `w` is, so the answer the file walk cached is a placeholder in
/// exactly the way a call or index read is, and it has to be retried on the same
/// terms. Without this it stays `unknown` forever and usage-context inference
/// guesses at it instead.
pub(crate) fn initializer_may_improve_after_resolve(expr: &LuaExpr) -> bool {
    initializer_reads_through_call_or_index(expr)
        || initializer_is_operator_expr(expr)
        || matches!(expr, LuaExpr::NameExpr(_))
}

pub(crate) fn initializer_is_operator_expr(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::BinaryExpr(_) | LuaExpr::UnaryExpr(_) => true,
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|inner| initializer_is_operator_expr(&inner)),
        _ => false,
    }
}

pub(crate) fn local_initializer_expr(
    db: &DbIndex,
    root: &LuaSyntaxNode,
    decl_id: LuaDeclId,
) -> Option<(usize, LuaExpr)> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    let node = initializer.get_expr_syntax_id().to_node_from_root(root)?;
    Some((initializer.get_ret_idx(), LuaExpr::cast(node)?))
}

fn type_is_uninformative(typ: &LuaType) -> bool {
    !crate::db_index::is_informative_type(typ) || crate::db_index::leaks_unsubstituted_tpl(typ)
}

fn synthesize_accessorfunc_members(db: &mut DbIndex, file_ids: &[FileId]) {
    for &file_id in file_ids {
        let file_calls = db.get_accessor_func_call_index().calls_in(file_id).to_vec();
        for call in file_calls {
            if call.accessor_name.is_empty() {
                continue;
            }

            let setter_member_id = LuaMemberId::new(call.syntax_id, file_id);

            let owner = LuaMemberOwner::Type(call.owner_type_id.clone());

            let getter_name = format!("Get{}", call.accessor_name);
            let getter_func =
                LuaFunctionType::new(AsyncState::None, true, false, vec![], LuaType::Any);
            let getter_syntax_id = call.name_arg_syntax_id.unwrap_or(call.syntax_id);
            let getter_member_id = LuaMemberId::new(getter_syntax_id, file_id);
            let getter_member = LuaMember::new(
                getter_member_id,
                LuaMemberKey::Name(getter_name.as_str().into()),
                LuaMemberFeature::FileMethodDecl,
                None,
            );
            db.get_member_index_mut()
                .add_member(owner.clone(), getter_member);
            write_type_cache(
                db,
                getter_member_id.into(),
                LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
                TypeCacheWriteMode::InsertOnly,
            );

            let setter_name = format!("Set{}", call.accessor_name);
            let setter_func = LuaFunctionType::new(
                AsyncState::None,
                true,
                false,
                vec![("value".to_string(), Some(LuaType::Any))],
                LuaType::Nil,
            );
            let setter_member = LuaMember::new(
                setter_member_id,
                LuaMemberKey::Name(setter_name.as_str().into()),
                LuaMemberFeature::FileMethodDecl,
                None,
            );
            db.get_member_index_mut().add_member(owner, setter_member);
            write_type_cache(
                db,
                setter_member_id.into(),
                LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
                TypeCacheWriteMode::InsertOnly,
            );
        }
    }
}

trait AnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext);
}

fn run_analysis<T: AnalysisPipeline>(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let name = std::any::type_name::<T>()
        .rsplit("::")
        .next()
        .unwrap_or_default();
    if context.tree_list.len() > 1 {
        crate::progress::enter_phase(
            crate::progress::phase_label(name),
            context.tree_list.len(),
            "files",
        );
    }
    // Timed through the phase accumulator rather than a `Profile`: several
    // pipelines already carry their own `Profile`, and an unconditional one
    // here would add a log line per pipeline per batch on a live server.
    crate::profile::phase(name, || T::analyze(db, context));
    crate::profile::phase_report(name);
}

#[derive(Debug)]
pub struct AnalyzeContext {
    tree_list: Vec<InFiled<LuaChunk>>,
    metas: HashSet<FileId>,
    scripted_scope_files: Option<Arc<HashSet<FileId>>>,
    scripted_scope_infos: Option<Arc<HashMap<FileId, GmodScopedClassInfo>>>,
    unresolves: Vec<(UnResolve, InferFailReason)>,
    pending_unresolve_decl_ids: HashSet<LuaDeclId>,
    infer_manager: InferCacheManager,
    inferred_guard_candidates: Vec<InFiled<LuaSyntaxId>>,
    early_callable_signatures: Vec<(crate::LuaTypeOwner, LuaSignatureId)>,
    early_member_owner_candidates: Vec<(LuaMemberId, LuaDeclId)>,
    pub workspace_id: Option<WorkspaceId>,
}

impl AnalyzeContext {
    pub fn new() -> Self {
        Self {
            tree_list: Vec::new(),
            metas: HashSet::default(),
            scripted_scope_files: None,
            scripted_scope_infos: None,
            unresolves: Vec::new(),
            pending_unresolve_decl_ids: HashSet::default(),
            infer_manager: InferCacheManager::new(),
            inferred_guard_candidates: Vec::new(),
            early_callable_signatures: Vec::new(),
            early_member_owner_candidates: Vec::new(),
            workspace_id: None,
        }
    }

    /// A context for evaluating derivations of `file_ids` against the settled
    /// index: every fact source visible, unresolved reads floored.
    pub(crate) fn for_files(db: &DbIndex, file_ids: Vec<FileId>, scopes: &ScriptedScopes) -> Self {
        let mut context = Self::new();
        context.scripted_scope_files = Some(scopes.files.clone());
        context.scripted_scope_infos = Some(scopes.infos.clone());
        for file_id in file_ids {
            let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
                continue;
            };
            if db.get_module_index().is_meta_file(&file_id) {
                context.add_meta(file_id);
            }
            if context.workspace_id.is_none() {
                context.workspace_id = db.get_module_index().get_workspace_id(file_id);
            }
            context.add_tree_chunk(InFiled::new(file_id, tree.get_chunk_node()));
        }
        context.infer_manager.set_force();
        context.infer_manager.set_dynamic_fields_visible();
        context.infer_manager.set_call_site_params_visible();
        context
    }

    pub fn add_meta(&mut self, file_id: FileId) {
        self.metas.insert(file_id);
    }

    pub fn add_tree_chunk(&mut self, tree: InFiled<LuaChunk>) {
        self.tree_list.push(tree);
    }

    pub fn add_unresolve(&mut self, un_resolve: UnResolve, reason: InferFailReason) {
        if let UnResolve::Decl(decl) = &un_resolve {
            self.pending_unresolve_decl_ids.insert(decl.decl_id);
        }
        self.unresolves.push((un_resolve, reason));
    }

    pub fn add_inferred_guard_candidate(&mut self, candidate: InFiled<LuaSyntaxId>) {
        self.inferred_guard_candidates.push(candidate);
    }

    pub fn add_early_callable_signature(
        &mut self,
        type_owner: crate::LuaTypeOwner,
        signature_id: LuaSignatureId,
    ) {
        self.early_callable_signatures
            .push((type_owner, signature_id));
    }

    pub fn add_early_member_owner_candidate(&mut self, member_id: LuaMemberId, decl_id: LuaDeclId) {
        self.early_member_owner_candidates
            .push((member_id, decl_id));
    }

    pub fn has_pending_decl_unresolve(&self, decl_id: LuaDeclId) -> bool {
        self.pending_unresolve_decl_ids.contains(&decl_id)
    }
    pub fn get_or_compute_scripted_scope_files(&mut self, db: &DbIndex) -> Arc<HashSet<FileId>> {
        self.ensure_scripted_scope_cache(db);

        self.scripted_scope_files
            .as_ref()
            .expect("set above")
            .clone()
    }

    pub fn get_or_compute_scripted_scope_infos(
        &mut self,
        db: &DbIndex,
    ) -> Arc<HashMap<FileId, GmodScopedClassInfo>> {
        self.ensure_scripted_scope_cache(db);

        self.scripted_scope_infos
            .as_ref()
            .expect("set above")
            .clone()
    }

    fn ensure_scripted_scope_cache(&mut self, db: &DbIndex) {
        if self.scripted_scope_files.is_some() && self.scripted_scope_infos.is_some() {
            return;
        }

        let scopes = &db.get_emmyrc().gmod.scripted_class_scopes;
        if scopes.resolved_definitions_slice().is_empty() {
            let file_ids = self
                .tree_list
                .iter()
                .map(|in_filed_tree| in_filed_tree.file_id)
                .collect::<HashSet<_>>();
            self.scripted_scope_files = Some(Arc::new(file_ids));
            self.scripted_scope_infos = Some(Arc::new(HashMap::default()));
            return;
        }

        let file_paths = self
            .tree_list
            .iter()
            .filter_map(|in_filed_tree| {
                db.get_vfs()
                    .get_file_path(&in_filed_tree.file_id)
                    .map(|path| (in_filed_tree.file_id, path.as_path()))
            })
            .collect::<Vec<_>>();
        let (scripted_scope_files, scoped_matches) =
            scopes.scan_scripted_class_scope_files(file_paths.iter().copied());
        let mut scripted_scope_files: HashSet<_> = scripted_scope_files.into_iter().collect();
        let mut scripted_scope_infos = scoped_matches
            .into_iter()
            .map(|(file_id, scope_match)| {
                (
                    file_id,
                    GmodScopedClassInfo {
                        class_name: scope_match.class_name,
                        global_name: scope_match.definition.class_global,
                        is_global_singleton: scope_match.definition.is_global_singleton,
                        aliases: scope_match.definition.aliases,
                        super_types: scope_match.definition.super_types,
                        class_name_prefix: scope_match.definition.class_name_prefix,
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        // A workspace rooted at a gamemode checkout has `gamemode/` directly
        // under the root — no `gamemodes/<name>` segment for the scope path
        // rules to anchor on — but its `gamemode/**` files author `GM` just
        // like `gamemodes/*/gamemode/**`. Scope them the same way.
        let root_gamemode = file_paths
            .first()
            .and_then(|(file_id, _)| gmod::detect_root_gamemode_scope(db, *file_id));
        if let Some((gamemode_dir, scope_match)) = root_gamemode {
            for (file_id, path) in &file_paths {
                if scripted_scope_infos.contains_key(file_id) || !path.starts_with(&gamemode_dir) {
                    continue;
                }
                scripted_scope_files.insert(*file_id);
                scripted_scope_infos.insert(
                    *file_id,
                    GmodScopedClassInfo {
                        class_name: scope_match.class_name.clone(),
                        global_name: scope_match.global_name.clone(),
                        is_global_singleton: scope_match.is_global_singleton,
                        aliases: scope_match.aliases.clone(),
                        super_types: scope_match.super_types.clone(),
                        class_name_prefix: scope_match.class_name_prefix.clone(),
                    },
                );
            }
        }

        self.scripted_scope_files = Some(Arc::new(scripted_scope_files));
        self.scripted_scope_infos = Some(Arc::new(scripted_scope_infos));
    }
}

#[cfg(test)]
mod union_widening_tests {
    use super::union_widens_cached_type;
    use crate::LuaType;

    fn union(arms: Vec<LuaType>) -> LuaType {
        LuaType::from_vec(arms)
    }

    #[test]
    fn widens_a_cached_union_the_settled_one_contains() {
        let settled = union(vec![
            LuaType::IntegerConst(4),
            LuaType::IntegerConst(5),
            LuaType::Number,
            LuaType::Any,
        ]);
        let cached = union(vec![
            LuaType::IntegerConst(4),
            LuaType::IntegerConst(5),
            LuaType::Any,
        ]);
        assert!(union_widens_cached_type(&settled, &cached));
    }

    #[test]
    fn widens_a_cached_single_arm() {
        let settled = union(vec![LuaType::Number, LuaType::Any]);
        assert!(union_widens_cached_type(&settled, &LuaType::Number));
    }

    #[test]
    fn rejects_a_cached_union_with_an_arm_the_settled_one_lacks() {
        let settled = union(vec![
            LuaType::IntegerConst(4),
            LuaType::Number,
            LuaType::Any,
        ]);
        let cached = union(vec![LuaType::IntegerConst(4), LuaType::String]);
        assert!(!union_widens_cached_type(&settled, &cached));
    }

    #[test]
    fn rejects_an_equal_union() {
        let settled = union(vec![LuaType::Number, LuaType::Any]);
        let cached = union(vec![LuaType::Number, LuaType::Any]);
        assert!(!union_widens_cached_type(&settled, &cached));
    }

    #[test]
    fn rejects_a_non_union_settled_type() {
        assert!(!union_widens_cached_type(
            &LuaType::Number,
            &LuaType::Number
        ));
    }
}
