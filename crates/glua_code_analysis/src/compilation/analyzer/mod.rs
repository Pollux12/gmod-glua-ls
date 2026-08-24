mod call_site_params;
mod common;
mod decl;
mod doc;
mod dynamic_field;
mod flow;
pub(crate) mod gmod;
mod infer_cache_manager;
mod local_inference;
mod lua;
pub(crate) mod parallel;
mod setmetatable_factory;
pub(crate) mod unresolve;

pub(crate) use lua::{dominating_guarded_table_bootstrap_range, infer_for_range_iter_expr_func};

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    AsyncState, FileId, GmodScopedClassInfo, InFiled, InferFailReason, LuaDeclId, LuaDefinitionId,
    LuaFunctionType, LuaInferenceNodeId, LuaInferredGuardOwner, LuaMember, LuaMemberFeature,
    LuaMemberId, LuaMemberKey, LuaSignatureId, LuaType, LuaTypeCache, LuaTypeDeclId, LuaTypeFact,
    LuaTypeOwner, LuaUnionType, WorkspaceId,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{DbIndex, LuaMemberOwner},
    profile::Profile,
    semantic::infer_expr_fact_with_cache,
};
use glua_parser::{
    BinaryOperator, LuaAstNode, LuaCallExpr, LuaChunk, LuaClosureExpr, LuaExpr, LuaNameExpr,
    LuaSyntaxId, LuaSyntaxNode,
};
use infer_cache_manager::InferCacheManager;
use lua::LuaReturnPoint;
use unresolve::{UnResolve, UnResolveReturn};

/// Ceiling on [`AnalyzeContext::resolve_call_site_return_consumers`] rounds.
/// The set converges in a handful of rounds on real workspaces; the bound only
/// stops a mutually recursive chain from spinning.
const CALL_SITE_RETURN_CONSUMER_ROUNDS: usize = 32;

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

pub fn analyze(db: &mut DbIndex, need_analyzed_files: Vec<InFiled<LuaChunk>>) {
    if need_analyzed_files.is_empty() {
        return;
    }

    let mut contexts = {
        let _p = Profile::new("module_analyze");
        module_analyze(db, need_analyzed_files)
    };

    // Declaration and documentation indexing runs for *every* workspace
    // group before any group enters resolution. Both passes are per-file
    // syntactic walks that only populate the decl/type/signature indexes,
    // so hoisting them costs nothing — each file is still visited exactly
    // once — but it guarantees that later stages observe the complete
    // signature and type declaration set rather than only the groups
    // analysed so far.
    for (workspace_id, context) in contexts.iter_mut() {
        context.workspace_id = Some(*workspace_id);
        let profile_log = format!("declare workspace {}", workspace_id);
        let _p = Profile::cond_new(&profile_log, context.tree_list.len() > 1);
        run_analysis::<decl::DeclAnalysisPipeline>(db, context);
        run_analysis::<doc::DocAnalysisPipeline>(db, context);
    }

    // Scripted-class and load call sites are collected for *every* group
    // here, for the same reason declaration and documentation indexing is
    // hoisted: they are per-file syntactic walks, and the passes that read
    // them are not per-group.
    {
        let _p = Profile::new("collect_gmod_call_sites");
        for (_, context) in contexts.iter_mut() {
            gmod::collect_gmod_call_sites(db, context);
        }
    }

    for (workspace_id, mut context) in contexts {
        let profile_log = format!("analyze workspace {}", workspace_id);
        let _p = Profile::cond_new(&profile_log, context.tree_list.len() > 1);
        let workspace_file_ids = context
            .tree_list
            .iter()
            .map(|in_filed_tree| in_filed_tree.file_id)
            .collect::<Vec<_>>();

        // Mirrors the per-group lifecycle of the infer-cache visibility flag:
        // this group's dynamic-field facts are not complete until its own
        // dynamic-field pass has run.
        db.get_dynamic_field_index_mut().set_sealed(false);

        run_analysis::<gmod::GmodPreAnalysisPipeline>(db, &mut context);
        let early_signature_owners = {
            let _p = Profile::new("publish_callable_signatures");
            publish_callable_signatures(db, &context)
        };
        run_analysis::<flow::FlowAnalysisPipeline>(db, &mut context);

        let early_member_owners = {
            let _p = Profile::new("resolve_early_member_owners");
            resolve_early_member_owners(db, &mut context)
        };
        let _p_guards = Profile::new("early inferred guards");
        local_inference::prepare_inferred_positive_guards(db, &context);
        let guard_candidates = context.inferred_guard_candidates.len();
        let early_guard_stats = stabilize_inferred_positive_guards(db, &mut context);
        drop(_p_guards);

        run_analysis::<lua::LuaAnalysisPipeline>(db, &mut context);

        // Gmod post-analysis: synthesize members that depend on metadata collected
        // during lua_analyze (AccessorFunc, NetworkVar, VGUI register calls).
        run_analysis::<gmod::GmodPostAnalysisPipeline>(db, &mut context);

        {
            let _p = Profile::new("synthesize_accessorfunc_members");
            synthesize_accessorfunc_members(db, &workspace_file_ids);
        }
        let infer_dynamic_fields =
            db.get_emmyrc().gmod.enabled && db.get_emmyrc().gmod.infer_dynamic_fields;
        if infer_dynamic_fields {
            // Special-call resolution needs dynamic fields that point at outparam
            // tables, while some dynamic fields need unresolve-refined aliases.
            // Seed only declared-member table fields before unresolve; the full
            // dynamic pass still runs afterward.
            run_analysis::<dynamic_field::EarlyDynamicFieldAnalysisPipeline>(db, &mut context);
        }
        // Every pass above ran against an empty dynamic-field index on a cold
        // build; a warm re-index must not let retained facts leak into them.
        context.infer_manager.set_dynamic_fields_visible();

        // Seed direct parent-to-child evidence before function returns are
        // resolved. The later pass still captures evidence unlocked by unresolve.
        // Both passes read the same declaration references and syntax trees, so
        // the first one's site walk is shared with the second.
        let mut unguarded_child_sites = local_inference::UnguardedChildSiteCache::new();
        let early_child_sources = local_inference::stabilize_unguarded_children(
            db,
            &mut context,
            true,
            &mut unguarded_child_sites,
        );
        context.invalidate_inferred_returns_for_sources(db, &early_child_sources);

        if infer_dynamic_fields {
            run_analysis::<unresolve::PreDynamicUnResolveAnalysisPipeline>(db, &mut context);
        } else {
            run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
        }
        setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);

        run_analysis::<call_site_params::CallSiteParamAnalysisPipeline>(db, &mut context);

        let call_site_return_invalidation_changed = context.call_site_return_invalidation_changed;
        let local_inference_changed = local_inference::stabilize_unknown_locals(db, &mut context);
        let late_guard_retries = context.inferred_guard_candidates.len();
        let late_guard_stats = stabilize_inferred_positive_guards(db, &mut context);
        let inferred_guard_changed = late_guard_stats.changed;
        let late_inference_changed = call_site_return_invalidation_changed
            || local_inference_changed
            || inferred_guard_changed;

        if infer_dynamic_fields {
            run_analysis::<dynamic_field::DynamicFieldAnalysisPipeline>(db, &mut context);
            db.get_dynamic_field_index_mut().set_sealed(true);
            context.infer_manager.clear();
            run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
            setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);
            refresh_initializer_caches(db, &mut context);
        } else if late_inference_changed {
            // Late inference facts can unlock deferred locals even when the
            // optional dynamic-field pass is disabled. Retry that retained work
            // in-place instead of reindexing the entire file batch.
            context.infer_manager.clear();
            run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
            setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);
            refresh_initializer_caches(db, &mut context);
        }

        context.resolve_call_site_return_consumers(db);

        // Unguarded-child inference is a fallback. Run it only after dynamic
        // fields and retained unresolves have stabilized declaration types.
        let late_child_sources = local_inference::stabilize_unguarded_children(
            db,
            &mut context,
            false,
            &mut unguarded_child_sites,
        );
        let late_child_local_changed = if late_child_sources.is_empty() {
            false
        } else {
            local_inference::stabilize_unknown_locals(db, &mut context)
        };
        let late_child_returns =
            context.requeue_inferred_returns_for_sources(db, &late_child_sources);
        if !late_child_sources.is_empty() {
            context.infer_manager.clear();
            if late_child_local_changed || late_child_returns != 0 {
                run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
                setmetatable_factory::synthesize_setmetatable_factory_members(
                    db,
                    &workspace_file_ids,
                );
            }
            refresh_initializer_caches(db, &mut context);
        }

        {
            let _p = Profile::new("attach_settled_index_expr_members");
            attach_settled_index_expr_members(db, &mut context);
        }

        {
            let _p = Profile::new("rederive_settled_inferred_returns");
            // A return settled here was still `unknown` when the call-site
            // consumers read it, so those consumers hold a value derived from
            // it that is now stale. A warm re-index inherits the settled return
            // and never sees the stale one, so leaving them is drift.
            if rederive_settled_inferred_returns(db, &mut context) {
                context.resolve_call_site_return_consumers(db);
            }
        }

        {
            let _p = Profile::new("rewiden_settled_member_assignments");
            rewiden_settled_member_assignments(db, &mut context);
        }

        // Members that landed on a global path before the global's owner was
        // known are attached now that it is. See
        // `reconcile_parked_global_path_members`.
        {
            let _p = Profile::new("reconcile_parked_global_path_members");
            common::reconcile_parked_global_path_members(db);
        }

        // Writes that inferred their prefix to one concrete declaration of a
        // multi-declaration global attach directly to that table and never
        // park, so which table won depends on batch composition. Re-apply the
        // ownership rule to them now that every declaration stands. See
        // `reconcile_directly_attached_candidate_members`.
        {
            let _p = Profile::new("reconcile_directly_attached_candidate_members");
            common::reconcile_directly_attached_candidate_members(db);
        }

        // Runs last of the settled passes: it needs every member to have reached
        // its final owner, because the writer set it merges is grouped by owner.
        {
            let _p = Profile::new("rederive_contributed_member_assignments");
            let analyzed_files = context.analyzed_file_ids();
            lua::rederive_contributed_member_assignments(db, &analyzed_files);
        }

        // Every settled pass above refines the types the member attach retry
        // reads, so candidates it could not place on the first attempt can be
        // placed now. Without this a member's existence depends on how far
        // inference had progressed when its file happened to be walked.
        {
            let _p = Profile::new("attach_settled_index_expr_members (late)");
            attach_settled_index_expr_members(db, &mut context);
        }

        // The late attach can still place members straight onto whichever
        // candidate table its prefix resolved to, so the direct-attached
        // repair has to see its results too.
        {
            let _p = Profile::new("reconcile_directly_attached_candidate_members (late)");
            common::reconcile_directly_attached_candidate_members(db);
        }

        // Every write and every alias this batch performed has landed, so the
        // slots they share can be settled from the ownership they ended on.
        {
            let _p = Profile::new("settle_alias_contributed_slots");
            db.get_member_index_mut().settle_alias_contributed_slots();
        }

        // Net flows are collected last: the collector resolves wrappers through
        // signatures, receiver types and members, none of which exist yet when
        // the gmod pre-pass runs. See `GmodNetworkAnalysisPipeline`.
        run_analysis::<gmod::GmodNetworkAnalysisPipeline>(db, &mut context);

        for (consumer_file_id, owners) in context.infer_manager.drain_inferred_guard_dependencies()
        {
            context.add_inferred_guard_dependencies(consumer_file_id, owners);
        }
        for (consumer_file_id, owners) in std::mem::take(&mut context.inferred_guard_dependencies) {
            db.get_signature_index_mut()
                .set_inferred_guard_dependencies(consumer_file_id, owners);
        }

        if std::env::var_os("GLUALS_PROFILE").is_some() {
            eprintln!(
                "[profile] member_assignment_contributions entries={}",
                db.get_member_index()
                    .member_assignment_contributions()
                    .entry_count(),
            );
            eprintln!(
                "[profile] inferred_guard candidates={} candidate_attempts={} candidate_iterations={} early_published={} late_retries={} late_published={} pending={} early_signature_owners={} early_member_owners={}",
                guard_candidates,
                early_guard_stats.attempts + late_guard_stats.attempts,
                early_guard_stats.iterations + late_guard_stats.iterations,
                early_guard_stats.published,
                late_guard_retries,
                late_guard_stats.published,
                context.inferred_guard_candidates.len(),
                early_signature_owners,
                early_member_owners,
            );
        }
    }
}

/// Retries the index-expression member attaches `set_index_expr_owner`
/// dropped.
fn attach_settled_index_expr_members(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let mut candidates = std::mem::take(&mut context.settled_member_attach_candidates);
    if candidates.is_empty() {
        return;
    }
    candidates.sort_by_key(|candidate| (candidate.file_id, candidate.value.get_range().start()));
    candidates.dedup();
    let mut retry = Vec::new();

    // Only the candidate files are re-inferred, so only their caches are stale.
    // Clearing the whole manager would also discard caches the passes that ran
    // before this one built for files this pass never touches.
    let candidate_files = candidates
        .iter()
        .map(|candidate| candidate.file_id)
        .collect::<HashSet<_>>();
    context.infer_manager.clear_files(&candidate_files);

    for candidate in candidates {
        let file_id = candidate.file_id;
        let member_id = LuaMemberId::new(candidate.value, file_id);
        // A member that already found a real owner is not the gap this pass
        // exists for: some other authority decided where it lives, and re-homing
        // it here would overrule that decision with a later, weaker read.
        if db
            .get_member_index()
            .get_current_owner(&member_id)
            .is_some_and(|owner| !matches!(owner, LuaMemberOwner::LocalUnresolve))
        {
            continue;
        }
        let Some(prefix_expr) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
            .and_then(|root| candidate.value.to_node_from_root(&root))
            .and_then(glua_parser::LuaIndexExpr::cast)
            .and_then(|index_expr| index_expr.get_prefix_expr())
        else {
            continue;
        };
        let mut unresolve_member = unresolve::UnResolveMember {
            file_id,
            member_id,
            expr: None,
            prefix: Some(prefix_expr),
            ret_idx: 0,
        };
        let cache = context.infer_manager.get_infer_cache(file_id);
        if unresolve::try_resolve_member(db, cache, &mut unresolve_member).is_err() {
            // The prefix still has not settled. Dropping it here is what made a
            // member's existence depend on analysis order: the passes that run
            // after this one go on refining the very types this retry needs, so
            // a candidate that fails now can succeed once they have. Keep it
            // queued for the next attempt instead.
            retry.push(candidate);
        }
    }

    context.settled_member_attach_candidates = retry;
}

/// Re-resolves inferred returns that settled on `any`/`unknown`.
fn rederive_settled_inferred_returns(db: &mut DbIndex, context: &mut AnalyzeContext) -> bool {
    let mut candidates = context
        .inferred_return_candidates
        .iter()
        .filter(|return_| {
            db.get_signature_index()
                .get(&return_.signature_id)
                .is_some_and(|signature| {
                    signature.resolve_return == crate::SignatureReturnStatus::InferResolve && {
                        let current = signature.get_return_type();
                        current.is_any() || current.is_unknown()
                    }
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return false;
    }
    candidates.sort_by_key(|return_| (return_.file_id, return_.signature_id.get_position()));

    // Only the candidate files are re-inferred, so only their caches are stale.
    let candidate_files = candidates
        .iter()
        .map(|return_| return_.file_id)
        .collect::<HashSet<_>>();
    context.infer_manager.clear_files(&candidate_files);

    let mut changed = false;
    for mut return_ in candidates {
        let signature_id = return_.signature_id.clone();
        let cache = context.infer_manager.get_infer_cache(return_.file_id);
        let _ = unresolve::try_resolve_return_point(db, cache, &mut return_);
        changed |= db
            .get_signature_index()
            .get(&signature_id)
            .is_some_and(|signature| {
                let resolved = signature.get_return_type();
                !resolved.is_any() && !resolved.is_unknown()
            });
    }
    changed
}

/// Re-derives member assignment widenings that ran against an incomplete
/// set of sibling writers.
fn rewiden_settled_member_assignments(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let candidates = std::mem::take(&mut context.settled_member_widening_candidates);
    if candidates.is_empty() {
        return;
    }
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by_key(|(member_id, _)| (member_id.file_id, member_id.get_position()));

    for (member_id, (assigned_type, preserve_table_literals)) in candidates {
        // Only an inferred assignment cache is this pass' to rewrite: a doc type
        // outranks inference, and anything else reaching the slot was written by
        // an authority this pass has no evidence to overrule.
        if !db
            .get_type_index()
            .get_type_cache(&member_id.into())
            .is_some_and(|cache| cache.is_infer())
        {
            continue;
        }

        let type_owner = LuaTypeOwner::Member(member_id);
        let Some(widened_type) = lua::get_widened_member_assignment_type(
            db,
            &type_owner,
            &assigned_type,
            preserve_table_literals,
            &mut false,
        ) else {
            continue;
        };

        db.get_type_index_mut()
            .force_bind_type(type_owner, LuaTypeCache::InferType(widened_type));
    }
}

#[derive(Default)]
struct InferredGuardFixedPointStats {
    attempts: usize,
    iterations: usize,
    published: usize,
    changed: bool,
}

fn stabilize_inferred_positive_guards(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
) -> InferredGuardFixedPointStats {
    let mut stats = InferredGuardFixedPointStats::default();
    let max_iterations = context.inferred_guard_candidates.len().saturating_add(1);

    while !context.inferred_guard_candidates.is_empty() && stats.iterations < max_iterations {
        let attempted = context.inferred_guard_candidates.len();
        let published = local_inference::publish_inferred_positive_guards(db, context);
        let changed = db
            .get_signature_index_mut()
            .take_inferred_positive_guards_changed();
        stats.attempts += attempted;
        stats.iterations += 1;
        stats.published += published;
        stats.changed |= changed;
        if !changed {
            break;
        }

        let pending_files = context
            .inferred_guard_candidates
            .iter()
            .map(|candidate| candidate.file_id)
            .collect::<HashSet<_>>();
        context.infer_manager.clear_files(&pending_files);
    }

    stats
}

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

fn refresh_initializer_caches(db: &mut DbIndex, context: &mut AnalyzeContext) {
    refresh_local_decl_initializer_caches(db, context);
    refresh_member_initializer_caches(db, context);
}

fn refresh_local_decl_initializer_caches(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if context.uninformative_local_decl_candidates.is_empty() {
        return;
    }

    let mut candidates_by_file = HashMap::<FileId, Vec<LuaDeclId>>::new();
    for decl_id in &context.uninformative_local_decl_candidates {
        candidates_by_file
            .entry(decl_id.file_id)
            .or_default()
            .push(*decl_id);
    }
    for candidates in candidates_by_file.values_mut() {
        candidates.sort_by_key(|decl_id| decl_id.position);
    }
    let mut file_ids = candidates_by_file.keys().copied().collect::<Vec<_>>();
    file_ids.sort();
    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();

    // Initializer inference reads the stabilized indexes and records candidate
    // cache writes without mutating the database. Process that read-only work
    // per file, then merge inference side effects and type writes in stable file
    // and source order on the caller thread.
    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let mut infer_cache = crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
            },
        );
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            return InitializerRefreshResult::new(file_id);
        };
        let mut result = InitializerRefreshResult::new(file_id);
        for decl_id in &candidates_by_file[&file_id] {
            let type_owner = (*decl_id).into();
            let current_cache = db.get_type_index().get_type_cache(&type_owner).cloned();
            if current_cache.as_ref().is_some_and(LuaTypeCache::is_doc) {
                continue;
            }
            let current_is_uninformative = type_cache_is_uninformative(current_cache.as_ref());
            let current_fact = db.get_type_index().get_type_fact(&type_owner);
            let target_node = LuaInferenceNodeId::TypeOwner(type_owner.clone());
            let can_upgrade_authority = current_fact.as_ref().is_some_and(|fact| {
                fact.base_provenance_kind()
                    != Some(crate::LuaInferenceProvenanceKind::ExplicitAnnotation)
            });
            let can_refine_nominal_type = !current_is_uninformative
                && db.get_emmyrc().gmod.enabled
                && current_cache
                    .as_ref()
                    .is_some_and(|current| single_nominal_type_id(current.as_type()).is_some())
                && db
                    .get_reference_index()
                    .get_decl_references(&decl_id.file_id, decl_id)
                    .is_none_or(|references| !references.mutable);
            let Some((ret_idx, expr)) = local_initializer_expr(db, &root, *decl_id) else {
                continue;
            };
            if !initializer_reads_through_call_or_index(&expr) {
                continue;
            }

            // Every pass before the dynamic-field one ran without those facts,
            // so an initializer that reads a dynamic field was answered blind
            // and the answer was cached as if it were final. Re-inferring with
            // the index hidden reproduces exactly that blind answer, so where
            // it differs from the settled one *and* matches what is cached, the
            // cache is provably the guess and the settled read replaces it.
            // Whether the field's writer had been walked yet is a property of
            // the batch, not of the source: cold cached `false` for
            // `local on = LocalPlayer()._flag or false` where re-analysing the
            // same unchanged file cached `true`.
            let inferred_fact = select_result_fact(
                infer_expr_fact_with_cache(db, &mut infer_cache, expr.clone()),
                ret_idx,
            );
            let inferred_type = inferred_fact.typ().clone();

            // Only asked when nothing else would let the settled read through
            // and it actually disagrees with the cache, so the second inference
            // is paid for the handful of decls whose answer it can change.
            let cached_a_blind_dynamic_field_read = !current_is_uninformative
                && dynamic_fields_visible
                && current_cache
                    .as_ref()
                    .is_some_and(|current| current.as_type() != &inferred_type)
                && {
                    let mut blind_cache = crate::LuaInferCache::new(
                        file_id,
                        crate::CacheOptions {
                            analysis_phase,
                            dynamic_fields_visible: false,
                            building_dynamic_field_index: false,
                        },
                    );
                    let blind_type = select_result_fact(
                        infer_expr_fact_with_cache(db, &mut blind_cache, expr),
                        ret_idx,
                    )
                    .typ()
                    .clone();
                    current_cache
                        .as_ref()
                        .is_some_and(|current| current.as_type() == &blind_type)
                        && blind_type != inferred_type
                };
            if !current_is_uninformative
                && !can_refine_nominal_type
                && !can_upgrade_authority
                && !cached_a_blind_dynamic_field_read
            {
                continue;
            }
            if type_is_uninformative(&inferred_type) {
                // When the cache and the settled re-derivation disagree
                // over *which* bottom an unresolvable initializer has, both
                // are false certainties — `nil` and `never` are only ever
                // reached here by giving up, so which one is cached is
                // decided by arrival order. Canonicalize to `unknown`,
                // which is the honest answer and is opaque to the checkers,
                // so it neither silences a real report nor invents one.
                if is_bottom(&inferred_type)
                    && current_cache
                        .as_ref()
                        .is_some_and(|current| is_bottom(current.as_type()))
                    && current_cache.as_ref().map(LuaTypeCache::as_type) != Some(&inferred_type)
                {
                    result.updates.push(InitializerCacheUpdate::Overwrite {
                        owner: type_owner,
                        fact: inferred_fact.with_runtime_type(LuaType::Unknown),
                    });
                }
                continue;
            }
            if current_cache
                .as_ref()
                .is_some_and(|current| current.as_type() == &inferred_type)
            {
                if current_fact.as_ref().is_some_and(|current| {
                    inferred_fact.has_independently_stronger_authority_than(current, &target_node)
                }) {
                    result.updates.push(InitializerCacheUpdate::ReplaceFact {
                        owner: type_owner,
                        fact: inferred_fact,
                    });
                }
                continue;
            }

            let has_stronger_declared_authority = can_upgrade_authority
                && current_fact.as_ref().is_some_and(|current| {
                    inferred_fact.base_provenance_kind()
                        == Some(crate::LuaInferenceProvenanceKind::ExplicitAnnotation)
                        && inferred_fact
                            .has_independently_stronger_authority_than(current, &target_node)
                });
            let is_nominal_refinement = can_refine_nominal_type
                && current_cache.as_ref().is_some_and(|current| {
                    is_strict_nominal_refinement(db, &inferred_type, current.as_type())
                });
            let is_settled_widening = current_cache
                .as_ref()
                .is_some_and(|current| union_widens_arm(&inferred_type, current.as_type()));
            if current_is_uninformative {
                result.updates.push(InitializerCacheUpdate::Bind {
                    owner: type_owner,
                    fact: inferred_fact,
                });
            } else if has_stronger_declared_authority
                || is_nominal_refinement
                || is_settled_widening
                || cached_a_blind_dynamic_field_read
            {
                result.updates.push(InitializerCacheUpdate::Overwrite {
                    owner: type_owner,
                    fact: inferred_fact,
                });
            }
        }
        result.pending_type_decls = infer_cache.take_pending_str_tpl_type_decls();
        result.guard_dependencies = infer_cache.take_inferred_guard_dependencies();
        result
    });

    let mut updates = Vec::new();
    for result in results {
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        updates.extend(result.updates);
    }
    apply_initializer_cache_updates(db, updates);
}

/// Whether the re-derived type is a union that already contains the cached one.
///
/// The cache then holds a subset snapshot taken before the other arms were
/// visible, so replacing it widens to the settled answer instead of guessing a
/// different one.
fn union_widens_arm(inferred: &LuaType, current: &LuaType) -> bool {
    match inferred {
        LuaType::Union(union) => union.types().any(|arm| arm == current),
        _ => false,
    }
}

/// Whether the re-derived union contains everything the cached type holds, plus
/// more — the union-to-union counterpart of [`union_widens_arm`].
///
/// A cached union is as much a subset snapshot as a cached single arm is: both
/// are decided by which contributors happened to be indexed first.
pub(crate) fn union_widens_cached_type(inferred: &LuaType, current: &LuaType) -> bool {
    let LuaType::Union(inferred_union) = inferred else {
        return false;
    };
    let inferred_arms = known_arms(inferred_union);
    match current {
        LuaType::Union(current_union) => {
            let current_arms = known_arms(current_union);
            current_arms.len() < inferred_arms.len()
                && current_arms.iter().all(|arm| inferred_arms.contains(arm))
        }
        current => inferred_arms.contains(current),
    }
}

/// The union arms that carry information. An `unknown` arm stands for a type
/// that has not settled yet, so it can neither widen a cache nor block one.
fn known_arms(union: &LuaUnionType) -> Vec<LuaType> {
    union
        .types()
        .filter(|arm| !matches!(arm, LuaType::Unknown))
        .cloned()
        .collect()
}

fn refresh_member_initializer_caches(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if context.member_initializer_reinfer_candidates.is_empty() {
        return;
    }

    let mut candidates_by_file = HashMap::<FileId, Vec<LuaMemberId>>::new();
    for member_id in &context.member_initializer_reinfer_candidates {
        candidates_by_file
            .entry(member_id.file_id)
            .or_default()
            .push(*member_id);
    }
    for candidates in candidates_by_file.values_mut() {
        candidates.sort_by_key(LuaMemberId::get_position);
    }
    let mut file_ids = candidates_by_file.keys().copied().collect::<Vec<_>>();
    file_ids.sort();
    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();

    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let mut infer_cache = crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
            },
        );
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            return InitializerRefreshResult::new(file_id);
        };
        let mut result = InitializerRefreshResult::new(file_id);
        for member_id in &candidates_by_file[&file_id] {
            let type_owner = LuaTypeOwner::Member(*member_id);
            let Some(current_cache) = db.get_type_index().get_type_cache(&type_owner).cloned()
            else {
                continue;
            };
            let current_is_uninformative = type_is_uninformative(current_cache.as_type());
            if current_cache.is_doc()
                || (!current_is_uninformative
                    && single_nominal_type_id(current_cache.as_type()).is_none())
            {
                continue;
            }
            let Some(expr) = member_initializer_expr(&root, *member_id) else {
                continue;
            };
            let Ok(inferred_type) = crate::infer_expr(db, &mut infer_cache, expr) else {
                continue;
            };
            if inferred_type == *current_cache.as_type() {
                continue;
            }
            let takes_inferred_type = if current_is_uninformative {
                // A placeholder is not an answer: it only records that the
                // member's initializer had not been inferred yet when the write
                // landed. Re-inferring it against the settled index is the same
                // question, asked once the facts exist.
                !type_is_uninformative(&inferred_type)
            } else {
                is_strict_nominal_refinement(db, &inferred_type, current_cache.as_type())
            };
            if !takes_inferred_type {
                continue;
            }

            result.updates.push(InitializerCacheUpdate::Overwrite {
                owner: type_owner,
                fact: LuaTypeFact::certain(inferred_type),
            });
        }
        result.pending_type_decls = infer_cache.take_pending_str_tpl_type_decls();
        result.guard_dependencies = infer_cache.take_inferred_guard_dependencies();
        result
    });

    let mut updates = Vec::new();
    for result in results {
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        updates.extend(result.updates);
    }
    apply_initializer_cache_updates(db, updates);
}

struct InitializerRefreshResult {
    file_id: FileId,
    pending_type_decls: Vec<crate::PendingStrTplTypeDecl>,
    guard_dependencies: HashSet<LuaInferredGuardOwner>,
    updates: Vec<InitializerCacheUpdate>,
}

impl InitializerRefreshResult {
    fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            pending_type_decls: Vec::new(),
            guard_dependencies: HashSet::new(),
            updates: Vec::new(),
        }
    }
}

enum InitializerCacheUpdate {
    Bind {
        owner: LuaTypeOwner,
        fact: LuaTypeFact,
    },
    Overwrite {
        owner: LuaTypeOwner,
        fact: LuaTypeFact,
    },
    ReplaceFact {
        owner: LuaTypeOwner,
        fact: LuaTypeFact,
    },
}

fn apply_initializer_cache_updates(db: &mut DbIndex, updates: Vec<InitializerCacheUpdate>) {
    let mut fact_updates = Vec::with_capacity(updates.len());
    for update in updates {
        match update {
            InitializerCacheUpdate::Bind { owner, fact } => {
                common::bind_resolved_type(
                    db,
                    owner.clone(),
                    LuaTypeCache::InferType(fact.typ().clone()),
                );
                if db
                    .get_type_index()
                    .get_type_cache(&owner)
                    .is_some_and(|cache| cache.as_type() == fact.typ())
                {
                    fact_updates.push((LuaInferenceNodeId::TypeOwner(owner), fact));
                }
            }
            InitializerCacheUpdate::Overwrite { owner, fact }
            | InitializerCacheUpdate::ReplaceFact { owner, fact } => {
                fact_updates.push((LuaInferenceNodeId::TypeOwner(owner), fact));
            }
        }
    }
    db.publish_inference_facts(fact_updates);
}

fn select_result_fact(fact: LuaTypeFact, result_idx: usize) -> LuaTypeFact {
    let typ = match fact.typ() {
        LuaType::Variadic(variadic) => variadic
            .get_type(result_idx)
            .cloned()
            .unwrap_or(LuaType::Nil),
        typ if result_idx == 0 => typ.clone(),
        _ => LuaType::Nil,
    };
    fact.with_runtime_type(typ)
}

fn member_initializer_expr(root: &LuaSyntaxNode, member_id: LuaMemberId) -> Option<LuaExpr> {
    let node = member_id.get_syntax_id().to_node_from_root(root)?;
    let index_expr = glua_parser::LuaIndexExpr::cast(node)?;
    let assign_stat = index_expr.get_parent::<glua_parser::LuaAssignStat>()?;
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let value_idx = vars
        .iter()
        .position(|var| var.syntax().text_range() == index_expr.get_range())?;
    exprs.get(value_idx).cloned()
}

fn is_strict_nominal_refinement(db: &DbIndex, candidate: &LuaType, current: &LuaType) -> bool {
    let Some(candidate_id) = single_nominal_type_id(candidate) else {
        return false;
    };
    let Some(current_id) = single_nominal_type_id(current) else {
        return false;
    };
    candidate_id != current_id && crate::semantic::is_sub_type_of(db, &candidate_id, &current_id)
}

fn single_nominal_type_id(typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Def(type_id) | LuaType::Ref(type_id) => Some(type_id.clone()),
        LuaType::Instance(instance) => single_nominal_type_id(instance.get_base()),
        LuaType::Union(union) => {
            let mut nominal = None;
            for component in union.types().filter(|component| !component.is_nil()) {
                let type_id = single_nominal_type_id(component)?;
                if nominal
                    .as_ref()
                    .is_some_and(|existing| existing != &type_id)
                {
                    return None;
                }
                nominal = Some(type_id);
            }
            nominal
        }
        _ => None,
    }
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
        LuaExpr::BinaryExpr(binary) => {
            matches!(
                binary.get_op_token().map(|op| op.get_op()),
                Some(BinaryOperator::OpOr | BinaryOperator::OpAnd)
            ) && binary.get_exprs().is_some_and(|(left, right)| {
                initializer_reads_through_call_or_index(&left)
                    || initializer_reads_through_call_or_index(&right)
            })
        }
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
    initializer_reads_through_call_or_index(expr) || initializer_is_operator_expr(expr)
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

fn type_cache_is_uninformative(type_cache: Option<&LuaTypeCache>) -> bool {
    match type_cache {
        Some(LuaTypeCache::InferType(typ)) => type_is_uninformative(typ),
        Some(LuaTypeCache::DocType(_)) => false,
        None => true,
    }
}

/// The two lattice bottoms. Both are reached by giving up on an expression, so
/// neither carries evidence about the value — unlike `any`/`unknown`, which the
/// checkers already treat as opaque.
fn is_bottom(typ: &LuaType) -> bool {
    matches!(typ, LuaType::Nil | LuaType::Never)
}

fn type_is_uninformative(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Never => true,
        LuaType::Union(union) => union.types().all(type_is_uninformative),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(typ, _)| type_is_uninformative(typ)),
        _ => false,
    }
}

fn synthesize_accessorfunc_members(db: &mut DbIndex, file_ids: &[FileId]) {
    let workspace_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
    let all_calls = db
        .get_accessor_func_call_index()
        .iter()
        .filter(|(file_id, _)| workspace_file_ids.contains(file_id))
        .map(|(file_id, calls)| (*file_id, calls.clone()))
        .collect::<Vec<_>>();

    for (file_id, file_calls) in all_calls {
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

fn module_analyze(
    db: &mut DbIndex,
    need_analyzed_files: Vec<InFiled<LuaChunk>>,
) -> Vec<(WorkspaceId, AnalyzeContext)> {
    if need_analyzed_files.len() == 1 {
        let in_filed_tree = need_analyzed_files[0].clone();
        let file_id = in_filed_tree.file_id;
        if let Some(path) = db.get_vfs().get_file_path(&file_id).cloned() {
            let path_str = match path.to_str() {
                Some(path) => path,
                None => {
                    log::warn!("file_id {:?} path not found", file_id);
                    return vec![];
                }
            };

            let workspace_id = db
                .get_module_index_mut()
                .add_module_by_path(file_id, path_str);
            let workspace_id = workspace_id.unwrap_or(WorkspaceId::MAIN);
            let mut context = AnalyzeContext::new();
            context.add_tree_chunk(in_filed_tree);
            return vec![(workspace_id, context)];
        } else if db.get_vfs().is_remote_file(&file_id) {
            let mut context = AnalyzeContext::new();
            context.add_tree_chunk(in_filed_tree);
            return vec![(WorkspaceId::REMOTE, context)];
        };

        return vec![];
    }

    let mut file_tree_map: HashMap<WorkspaceId, Vec<InFiled<LuaChunk>>> = HashMap::new();
    for in_filed_tree in need_analyzed_files {
        let file_id = in_filed_tree.file_id;
        if let Some(path) = db.get_vfs().get_file_path(&file_id).cloned() {
            let path_str = match path.to_str() {
                Some(path) => path,
                None => {
                    log::warn!("file_id {:?} path not found", file_id);
                    continue;
                }
            };

            let workspace_id = db
                .get_module_index_mut()
                .add_module_by_path(file_id, path_str);
            let workspace_id = workspace_id.unwrap_or(WorkspaceId::MAIN);
            file_tree_map
                .entry(workspace_id)
                .or_default()
                .push(in_filed_tree);
        } else if db.get_vfs().is_remote_file(&file_id) {
            file_tree_map
                .entry(WorkspaceId::REMOTE)
                .or_default()
                .push(in_filed_tree);
        }
    }

    let mut contexts = Vec::new();
    if let Some(std_lib) = file_tree_map.remove(&WorkspaceId::STD) {
        let mut context = AnalyzeContext::new();
        context.tree_list = std_lib;
        contexts.push((WorkspaceId::STD, context));
    }

    let mut main_vec = Vec::new();
    for (workspace_id, tree_list) in file_tree_map {
        let mut context = AnalyzeContext::new();
        context.tree_list = tree_list;
        if db.get_module_index().is_library_workspace_id(workspace_id)
            || db.get_module_index().is_remote_workspace_id(workspace_id)
        {
            contexts.push((workspace_id, context));
        } else {
            main_vec.push((workspace_id, context));
        }
    }

    contexts.sort_by_key(|a| a.0);
    main_vec.sort_by_key(|a| a.0);

    contexts.extend(main_vec);
    contexts
}

#[derive(Debug)]
pub struct AnalyzeContext {
    tree_list: Vec<InFiled<LuaChunk>>,
    metas: HashSet<FileId>,
    scripted_scope_files: Option<Arc<HashSet<FileId>>>,
    scripted_scope_infos: Option<Arc<HashMap<FileId, GmodScopedClassInfo>>>,
    /// Annotated global call-role map built by the gmod pre-pass, keyed by the
    /// helper-registry revision it was derived from. Building it is a full
    /// signature-index scan, so the late network pass reuses it whenever the
    /// index has not grown since.
    #[allow(clippy::type_complexity)]
    gmod_global_call_roles: Option<(u64, Arc<gmod::AnnotatedGmodGlobalCallRoleMap>)>,
    unresolves: Vec<(UnResolve, InferFailReason)>,
    inferred_return_candidates: Vec<UnResolveReturn>,
    pending_call_site_return_consumers: Vec<UnResolve>,
    pending_call_site_definition_refreshes: Vec<(LuaDefinitionId, LuaTypeOwner)>,
    /// Consumers already resolved once, kept so a later pass that settles a
    /// function return can have them re-resolved against it.
    call_site_return_targets: Vec<(FileId, LuaTypeOwner, LuaExpr, usize)>,
    call_site_return_definition_refreshes: HashMap<LuaTypeOwner, Vec<LuaDefinitionId>>,
    pending_unresolve_decl_ids: HashSet<LuaDeclId>,
    uninformative_local_decl_candidates: HashSet<LuaDeclId>,
    member_initializer_reinfer_candidates: HashSet<LuaMemberId>,
    infer_manager: InferCacheManager,
    inferred_guard_dependencies: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_guard_candidates: Vec<InFiled<LuaSyntaxId>>,
    early_callable_signatures: Vec<(crate::LuaTypeOwner, LuaSignatureId)>,
    early_member_owner_candidates: Vec<(LuaMemberId, LuaDeclId)>,
    /// Index-expression member attaches that `set_index_expr_owner` dropped
    /// because the prefix carried no owner information yet. See
    /// [`attach_settled_index_expr_members`].
    settled_member_attach_candidates: Vec<InFiled<LuaSyntaxId>>,
    /// Member assignments whose widening ran against an incomplete sibling set,
    /// with the type each one actually assigned. See
    /// [`rewiden_settled_member_assignments`].
    settled_member_widening_candidates: HashMap<LuaMemberId, (LuaType, bool)>,
    call_site_return_invalidation_changed: bool,
    pub workspace_id: Option<WorkspaceId>,
}

impl AnalyzeContext {
    pub fn new() -> Self {
        Self {
            tree_list: Vec::new(),
            metas: HashSet::new(),
            scripted_scope_files: None,
            scripted_scope_infos: None,
            gmod_global_call_roles: None,
            unresolves: Vec::new(),
            inferred_return_candidates: Vec::new(),
            pending_call_site_return_consumers: Vec::new(),
            pending_call_site_definition_refreshes: Vec::new(),
            call_site_return_targets: Vec::new(),
            call_site_return_definition_refreshes: HashMap::new(),
            pending_unresolve_decl_ids: HashSet::new(),
            uninformative_local_decl_candidates: HashSet::new(),
            member_initializer_reinfer_candidates: HashSet::new(),
            infer_manager: InferCacheManager::new(),
            inferred_guard_dependencies: HashMap::new(),
            inferred_guard_candidates: Vec::new(),
            early_callable_signatures: Vec::new(),
            early_member_owner_candidates: Vec::new(),
            settled_member_attach_candidates: Vec::new(),
            settled_member_widening_candidates: HashMap::new(),
            call_site_return_invalidation_changed: false,
            workspace_id: None,
        }
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

    pub fn add_inferred_return_candidate(&mut self, return_: UnResolveReturn) {
        self.inferred_return_candidates.push(return_);
    }

    pub(crate) fn analyzed_file_ids(&self) -> HashSet<FileId> {
        self.tree_list.iter().map(|tree| tree.file_id).collect()
    }

    /// Remembers an assignment whose widening skipped a sibling that had no type
    /// yet. The assigned type is kept as written, not as widened, so the settled
    /// pass can re-derive the merge instead of growing the partial answer.
    pub(crate) fn record_settled_member_widening_candidate(
        &mut self,
        member_id: LuaMemberId,
        assigned_type: LuaType,
        preserve_table_literals: bool,
    ) {
        self.settled_member_widening_candidates
            .insert(member_id, (assigned_type, preserve_table_literals));
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

    pub(crate) fn add_settled_member_attach_candidate(&mut self, candidate: InFiled<LuaSyntaxId>) {
        self.settled_member_attach_candidates.push(candidate);
    }

    pub(crate) fn requeue_call_site_inferred_returns(
        &mut self,
        db: &mut DbIndex,
        signature_ids: &HashSet<LuaSignatureId>,
        consumers: Vec<(
            LuaSignatureId,
            UnResolve,
            Option<(LuaDefinitionId, LuaTypeOwner)>,
        )>,
    ) -> (usize, usize) {
        let mut returns = self
            .inferred_return_candidates
            .iter()
            .filter(|return_| signature_ids.contains(&return_.signature_id))
            .cloned()
            .collect::<Vec<_>>();
        let current_signatures = returns
            .iter()
            .map(|return_| return_.signature_id)
            .collect::<HashSet<_>>();
        returns.extend(
            signature_ids
                .iter()
                .filter(|signature_id| !current_signatures.contains(signature_id))
                .filter_map(|signature_id| inferred_return_candidate(db, *signature_id)),
        );
        returns.retain(|return_| inferred_return_uses_receiver_metatable(db, return_));

        let mut requeued = Vec::new();
        for return_ in returns {
            if let Some(signature) = db.get_signature_index_mut().get_mut(&return_.signature_id)
                && signature.resolve_return == crate::SignatureReturnStatus::InferResolve
            {
                signature.resolve_return = crate::SignatureReturnStatus::UnResolve;
                requeued.push(return_);
            }
        }

        let count = requeued.len();
        let requeued_signatures = requeued
            .iter()
            .map(|return_| return_.signature_id)
            .collect::<HashSet<_>>();
        self.unresolves.extend(
            requeued
                .into_iter()
                .map(|return_| (return_.into(), InferFailReason::None)),
        );

        let mut requeued_consumers = 0;
        for (signature_id, consumer, definition_refresh) in consumers {
            if requeued_signatures.contains(&signature_id) {
                self.pending_call_site_return_consumers.push(consumer);
                if let Some(definition_refresh) = definition_refresh {
                    self.pending_call_site_definition_refreshes
                        .push(definition_refresh);
                }
                requeued_consumers += 1;
            }
        }

        self.call_site_return_invalidation_changed |= count != 0 || requeued_consumers != 0;
        (count, requeued_consumers)
    }

    pub(crate) fn queue_call_site_return_consumers(
        &mut self,
        consumers: Vec<(UnResolve, Option<(LuaDefinitionId, LuaTypeOwner)>)>,
    ) -> usize {
        let count = consumers.len();
        for (consumer, definition_refresh) in consumers {
            self.pending_call_site_return_consumers.push(consumer);
            if let Some(definition_refresh) = definition_refresh {
                self.pending_call_site_definition_refreshes
                    .push(definition_refresh);
            }
        }
        count
    }

    fn resolve_call_site_return_consumers(&mut self, db: &mut DbIndex) -> usize {
        for consumer in std::mem::take(&mut self.pending_call_site_return_consumers) {
            match consumer {
                UnResolve::Decl(decl) => self.call_site_return_targets.push((
                    decl.file_id,
                    LuaTypeOwner::Decl(decl.decl_id),
                    decl.expr,
                    decl.ret_idx,
                )),
                UnResolve::Member(member) => {
                    if let Some(expr) = member.expr {
                        self.call_site_return_targets.push((
                            member.file_id,
                            LuaTypeOwner::Member(member.member_id),
                            expr,
                            member.ret_idx,
                        ));
                    }
                }
                _ => {}
            }
        }
        for (definition, owner) in std::mem::take(&mut self.pending_call_site_definition_refreshes)
        {
            self.call_site_return_definition_refreshes
                .entry(owner)
                .or_default()
                .push(definition);
        }
        if self.call_site_return_targets.is_empty() {
            return 0;
        }

        // These consumers feed each other: one's expression can read a local, or
        // a function return, that another one settles. Inferring the whole set
        // against the pre-publish index leaves every such reader holding its
        // neighbour's *unresolved* value, and whether a neighbour is in this
        // batch or was already published by an earlier build is a property of
        // the batch rather than of the source. Iterate until publishing stops
        // moving anything, so a partial re-index and a cold build agree.
        for _ in 0..CALL_SITE_RETURN_CONSUMER_ROUNDS {
            self.infer_manager.clear();
            let mut fact_updates = Vec::with_capacity(
                self.call_site_return_targets.len()
                    + self
                        .call_site_return_definition_refreshes
                        .values()
                        .map(Vec::len)
                        .sum::<usize>(),
            );
            for (file_id, owner, expr, ret_idx) in &self.call_site_return_targets {
                let cache = self.infer_manager.get_infer_cache(*file_id);
                let fact = select_result_fact(
                    infer_expr_fact_with_cache(db, cache, expr.clone()),
                    *ret_idx,
                );
                fact_updates.push((LuaInferenceNodeId::TypeOwner(owner.clone()), fact.clone()));
                if let Some(definitions) = self.call_site_return_definition_refreshes.get(owner) {
                    for definition in definitions {
                        fact_updates
                            .push((LuaInferenceNodeId::Definition(*definition), fact.clone()));
                    }
                }
            }
            if db.publish_inference_facts(fact_updates).is_empty() {
                break;
            }
        }
        self.call_site_return_targets.len()
    }

    fn invalidate_inferred_returns_for_sources(
        &self,
        db: &mut DbIndex,
        sources: &[InFiled<glua_parser::LuaSyntaxId>],
    ) {
        for return_ in self.inferred_return_candidates.iter().filter(|return_| {
            sources.iter().any(|source| {
                source.file_id == return_.file_id
                    && (return_.body.as_ref().is_some_and(|body| {
                        body.get_range().contains_range(source.value.get_range())
                    }) || return_
                        .return_points
                        .iter()
                        .any(|point| return_point_contains_range(point, source.value.get_range())))
            })
        }) {
            if let Some(signature) = db.get_signature_index_mut().get_mut(&return_.signature_id)
                && signature.resolve_return == crate::SignatureReturnStatus::InferResolve
            {
                signature.resolve_return = crate::SignatureReturnStatus::UnResolve;
            }
        }
    }

    fn requeue_inferred_returns_for_sources(
        &mut self,
        db: &mut DbIndex,
        sources: &[InFiled<glua_parser::LuaSyntaxId>],
    ) -> usize {
        if sources.is_empty() {
            return 0;
        }

        let returns = self
            .inferred_return_candidates
            .iter()
            .filter(|return_| {
                sources.iter().any(|source| {
                    source.file_id == return_.file_id
                        && (return_.body.as_ref().is_some_and(|body| {
                            body.get_range().contains_range(source.value.get_range())
                        }) || return_.return_points.iter().any(|point| {
                            return_point_contains_range(point, source.value.get_range())
                        }))
                })
            })
            .cloned()
            .collect::<Vec<_>>();

        let mut requeued = 0;
        for return_ in returns {
            if let Some(signature) = db.get_signature_index_mut().get_mut(&return_.signature_id)
                && signature.resolve_return == crate::SignatureReturnStatus::InferResolve
            {
                signature.resolve_return = crate::SignatureReturnStatus::UnResolve;
                self.unresolves
                    .push((return_.into(), InferFailReason::None));
                requeued += 1;
            }
        }
        requeued
    }

    pub fn has_pending_decl_unresolve(&self, decl_id: LuaDeclId) -> bool {
        self.pending_unresolve_decl_ids.contains(&decl_id)
    }

    pub fn request_uninformative_local_decl_reinfer(&mut self, decl_id: LuaDeclId) {
        self.uninformative_local_decl_candidates.insert(decl_id);
    }

    pub fn request_member_initializer_reinfer(&mut self, member_id: LuaMemberId) {
        self.member_initializer_reinfer_candidates.insert(member_id);
    }

    fn add_inferred_guard_dependencies(
        &mut self,
        file_id: FileId,
        owners: HashSet<LuaInferredGuardOwner>,
    ) {
        self.inferred_guard_dependencies
            .entry(file_id)
            .or_default()
            .extend(owners);
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
            self.scripted_scope_infos = Some(Arc::new(HashMap::new()));
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
            scopes.scan_scripted_class_scope_files(file_paths);
        let scripted_scope_infos = scoped_matches
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

        self.scripted_scope_files = Some(Arc::new(scripted_scope_files));
        self.scripted_scope_infos = Some(Arc::new(scripted_scope_infos));
    }
}

fn inferred_return_candidate(
    db: &DbIndex,
    signature_id: LuaSignatureId,
) -> Option<UnResolveReturn> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&signature_id.get_file_id())?
        .get_red_root();
    let closure = root
        .token_at_offset(signature_id.get_position())
        .right_biased()?
        .parent_ancestors()
        .find_map(LuaClosureExpr::cast)
        .filter(|closure| {
            LuaSignatureId::from_closure(signature_id.get_file_id(), closure) == signature_id
        })?;
    let body = closure.get_block()?;
    let return_points = lua::func_body::analyze_func_body_returns(body.clone());
    Some(UnResolveReturn {
        file_id: signature_id.get_file_id(),
        signature_id,
        body: Some(body),
        return_points,
    })
}

fn inferred_return_uses_receiver_metatable(db: &DbIndex, return_: &UnResolveReturn) -> bool {
    return_.return_points.iter().any(|point| {
        let exprs = match point {
            LuaReturnPoint::Expr(expr) => std::slice::from_ref(expr),
            LuaReturnPoint::MuliExpr(exprs) => exprs.as_slice(),
            LuaReturnPoint::Nil | LuaReturnPoint::Error => return false,
        };
        exprs.iter().any(|expr| {
            expr.descendants::<LuaCallExpr>().any(|call| {
                if !call.is_setmetatable() {
                    return false;
                }
                let Some(metatable) = call.get_args_list().and_then(|args| args.get_args().nth(1))
                else {
                    return false;
                };
                metatable.descendants::<LuaNameExpr>().any(|name| {
                    db.get_reference_index()
                        .get_var_reference_decl(&return_.file_id, name.get_range())
                        .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
                        .is_some_and(|decl| {
                            matches!(
                                decl.extra,
                                crate::LuaDeclExtra::Param {
                                    idx: 0,
                                    signature_id,
                                    ..
                                } if signature_id == return_.signature_id
                            )
                        })
                })
            })
        })
    })
}

fn return_point_contains_range(point: &LuaReturnPoint, range: rowan::TextRange) -> bool {
    match point {
        LuaReturnPoint::Expr(expr) => expr.get_range().contains_range(range),
        LuaReturnPoint::MuliExpr(exprs) => exprs
            .iter()
            .any(|expr| expr.get_range().contains_range(range)),
        LuaReturnPoint::Nil | LuaReturnPoint::Error => false,
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
