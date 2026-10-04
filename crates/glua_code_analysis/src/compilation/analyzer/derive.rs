//! Which derivations a file has, and how each kind is evaluated.

use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaSyntaxId};

use rustc_hash::FxHashMap;

use super::{
    AnalysisPipeline, AnalyzeContext, ScriptedScopes, call_site_params, call_target, dynamic_field,
    gmod, local_inference, lua, setmetatable_factory, synthesize_accessorfunc_members, unresolve,
};
use crate::db_index::derivation::{DerivationId, DerivationKind};
use crate::{DbIndex, FileId, InFiled};

/// The derivations of one file's statements, closures, calls and table
/// fields, in tree order.
pub(super) fn syntax_derivations(file_id: FileId, root: &LuaChunk) -> Vec<DerivationId> {
    let mut ids = Vec::new();
    for node in root.descendants::<LuaAst>() {
        let kind = match &node {
            LuaAst::LuaLocalStat(_) => DerivationKind::LocalStat,
            LuaAst::LuaAssignStat(_) => DerivationKind::AssignStat,
            LuaAst::LuaForRangeStat(_) => DerivationKind::ForRangeStat,
            LuaAst::LuaFuncStat(_) => DerivationKind::FuncStat,
            LuaAst::LuaLocalFuncStat(_) => DerivationKind::LocalFuncStat,
            LuaAst::LuaTableField(_) => DerivationKind::TableField,
            LuaAst::LuaClosureExpr(_) => DerivationKind::Closure,
            LuaAst::LuaCallExpr(call) if call.is_setmetatable() => DerivationKind::Setmetatable,
            LuaAst::LuaCallExpr(_) => DerivationKind::Call,
            _ => continue,
        };
        ids.push(DerivationId::new(
            kind,
            file_id,
            LuaSyntaxId::from_node(node.syntax()),
        ));
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
        DerivationKind::AccessorFunc,
        DerivationKind::DynamicFields,
        DerivationKind::SetmetatableFactory,
        DerivationKind::CallTargets,
        DerivationKind::CallSiteParams,
        DerivationKind::LocalFallbacks,
        DerivationKind::NetHelpers,
        DerivationKind::Network,
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
pub(super) struct Recipes {
    special_call_direct_matcher: lua::call::SpecialCallDirectMatcher,
    scopes: ScriptedScopes,
    /// Each file's inference memo, reused across its evaluations: an entry
    /// holds for as long as the reads it was computed from do.
    caches: std::sync::Mutex<FxHashMap<FileId, crate::LuaInferCache>>,
}

struct ComputedCallTargets {
    targets: crate::FileCallTargets,
    cache: crate::LuaInferCache,
}

struct ComputedDynamicFields {
    fields: dynamic_field::FileDynamicFields,
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

    /// Returns the file's inference memo to the store.
    fn store_cache(&self, file_id: FileId, cache: crate::LuaInferCache) {
        self.caches
            .lock()
            .expect("cache store")
            .insert(file_id, cache);
    }

    /// Each file's inference memo.
    pub(super) fn into_memos(self) -> FxHashMap<FileId, crate::LuaInferCache> {
        self.caches
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl super::engine::Recipe for Recipes {
    fn splits(&self, kind: DerivationKind) -> bool {
        // Call-site contributions are collected with call-site facts invisible;
        // net helpers read other files' helper scans, and net flows their net
        // helpers: no file's computation reads what another's of its kind writes.
        // Helper and valid-guard scans read source facts and the folds over other
        // kinds; gmod metadata reads the source layer, call targets and call
        // roles, which no gmod pre pass writes.
        matches!(
            kind,
            DerivationKind::CallSiteParams
                | DerivationKind::NetHelpers
                | DerivationKind::Network
                | DerivationKind::GmodLoadCallSites
                | DerivationKind::GmodClassCallSites
                | DerivationKind::LocalFallbacks
                | DerivationKind::GmodHelperScan
                | DerivationKind::ValidGuardScan
                | DerivationKind::GmodPre
        )
    }

    fn records_on_workers(&self, kind: DerivationKind) -> bool {
        // A file's dynamic fields infer through owners whose wildcard and
        // finite-name state another file's dynamic fields write.
        matches!(
            kind,
            DerivationKind::CallTargets | DerivationKind::DynamicFields
        )
    }

    fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn std::any::Any + Send> {
        match id.kind {
            DerivationKind::CallTargets => {
                let mut context = AnalyzeContext::for_files(db, vec![id.file_id], &self.scopes);
                if let Some(cache) = self.caches.lock().expect("cache store").remove(&id.file_id) {
                    context.infer_manager.restore_file(id.file_id, cache);
                }
                let cache = context.infer_manager.get_infer_cache(id.file_id);
                let transaction = cache.begin_transaction();
                cache.config_mut().call_site_params_visible = false;
                let targets = call_target::compute_file_call_targets_with(db, cache);
                cache.config_mut().call_site_params_visible = true;
                drop(transaction);
                let cache = context
                    .infer_manager
                    .take_file(id.file_id)
                    .expect("call-target cache");
                Box::new(ComputedCallTargets { targets, cache })
            }
            DerivationKind::CallSiteParams => {
                Box::new(call_site_params::collect_file_contributions(db, id.file_id))
            }
            DerivationKind::NetHelpers => Box::new(gmod::compute_net_helpers(db, id.file_id)),
            DerivationKind::Network => Box::new(gmod::compute_network_flows(db, id.file_id)),
            DerivationKind::LocalFallbacks => Box::new(
                guesses_locals(db)
                    .then(|| local_inference::scan_unguarded_children(db, id.file_id))
                    .flatten(),
            ),
            DerivationKind::GmodLoadCallSites => Box::new(gmod::compute_annotated_call_sites(
                db, id.file_id, true, false,
            )),
            DerivationKind::GmodClassCallSites => Box::new(gmod::compute_annotated_call_sites(
                db, id.file_id, false, true,
            )),
            DerivationKind::DynamicFields => {
                let mut cache = self.take_cache(db, id.file_id);
                let transaction = cache.begin_transaction();
                let fields = dynamic_field::compute_file_dynamic_fields(
                    db,
                    &mut cache,
                    dynamic_field::DynamicFieldAnalysisMode::Full,
                );
                drop(transaction);
                Box::new(ComputedDynamicFields { fields, cache })
            }
            DerivationKind::GmodHelperScan => Box::new(gmod::compute_helper_scan(db, id.file_id)),
            DerivationKind::ValidGuardScan => Box::new(
                crate::db_index::compute_valid_guard_signatures(db, id.file_id),
            ),
            DerivationKind::GmodPre => Box::new(gmod::compute_pre(db, id.file_id, &self.scopes)),
            kind => unreachable!("{kind:?} does not split"),
        }
    }

    fn apply(&self, db: &mut DbIndex, id: DerivationId, computed: Box<dyn std::any::Any + Send>) {
        match id.kind {
            DerivationKind::CallTargets => {
                let ComputedCallTargets { targets, mut cache } = *computed
                    .downcast::<ComputedCallTargets>()
                    .expect("call targets");
                call_target::apply_file_call_targets(db, id.file_id, targets);
                cache.commit_transaction();
                cache.clear();
                self.store_cache(id.file_id, cache);
            }
            DerivationKind::CallSiteParams => {
                let contributions = *computed
                    .downcast::<Vec<call_site_params::Contribution>>()
                    .expect("call-site contributions");
                db.get_call_site_param_index_mut()
                    .set_file_contributions(id.file_id, contributions);
            }
            DerivationKind::NetHelpers => {
                let helpers = *computed
                    .downcast::<Option<gmod::FileNetHelpers>>()
                    .expect("net helpers");
                gmod::apply_net_helpers(db, id.file_id, helpers);
            }
            DerivationKind::Network => {
                let flows = *computed
                    .downcast::<crate::db_index::FileNetworkData>()
                    .expect("net flows");
                gmod::apply_network_flows(db, id.file_id, flows);
            }
            DerivationKind::LocalFallbacks => {
                let scan = *computed
                    .downcast::<Option<local_inference::UnguardedChildFileResult>>()
                    .expect("unguarded-child evidence");
                if !guesses_locals(db) {
                    return;
                }
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
            DerivationKind::DynamicFields => {
                let ComputedDynamicFields { fields, mut cache } = *computed
                    .downcast::<ComputedDynamicFields>()
                    .expect("dynamic fields");
                dynamic_field::apply_dynamic_fields(db, [(fields, id.file_id)]);
                cache.commit_transaction();
                cache.clear();
                self.store_cache(id.file_id, cache);
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
            kind => unreachable!("{kind:?} does not split"),
        }
    }

    fn discard(&self, id: DerivationId, computed: Box<dyn std::any::Any + Send>) {
        let mut cache = match id.kind {
            DerivationKind::CallTargets => {
                computed
                    .downcast::<ComputedCallTargets>()
                    .expect("call targets")
                    .cache
            }
            DerivationKind::DynamicFields => {
                computed
                    .downcast::<ComputedDynamicFields>()
                    .expect("dynamic fields")
                    .cache
            }
            _ => return,
        };
        cache.rollback_transaction();
        self.store_cache(id.file_id, cache);
    }

    fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
        match id.kind {
            DerivationKind::NetHelpers => gmod::evaluate_net_helpers(db, id.file_id),
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
            | DerivationKind::DynamicFields
            | DerivationKind::GmodHelperScan
            | DerivationKind::ValidGuardScan
            | DerivationKind::GmodPre => {
                let computed = super::engine::Recipe::compute(self, db, id);
                super::engine::Recipe::apply(self, db, id, computed);
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
            cache.clear();
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
            DerivationKind::AccessorFunc => synthesize_accessorfunc_members(db, &file_ids),
            DerivationKind::SetmetatableFactory => {
                setmetatable_factory::synthesize_setmetatable_factory_members(db, &file_ids)
            }
            DerivationKind::CallTargets => {
                let cache = context.infer_manager.get_infer_cache(id.file_id);
                cache.config_mut().call_site_params_visible = false;
                let targets = call_target::compute_file_call_targets_with(db, cache);
                cache.config_mut().call_site_params_visible = true;
                call_target::apply_file_call_targets(db, id.file_id, targets);
            }
            DerivationKind::CallSiteParams => {
                call_site_params::CallSiteParamAnalysisPipeline::analyze(db, context)
            }
            DerivationKind::Network => gmod::evaluate_network(db, context),
            DerivationKind::Source
            | DerivationKind::LocalFallbacks
            | DerivationKind::DynamicFields
            | DerivationKind::GmodPre
            | DerivationKind::GmodHelperScan
            | DerivationKind::NetHelpers
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
