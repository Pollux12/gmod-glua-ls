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
pub(crate) use common::{
    is_falsy_literal_expr, is_nil_literal_expr, is_presence_tested, union_widens_cached_type,
    widen_mutable_decl_literal,
};
pub(crate) use dynamic_field::is_provably_builtin_iterator_call;
pub(crate) use dynamic_field::{
    copies_table_into_itself, for_range_pairs_source_for_var, writes_own_pairs_key,
};
pub(crate) use engine::EngineStats;
#[cfg(test)]
pub(crate) use engine::serial_engine;
pub use engine::{Schedule, Slice};
pub(crate) use local_inference::{
    SubtypesWithMember, class_union_arms, class_union_arms_with_member,
    is_guarded_by_enclosing_condition, is_member_path_presence_guarded, unguarded_child_candidates,
};
pub use lua::is_member_assignment_in_conditional_branch;
pub(crate) use lua::{
    dominating_guarded_table_bootstrap_range, infer_for_range_iter_expr_func,
    is_guarded_table_definition_site,
};

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::path::{Path, PathBuf};
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
    #[cfg(test)]
    fn new(db: &DbIndex) -> Self {
        Self::of(db, &live_paths(db), &root_gamemodes(db))
    }

    /// The scopes of the live files at `paths`, given the gamemode folders
    /// the main workspaces hold.
    fn of(
        db: &DbIndex,
        paths: &HashMap<FileId, Option<PathBuf>>,
        gamemodes: &[(PathBuf, GmodScopedClassInfo)],
    ) -> Self {
        let files = paths
            .iter()
            .map(|(file_id, path)| (*file_id, path.as_deref()))
            .collect::<Vec<_>>();
        let (files, infos) = scan_scopes(db, &files, gamemodes);
        Self {
            files: Arc::new(files),
            infos: Arc::new(infos),
        }
    }

    /// The files `self` and `other` scope differently, sorted.
    fn rescoped(&self, other: &Self) -> Vec<FileId> {
        let mut files = self
            .files
            .symmetric_difference(&other.files)
            .chain(
                self.infos
                    .keys()
                    .chain(other.infos.keys())
                    .filter(|file_id| self.infos.get(*file_id) != other.infos.get(*file_id)),
            )
            .copied()
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        files
    }
}

/// Which of `files` are scripted-class scopes, and the class info of those
/// that author one, given the gamemode folders the main workspaces hold.
fn scan_scopes(
    db: &DbIndex,
    files: &[(FileId, Option<&Path>)],
    gamemodes: &[(PathBuf, GmodScopedClassInfo)],
) -> (HashSet<FileId>, HashMap<FileId, GmodScopedClassInfo>) {
    let scopes = &db.get_emmyrc().gmod.scripted_class_scopes;
    if scopes.resolved_definitions_slice().is_empty() {
        let files = files.iter().map(|(file_id, _)| *file_id).collect();
        return (files, HashMap::default());
    }
    let paths = files
        .iter()
        .filter_map(|(file_id, path)| Some((*file_id, (*path)?)))
        .collect::<Vec<_>>();
    let (scope_files, scope_matches) =
        scopes.scan_scripted_class_scope_files(paths.iter().copied());
    let mut scope_files: HashSet<_> = scope_files.into_iter().collect();
    let mut infos = scope_matches
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
    for (gamemode_dir, info) in gamemodes {
        for (file_id, path) in &paths {
            if infos.contains_key(file_id) || !path.starts_with(gamemode_dir) {
                continue;
            }
            scope_files.insert(*file_id);
            infos.insert(*file_id, info.clone());
        }
    }
    (scope_files, infos)
}

/// The gamemode folders the main workspaces hold, each with the class its
/// files author, in workspace order.
fn root_gamemodes(db: &DbIndex) -> Vec<(PathBuf, GmodScopedClassInfo)> {
    db.get_module_index()
        .get_main_workspace_ids()
        .into_iter()
        .filter_map(|workspace_id| gmod::root_gamemode_scope(db, workspace_id))
        .map(|(gamemode_dir, scope_match)| {
            (
                gamemode_dir,
                GmodScopedClassInfo {
                    class_name: scope_match.class_name,
                    global_name: scope_match.global_name,
                    is_global_singleton: scope_match.is_global_singleton,
                    aliases: scope_match.aliases,
                    super_types: scope_match.super_types,
                    class_name_prefix: scope_match.class_name_prefix,
                },
            )
        })
        .collect()
}

/// Each live file's path.
fn live_paths(db: &DbIndex) -> HashMap<FileId, Option<PathBuf>> {
    let vfs = db.get_vfs();
    vfs.get_all_local_file_ids()
        .into_iter()
        .filter(|file_id| vfs.get_syntax_tree(file_id).is_some())
        .map(|file_id| (file_id, vfs.get_file_path(&file_id).cloned()))
        .collect()
}

/// The main workspaces with their roots, by id.
fn main_workspace_roots(db: &DbIndex) -> Vec<(WorkspaceId, Option<PathBuf>)> {
    let module_index = db.get_module_index();
    let mut workspaces = module_index
        .get_main_workspace_ids()
        .into_iter()
        .map(|id| (id, module_index.get_workspace_root(id).map(Into::into)))
        .collect::<Vec<_>>();
    workspaces.sort_by_key(|(id, _)| id.id);
    workspaces
}

/// [`ScriptedScopes`] with what they were computed from besides the config:
/// each live file's path, the main workspaces and their gamemode folders.
#[derive(Clone)]
struct HeldScopes {
    paths: HashMap<FileId, Option<PathBuf>>,
    workspaces: Vec<(WorkspaceId, Option<PathBuf>)>,
    gamemodes: Vec<(PathBuf, GmodScopedClassInfo)>,
    scopes: ScriptedScopes,
}

impl HeldScopes {
    /// Takes in the live files' paths as they stand, scanning again the files
    /// that moved, came or went: a file's scope follows from its own path.
    /// Returns those it scopes otherwise, sorted.
    fn follow_paths(&mut self, db: &DbIndex) -> Vec<FileId> {
        let vfs = db.get_vfs();
        let mut live = HashSet::default();
        let mut changed = Vec::new();
        for file_id in vfs.get_all_local_file_ids() {
            if vfs.get_syntax_tree(&file_id).is_none() {
                continue;
            }
            live.insert(file_id);
            let path = vfs.get_file_path(&file_id);
            if self
                .paths
                .get(&file_id)
                .is_none_or(|held| held.as_ref() != path)
            {
                self.paths.insert(file_id, path.cloned());
                changed.push(file_id);
            }
        }
        self.paths.retain(|file_id, _| {
            let kept = live.contains(file_id);
            if !kept {
                changed.push(*file_id);
            }
            kept
        });
        if changed.is_empty() {
            return Vec::new();
        }
        changed.sort_unstable();
        let before = changed
            .iter()
            .map(|file_id| {
                (
                    self.scopes.files.contains(file_id),
                    self.scopes.infos.get(file_id).cloned(),
                )
            })
            .collect::<Vec<_>>();
        let scanned = changed
            .iter()
            .filter_map(|file_id| Some((*file_id, self.paths.get(file_id)?.as_deref())))
            .collect::<Vec<_>>();
        let (files, infos) = scan_scopes(db, &scanned, &self.gamemodes);
        let scope_files = Arc::make_mut(&mut self.scopes.files);
        let scope_infos = Arc::make_mut(&mut self.scopes.infos);
        for file_id in &changed {
            scope_files.remove(file_id);
            scope_infos.remove(file_id);
        }
        scope_files.extend(files);
        scope_infos.extend(infos);
        changed
            .into_iter()
            .zip(before)
            .filter(|(file_id, (scoped, info))| {
                *scoped != scope_files.contains(file_id)
                    || info.as_ref() != scope_infos.get(file_id)
            })
            .map(|(file_id, _)| file_id)
            .collect()
    }
}

/// The last [`ScriptedScopes`], followed file by file while the main
/// workspaces and their gamemode folders stand. A new config clears it.
#[derive(Clone, Default)]
pub(crate) struct ScopesCache(Option<HeldScopes>);

impl std::fmt::Debug for ScopesCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopesCache")
            .field("filled", &self.0.is_some())
            .finish()
    }
}

impl ScopesCache {
    /// The scopes of the files as they stand, and the files they scope
    /// otherwise than the scopes held last did, sorted.
    fn get(&mut self, db: &DbIndex) -> (ScriptedScopes, Vec<FileId>) {
        let workspaces = main_workspace_roots(db);
        let gamemodes = root_gamemodes(db);
        if let Some(held) = &mut self.0
            && held.workspaces == workspaces
            && held.gamemodes == gamemodes
        {
            let rescoped = held.follow_paths(db);
            return (held.scopes.clone(), rescoped);
        }
        let paths = live_paths(db);
        let scopes = ScriptedScopes::of(db, &paths, &gamemodes);
        let rescoped = self
            .0
            .as_ref()
            .map(|held| held.scopes.rescoped(&scopes))
            .unwrap_or_default();
        self.0 = Some(HeldScopes {
            paths,
            workspaces,
            gamemodes,
            scopes: scopes.clone(),
        });
        (scopes, rescoped)
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
/// treated as deleted; a file that only moved keeps its analysis, which takes
/// in where it now is.
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
    /// Whether the run settled and was wrapped up, leaving the check of
    /// which diagnostics went stale; see [`Self::wrap_up`].
    wrapped: bool,
    /// The files whose diagnostics went stale, once checked.
    stale: Option<Vec<FileId>>,
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
        let pass = start_pass(db, &mut engine, file_ids, &mut scopes, HashSet::default());
        let mut reanalysis = Self {
            engine,
            pass,
            access_paths: crate::semantic::AccessPaths::default(),
            stage: derivation::AnalysisStage::Closed,
            scopes,
            memos: HashMap::default(),
            wrapped: false,
            stale: None,
        };
        reanalysis.pause(db);
        crate::profile::phase_report("take");
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
        // What a pending pass's rebuild moved follows these edits with what
        // this rebuild moves; source layers are rebuilt at the stage outside
        // a run.
        let pending = match &mut self.pass {
            Some(Pass::Replaced { touched, .. }) if !file_ids.is_empty() => std::mem::take(touched),
            _ => HashSet::default(),
        };
        db.set_analysis_stage(derivation::AnalysisStage::Closed);
        self.wrapped = false;
        self.stale = None;
        refresh_payload_refs(db, self.engine.payload_holders());
        self.engine.take_edits(edits);
        if let Some(pass) = start_pass(db, &mut self.engine, file_ids, &mut self.scopes, pending) {
            self.pass = Some(pass);
            self.access_paths = crate::semantic::AccessPaths::default();
        }
        self.pause(db);
        crate::profile::phase_report("take");
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
                self.engine.run(db, &**recipes, slice)
            });
            if !settled {
                break false;
            }
            crate::profile::memory::checkpoint("after the engine run");
            if let Some(Pass::Open(recipes)) = self.pass.take() {
                let recipes = std::sync::Arc::into_inner(recipes)
                    .expect("no superstep's tasks outlive their pass");
                self.memos.extend(recipes.into_memos());
            }
            let stale = self.engine.take_stale_sources();
            crate::semantic::stop_keeping_access_paths();
            self.pass = start_pass(
                db,
                &mut self.engine,
                stale.into_iter().collect(),
                &mut self.scopes,
                HashSet::default(),
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
        self.pass = Some(Pass::Open(std::sync::Arc::new(recipes)));
    }

    /// Leaves the index as it reads outside any run until the next slice.
    fn pause(&mut self, db: &mut DbIndex) {
        self.engine.flush_slot_notifications(db);
        self.stage = db.analysis_stage();
        db.set_analysis_stage(derivation::AnalysisStage::Closed);
        crate::profile::phase("reanalyze: flush", || db.flush_inference_derived_state());
    }

    /// Has a slice that ends with a worker superstep due leave its tasks to
    /// run outside it; see [`Self::take_detached`].
    pub(crate) fn detach_supersteps(&mut self) {
        self.engine.detach_supersteps();
    }

    /// The superstep whose tasks the last slice left to run outside it, with
    /// the recipes they evaluate by: run them while readers share the index
    /// and hand the superstep back with [`Self::restore_detached`].
    pub(crate) fn take_detached(
        &mut self,
    ) -> Option<(engine::Superstep, std::sync::Arc<derive::Recipes>)> {
        let Some(Pass::Open(recipes)) = &self.pass else {
            return None;
        };
        let recipes = std::sync::Arc::clone(recipes);
        Some((self.engine.take_detached()?, recipes))
    }

    pub(crate) fn restore_detached(&mut self, superstep: engine::Superstep) {
        self.engine.restore_detached(superstep);
    }

    pub(crate) fn has_detached(&self) -> bool {
        self.engine.has_detached()
    }

    /// Installs the tasks of a superstep left to run outside the slices, so
    /// the index can be written.
    pub(crate) fn install_detached(&mut self, db: &mut DbIndex) {
        if self.has_detached() {
            self.run(db, engine::Slice::Evaluations(0));
        }
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

    /// Whether the run settled and was wrapped up; see [`Self::wrap_up`].
    pub(crate) fn wrapped(&self) -> bool {
        self.wrapped
    }

    /// Writes what a settled run leaves the index: what the payloads its
    /// files hold refer to, and the call targets it settled. What is left is
    /// the check of which diagnostics went stale, which only reads the index
    /// ([`Self::check_stale`]), and [`Self::finish`].
    pub(crate) fn wrap_up(&mut self, db: &mut DbIndex) {
        debug_assert!(self.pass.is_none(), "wrapping up an unsettled re-analysis");
        if std::mem::replace(&mut self.wrapped, true) {
            return;
        }
        crate::profile::phase("reanalyze: payload refs", || {
            refresh_payload_refs(db, self.engine.payload_holders())
        });
        call_target::check_settled_call_targets(db);
        crate::profile::phase_report("reanalyze");
        self.engine
            .stats()
            .report(self.engine.evaluated_files().len());
        if crate::profile::profiling() {
            for (kind, query_stats) in db.queries().stats() {
                eprintln!("[query] {kind:?} {query_stats:?}");
            }
        }
        crate::profile::memory::checkpoint("end of re-analysis");
    }

    /// Whether the run is wrapped up and its stale diagnostics not checked.
    pub(crate) fn needs_stale_check(&self) -> bool {
        self.wrapped && self.stale.is_none()
    }

    /// Checks which files' diagnostics went stale, on the wrapped-up index.
    pub(crate) fn check_stale(&mut self, db: &DbIndex) {
        if self.stale.is_none() {
            self.stale = Some(crate::profile::phase(
                "reanalyze: stale diagnostics",
                || self.engine.stale_diagnostics(db),
            ));
        }
    }

    /// Wraps up a settled re-analysis.
    pub(crate) fn finish(mut self, db: &mut DbIndex) -> Reanalysed {
        self.wrap_up(db);
        self.check_stale(db);
        Reanalysed {
            stale_diagnostics: self.stale.take().unwrap_or_default(),
            evaluated_files: self.engine.evaluated_files(),
            stats: self.engine.stats().clone(),
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
    Open(std::sync::Arc<derive::Recipes>),
}

/// Stops keeping access paths if a slice unwinds.
struct Unwinding;

impl Drop for Unwinding {
    fn drop(&mut self) {
        crate::semantic::stop_keeping_access_paths();
    }
}

/// Rebuilds `file_ids`' source layers, or takes in where a file that only
/// moved now is, and queues their derivations ahead of the work already
/// queued, alongside any files still settling first; `None` when there is
/// nothing to re-analyse. `pending` holds keys an earlier rebuild moved whose
/// readers are not queued yet, positioned as before these edits.
fn start_pass(
    db: &mut DbIndex,
    engine: &mut engine::Engine,
    mut file_ids: Vec<FileId>,
    scopes: &mut ScopesCache,
    pending: HashSet<derivation::HashedKey>,
) -> Option<Pass> {
    file_ids.sort();
    file_ids.dedup();
    if file_ids.is_empty() {
        return None;
    }
    let (scopes, rescoped) = crate::profile::phase("reanalyze: scopes", || scopes.get(db));
    let repathed = repathed_files(db, engine, &file_ids, &rescoped);
    // A file whose scope changed with its own or other files' paths is
    // re-indexed.
    file_ids.extend(rescoped);
    file_ids.sort();
    file_ids.dedup();
    let replaced = file_ids
        .iter()
        .copied()
        .filter(|file_id| repathed.binary_search(file_id).is_err())
        .collect::<Vec<_>>();
    // Queries computed since the last run read the index as it stood before
    // the rebuild moves it.
    let mut moved = pending;
    moved.extend(engine::file_queries(db));
    engine.replacing(&replaced);
    engine.note_rebuilt(&file_ids);
    engine.prioritise(&file_ids);
    crate::profile::memory::checkpoint("before the source layer");
    let touched = crate::profile::phase("reanalyze: replace files", || {
        replace_files(db, engine, &replaced, &repathed, &scopes, moved)
    });
    crate::profile::memory::checkpoint("after the source layer");
    db.reader_graph_mut().forget_fingerprints();
    Some(Pass::Replaced { touched, scopes })
}

/// The files among `file_ids` that only moved, sorted: their source layer was
/// built from the text they hold, and where they now are leaves their scope,
/// their workspace and the files they load by path as they were. What their
/// source layer derived from their path [`repath_files`] takes in; the rest of
/// it, and every derivation of theirs that did not read where they are,
/// stands.
fn repathed_files(
    db: &DbIndex,
    engine: &engine::Engine,
    file_ids: &[FileId],
    rescoped: &[FileId],
) -> Vec<FileId> {
    let vfs = db.get_vfs();
    file_ids
        .iter()
        .copied()
        .filter(|file_id| {
            if !vfs.has_left_paths(file_id)
                || engine.edits_text_of(*file_id)
                || rescoped.binary_search(file_id).is_ok()
            {
                return false;
            }
            let (Some(tree), Some(path)) = (
                vfs.get_syntax_tree(file_id),
                vfs.get_file_path(file_id).and_then(|path| path.to_str()),
            ) else {
                return false;
            };
            let source = derivation::DerivationId::new(
                derivation::DerivationKind::Source,
                *file_id,
                LuaSyntaxId::from_node(&tree.get_red_root()),
            );
            db.reader_graph().contains(source)
                && db.get_module_index().belongs_alike_at(*file_id, path)
                && !db.get_file_dependencies_index().loads_by_path(*file_id)
        })
        .collect()
}

/// Touches `file_id`'s path and the paths it is and was at, taking the ones it
/// left.
fn touch_paths(db: &mut DbIndex, file_id: FileId, touched: &mut HashSet<derivation::HashedKey>) {
    touched.insert(derivation::HashedKey::new(derivation::FactKey::FilePath(
        file_id,
    )));
    let vfs = db.get_vfs_mut();
    let paths = vfs.take_left_paths(&file_id);
    for path in paths
        .into_iter()
        .chain(vfs.get_file_path(&file_id).cloned())
    {
        touched.insert(derivation::HashedKey::new(derivation::FactKey::PathFile(
            Arc::from(path),
        )));
    }
}

/// Takes in where each of `file_ids`, files that only moved, now is: re-files
/// its module under its path and ranks what its source layer ordered by its
/// path by where it is, as building it there would. Returns every key whose
/// value may have changed.
fn repath_files(db: &mut DbIndex, file_ids: &[FileId]) -> Vec<derivation::HashedKey> {
    if file_ids.is_empty() {
        return Vec::new();
    }
    let _recording = derivation::begin(derivation::DerivationId::workspace(
        derivation::DerivationKind::Source,
    ));
    let mut order_keys = Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        db.get_file_dependencies_index_mut()
            .forget_unresolved_target(*file_id);
        if let Some(path) = db
            .get_vfs()
            .get_file_path(file_id)
            .and_then(|path| path.to_str())
            .map(str::to_string)
        {
            db.get_module_index_mut().repath_module(*file_id, &path);
        }
        order_keys.push((*file_id, db.get_vfs().file_order_key(file_id)));
    }
    db.get_global_index_mut().rekey_files(&order_keys);
    db.get_property_index_mut().rekey_files(&order_keys);
    db.get_type_index_mut().rekey_files(&order_keys);
    let refresh = derivation::end();
    let mut touched = refresh.moved().cloned().collect::<Vec<_>>();
    let (_, _, written) = refresh.into_parts(|_| derivation::ABSENT);
    touched.extend(written);
    touched
}

/// For each derivation among the `foreign` producers of the replaced files'
/// facts, the live files among `live_files` it stored a fact in that the
/// indexes do not keep when the file is re-indexed. Each producer's outputs
/// are read once, on the pool, however many files it stored in.
fn unkept_foreign_outputs(
    db: &DbIndex,
    foreign: &[Vec<derivation::DerivationId>],
    replaced: &HashSet<FileId>,
    live_files: &HashSet<FileId>,
) -> HashMap<derivation::DerivationId, HashSet<FileId>> {
    use rayon::prelude::*;
    let mut producers = foreign
        .iter()
        .flatten()
        .copied()
        .filter(|producer| !replaced.contains(&producer.file_id))
        .collect::<Vec<_>>();
    producers.sort_unstable();
    producers.dedup();
    producers
        .into_par_iter()
        .map(|producer| {
            let unkept = db
                .reader_graph()
                .outputs_of(producer)
                .filter(|key| !key.keeps_foreign_contributions())
                .filter_map(|key| key.owning_file())
                .filter(|file_id| live_files.contains(file_id))
                .collect::<HashSet<_>>();
            (producer, unkept)
        })
        .collect()
}

/// Forgets `file_ids`' derivations and facts, rebuilds the live ones' source
/// layer and queues their derivations, and takes in where each of
/// `repathed`, files that only moved, now is. Returns every key whose value
/// may have changed, `moved` among them, each also at its new position.
fn replace_files(
    db: &mut DbIndex,
    engine: &mut engine::Engine,
    file_ids: &[FileId],
    repathed: &[FileId],
    scopes: &ScriptedScopes,
    moved: HashSet<derivation::HashedKey>,
) -> HashSet<derivation::HashedKey> {
    let mut touched = moved;
    touched.insert(derivation::HashedKey::new(derivation::FactKey::FileSet));
    let forgetting = crate::profile::PhaseGuard::new("replace: forget derivations");
    let mut outputs = HashSet::default();
    let replaced = file_ids.iter().copied().collect::<HashSet<_>>();
    let live_files = file_ids
        .iter()
        .copied()
        .filter(|file_id| db.get_vfs().get_syntax_tree(file_id).is_some())
        .collect::<HashSet<_>>();
    let foreign = file_ids
        .iter()
        .map(|file_id| db.reader_graph().foreign_producers(*file_id))
        .collect::<Vec<_>>();
    let unkept = unkept_foreign_outputs(db, &foreign, &replaced, &live_files);
    for (file_id, producers) in file_ids.iter().zip(foreign) {
        touched.insert(derivation::HashedKey::new(derivation::FactKey::FileText(
            *file_id,
        )));
        touch_paths(db, *file_id, &mut touched);
        let live = live_files.contains(file_id);
        for producer in producers {
            if replaced.contains(&producer.file_id) {
                continue;
            }
            // What it stored in a file that is re-indexed, not deleted, the
            // indexes keep, and the edit remap carries to its new place.
            if live
                && unkept
                    .get(&producer)
                    .is_none_or(|lost| !lost.contains(file_id))
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
        if live
            && db
                .reader_graph()
                .derivations_of_file(*file_id)
                .next()
                .is_some()
        {
            engine.rebuilding(*file_id);
        }
        outputs.extend(
            db.reader_graph()
                .derivations_of_file(*file_id)
                .flat_map(|id| db.reader_graph().outputs_of(id).cloned()),
        );
    }
    // A key read out of a removed output held, before the removal, what its
    // base did: its readers count against that, as the base's do.
    let derived = outputs
        .iter()
        .filter(|key| key.is_base())
        .flat_map(|key| db.reader_graph().derived_keys(key))
        .collect::<Vec<_>>();
    outputs.extend(derived);
    let outputs = outputs.into_iter().collect::<Vec<_>>();
    let fingerprints = {
        use rayon::prelude::*;
        outputs
            .par_iter()
            .map(|key| db.fact_fingerprint(key))
            .collect::<Vec<_>>()
    };
    for (key, fingerprint) in outputs.into_iter().zip(fingerprints) {
        engine.note_removed_output(key, fingerprint);
    }
    let removed = db.reader_graph_mut().remove_files(file_ids);
    for (id, outputs) in removed {
        for key in outputs.iter() {
            if key.is_primary() && key.owning_file() != Some(id.file_id) {
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
    drop(forgetting);
    // What removal takes out, read off the indexes as they drop it: a
    // recorded output can name a slot the index has since re-homed.
    // A file taken in at another path than its module was registered from
    // moved: the sites that named it by its old path stay unresolved.
    let moved = file_ids
        .iter()
        .copied()
        .filter(|file_id| {
            let registered = db.get_module_index().registered_path(*file_id);
            let current = db
                .get_vfs()
                .get_file_path(file_id)
                .map(|path| path.to_string_lossy().replace('\\', "/"));
            registered.is_some() && registered != current.as_deref()
        })
        .collect::<HashSet<_>>();
    let _recording = derivation::begin(derivation::DerivationId::workspace(
        derivation::DerivationKind::Source,
    ));
    db.remove_index(file_ids.to_vec());
    let removal = derivation::end();
    touched.extend(removal.moved().cloned());
    let (_, _, removed) = removal.into_parts(|_| derivation::ABSENT);
    touched.extend(removed);
    // Files that only moved re-file their modules before any file registers,
    // so a load by path resolves against the workspace as it stands.
    for file_id in repathed {
        touch_paths(db, *file_id, &mut touched);
    }
    touched.extend(crate::profile::phase("replace: repath files", || {
        repath_files(db, repathed)
    }));

    let live = file_ids
        .iter()
        .copied()
        .filter(|file_id| db.get_vfs().get_syntax_tree(file_id).is_some())
        .collect::<Vec<_>>();
    for file_id in &live {
        let dependencies = db.get_file_dependencies_index_mut();
        if moved.contains(file_id) {
            dependencies.forget_unresolved_target(*file_id);
        } else {
            dependencies.relink_unresolved_target(*file_id);
        }
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
    let mut remapped = Vec::new();
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
        // A fact another file holds that names a moved position is filed
        // again; its readers check it as the edit moved it.
        let _recording = derivation::begin(derivation::DerivationId::workspace(
            derivation::DerivationKind::Source,
        ));
        let lost = crate::profile::phase("follow edits: remap indexes", || {
            crate::remap_into_file(db, &remap)
        });
        let remap_writes = derivation::end();
        remapped.extend(remap_writes.moved().cloned());
        let (_, _, written) = remap_writes.into_parts(|_| derivation::ABSENT);
        remapped.extend(written);
        if crate::profile::tracing_evaluations() && !lost.is_empty() {
            let keys = lost.iter().map(|fact| &fact.key).collect::<Vec<_>>();
            eprintln!("[trace] remap of {file_id:?} lost positions in {keys:?}");
        }
        for id in producers_of_lost(db, file_id, &lost) {
            rerun(engine, id);
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
        .chain(remapped)
        .collect()
}

/// The derivations that compute `lost` again, sorted: each fact's producers
/// outside the edited file, whose own derivations its source layer already
/// re-runs. A fact no derivation produced is rebuilt by re-analysing the file
/// storing it whole. Read before the reader graph takes the edits, while it
/// still files each key at the positions the remap found it under.
fn producers_of_lost(
    db: &DbIndex,
    edited: FileId,
    lost: &[crate::LostFact],
) -> Vec<derivation::DerivationId> {
    let graph = db.reader_graph();
    let mut ids = Vec::new();
    let mut unproduced = Vec::new();
    for fact in lost {
        let producers = graph
            .producers(&derivation::HashedKey::new(fact.key.clone()))
            .collect::<Vec<_>>();
        if producers.is_empty() {
            unproduced.push(fact.file_id);
        }
        ids.extend(producers.into_iter().filter(|id| id.file_id != edited));
    }
    unproduced.sort_unstable();
    unproduced.dedup();
    if crate::profile::tracing_evaluations() && !unproduced.is_empty() {
        eprintln!("[trace] lost facts no derivation produced in {unproduced:?}");
    }
    for file_id in unproduced {
        ids.extend(graph.derivations_of_file(file_id));
    }
    ids.sort_unstable();
    ids.dedup();
    ids
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
    let _recording = derivation::begin(derivation::DerivationId::new(
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
    let _recording = derivation::resume(evaluation);
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
    let _recording = derivation::resume(evaluation);
    run_analysis::<doc::DocAnalysisPipeline>(db, &mut context);
    let source_root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .expect("a live file has a syntax tree")
        .get_chunk_node();
    doc::register_inferred_accessor_generators(db, file_id, &source_root);
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
        for member in file_calls.into_iter().flat_map(|call| call.members) {
            let member_id = LuaMemberId::new_in_slot(member.syntax_id, file_id, member.slot);
            let func = match member.signature {
                crate::AccessorSignature::Setter => LuaFunctionType::new(
                    AsyncState::None,
                    true,
                    false,
                    vec![("value".to_string(), Some(LuaType::Any))],
                    LuaType::Nil,
                ),
                crate::AccessorSignature::Getter => {
                    LuaFunctionType::new(AsyncState::None, true, false, vec![], LuaType::Any)
                }
                crate::AccessorSignature::Any => LuaFunctionType::new(
                    AsyncState::None,
                    true,
                    false,
                    vec![("...".to_string(), Some(LuaType::Any))],
                    LuaType::Any,
                ),
            };
            db.get_member_index_mut().add_member(
                LuaMemberOwner::Type(member.owner_type_id),
                LuaMember::new(
                    member_id,
                    LuaMemberKey::Name(member.name),
                    LuaMemberFeature::FileMethodDecl,
                    None,
                ),
            );
            write_type_cache(
                db,
                member_id.into(),
                LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(func))),
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

    fn ensure_scripted_scope_cache(&mut self, db: &DbIndex) {
        if self.scripted_scope_files.is_some() {
            return;
        }
        let files = self
            .tree_list
            .iter()
            .map(|in_filed_tree| {
                let file_id = in_filed_tree.file_id;
                (
                    file_id,
                    db.get_vfs().get_file_path(&file_id).map(PathBuf::as_path),
                )
            })
            .collect::<Vec<_>>();
        let (files, _) = scan_scopes(db, &files, &root_gamemodes(db));
        self.scripted_scope_files = Some(Arc::new(files));
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
