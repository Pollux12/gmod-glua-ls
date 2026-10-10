//! Which derivations a file has, and how each kind is evaluated.

use glua_parser::{
    LuaAst, LuaAstNode, LuaChunk, LuaExpr, LuaSyntaxId, LuaTableExpr, LuaTableField,
};

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use super::{
    AnalyzeContext, ScriptedScopes, call_site_params, call_target, dynamic_field, gmod,
    local_inference, lua, setmetatable_factory, synthesize_accessorfunc_members, unresolve,
};
use crate::db_index::derivation::{DerivationId, DerivationKind};
use crate::{DbIndex, FileId, InFiled};

/// Whether a table field is a positional value of a table that is not a
/// shaped array literal and is not a trailing call or `...` of an object
/// literal, which analysis never reads or writes.
fn is_inert_value_field(
    field: &LuaTableField,
    shaped_tables: &mut FxHashMap<LuaSyntaxId, bool>,
) -> bool {
    if !field.is_value_field() {
        return false;
    }
    if field.is_trailing_spread() {
        return false;
    }
    if !matches!(field.get_value_expr(), Some(LuaExpr::TableExpr(_))) {
        return true;
    }
    let Some(table) = field.get_parent::<LuaTableExpr>() else {
        return true;
    };
    let shaped = *shaped_tables
        .entry(LuaSyntaxId::from_node(table.syntax()))
        .or_insert_with(|| table.is_shaped_array_literal());
    !shaped
}

/// The derivations of one file's statements, closures, calls, the targets,
/// call-site parameters, net facts and net flows of its calls, the net
/// summaries of its function bodies, its table fields and its dynamic-field
/// write sites, in tree order; positional fields of tables that are not
/// shaped array literals have none.
pub(super) fn syntax_derivations(file_id: FileId, root: &LuaChunk) -> Vec<DerivationId> {
    let mut ids = Vec::new();
    let mut shaped_tables = FxHashMap::default();
    for node in root.descendants::<LuaAst>() {
        let kind = match &node {
            LuaAst::LuaLocalStat(_) => DerivationKind::LocalStat,
            LuaAst::LuaAssignStat(_) => DerivationKind::AssignStat,
            LuaAst::LuaForRangeStat(_) => DerivationKind::ForRangeStat,
            LuaAst::LuaFuncStat(_) => DerivationKind::FuncStat,
            LuaAst::LuaLocalFuncStat(_) => DerivationKind::LocalFuncStat,
            LuaAst::LuaTableField(field) => {
                if is_inert_value_field(field, &mut shaped_tables) {
                    continue;
                }
                DerivationKind::TableField
            }
            LuaAst::LuaClosureExpr(_) => DerivationKind::Closure,
            LuaAst::LuaCallExpr(call) if call.is_setmetatable() => DerivationKind::Setmetatable,
            LuaAst::LuaCallExpr(_) => DerivationKind::Call,
            _ => continue,
        };
        let syntax_id = LuaSyntaxId::from_node(node.syntax());
        ids.push(DerivationId::new(kind, file_id, syntax_id));
        if matches!(node, LuaAst::LuaCallExpr(_)) {
            for kind in [
                DerivationKind::CallTarget,
                DerivationKind::CallSiteParam,
                DerivationKind::NetCall,
                DerivationKind::NetFlows,
            ] {
                ids.push(DerivationId::new(kind, file_id, syntax_id));
            }
        }
        if let LuaAst::LuaClosureExpr(closure) = &node
            && closure.get_block().is_some()
        {
            ids.push(DerivationId::new(
                DerivationKind::NetHelper,
                file_id,
                syntax_id,
            ));
        }
        if matches!(
            kind,
            DerivationKind::AssignStat
                | DerivationKind::FuncStat
                | DerivationKind::Call
                | DerivationKind::Setmetatable
        ) && dynamic_field::is_write_site(node.syntax())
        {
            ids.push(DerivationId::new(
                DerivationKind::DynamicFieldWrite,
                file_id,
                syntax_id,
            ));
        }
    }
    ids
}

/// Every derivation of one file except its source layer, in canonical order:
/// the `syntax` ones, then the guards, then the file-wide kinds.
pub(super) fn file_derivations(
    file_id: FileId,
    root: &LuaChunk,
    mut ids: Vec<DerivationId>,
    guard_candidates: &[InFiled<LuaSyntaxId>],
) -> Vec<DerivationId> {
    let chunk = LuaSyntaxId::from_node(root.syntax());
    ids.extend(
        guard_candidates
            .iter()
            .filter(|candidate| candidate.file_id == file_id)
            .map(|candidate| DerivationId::new(DerivationKind::Guard, file_id, candidate.value)),
    );
    for kind in [
        DerivationKind::GmodHelperScan,
        DerivationKind::ValidGuardScan,
        DerivationKind::GmodLoadCallSites,
        DerivationKind::GmodPre,
        DerivationKind::ChunkReturn,
        DerivationKind::GmodClassCallSites,
        DerivationKind::GmodPost,
        DerivationKind::VguiRelations,
        DerivationKind::AccessorFunc,
        DerivationKind::SetmetatableFactory,
        DerivationKind::CallSiteParams,
        DerivationKind::LocalFallbacks,
    ] {
        ids.push(DerivationId::new(kind, file_id, chunk));
    }
    ids
}

/// The workspace derivations: each folds per-file facts into one result.
pub(super) const WORKSPACE_DERIVATIONS: [DerivationKind; 5] = [
    DerivationKind::GmodCallRoles,
    DerivationKind::NetHelperNames,
    DerivationKind::GmodLoad,
    DerivationKind::ValidGuards,
    DerivationKind::VguiChains,
];

/// What every evaluation of one engine run shares.
pub(crate) struct Recipes {
    special_call_direct_matcher: lua::call::SpecialCallDirectMatcher,
    scopes: ScriptedScopes,
    /// Each file's inference memo, reused across its evaluations: an entry
    /// holds for as long as the reads it was computed from do.
    caches: std::sync::Mutex<FxHashMap<FileId, crate::LuaInferCache>>,
    /// Memos of applied results, committed and stored by [`Self::settle_caches`].
    unsettled: std::sync::Mutex<Vec<(FileId, crate::LuaInferCache)>>,
    /// Each file's dynamic-field write targets, read off its syntax when its
    /// first write site is evaluated.
    write_sites: std::sync::Mutex<FxHashMap<FileId, std::sync::Arc<dynamic_field::FileWriteSites>>>,
    /// Each file's function lookup and local bindings for its net derivations,
    /// read off its syntax when the first of them is evaluated.
    net_syntax: std::sync::Mutex<FxHashMap<FileId, std::sync::Arc<gmod::FileNetSyntax>>>,
    /// Each file's net helper compositions, taken out for one evaluation of a
    /// call's flows and returned for the next.
    net_compositions: std::sync::Mutex<FxHashMap<FileId, gmod::NetCompositions>>,
}

struct ComputedDynamicFields {
    fields: dynamic_field::SiteDynamicFields,
    cache: crate::LuaInferCache,
}

impl Recipes {
    pub(super) fn new(db: &DbIndex, scopes: ScriptedScopes) -> Self {
        let roots = db
            .get_vfs()
            .get_all_local_file_ids()
            .into_iter()
            .filter_map(|file_id| {
                let tree = db.get_vfs().get_syntax_tree(&file_id)?;
                Some((file_id, tree.get_chunk_node()))
            })
            .collect::<FxHashMap<_, _>>();
        Self {
            special_call_direct_matcher: lua::call::build_special_call_direct_matcher(db, &roots),
            scopes,
            caches: std::sync::Mutex::default(),
            unsettled: std::sync::Mutex::default(),
            write_sites: std::sync::Mutex::default(),
            net_syntax: std::sync::Mutex::default(),
            net_compositions: std::sync::Mutex::default(),
        }
    }
}

impl Recipes {
    /// The file's inference memo, taken out of the store for one computation,
    /// or a new one under the view a file derivation infers with.
    fn take_cache(&self, db: &DbIndex, file_id: FileId) -> crate::LuaInferCache {
        let mut context = AnalyzeContext::for_files(db, vec![file_id], &self.scopes);
        if let Some(cache) = self.caches.lock().expect("cache store").remove(&file_id) {
            context.infer_manager.restore_file(file_id, cache);
        }
        context.infer_manager.get_infer_cache(file_id);
        context
            .infer_manager
            .take_file(file_id)
            .expect("file cache")
    }

    /// The file's dynamic-field write targets, read once per pass: a pass
    /// rebuilds no source layer.
    fn write_sites(
        &self,
        db: &DbIndex,
        file_id: FileId,
    ) -> std::sync::Arc<dynamic_field::FileWriteSites> {
        if let Some(sites) = self.write_sites.lock().expect("write sites").get(&file_id) {
            return sites.clone();
        }
        let sites = std::sync::Arc::new(
            db.get_vfs()
                .get_syntax_tree(&file_id)
                .map(|tree| dynamic_field::FileWriteSites::new(&tree.get_chunk_node()))
                .unwrap_or_default(),
        );
        self.write_sites
            .lock()
            .expect("write sites")
            .entry(file_id)
            .or_insert(sites)
            .clone()
    }

    /// The file's net syntax, read once per pass: a pass rebuilds no source
    /// layer.
    fn net_syntax(&self, db: &DbIndex, file_id: FileId) -> std::sync::Arc<gmod::FileNetSyntax> {
        if let Some(syntax) = self.net_syntax.lock().expect("net syntax").get(&file_id) {
            return syntax.clone();
        }
        let syntax = std::sync::Arc::new(gmod::FileNetSyntax::new(db, file_id));
        self.net_syntax
            .lock()
            .expect("net syntax")
            .entry(file_id)
            .or_insert(syntax)
            .clone()
    }

    /// Returns the file's inference memo to the store.
    fn store_cache(&self, file_id: FileId, cache: crate::LuaInferCache) {
        self.caches
            .lock()
            .expect("cache store")
            .insert(file_id, cache);
    }

    /// Holds the memo of a result just applied until [`Self::settle_caches`].
    fn settle_later(&self, file_id: FileId, cache: crate::LuaInferCache) {
        self.unsettled
            .lock()
            .expect("unsettled memos")
            .push((file_id, cache));
    }

    /// Commits the transaction and parks every memo held
    /// by [`Self::settle_later`], concurrently, and returns it to the store.
    /// Inside a superstep task it settles the memos of the task's files
    /// alone, in place.
    fn settle_caches(&self) {
        let settle = |(file_id, mut cache): (FileId, crate::LuaInferCache)| {
            cache.commit_transaction();
            cache.park();
            self.store_cache(file_id, cache);
        };
        let mut held = self.unsettled.lock().expect("unsettled memos");
        if !super::parallel::task_running() {
            let unsettled = std::mem::take(&mut *held);
            drop(held);
            unsettled.into_par_iter().for_each(settle);
            return;
        }
        let (own, others): (Vec<_>, Vec<_>) = std::mem::take(&mut *held)
            .into_iter()
            .partition(|(file_id, _)| super::parallel::task_covers(*file_id));
        *held = others;
        drop(held);
        own.into_iter().for_each(settle);
    }

    /// Each file's inference memo.
    pub(super) fn into_memos(self) -> FxHashMap<FileId, crate::LuaInferCache> {
        let memos = self
            .caches
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if crate::profile::profiling() {
            let mut footprint = crate::semantic::MemoFootprint::default();
            for cache in memos.values() {
                footprint.add(cache);
            }
            footprint.report("memos of the run");
        }
        memos
    }
}

impl super::engine::Recipe for Recipes {
    fn splits(&self, kind: DerivationKind) -> bool {
        // Call-site contributions are collected with call-site facts invisible.
        // Helper and valid-guard scans read source facts and the folds over other
        // kinds; gmod metadata reads the source layer, call targets and call
        // roles, which no gmod pre pass writes.
        matches!(
            kind,
            DerivationKind::CallSiteParams
                | DerivationKind::GmodLoadCallSites
                | DerivationKind::GmodClassCallSites
                | DerivationKind::GmodHelperScan
                | DerivationKind::ValidGuardScan
                | DerivationKind::GmodPre
        )
    }

    fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn std::any::Any + Send> {
        match id.kind {
            DerivationKind::CallSiteParams => Box::new(
                call_site_params::collect_callback_contributions(db, id.file_id),
            ),
            DerivationKind::LocalFallbacks => Box::new(if guesses_locals(db) {
                local_inference::scan_file(db, id.file_id)
            } else {
                local_inference::FileScan::skipped()
            }),
            DerivationKind::GmodLoadCallSites => Box::new(gmod::compute_annotated_call_sites(
                db, id.file_id, true, false,
            )),
            DerivationKind::GmodClassCallSites => Box::new(gmod::compute_annotated_call_sites(
                db, id.file_id, false, true,
            )),
            DerivationKind::DynamicFieldWrite => {
                let sites = self.write_sites(db, id.file_id);
                let mut cache = self.take_cache(db, id.file_id);
                let transaction = cache.begin_transaction();
                let fields = dynamic_field::compute_site_dynamic_fields(
                    db,
                    &mut cache,
                    &sites,
                    id.syntax_id(),
                );
                drop(transaction);
                Box::new(ComputedDynamicFields { fields, cache })
            }
            DerivationKind::GmodHelperScan => Box::new(gmod::compute_helper_scan(db, id.file_id)),
            DerivationKind::ValidGuardScan => Box::new(
                crate::db_index::compute_valid_guard_signatures(db, id.file_id),
            ),
            DerivationKind::GmodPre => Box::new(gmod::compute_pre(db, id.file_id, &self.scopes)),
            DerivationKind::ReceiverAuthority | DerivationKind::CallbackParams => {
                db.queries().compute(db, id)
            }
            kind => unreachable!("{kind:?} does not split"),
        }
    }

    fn apply(&self, db: &mut DbIndex, id: DerivationId, computed: Box<dyn std::any::Any + Send>) {
        match id.kind {
            DerivationKind::CallSiteParams => {
                let contributions = *computed
                    .downcast::<Vec<call_site_params::Contribution>>()
                    .expect("call-site contributions");
                db.get_call_site_param_index_mut()
                    .set_callback_contributions(id.file_id, contributions);
            }
            DerivationKind::LocalFallbacks => {
                let scan = *computed
                    .downcast::<local_inference::FileScan>()
                    .expect("local fallback scan");
                if scan.is_idle() || !guesses_locals(db) {
                    return;
                }
                let scan = scan.into_evidence();
                self.with_file_context(db, id.file_id, |db, context| {
                    if let Some(scan) = scan {
                        local_inference::publish_unguarded_children(db, context, vec![scan]);
                    }
                    local_inference::stabilize_unknown_locals(db, context);
                });
            }
            DerivationKind::GmodLoadCallSites | DerivationKind::GmodClassCallSites => {
                let sites = *computed
                    .downcast::<Vec<gmod::PendingCallSite>>()
                    .expect("annotated call sites");
                gmod::apply_annotated_call_sites(db, id.file_id, sites);
            }
            DerivationKind::DynamicFieldWrite => {
                let ComputedDynamicFields { fields, cache } = *computed
                    .downcast::<ComputedDynamicFields>()
                    .expect("dynamic fields");
                let site = InFiled::new(id.file_id, id.syntax_id().get_range());
                dynamic_field::apply_site_dynamic_fields(db, &site, fields);
                self.settle_later(id.file_id, cache);
            }
            DerivationKind::GmodHelperScan => {
                let scan = *computed
                    .downcast::<Option<gmod::FileHelperScan>>()
                    .expect("helper scan");
                gmod::apply_helper_scan(db, id.file_id, scan);
            }
            DerivationKind::ValidGuardScan => {
                let found = *computed
                    .downcast::<Vec<(crate::LuaSignatureId, crate::GmodStateMask)>>()
                    .expect("valid-guard signatures");
                crate::db_index::store_valid_guard_signatures(db, id.file_id, found);
            }
            DerivationKind::GmodPre => {
                let metadata = *computed
                    .downcast::<Option<gmod::GmodFileMetadataResult>>()
                    .expect("gmod metadata");
                self.with_file_context(db, id.file_id, |db, context| {
                    gmod::apply_pre(db, context, metadata);
                });
            }
            DerivationKind::ReceiverAuthority | DerivationKind::CallbackParams => {
                db.queries().publish(id, computed)
            }
            kind => unreachable!("{kind:?} does not split"),
        }
    }

    fn settle(&self) {
        self.settle_caches();
    }

    fn discard(&self, id: DerivationId, computed: Box<dyn std::any::Any + Send>) {
        if id.kind != DerivationKind::DynamicFieldWrite {
            return;
        }
        let mut cache = computed
            .downcast::<ComputedDynamicFields>()
            .expect("dynamic fields")
            .cache;
        cache.rollback_transaction();
        self.store_cache(id.file_id, cache);
    }

    fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
        match id.kind {
            DerivationKind::NetCall => {
                let syntax = self.net_syntax(db, id.file_id);
                gmod::evaluate_net_call(db, id, &syntax);
            }
            DerivationKind::NetHelper => {
                let syntax = self.net_syntax(db, id.file_id);
                gmod::evaluate_net_helper(db, id, &syntax);
            }
            DerivationKind::NetFlows => {
                let syntax = self.net_syntax(db, id.file_id);
                let mut compositions = self
                    .net_compositions
                    .lock()
                    .expect("net compositions")
                    .remove(&id.file_id)
                    .unwrap_or_default();
                gmod::evaluate_net_flows(db, id, &syntax, &mut compositions);
                self.net_compositions
                    .lock()
                    .expect("net compositions")
                    .insert(id.file_id, compositions);
            }
            DerivationKind::GmodCallRoles => gmod::evaluate_call_roles(db),
            DerivationKind::NetHelperNames => gmod::evaluate_net_helper_names(db),
            DerivationKind::GmodLoad => gmod::evaluate_load(db),
            DerivationKind::ValidGuards => gmod::evaluate_valid_guards(db),
            DerivationKind::VguiChains => {
                let mut context = AnalyzeContext::for_files(db, Vec::new(), &self.scopes);
                gmod::evaluate_vgui_chains(db, &mut context);
            }
            DerivationKind::Source | DerivationKind::Diagnostics => {
                unreachable!("{:?} derivations are not evaluated by recipe", id.kind)
            }
            kind if kind.is_query() => {
                let db: &DbIndex = db;
                db.queries().recompute(db, id);
            }
            DerivationKind::LocalFallbacks
            | DerivationKind::DynamicFieldWrite
            | DerivationKind::GmodHelperScan
            | DerivationKind::ValidGuardScan
            | DerivationKind::GmodPre => {
                let computed = super::engine::Recipe::compute(self, db, id);
                super::engine::Recipe::apply(self, db, id, computed);
                self.settle_caches();
            }
            _ => self.with_file_context(db, id.file_id, |db, context| {
                self.evaluate_file_derivation(db, context, id);
            }),
        }
    }
}

/// Whether a file's locals are guessed from their uses: only once the run has
/// closed, since a local still waiting on its initializer is not one the
/// initializer left without a type.
fn guesses_locals(db: &DbIndex) -> bool {
    db.stage_rule_holds(crate::db_index::derivation::StageRule::FloorUnsettled)
}

impl Recipes {
    /// Runs `f` with a context for `file_id` that reuses the file's inference
    /// memo, then resolves what it left unresolved.
    fn with_file_context(
        &self,
        db: &mut DbIndex,
        file_id: FileId,
        f: impl FnOnce(&mut DbIndex, &mut AnalyzeContext),
    ) {
        let mut context = AnalyzeContext::for_files(db, vec![file_id], &self.scopes);
        if let Some(cache) = self.caches.lock().expect("cache store").remove(&file_id) {
            context.infer_manager.restore_file(file_id, cache);
        }
        f(db, &mut context);
        unresolve::resolve_inline(db, &mut context);
        if let Some(mut cache) = context.infer_manager.take_file(file_id) {
            cache.park();
            self.caches
                .lock()
                .expect("cache store")
                .insert(file_id, cache);
        }
    }

    /// Analyses a statement without writing, then applies what it wrote; an
    /// assignment in two stages, its homing applied before its values are
    /// analysed.
    fn evaluate_statement(&self, db: &mut DbIndex, context: &mut AnalyzeContext, id: DerivationId) {
        let stage = if id.kind == DerivationKind::AssignStat {
            let (writes, homing) = self.compute(
                db,
                lua::AssignmentStage::Homing(lua::AssignmentHoming::default()),
                context,
                id,
            );
            for write in &writes {
                write.apply(db);
            }
            lua::AssignmentStage::Values(homing)
        } else {
            lua::AssignmentStage::Whole
        };
        let (writes, _) = self.compute(db, stage, context, id);
        for write in &writes {
            write.apply(db);
        }
    }

    fn compute(
        &self,
        db: &DbIndex,
        stage: lua::AssignmentStage,
        context: &mut AnalyzeContext,
        id: DerivationId,
    ) -> (Vec<lua::StatementWrite>, lua::AssignmentHoming) {
        lua::compute_node(
            db,
            stage,
            context,
            &self.special_call_direct_matcher,
            id.file_id,
            id.syntax_id(),
        )
    }

    fn evaluate_file_derivation(
        &self,
        db: &mut DbIndex,
        context: &mut AnalyzeContext,
        id: DerivationId,
    ) {
        let file_ids = [id.file_id];
        match id.kind {
            DerivationKind::Guard => {
                context.add_inferred_guard_candidate(InFiled::new(id.file_id, id.syntax_id()));
                local_inference::publish_inferred_positive_guards(db, context);
            }
            DerivationKind::LocalStat
            | DerivationKind::AssignStat
            | DerivationKind::ForRangeStat
            | DerivationKind::FuncStat
            | DerivationKind::LocalFuncStat
            | DerivationKind::TableField
            | DerivationKind::Closure
            | DerivationKind::Call
            | DerivationKind::Setmetatable
            | DerivationKind::ChunkReturn => self.evaluate_statement(db, context, id),
            DerivationKind::GmodLoadCallSites => gmod::evaluate_load_call_sites(db, context),
            DerivationKind::GmodClassCallSites => gmod::evaluate_class_call_sites(db, context),
            DerivationKind::GmodPost => gmod::evaluate_post(db, context),
            DerivationKind::VguiRelations => gmod::evaluate_vgui_relations(db, context),
            DerivationKind::AccessorFunc => synthesize_accessorfunc_members(db, &file_ids),
            DerivationKind::SetmetatableFactory => {
                setmetatable_factory::synthesize_setmetatable_factory_members(db, &file_ids)
            }
            DerivationKind::CallTarget => {
                let cache = context.infer_manager.get_infer_cache(id.file_id);
                cache.config_mut().call_site_params_visible = false;
                call_target::derive_call_target(db, cache, id.syntax_id());
                cache.config_mut().call_site_params_visible = true;
            }
            DerivationKind::CallSiteParam => {
                let cache = context.infer_manager.get_infer_cache(id.file_id);
                cache.config_mut().call_site_params_visible = false;
                call_site_params::derive_call_site_param(db, cache, id.syntax_id());
                cache.config_mut().call_site_params_visible = true;
            }
            DerivationKind::CallSiteParams => {
                let contributions =
                    call_site_params::collect_callback_contributions(db, id.file_id);
                db.get_call_site_param_index_mut()
                    .set_callback_contributions(id.file_id, contributions);
            }
            DerivationKind::Source
            | DerivationKind::LocalFallbacks
            | DerivationKind::DynamicFieldWrite
            | DerivationKind::GmodPre
            | DerivationKind::GmodHelperScan
            | DerivationKind::NetCall
            | DerivationKind::NetHelper
            | DerivationKind::NetFlows
            | DerivationKind::ValidGuardScan
            | DerivationKind::GmodCallRoles
            | DerivationKind::NetHelperNames
            | DerivationKind::GmodLoad
            | DerivationKind::ValidGuards
            | DerivationKind::VguiChains
            | DerivationKind::ReceiverAuthority
            | DerivationKind::CallbackParams
            | DerivationKind::Diagnostics => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CacheOptions, LuaInferCache, VirtualWorkspace};

    #[test]
    fn a_task_settles_only_its_own_files_memos() {
        let mut ws = VirtualWorkspace::new();
        let own = ws.def_file("lua/autorun/own.lua", "local a = 1");
        let peer = ws.def_file("lua/autorun/peer.lua", "local b = 1");
        let db = ws.get_db_mut();
        let recipes = Recipes::new(db, ScriptedScopes::new(db));
        for file_id in [own, peer] {
            recipes.settle_later(
                file_id,
                LuaInferCache::new(file_id, CacheOptions::default()),
            );
        }

        super::super::parallel::in_task(&[own], || recipes.settle_caches());

        let unsettled = recipes.unsettled.lock().expect("unsettled memos");
        assert_eq!(
            unsettled
                .iter()
                .map(|(file_id, _)| *file_id)
                .collect::<Vec<_>>(),
            [peer]
        );
        assert!(
            recipes
                .caches
                .lock()
                .expect("cache store")
                .contains_key(&own)
        );
    }
}
