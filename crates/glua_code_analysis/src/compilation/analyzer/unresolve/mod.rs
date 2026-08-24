mod check_reason;
mod find_decl_function;
mod resolve;
mod resolve_closure;

use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::Duration;

use crate::{
    FileId, InferFailReason, LuaDeclTypeKind, LuaMemberFeature, LuaSemanticDeclId, LuaTypeDecl,
    LuaTypeFlag,
    compilation::analyzer::{
        AnalysisPipeline, call_site_params::materialize_call_result_consumer,
        unresolve::resolve::try_resolve_special_call,
    },
    db_index::{DbIndex, LuaDeclId, LuaMemberId, LuaSignatureId},
    profile::Profile,
};
use check_reason::{check_reach_reason, resolve_all_reason};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaBlock, LuaCallExpr, LuaExpr, LuaFuncStat,
    LuaNameToken, LuaTableExpr, LuaTableField,
};
use resolve::{
    try_resolve_call_site_contribution, try_resolve_decl, try_resolve_iter_var, try_resolve_module,
    try_resolve_module_ref, try_resolve_table_field,
};
use resolve_closure::{
    try_resolve_call_closure_params, try_resolve_closure_parent_params, try_resolve_closure_return,
};

pub(crate) use resolve::get_wrapped_callable_target_expr;
pub(crate) use resolve::{resolve_settled_iter_var, try_resolve_member, try_resolve_return_point};
pub use resolve_closure::extract_hook_name;
pub use resolve_closure::{
    resolve_gmod_hook_add_callback_doc_function, resolve_gmod_hook_callback_doc_function,
};
use rowan::TextRange;

use super::{AnalyzeContext, infer_cache_manager::InferCacheManager, lua::LuaReturnPoint};

type ResolveResult = Result<(), InferFailReason>;

pub struct PreDynamicUnResolveAnalysisPipeline;
impl AnalysisPipeline for PreDynamicUnResolveAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let (ready, deferred) =
            partition_pre_dynamic_unresolves(std::mem::take(&mut context.unresolves));
        context.unresolves = ready;
        UnResolveAnalysisPipeline::analyze(db, context);
        context.unresolves.extend(deferred);
    }
}

fn partition_pre_dynamic_unresolves(
    candidates: Vec<(UnResolve, InferFailReason)>,
) -> (
    Vec<(UnResolve, InferFailReason)>,
    Vec<(UnResolve, InferFailReason)>,
) {
    let mut deferred = Vec::new();
    let mut ready = Vec::new();
    for (unresolve, reason) in candidates {
        // An unbound `pairs` generic is missing exactly the keys the
        // dynamic-field pass synthesizes. Retrying it here reaches a weaker
        // answer, force-writes it over the template placeholder and retires
        // the item, so the type ends up a function of whether this pass had
        // run yet.
        if matches!(reason, InferFailReason::UnResolveIterTemplate) {
            deferred.push((unresolve, reason));
            continue;
        }

        let UnResolve::Member(mut member) = unresolve else {
            ready.push((unresolve, reason));
            continue;
        };
        if !matches!(reason, InferFailReason::FieldNotFound) {
            ready.push((UnResolve::Member(member), reason));
            continue;
        }

        if matches!(member.prefix.as_ref(), Some(LuaExpr::IndexExpr(_))) {
            deferred.push((UnResolve::Member(member), reason));
            continue;
        }
        if !matches!(member.expr.as_ref(), Some(LuaExpr::IndexExpr(_))) {
            ready.push((UnResolve::Member(member), reason));
            continue;
        }

        if let Some(prefix) = member.prefix.take() {
            ready.push((
                UnResolveMember {
                    file_id: member.file_id,
                    member_id: member.member_id,
                    expr: None,
                    prefix: Some(prefix),
                    ret_idx: member.ret_idx,
                }
                .into(),
                InferFailReason::FieldNotFound,
            ));
        }
        deferred.push((UnResolve::Member(member), reason));
    }
    (ready, deferred)
}

/// Per-reason unresolve attribution, gated on `GLUALS_PROFILE_UNRESOLVE`. Kept
/// off `GLUALS_PROFILE_PHASE` because the per-attempt `Instant` pairs inflate
/// the pass by ~50%, which would distort every other phase reading.
fn unresolve_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE_UNRESOLVE").is_some())
}

pub struct UnResolveAnalysisPipeline;

impl AnalysisPipeline for UnResolveAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("resolve analyze", context.tree_list.len() > 1);
        let log_enabled = log::log_enabled!(log::Level::Info) || unresolve_profile_enabled();
        let mut infer_manager = std::mem::take(&mut context.infer_manager);

        let mat_start = log_enabled.then(std::time::Instant::now);
        materialize_pending_str_tpl_type_decls(db, &mut infer_manager);
        if let Some(mat_start) = mat_start {
            log::info!(
                "unresolve: initial materialize_pending cost {:?}",
                mat_start.elapsed()
            );
        }

        infer_manager.clear_for_unresolve(db);

        // Use FxHashMap for O(1) reason grouping (matching upstream)
        let had_unresolves = !context.unresolves.is_empty();
        let mut reason_resolve: FxHashMap<InferFailReason, Vec<UnResolve>> = FxHashMap::default();
        for (unresolve, reason) in context.unresolves.drain(..) {
            reason_resolve.entry(reason).or_default().push(unresolve);
        }

        if log_enabled {
            let total_unresolves: usize = reason_resolve.values().map(|v| v.len()).sum();
            log::info!(
                "unresolve: starting with {} unresolves in {} reason groups",
                total_unresolves,
                reason_resolve.len()
            );
        }

        let mut loop_count = 0;
        while !reason_resolve.is_empty() {
            let iter_start = log_enabled.then(std::time::Instant::now);

            let resolve_start = log_enabled.then(std::time::Instant::now);
            let profile = crate::profile::phase("unresolve/try_resolve", || {
                try_resolve(db, &mut infer_manager, &mut reason_resolve, log_enabled)
            });
            if let Some(resolve_start) = resolve_start {
                log::info!(
                    "unresolve: loop {} try_resolve cost {:?}",
                    loop_count,
                    resolve_start.elapsed()
                );
            }
            if let Some(profile) = profile {
                profile.log(loop_count);
            }

            let mat_start = log_enabled.then(std::time::Instant::now);
            crate::profile::phase("unresolve/materialize_pending", || {
                materialize_pending_str_tpl_type_decls(db, &mut infer_manager)
            });
            if let Some(mat_start) = mat_start {
                log::info!(
                    "unresolve: loop {} materialize_pending cost {:?}",
                    loop_count,
                    mat_start.elapsed()
                );
            }

            if reason_resolve.is_empty() {
                if let Some(iter_start) = iter_start {
                    log::info!(
                        "unresolve: loop {} total cost {:?} (resolved all)",
                        loop_count,
                        iter_start.elapsed()
                    );
                }
                break;
            }

            if log_enabled {
                let remaining: usize = reason_resolve.values().map(|v| v.len()).sum();
                log::info!(
                    "unresolve: loop {} remaining {} unresolves",
                    loop_count,
                    remaining
                );
            }

            if loop_count == 0 {
                infer_manager.set_force();
            }

            let reason_start = log_enabled.then(std::time::Instant::now);
            crate::profile::phase("unresolve/resolve_all_reason", || {
                resolve_all_reason(db, &mut reason_resolve, loop_count)
            });
            if let Some(reason_start) = reason_start {
                log::info!(
                    "unresolve: loop {} resolve_all_reason cost {:?}",
                    loop_count,
                    reason_start.elapsed()
                );
            }

            if let Some(iter_start) = iter_start {
                log::info!(
                    "unresolve: loop {} total cost {:?}",
                    loop_count,
                    iter_start.elapsed()
                );
            }

            if loop_count >= 5 {
                break;
            }
            loop_count += 1;
        }

        // Applied once per pipeline run rather than per resolution: every apply
        // rebuilds the index's whole derived contribution state.
        let changed_signatures = db
            .get_call_site_param_index_mut()
            .flush_deferred_contributions();
        if !changed_signatures.is_empty() {
            requeue_flushed_call_site_returns(
                db,
                context,
                &mut infer_manager,
                &mut reason_resolve,
                &changed_signatures,
                log_enabled,
            );
            // The requeued wave can defer contributions of its own. Applying
            // them here keeps the queue empty when this pipeline returns, as it
            // was before the wave existed. Their changed set is deliberately
            // dropped: acting on it would make the flush re-entrant, and the
            // returns it would requeue are exactly as stale as they were before
            // this fix rather than newly wrong.
            let _ = db
                .get_call_site_param_index_mut()
                .flush_deferred_contributions();
        }

        for (reason, unresolves) in reason_resolve {
            context.unresolves.extend(
                unresolves
                    .into_iter()
                    .map(|unresolve| (unresolve, reason.clone())),
            );
        }

        // Resolving deferred items mutates type/member indexes, so any inference
        // cached while resolution was still in progress can be stale.
        if had_unresolves {
            crate::profile::phase("unresolve/infer_manager_clear", || infer_manager.clear());
        }

        // Return the infer_manager so later phases (e.g. dynamic field) can
        // reuse cached inference results rather than recomputing from scratch
        // when no deferred resolution changed the indexes.
        context.infer_manager = infer_manager;
    }
}

/// Re-derives the inferred returns invalidated by the deferred-contribution
/// flush.
///
/// A deferred call-site argument that only resolved in this run changes its
/// callee's parameter type, which is an input to every return derived from it.
/// The requeued items are serviced here rather than left in `context.unresolves`
/// because no later wave is guaranteed to run, and a requeued signature parked
/// at `SignatureReturnStatus::UnResolve` is worse than the stale answer.
fn requeue_flushed_call_site_returns(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    infer_manager: &mut InferCacheManager,
    reason_resolve: &mut FxHashMap<InferFailReason, Vec<UnResolve>>,
    changed_signatures: &HashSet<LuaSignatureId>,
    log_enabled: bool,
) {
    let consumers = db
        .get_call_site_param_index()
        .get_return_consumers(changed_signatures);
    let consumers = consumers
        .into_iter()
        .filter_map(|consumer| materialize_call_result_consumer(db, consumer))
        .collect();
    context.requeue_call_site_inferred_returns(db, changed_signatures, consumers);

    let mut requeued: FxHashMap<InferFailReason, Vec<UnResolve>> = FxHashMap::default();
    for (unresolve, reason) in context.unresolves.drain(..) {
        requeued.entry(reason).or_default().push(unresolve);
    }
    if requeued.is_empty() {
        return;
    }

    // Pending template decls are index writes rather than cache entries, and
    // `clear` drops them unmaterialized, so they are drained either side of it
    // exactly as the main loop drains them around `clear_for_unresolve`.
    materialize_pending_str_tpl_type_decls(db, infer_manager);
    // The flush rewrote `inferred_params`, which is an input to every param type
    // and so to every expression inferred from one. Replaying that memoised
    // inference here would commit a pre-flush answer. Everything `clear` drops
    // is derived from the indexes and recomputed on demand; the phase and
    // dynamic-field visibility live on the manager and survive it.
    infer_manager.clear();

    let _ = try_resolve(db, infer_manager, &mut requeued, log_enabled);
    materialize_pending_str_tpl_type_decls(db, infer_manager);

    // Whatever is still unresolved rejoins the retained set, so it leaves this
    // pipeline through the same path as the main loop's leftovers.
    for (reason, unresolves) in requeued {
        reason_resolve.entry(reason).or_default().extend(unresolves);
    }
}

fn materialize_pending_str_tpl_type_decls(db: &mut DbIndex, infer_manager: &mut InferCacheManager) {
    let pending_type_decls = infer_manager.drain_pending_str_tpl_type_decls();

    for pending in pending_type_decls {
        if db
            .get_type_index()
            .get_type_decl(&pending.type_decl_id)
            .is_none()
        {
            db.get_type_index_mut().add_type_decl(
                pending.file_id,
                LuaTypeDecl::new(
                    pending.file_id,
                    TextRange::default(),
                    pending.type_decl_id.get_simple_name().to_string(),
                    LuaDeclTypeKind::Class,
                    LuaTypeFlag::AutoGenerated.into(),
                    pending.type_decl_id.clone(),
                ),
            );
        }

        db.get_type_index_mut().add_super_type_if_missing(
            pending.type_decl_id.clone(),
            pending.file_id,
            pending.source_range,
            pending.super_type,
        );
    }
}

fn attempt_resolve(
    db: &mut DbIndex,
    infer_manager: &mut InferCacheManager,
    file_id: FileId,
    unresolve: &mut UnResolve,
) -> ResolveResult {
    let cache = infer_manager.get_infer_cache(file_id);
    match unresolve {
        UnResolve::Decl(un_resolve_decl) => try_resolve_decl(db, cache, un_resolve_decl),
        UnResolve::Member(un_resolve_member) => try_resolve_member(db, cache, un_resolve_member),
        UnResolve::Module(un_resolve_module) => try_resolve_module(db, cache, un_resolve_module),
        UnResolve::Return(un_resolve_return) => {
            try_resolve_return_point(db, cache, un_resolve_return)
        }
        UnResolve::ClosureParams(un_resolve_closure_params) => {
            try_resolve_call_closure_params(db, cache, un_resolve_closure_params)
        }
        UnResolve::ClosureReturn(un_resolve_closure_return) => {
            try_resolve_closure_return(db, cache, un_resolve_closure_return)
        }
        UnResolve::IterDecl(un_resolve_iter_var) => {
            try_resolve_iter_var(db, cache, un_resolve_iter_var)
        }
        UnResolve::ModuleRef(module_ref) => try_resolve_module_ref(db, cache, module_ref),
        UnResolve::ClosureParentParams(un_resolve_closure_params) => {
            try_resolve_closure_parent_params(db, cache, un_resolve_closure_params)
        }
        UnResolve::TableField(un_resolve_table_field) => {
            try_resolve_table_field(db, cache, un_resolve_table_field)
        }
        UnResolve::SpecialCall(un_resolve_special_call) => {
            try_resolve_special_call(db, cache, un_resolve_special_call)
        }
        UnResolve::CallSiteContribution(contribution) => {
            try_resolve_call_site_contribution(db, cache, contribution)
        }
    }
}

/// Whether the owner currently holds a type recording that no value was found.
fn root_type_is_undetermined(db: &DbIndex, root: &crate::semantic::VarRefCacheRootKey) -> bool {
    let owner = match root {
        crate::semantic::VarRefCacheRootKey::Decl(decl_id)
        | crate::semantic::VarRefCacheRootKey::SelfRef(decl_id) => {
            crate::LuaTypeOwner::Decl(*decl_id)
        }
        crate::semantic::VarRefCacheRootKey::Member(member_id) => {
            crate::LuaTypeOwner::Member(*member_id)
        }
    };
    db.get_type_index()
        .get_type_cache(&owner)
        .is_none_or(|cache| crate::db_index::is_undetermined_type(cache.as_type()))
}

fn try_resolve(
    db: &mut DbIndex,
    infer_manager: &mut InferCacheManager,
    reason_resolve: &mut FxHashMap<InferFailReason, Vec<UnResolve>>,
    profile_enabled: bool,
) -> Option<TryResolveProfile> {
    let mut profile = profile_enabled.then(TryResolveProfile::default);
    let mut cached_sorted_keys: Option<Vec<InferFailReason>> = None;
    // Which `(item, reason)` pairs this call has already re-queued.
    //
    // A wave only continues while something `changed`, and moving an item to a
    // *different* reason counts as change. Two items can hand each other the
    // same pair of reasons indefinitely — A fails under X naming Y, B fails
    // under Y naming X — so `changed` never settles and the wave loop never
    // returns. Each wave also purges the inference caches of every file it
    // touched, so the same failures are re-derived from scratch every time and
    // the loop makes no progress at all.
    //
    // Re-queueing an item under a reason it has already been re-queued under is
    // therefore not progress, and is parked instead. Every wave now either
    // resolves an item or retires an `(item, reason)` pair, both of which are
    // finite, so the loop terminates.
    let mut requeued: HashSet<(UnResolveIdentity, InferFailReason)> = HashSet::new();
    // Waves have no file count to report, so they report what is still deferred.
    let initial_outstanding: usize = reason_resolve.values().map(Vec::len).sum();
    loop {
        if crate::progress::is_active() {
            let outstanding: usize = reason_resolve.values().map(Vec::len).sum();
            crate::progress::advance_current_phase(
                initial_outstanding.saturating_sub(outstanding),
                initial_outstanding,
                "deferred",
            );
        }
        let mut changed = false;
        let mut to_be_remove = Vec::new();
        let mut retain_unresolve = Vec::new();
        let mut parked = Vec::new();
        let mut retry_file_ids = HashSet::new();

        // Only re-sort keys when the set of reason groups has changed.
        // This avoids cloning and sorting on every inner loop iteration.
        let sorted_keys = cached_sorted_keys
            .take()
            .unwrap_or_else(|| sorted_reason_keys(reason_resolve));

        for check_reason in &sorted_keys {
            if let Some(profile) = profile.as_mut() {
                profile.record_group_seen(
                    &check_reason,
                    reason_resolve.get(&check_reason).map_or(0, Vec::len),
                );
            }
            let Some(unresolves) = reason_resolve.get_mut(&check_reason) else {
                continue;
            };

            let reach_start = profile_enabled.then(std::time::Instant::now);
            let reached = check_reach_reason(db, infer_manager, &check_reason).unwrap_or(false);
            if let (Some(profile), Some(reach_start)) = (profile.as_mut(), reach_start) {
                profile.record_reach_check(&check_reason, reached, reach_start.elapsed());
            }
            if !reached {
                continue;
            }

            unresolves.sort_unstable_by(unresolve_stable_cmp);
            for mut unresolve in unresolves.drain(..) {
                let file_id = unresolve.get_file_id().unwrap_or(FileId { id: 0 });
                let attempt_start = profile_enabled.then(std::time::Instant::now);
                let writes_before = db.get_type_index().type_writes();
                let narrowing_root = unresolve.narrowing_root();
                let was_undetermined = narrowing_root
                    .as_ref()
                    .is_none_or(|root| root_type_is_undetermined(db, root));
                let resolve_result = attempt_resolve(db, infer_manager, file_id, &mut unresolve);
                // A resolution that moved a type invalidates every inference
                // memoised against the old one. The end-of-wave purge is too
                // late for the items still to be drained here: they would read
                // the pre-resolve value, and whether a reader shares a wave
                // with its writer is a property of the batch rather than of the
                // source.
                if db.get_type_index().type_writes() != writes_before {
                    infer_manager.clear_file_deferred_results(file_id);
                    retry_file_ids.insert(file_id);
                    // Narrowing answers are the expensive half of that cache and
                    // are keyed by the variable they narrow, not by what the
                    // walk consulted, so there is no way to drop only the ones
                    // that read this owner. They go when the resolution turned a
                    // value the walk could have read as "not determined yet"
                    // into a known one — the transition that makes an answer
                    // derived from it wrong rather than merely older.
                    if was_undetermined
                        && narrowing_root
                            .as_ref()
                            .is_some_and(|root| !root_type_is_undetermined(db, root))
                    {
                        infer_manager.clear_file_undetermined_flow_results(file_id);
                    }
                }
                let cache = infer_manager.get_infer_cache(file_id);
                if let (Some(profile), Some(attempt_start)) = (profile.as_mut(), attempt_start) {
                    profile.record_attempt(
                        &check_reason,
                        resolve_result.as_ref().err(),
                        attempt_start.elapsed(),
                    );
                }

                match resolve_result {
                    Ok(_) => {
                        changed = true;
                    }
                    Err(InferFailReason::None | InferFailReason::RecursiveInfer) => {}
                    Err(InferFailReason::FieldNotFound) => {
                        if !cache.get_config().analysis_phase.is_force() {
                            retain_unresolve.push((unresolve, InferFailReason::FieldNotFound));
                        }
                    }
                    Err(InferFailReason::UnResolveOperatorCall) => {
                        if !cache.get_config().analysis_phase.is_force() {
                            retain_unresolve
                                .push((unresolve, InferFailReason::UnResolveOperatorCall));
                        }
                    }
                    Err(reason) => {
                        if reason != *check_reason
                            && requeued.insert((unresolve_identity(&unresolve), reason.clone()))
                        {
                            changed = true;
                            retry_file_ids.insert(file_id);
                            retain_unresolve.push((unresolve, reason));
                        } else {
                            // Re-failing on the dependency the group was
                            // reached on usually means the attempt replayed
                            // a memoised `CacheEntry::Error` from before
                            // that dependency landed, so the reason names a
                            // stale fact. Purging the file and parking the
                            // item gives it one genuine look per wave
                            // against the settled index — dying here writes
                            // no type cache at all, which leaves a decl
                            // owner with none and lets
                            // `stabilize_unknown_locals` fabricate a usage
                            // guess.
                            retry_file_ids.insert(file_id);
                            parked.push((unresolve, reason));
                        }
                    }
                }
            }

            to_be_remove.push(check_reason.clone());
        }

        for reason in to_be_remove {
            reason_resolve.remove(&reason);
        }

        let mut keys_changed = !retain_unresolve.is_empty();
        for (unresolve, reason) in retain_unresolve {
            reason_resolve.entry(reason).or_default().push(unresolve);
        }

        // Parking is a within-wave retry, not a hand-back: an item parked on the
        // settling wave is dropped with it, and nothing re-adds it, so it never
        // reaches the outer round's `set_force` and `resolve_all_reason`. That is
        // deliberate. A parked item's reason names a dependency it has already
        // failed on, and carrying the reason into the outer round makes
        // `resolve_as_unknown` floor that dependency's type cache to `Unknown`.
        // The floor is terminal, and it lands on facts the later passes would
        // otherwise still derive, so surviving the settle costs more inference
        // than the missing floor does.
        if !changed || reason_resolve.is_empty() {
            break;
        }

        // Successful deferred resolutions can make cached failures reachable in
        // the next wave, including across files. Keep syntax-derived cache data,
        // but discard inference results computed against the previous DB state.
        materialize_pending_str_tpl_type_decls(db, infer_manager);
        infer_manager.clear_files_deferred_results(&retry_file_ids);

        keys_changed |= !parked.is_empty();
        for (unresolve, reason) in parked {
            reason_resolve.entry(reason).or_default().push(unresolve);
        }

        // Re-use cached sorted keys if no new reason groups were added.
        // When retain_unresolve adds items to new/existing groups, the key
        // set may have changed, so we need to re-sort.
        if !keys_changed {
            cached_sorted_keys = Some(sorted_keys);
        }
    }
    profile
}

#[derive(Default)]
struct TryResolveProfile {
    reason_stats: FxHashMap<&'static str, TryResolveReasonStats>,
}

#[derive(Default)]
struct TryResolveReasonStats {
    groups_seen: usize,
    unresolves_seen: usize,
    reach_checks: usize,
    reach_hits: usize,
    reach_time: Duration,
    attempts: usize,
    ok: usize,
    err_none: usize,
    err_recursive: usize,
    err_field_not_found: usize,
    err_operator_call: usize,
    err_same_reason: usize,
    err_other_reason: usize,
    attempt_time: Duration,
}

impl TryResolveProfile {
    fn stats_mut(&mut self, reason: &InferFailReason) -> &mut TryResolveReasonStats {
        self.reason_stats
            .entry(infer_fail_reason_label(reason))
            .or_default()
    }

    fn record_group_seen(&mut self, reason: &InferFailReason, len: usize) {
        let stats = self.stats_mut(reason);
        stats.groups_seen += 1;
        stats.unresolves_seen += len;
    }

    fn record_reach_check(&mut self, reason: &InferFailReason, reached: bool, elapsed: Duration) {
        let stats = self.stats_mut(reason);
        stats.reach_checks += 1;
        if reached {
            stats.reach_hits += 1;
        }
        stats.reach_time += elapsed;
    }

    fn record_attempt(
        &mut self,
        check_reason: &InferFailReason,
        err_reason: Option<&InferFailReason>,
        elapsed: Duration,
    ) {
        let stats = self.stats_mut(check_reason);
        stats.attempts += 1;
        stats.attempt_time += elapsed;
        match err_reason {
            None => stats.ok += 1,
            Some(InferFailReason::None) => stats.err_none += 1,
            Some(InferFailReason::RecursiveInfer) => stats.err_recursive += 1,
            Some(InferFailReason::FieldNotFound) => stats.err_field_not_found += 1,
            Some(InferFailReason::UnResolveOperatorCall) => stats.err_operator_call += 1,
            Some(reason) if reason == check_reason => stats.err_same_reason += 1,
            Some(_) => stats.err_other_reason += 1,
        }
    }

    fn log(&self, loop_count: usize) {
        let mut stats = self
            .reason_stats
            .iter()
            .map(|(reason, stats)| (*reason, stats))
            .collect::<Vec<_>>();
        stats.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.attempt_time + stats.reach_time));
        for (reason, stats) in stats.into_iter().take(12) {
            if unresolve_profile_enabled() {
                eprintln!(
                    "  [unres] loop {loop_count} {reason:<28} groups={} unres={} reach={}/{} reach_t={:>7.3}s attempts={} ok={} attempt_t={:>7.3}s",
                    stats.groups_seen,
                    stats.unresolves_seen,
                    stats.reach_hits,
                    stats.reach_checks,
                    stats.reach_time.as_secs_f64(),
                    stats.attempts,
                    stats.ok,
                    stats.attempt_time.as_secs_f64(),
                );
            }
            log::info!(
                "unresolve: loop {} reason {} groups={} unresolves={} reach={}/{} reach_time={:?} attempts={} ok={} same_err={} other_err={} field_err={} op_err={} none_err={} recursive_err={} attempt_time={:?}",
                loop_count,
                reason,
                stats.groups_seen,
                stats.unresolves_seen,
                stats.reach_hits,
                stats.reach_checks,
                stats.reach_time,
                stats.attempts,
                stats.ok,
                stats.err_same_reason,
                stats.err_other_reason,
                stats.err_field_not_found,
                stats.err_operator_call,
                stats.err_none,
                stats.err_recursive,
                stats.attempt_time,
            );
        }
    }
}

pub(crate) fn infer_fail_reason_label(reason: &InferFailReason) -> &'static str {
    match reason {
        InferFailReason::None => "none",
        InferFailReason::RecursiveInfer => "recursive_infer",
        InferFailReason::FieldNotFound => "field_not_found",
        InferFailReason::UnResolveOperatorCall => "operator_call",
        InferFailReason::UnResolveDeclType(_) => "decl_type",
        InferFailReason::UnResolveMemberType(_) => "member_type",
        InferFailReason::UnResolveExpr(_) => "expr",
        InferFailReason::UnResolveSignatureReturn(_) => "signature_return",
        InferFailReason::UnResolveTypeDecl(_) => "type_decl",
        InferFailReason::UnResolveModuleExport(_) => "module_export",
        InferFailReason::UnSealedDynamicFields => "unsealed_dynamic_fields",
        InferFailReason::UnResolveIterTemplate => "iter_template",
    }
}

fn sorted_reason_keys(
    reason_resolve: &FxHashMap<InferFailReason, Vec<UnResolve>>,
) -> Vec<InferFailReason> {
    let mut keys: Vec<InferFailReason> = reason_resolve.keys().cloned().collect();
    keys.sort_unstable_by(infer_fail_reason_stable_cmp);
    keys
}

fn infer_fail_reason_kind_rank(reason: &InferFailReason) -> u8 {
    match reason {
        // Unlike every other reason, this one names the index's build state
        // rather than another item's fact: it is unreachable until the
        // seal, and once it is reachable the facts these items produce are
        // *inputs* to the other groups. Ordering it last let a consumer
        // resolve against the pre-seal picture and commit a floor, which is
        // terminal — the item is gone before the fact it needed lands.
        InferFailReason::UnSealedDynamicFields => 0,
        InferFailReason::None => 1,
        InferFailReason::RecursiveInfer => 2,
        InferFailReason::FieldNotFound => 3,
        // Ordered where these items sat while they shared the `FieldNotFound`
        // group: they only upgrade their own placeholders, so the rank exists to
        // keep that timing, not to express a dependency.
        InferFailReason::UnResolveIterTemplate => 4,
        InferFailReason::UnResolveOperatorCall => 5,
        InferFailReason::UnResolveDeclType(_) => 6,
        InferFailReason::UnResolveMemberType(_) => 7,
        InferFailReason::UnResolveExpr(_) => 8,
        InferFailReason::UnResolveSignatureReturn(_) => 9,
        InferFailReason::UnResolveTypeDecl(_) => 10,
        InferFailReason::UnResolveModuleExport(_) => 11,
    }
}

fn infer_fail_reason_stable_cmp(a: &InferFailReason, b: &InferFailReason) -> Ordering {
    let rank_cmp = infer_fail_reason_kind_rank(a).cmp(&infer_fail_reason_kind_rank(b));
    if rank_cmp != Ordering::Equal {
        return rank_cmp;
    }

    match (a, b) {
        (
            InferFailReason::UnResolveDeclType(a_decl),
            InferFailReason::UnResolveDeclType(b_decl),
        ) => a_decl
            .file_id
            .id
            .cmp(&b_decl.file_id.id)
            .then_with(|| u32::from(a_decl.position).cmp(&u32::from(b_decl.position))),
        (
            InferFailReason::UnResolveMemberType(a_member),
            InferFailReason::UnResolveMemberType(b_member),
        ) => a_member.file_id.id.cmp(&b_member.file_id.id).then_with(|| {
            u32::from(a_member.get_position()).cmp(&u32::from(b_member.get_position()))
        }),
        (InferFailReason::UnResolveExpr(a_expr), InferFailReason::UnResolveExpr(b_expr)) => {
            a_expr.file_id.id.cmp(&b_expr.file_id.id).then_with(|| {
                u32::from(a_expr.value.syntax().text_range().start())
                    .cmp(&u32::from(b_expr.value.syntax().text_range().start()))
            })
        }
        (
            InferFailReason::UnResolveSignatureReturn(a_signature),
            InferFailReason::UnResolveSignatureReturn(b_signature),
        ) => a_signature
            .get_file_id()
            .id
            .cmp(&b_signature.get_file_id().id)
            .then_with(|| {
                u32::from(a_signature.get_position()).cmp(&u32::from(b_signature.get_position()))
            }),
        (
            InferFailReason::UnResolveTypeDecl(a_type),
            InferFailReason::UnResolveTypeDecl(b_type),
        ) => a_type.get_name().cmp(b_type.get_name()),
        (
            InferFailReason::UnResolveModuleExport(a_file_id),
            InferFailReason::UnResolveModuleExport(b_file_id),
        ) => a_file_id.id.cmp(&b_file_id.id),
        _ => Ordering::Equal,
    }
}

fn unresolve_kind_rank(unresolve: &UnResolve) -> u8 {
    match unresolve {
        // Ahead of every consumer: a loop variable is an input to the facts the
        // body derives from it, and both sit in the same reason group.
        UnResolve::IterDecl(_) => 0,
        UnResolve::Decl(_) => 1,
        UnResolve::Member(_) => 2,
        UnResolve::Module(_) => 3,
        UnResolve::Return(_) => 4,
        UnResolve::ClosureParams(_) => 5,
        UnResolve::ClosureReturn(_) => 6,
        UnResolve::ClosureParentParams(_) => 7,
        UnResolve::ModuleRef(_) => 8,
        UnResolve::TableField(_) => 9,
        UnResolve::SpecialCall(_) => 10,
        UnResolve::CallSiteContribution(_) => 11,
    }
}

/// Separates items whose kind, file and position are shared with a sibling.
/// Closure-argument and call-site-contribution items are all keyed on their
/// call's start position, and a module ref is keyed on the module rather than on
/// the owner receiving it.
#[derive(PartialEq, Eq, Hash)]
enum UnResolveDiscriminator {
    None,
    ParamIdx(usize),
    Owner(LuaSemanticDeclId),
}

/// Identifies an unresolve item across waves: the same syntax position in the
/// same file for the same item kind, plus the discriminator that separates
/// siblings sharing that position, is the same item.
type UnResolveIdentity = (u8, u32, u32, UnResolveDiscriminator);

fn unresolve_identity(unresolve: &UnResolve) -> UnResolveIdentity {
    let (file_id, position) = unresolve.sort_key();
    let discriminator = match unresolve {
        UnResolve::ClosureParams(d) => UnResolveDiscriminator::ParamIdx(d.param_idx),
        UnResolve::ClosureReturn(d) => UnResolveDiscriminator::ParamIdx(d.param_idx),
        UnResolve::CallSiteContribution(d) => UnResolveDiscriminator::ParamIdx(d.param_idx),
        UnResolve::ModuleRef(d) => UnResolveDiscriminator::Owner(d.owner_id.clone()),
        _ => UnResolveDiscriminator::None,
    };
    (
        unresolve_kind_rank(unresolve),
        file_id,
        position,
        discriminator,
    )
}

fn unresolve_stable_cmp(a: &UnResolve, b: &UnResolve) -> Ordering {
    unresolve_kind_rank(a)
        .cmp(&unresolve_kind_rank(b))
        .then_with(|| a.sort_key().cmp(&b.sort_key()))
}

#[derive(Debug)]
pub enum UnResolve {
    Decl(Box<UnResolveDecl>),
    IterDecl(Box<UnResolveIterVar>),
    Member(Box<UnResolveMember>),
    Module(Box<UnResolveModule>),
    Return(Box<UnResolveReturn>),
    ClosureParams(Box<UnResolveCallClosureParams>),
    ClosureReturn(Box<UnResolveClosureReturn>),
    ClosureParentParams(Box<UnResolveParentClosureParams>),
    ModuleRef(Box<UnResolveModuleRef>),
    TableField(Box<UnResolveTableField>),
    SpecialCall(Box<UnResolveSpecialCall>),
    CallSiteContribution(Box<UnResolveCallSiteContribution>),
}

#[allow(dead_code)]
impl UnResolve {
    pub fn get_file_id(&self) -> Option<FileId> {
        match self {
            UnResolve::Decl(un_resolve_decl) => Some(un_resolve_decl.file_id),
            UnResolve::IterDecl(un_resolve_iter_var) => Some(un_resolve_iter_var.file_id),
            UnResolve::Member(un_resolve_member) => Some(un_resolve_member.file_id),
            UnResolve::Module(un_resolve_module) => Some(un_resolve_module.file_id),
            UnResolve::Return(un_resolve_return) => Some(un_resolve_return.file_id),
            UnResolve::ClosureParams(un_resolve_closure_params) => {
                Some(un_resolve_closure_params.file_id)
            }
            UnResolve::ClosureReturn(un_resolve_closure_return) => {
                Some(un_resolve_closure_return.file_id)
            }
            UnResolve::ClosureParentParams(un_resolve_closure_params) => {
                Some(un_resolve_closure_params.file_id)
            }
            UnResolve::TableField(un_resolve_table_field) => Some(un_resolve_table_field.file_id),
            UnResolve::ModuleRef(_) => None,
            UnResolve::SpecialCall(un_resolve_special_call) => {
                Some(un_resolve_special_call.file_id)
            }
            // The retry infers in the file the expression lives in, which is the
            // callee's file for callback snapshots.
            UnResolve::CallSiteContribution(contribution) => Some(contribution.expr_file_id),
        }
    }

    /// The declaration or member this item writes to, when narrowing can read
    /// it. Used to tell whether a resolution settled a value the flow walk
    /// could still have been reading as undetermined.
    pub fn narrowing_root(&self) -> Option<crate::semantic::VarRefCacheRootKey> {
        match self {
            UnResolve::Decl(decl) => Some(crate::semantic::VarRefCacheRootKey::Decl(decl.decl_id)),
            UnResolve::Member(member) => Some(crate::semantic::VarRefCacheRootKey::Member(
                member.member_id,
            )),
            _ => None,
        }
    }

    /// Returns a deterministic sort key (file_id, text_position) for stable ordering.
    /// This ensures unresolves are processed in a consistent order regardless of
    /// HashMap iteration order or other non-deterministic sources during collection.
    fn sort_key(&self) -> (u32, u32) {
        match self {
            UnResolve::Decl(d) => (d.file_id.id, u32::from(d.decl_id.position)),
            UnResolve::IterDecl(d) => (
                d.file_id.id,
                d.iter_vars
                    .first()
                    .map(|v| u32::from(v.syntax().text_range().start()))
                    .unwrap_or(0),
            ),
            UnResolve::Member(d) => (d.file_id.id, u32::from(d.member_id.get_position())),
            UnResolve::Module(d) => (
                d.file_id.id,
                u32::from(d.expr.syntax().text_range().start()),
            ),
            UnResolve::Return(d) => (d.file_id.id, u32::from(d.signature_id.get_position())),
            UnResolve::ClosureParams(d) => (
                d.file_id.id,
                u32::from(d.call_expr.syntax().text_range().start()),
            ),
            UnResolve::ClosureReturn(d) => (
                d.file_id.id,
                u32::from(d.call_expr.syntax().text_range().start()),
            ),
            UnResolve::ClosureParentParams(d) => {
                (d.file_id.id, u32::from(d.signature_id.get_position()))
            }
            UnResolve::ModuleRef(d) => (0, d.module_file_id.id),
            UnResolve::TableField(d) => (
                d.file_id.id,
                u32::from(d.field.syntax().text_range().start()),
            ),
            UnResolve::SpecialCall(d) => (
                d.file_id.id,
                u32::from(d.call_expr.syntax().text_range().start()),
            ),
            UnResolve::CallSiteContribution(d) => (
                d.expr_file_id.id,
                u32::from(d.expr_syntax_id.get_range().start()),
            ),
        }
    }
}

#[derive(Debug)]
pub struct UnResolveDecl {
    pub file_id: FileId,
    pub decl_id: LuaDeclId,
    pub expr: LuaExpr,
    pub ret_idx: usize,
}

impl From<UnResolveDecl> for UnResolve {
    fn from(un_resolve_decl: UnResolveDecl) -> Self {
        UnResolve::Decl(Box::new(un_resolve_decl))
    }
}

#[derive(Debug)]
pub struct UnResolveMember {
    pub file_id: FileId,
    pub member_id: LuaMemberId,
    pub expr: Option<LuaExpr>,
    pub prefix: Option<LuaExpr>,
    pub ret_idx: usize,
}

impl From<UnResolveMember> for UnResolve {
    fn from(un_resolve_member: UnResolveMember) -> Self {
        UnResolve::Member(Box::new(un_resolve_member))
    }
}

#[derive(Debug)]
pub struct UnResolveModule {
    pub file_id: FileId,
    pub expr: LuaExpr,
}

impl From<UnResolveModule> for UnResolve {
    fn from(un_resolve_module: UnResolveModule) -> Self {
        UnResolve::Module(Box::new(un_resolve_module))
    }
}

#[derive(Debug, Clone)]
pub struct UnResolveReturn {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub body: Option<LuaBlock>,
    pub return_points: Vec<LuaReturnPoint>,
}

impl From<UnResolveReturn> for UnResolve {
    fn from(un_resolve_return: UnResolveReturn) -> Self {
        UnResolve::Return(Box::new(un_resolve_return))
    }
}

#[derive(Debug)]
pub struct UnResolveCallClosureParams {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub call_expr: LuaCallExpr,
    pub param_idx: usize,
}

impl From<UnResolveCallClosureParams> for UnResolve {
    fn from(un_resolve_closure_params: UnResolveCallClosureParams) -> Self {
        UnResolve::ClosureParams(Box::new(un_resolve_closure_params))
    }
}

#[derive(Debug, Clone)]
pub struct UnResolveIterVar {
    pub file_id: FileId,
    pub iter_exprs: Vec<LuaExpr>,
    pub iter_vars: Vec<LuaNameToken>,
}

impl From<UnResolveIterVar> for UnResolve {
    fn from(un_resolve_iter_var: UnResolveIterVar) -> Self {
        UnResolve::IterDecl(Box::new(un_resolve_iter_var))
    }
}

#[derive(Debug)]
pub struct UnResolveClosureReturn {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub call_expr: LuaCallExpr,
    pub param_idx: usize,
    pub body: Option<LuaBlock>,
    pub return_points: Vec<LuaReturnPoint>,
}

impl From<UnResolveClosureReturn> for UnResolve {
    fn from(un_resolve_closure_return: UnResolveClosureReturn) -> Self {
        UnResolve::ClosureReturn(Box::new(un_resolve_closure_return))
    }
}

#[derive(Debug)]
pub struct UnResolveModuleRef {
    pub owner_id: LuaSemanticDeclId,
    pub module_file_id: FileId,
}

impl From<UnResolveModuleRef> for UnResolve {
    fn from(un_resolve_module_ref: UnResolveModuleRef) -> Self {
        UnResolve::ModuleRef(Box::new(un_resolve_module_ref))
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum UnResolveParentAst {
    LuaFuncStat(LuaFuncStat),
    LuaTableField(LuaTableField),
    LuaAssignStat(LuaAssignStat),
}

#[derive(Debug)]
pub struct UnResolveParentClosureParams {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub parent_ast: UnResolveParentAst,
}

impl From<UnResolveParentClosureParams> for UnResolve {
    fn from(un_resolve_closure_params: UnResolveParentClosureParams) -> Self {
        UnResolve::ClosureParentParams(Box::new(un_resolve_closure_params))
    }
}

#[derive(Debug)]
pub struct UnResolveTableField {
    pub file_id: FileId,
    pub table_expr: LuaTableExpr,
    pub field: LuaTableField,
    pub decl_feature: LuaMemberFeature,
}

impl From<UnResolveTableField> for UnResolve {
    fn from(un_resolve_table_field: UnResolveTableField) -> Self {
        UnResolve::TableField(Box::new(un_resolve_table_field))
    }
}

#[derive(Debug)]
pub struct UnResolveSpecialCall {
    pub file_id: FileId,
    pub call_expr: LuaCallExpr,
}

impl From<UnResolveSpecialCall> for UnResolve {
    fn from(un_resolve_special_call: UnResolveSpecialCall) -> Self {
        UnResolve::SpecialCall(Box::new(un_resolve_special_call))
    }
}

/// Which call-site collection produced a contribution, and therefore how the
/// retry re-derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSiteContributionKind {
    /// Receiver of an exact `obj:method()` call bound to an explicit `self` param.
    ExactReceiver,
    /// A supported argument shape bound to a named callee parameter.
    Argument,
    /// A concrete table passed to a callback, snapshotted structurally.
    CallbackTable,
}

/// A call-site parameter contribution whose type was not inferable yet.
///
/// Carries only ids so it can cross the per-file parallel collection boundary
/// (rowan red nodes are `!Send`); the retry re-materializes the expression from
/// `expr_file_id`'s red root and re-runs exactly that one collection.
#[derive(Debug, Clone)]
pub struct UnResolveCallSiteContribution {
    /// File whose contribution set receives the recovered fact. Equal to
    /// `expr_file_id` except for callback snapshots, which are attributed to the
    /// caller file while the expression lives in the callee's.
    pub file_id: FileId,
    pub expr_file_id: FileId,
    pub expr_syntax_id: glua_parser::LuaSyntaxId,
    pub signature_id: LuaSignatureId,
    pub param_idx: usize,
    pub kind: CallSiteContributionKind,
}

impl From<UnResolveCallSiteContribution> for UnResolve {
    fn from(contribution: UnResolveCallSiteContribution) -> Self {
        UnResolve::CallSiteContribution(Box::new(contribution))
    }
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;

    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaParser, ParserConfig};
    use rowan::TextSize;

    use crate::{FileId, InferFailReason, LuaDeclId, LuaMemberId, LuaTypeDeclId};

    use super::{
        UnResolve, UnResolveIterVar, UnResolveMember, partition_pre_dynamic_unresolves,
        sorted_reason_keys,
    };

    #[test]
    fn reason_group_order_is_stable_across_hashmap_insertion_order() {
        let reasons = [
            InferFailReason::FieldNotFound,
            InferFailReason::UnResolveDeclType(LuaDeclId::new(FileId::new(2), TextSize::new(20))),
            InferFailReason::UnResolveDeclType(LuaDeclId::new(FileId::new(2), TextSize::new(8))),
            InferFailReason::UnResolveTypeDecl(LuaTypeDeclId::local(FileId::new(3), "Local.Zed")),
            InferFailReason::UnResolveTypeDecl(LuaTypeDeclId::global("Global.A")),
            InferFailReason::UnResolveModuleExport(FileId::new(9)),
        ];

        let mut forward: FxHashMap<InferFailReason, Vec<super::UnResolve>> = FxHashMap::default();
        for reason in reasons.iter().cloned() {
            forward.insert(reason, Vec::new());
        }

        let mut reverse: FxHashMap<InferFailReason, Vec<super::UnResolve>> = FxHashMap::default();
        for reason in reasons.iter().rev().cloned() {
            reverse.insert(reason, Vec::new());
        }

        assert_eq!(sorted_reason_keys(&forward), sorted_reason_keys(&reverse));
    }

    #[test]
    fn pre_dynamic_partition_defers_only_template_iter_vars() {
        let tree = LuaParser::parse("for k, v in pairs(t) do end", ParserConfig::default());
        let for_range = tree
            .get_chunk_node()
            .descendants::<glua_parser::LuaForRangeStat>()
            .next()
            .expect("for range stat");
        let iter_var = |file_id| UnResolveIterVar {
            file_id,
            iter_exprs: for_range.get_expr_list().collect(),
            iter_vars: for_range.get_var_name_list().collect(),
        };

        let (ready, deferred) = partition_pre_dynamic_unresolves(vec![
            (
                iter_var(FileId::new(1)).into(),
                InferFailReason::UnResolveIterTemplate,
            ),
            (
                iter_var(FileId::new(2)).into(),
                InferFailReason::FieldNotFound,
            ),
        ]);

        assert_eq!(deferred.len(), 1);
        assert!(matches!(
            deferred[0],
            (
                UnResolve::IterDecl(_),
                InferFailReason::UnResolveIterTemplate
            )
        ));
        assert_eq!(ready.len(), 1);
        assert!(matches!(
            ready[0],
            (UnResolve::IterDecl(_), InferFailReason::FieldNotFound)
        ));
    }

    #[test]
    fn pre_dynamic_partition_resolves_ordinary_owner_before_deferring_dynamic_rhs() {
        let tree = LuaParser::parse("owner.field = dynamic.value", ParserConfig::default());
        let index_exprs = tree
            .get_chunk_node()
            .descendants::<LuaIndexExpr>()
            .collect::<Vec<_>>();
        let target = &index_exprs[0];
        let dynamic_rhs = index_exprs[1].clone();
        let file_id = FileId::new(1);
        let candidate = UnResolveMember {
            file_id,
            member_id: LuaMemberId::new(target.get_syntax_id(), file_id),
            expr: Some(LuaExpr::IndexExpr(dynamic_rhs)),
            prefix: target.get_prefix_expr(),
            ret_idx: 0,
        };

        let (ready, deferred) = partition_pre_dynamic_unresolves(vec![(
            candidate.into(),
            InferFailReason::FieldNotFound,
        )]);

        assert_eq!(ready.len(), 1);
        let UnResolve::Member(ready_member) = &ready[0].0 else {
            panic!("expected ready member owner");
        };
        assert!(ready_member.prefix.is_some());
        assert!(ready_member.expr.is_none());
        assert_eq!(deferred.len(), 1);
        let UnResolve::Member(deferred_member) = &deferred[0].0 else {
            panic!("expected deferred member value");
        };
        assert!(deferred_member.prefix.is_none());
        assert!(matches!(deferred_member.expr, Some(LuaExpr::IndexExpr(_))));
    }
}
