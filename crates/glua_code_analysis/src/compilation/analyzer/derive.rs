//! Which derivations a file has, and how each kind is evaluated.

use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaSyntaxId};

use rustc_hash::FxHashMap;

use super::{
    AnalysisPipeline, AnalyzeContext, ScriptedScopes, call_site_params, call_target, dynamic_field,
    gmod, local_inference, lua, setmetatable_factory, synthesize_accessorfunc_members, unresolve,
};
use crate::db_index::derivation::{DerivationId, DerivationKind};
use crate::{DbIndex, FileId, InFiled};

/// Every derivation of one file except its source layer, in canonical order.
pub(super) fn file_derivations(
    file_id: FileId,
    root: &LuaChunk,
    guard_candidates: &[InFiled<LuaSyntaxId>],
) -> Vec<DerivationId> {
    let chunk = LuaSyntaxId::from_node(root.syntax());
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
        // Dynamic fields are collected with dynamic fields invisible.
        matches!(
            kind,
            DerivationKind::CallSiteParams
                | DerivationKind::NetHelpers
                | DerivationKind::Network
                | DerivationKind::DynamicFields
                | DerivationKind::GmodLoadCallSites
                | DerivationKind::GmodClassCallSites
                | DerivationKind::LocalFallbacks
        )
    }

    fn compute(&self, db: &DbIndex, id: DerivationId) -> Box<dyn std::any::Any + Send> {
        match id.kind {
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
            DerivationKind::DynamicFields => Box::new(dynamic_field::compute_file_dynamic_fields(
                db,
                id.file_id,
                dynamic_field::DynamicFieldAnalysisMode::Full,
            )),
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
                let mut context = AnalyzeContext::for_files(db, vec![id.file_id], &self.scopes);
                if let Some(scan) = scan {
                    local_inference::publish_unguarded_children(db, &mut context, vec![scan]);
                }
                local_inference::stabilize_unknown_locals(db, &mut context);
                unresolve::resolve_inline(db, &mut context);
            }
            DerivationKind::GmodLoadCallSites | DerivationKind::GmodClassCallSites => {
                let sites = *computed
                    .downcast::<Vec<gmod::PendingCallSite>>()
                    .expect("annotated call sites");
                gmod::apply_annotated_call_sites(db, id.file_id, sites);
            }
            DerivationKind::DynamicFields => {
                let fields = *computed
                    .downcast::<dynamic_field::FileDynamicFields>()
                    .expect("dynamic fields");
                dynamic_field::apply_dynamic_fields(db, [(fields, id.file_id)]);
            }
            kind => unreachable!("{kind:?} does not split"),
        }
    }

    fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
        match id.kind {
            DerivationKind::GmodHelperScan => gmod::evaluate_helper_scan(db, id.file_id),
            DerivationKind::NetHelpers => gmod::evaluate_net_helpers(db, id.file_id),
            DerivationKind::ValidGuardScan => {
                crate::db_index::scan_valid_guard_signatures(db, id.file_id)
            }
            DerivationKind::GmodCallRoles => gmod::evaluate_call_roles(db),
            DerivationKind::NetHelperNames => gmod::evaluate_net_helper_names(db),
            DerivationKind::GmodLoad => gmod::evaluate_load(db),
            DerivationKind::ValidGuards => gmod::evaluate_valid_guards(db),
            DerivationKind::VguiChains => {
                let file_ids = db.get_vfs().get_all_local_file_ids();
                let mut context = AnalyzeContext::for_files(db, file_ids, &self.scopes);
                gmod::evaluate_vgui_chains(db, &mut context);
            }
            DerivationKind::Source | DerivationKind::Diagnostics => {
                unreachable!("{:?} derivations are not evaluated by recipe", id.kind)
            }
            kind if kind.is_query() => {
                let db: &DbIndex = db;
                db.queries().recompute(db, id);
            }
            _ => {
                let mut context = AnalyzeContext::for_files(db, vec![id.file_id], &self.scopes);
                if let Some(cache) = self.caches.lock().expect("cache store").remove(&id.file_id) {
                    context.infer_manager.restore_file(id.file_id, cache);
                }
                self.evaluate_file_derivation(db, &mut context, id);
                unresolve::resolve_inline(db, &mut context);
                if let Some(mut cache) = context.infer_manager.take_file(id.file_id) {
                    cache.clear();
                    self.caches
                        .lock()
                        .expect("cache store")
                        .insert(id.file_id, cache);
                }
            }
        }
    }
}

/// `GLUALS_CHECK_PURE`: analyse each statement that defers its writes both
/// ways and report where the deferred analysis wrote differently.
fn check_pure() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_CHECK_PURE").is_some())
}

/// Per kind, the statements checked and how many of them wrote differently
/// deferred.
static PURE_CHECKS: std::sync::Mutex<std::collections::BTreeMap<DerivationKind, (usize, usize)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Under `GLUALS_CHECK_PURE`, prints per kind how many statements were checked
/// and how many wrote differently deferred, then the totals.
pub(super) fn report_pure_check() {
    if !check_pure() {
        return;
    }
    let checks = PURE_CHECKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (kind, (checked, mismatched)) in checks.iter() {
        eprintln!("[pure] {kind:?} checked {checked} mismatched {mismatched}");
    }
    let (checked, mismatched) = checks
        .values()
        .fold((0, 0), |(c, m), (checked, mismatched)| {
            (c + checked, m + mismatched)
        });
    eprintln!("[pure] checked {checked} mismatched {mismatched}");
}

/// Whether a file's locals are guessed from their uses: only once the run has
/// closed, since a local still waiting on its initializer is not one the
/// initializer left without a type.
fn guesses_locals(db: &DbIndex) -> bool {
    db.stage_rule_holds(crate::db_index::derivation::StageRule::FloorUnsettled)
}

impl Recipes {
    /// Analyses a statement without writing, then applies what it wrote.
    /// Under `GLUALS_CHECK_PURE` it also analyses it inline, which is what
    /// takes effect, and reports where the two wrote differently.
    fn evaluate_deferred(&self, db: &mut DbIndex, context: &mut AnalyzeContext, id: DerivationId) {
        if !check_pure() {
            let writes = lua::compute_node(
                db,
                context,
                &self.special_call_direct_matcher,
                id.file_id,
                id.syntax_id(),
            );
            for write in &writes {
                write.apply(db);
            }
            return;
        }
        let mut deferred_context = AnalyzeContext::for_files(db, vec![id.file_id], &self.scopes);
        let memo = context.infer_manager.get_infer_cache(id.file_id).clone();
        deferred_context
            .infer_manager
            .restore_file(id.file_id, memo);
        let deferred = lua::compute_node(
            db,
            &mut deferred_context,
            &self.special_call_direct_matcher,
            id.file_id,
            id.syntax_id(),
        );
        let inline = lua::evaluate_node(
            db,
            context,
            &self.special_call_direct_matcher,
            id.file_id,
            id.syntax_id(),
        );
        let deferred = (
            format!("{deferred:?}"),
            format!("{:?}", deferred_context.unresolves),
        );
        let inline = (format!("{inline:?}"), format!("{:?}", context.unresolves));
        let mut checks = PURE_CHECKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (checked, mismatched) = checks.entry(id.kind).or_default();
        *checked += 1;
        if deferred != inline {
            *mismatched += 1;
            if *mismatched <= 5 {
                let text = db
                    .get_vfs()
                    .get_syntax_tree(&id.file_id)
                    .and_then(|tree| {
                        id.syntax_id()
                            .to_node_from_root(tree.get_chunk_node().syntax())
                    })
                    .map(|node| node.text().to_string())
                    .unwrap_or_default();
                let path = db
                    .get_vfs()
                    .get_file_path(&id.file_id)
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                eprintln!(
                    "[pure] {id:?} {path}: {text:?}
[pure] {id:?} deferred {deferred:?}
[pure] {id:?} inline   {inline:?}"
                );
            }
        }
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
            | DerivationKind::ChunkReturn => {
                if lua::defers_writes(id.kind) {
                    self.evaluate_deferred(db, context, id);
                } else {
                    lua::evaluate_node(
                        db,
                        context,
                        &self.special_call_direct_matcher,
                        id.file_id,
                        id.syntax_id(),
                    );
                }
            }
            DerivationKind::GmodLoadCallSites => gmod::evaluate_load_call_sites(db, context),
            DerivationKind::GmodPre => gmod::evaluate_pre(db, context),
            DerivationKind::GmodClassCallSites => gmod::evaluate_class_call_sites(db, context),
            DerivationKind::GmodPost => gmod::evaluate_post(db, context),
            DerivationKind::AccessorFunc => synthesize_accessorfunc_members(db, &file_ids),
            DerivationKind::DynamicFields => {
                dynamic_field::DynamicFieldAnalysisPipeline::analyze(db, context)
            }
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
            DerivationKind::LocalFallbacks => {
                if guesses_locals(db) {
                    // What the split evaluation does, on the cache its scan
                    // infers with: which way the engine runs the pass must not
                    // change what it reads. The file's memo waits aside.
                    let memo = context.infer_manager.take_file(id.file_id);
                    if let Some(scan) = local_inference::scan_unguarded_children(db, id.file_id) {
                        local_inference::publish_unguarded_children(db, context, vec![scan]);
                    }
                    local_inference::stabilize_unknown_locals(db, context);
                    context.infer_manager.take_file(id.file_id);
                    if let Some(memo) = memo {
                        context.infer_manager.restore_file(id.file_id, memo);
                    }
                }
            }
            DerivationKind::Network => gmod::evaluate_network(db, context),
            DerivationKind::Source
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
