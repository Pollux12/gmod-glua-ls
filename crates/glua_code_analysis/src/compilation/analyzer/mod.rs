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
mod owner_settle;
pub(crate) mod parallel;
mod setmetatable_factory;
pub(crate) mod unresolve;

pub(crate) use common::FixpointFuse;
pub(crate) use dynamic_field::for_range_pairs_source_for_var;
pub(crate) use dynamic_field::is_provably_builtin_iterator_call;
pub use lua::is_member_assignment_in_conditional_branch;
pub(crate) use lua::{
    dominating_guarded_table_bootstrap_range, infer_for_range_iter_expr_func,
    is_guarded_table_definition_site,
};

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::Arc;

use rayon::prelude::*;

use crate::{
    AsyncState, FileId, GmodScopedClassInfo, InFiled, InferFailReason, LuaDeclId, LuaDefinitionId,
    LuaFunctionType, LuaInferenceNodeId, LuaInferredGuardOwner, LuaMember, LuaMemberFeature,
    LuaMemberId, LuaMemberKey, LuaSignatureId, LuaType, LuaTypeCache, LuaTypeDeclId, LuaTypeFact,
    LuaTypeOwner, LuaUnionType, WorkspaceId,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{DbIndex, LuaMemberOwner, member_id_sort_key},
    profile::Profile,
    semantic::infer_expr_fact_with_cache,
};
use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaChunk, LuaClosureExpr, LuaExpr, LuaIndexExpr, LuaNameExpr,
    LuaSyntaxId, LuaSyntaxNode, LuaTableField,
};
use infer_cache_manager::InferCacheManager;
use lua::LuaReturnPoint;
use unresolve::{UnResolve, UnResolveIterVar, UnResolveReturn};

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
        context.infer_manager.set_call_site_params_visible();

        let call_site_return_invalidation_changed = context.call_site_return_invalidation_changed;
        let late_guard_retries = context.inferred_guard_candidates.len();
        let late_guard_stats = stabilize_inferred_positive_guards(db, &mut context);
        let inferred_guard_changed = late_guard_stats.changed;
        let late_inference_changed =
            call_site_return_invalidation_changed || inferred_guard_changed;

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

        // The receiver of a `panel:Add(child)` written through a member is typed
        // by the passes above, so the parent calls the scripted-class scan had
        // to defer can be collected now.
        {
            let _p = Profile::new("resolve_deferred_vgui_parent_calls");
            gmod::resolve_deferred_vgui_parent_calls(db, &mut context);
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
        let late_child_returns =
            context.requeue_inferred_returns_for_sources(db, &late_child_sources);
        if !late_child_sources.is_empty() {
            context.infer_manager.clear();
            if late_child_returns != 0 {
                run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
                setmetatable_factory::synthesize_setmetatable_factory_members(
                    db,
                    &workspace_file_ids,
                );
            }
            refresh_initializer_caches(db, &mut context);
        }

        let attached_settled_members = {
            let _p = Profile::new("attach_settled_index_expr_members");
            attach_settled_index_expr_members(db, &mut context)
        };
        // Every source of facts has run by now, so this is the only run that
        // may answer a blocked item with a floor.
        context.finalize_unresolves = true;
        if attached_settled_members
            || !context.force_finalized.is_empty()
            || !context.unresolves.is_empty()
        {
            let _p = Profile::new("finalize_unresolves");
            requeue_force_finalized(db, &mut context);
            setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);
            refresh_initializer_caches(db, &mut context);
        }

        // Every settled pass above refines the types the member attach retry
        // reads, so candidates it could not place on the first attempt can be
        // placed now. Without this a member's existence depends on how far
        // inference had progressed when its file happened to be walked.
        {
            let _p = Profile::new("attach_settled_index_expr_members (late)");
            attach_settled_index_expr_members(db, &mut context);
        }
        context.force_finalized.clear();

        {
            // A guess from a local's uses is a fallback for a local inference
            // cannot type, so it is taken only once inference has finished:
            // taken earlier it stands in for an answer a later pass lands. It
            // runs before the settled tail, not after it: the tail's candidates
            // read these locals, and a guess committed only after the fixpoint
            // would leave every dependent frozen at the pre-stabilization read
            // while a warm re-derivation sees the settled one.
            let _p = Profile::new("local inference stabilize");
            local_inference::stabilize_unknown_locals(db, &mut context);
        }

        // A loop over a global has to enumerate the table every realm's file
        // contributed to, so the copies of that global settle first; and only
        // now is every member attached, so a loop that enumerates a table can
        // be answered from the whole map rather than the part of it this batch
        // had reached.
        //
        // Run to a fixpoint. Each pass reads one immutable snapshot of the
        // database, so an answer corrected in this round is invisible to the
        // reads that feed it until the next one -- and the chain runs from a
        // member map to the table a read answers with, to the loop variable
        // that enumerates it, to the key a dynamic write is filed under.
        // Looping rounds is what lets the later hops see the earlier ones.
        //
        // The loop ends when a round moves no type write and no non-type
        // state either (dynamic-field records, inferred signature returns,
        // expression-member keys). Each round re-derives the same candidates
        // against a strictly more complete database than the last, so the
        // answers stop moving once every hop of the chain has seen the hop
        // before it. On CityRP cold that is four rounds, three of them
        // productive. That convergence is measured on the net boundary state
        // below, never on any pass's mid-round report. Two guards watch the
        // loop itself (see `common::FixpointFuse`): exact state-repetition
        // detection is primary — the same complete post-round boundary state
        // recurring confirms an oscillation bug and breaks out — and a high
        // absolute round cap sits behind it as a pure hang guard. Neither is
        // a round cap on healthy behavior: a converging wavefront advances
        // every round and never trips either one.
        {
            let _p = Profile::new("settled_tail_fixpoint");
            // `None` on the first round: every candidate is re-derived once.
            // After that a candidate is only worth re-deriving when a cache it
            // actually read has since moved, which is what each pass records as
            // it runs. See [`crate::db_index::read_set`].
            let mut moved_owners: Option<HashSet<LuaTypeOwner>> = None;
            let late_resolved_decls = std::mem::take(&mut context.late_resolved_decls);
            queue_settled_decl_dependents(db, &mut context, &late_resolved_decls);
            // Termination safety nets; see `common::FixpointFuse` (fuses, not
            // performance budgets): `observe_boundary` confirms oscillation
            // from the exact post-round state, and `trip` bounds total rounds
            // as a pure hang guard.
            let mut fuse = common::FixpointFuse::with_state("settled_tail_fixpoint");
            let mut guarded_bootstrap_candidates: Vec<LuaMemberId> = Vec::new();
            loop {
                let _round = Profile::new("settled_tail_round");
                if fuse.trip() {
                    break;
                }
                // Whole-tail convergence reads the net boundary state only.
                // Each pass below reports its own gross moves, and mid-round
                // reports can ping-pong — one pass moving an owner the next
                // moves back — while the boundary holds still. Snapshotting
                // every movable owner first and diffing after cancels that:
                // continuation, dependent queueing, and the next round's
                // read-set filter all see only owners whose cached type
                // actually differs across the round boundary.
                let boundary_before = crate::profile::phase("tail:snapshot_boundary", || {
                    snapshot_settled_tail_boundary(db, &context)
                });
                let global_owners = crate::profile::phase("tail:global_reads", || {
                    rederive_settled_global_reads(db, &mut context, moved_owners.as_ref())
                });
                // Runs inside the loop: the records it adds are read by the
                // iterated-table answers below, and its own prefixes read the
                // loop variables those answers settle — each direction needs
                // the other's previous round.
                let dynamic_fields_moved = crate::profile::phase("tail:dynamic_fields", || {
                    dynamic_field::rederive_settled_dynamic_fields(
                        db,
                        &mut context,
                        moved_owners.as_ref(),
                    )
                });
                let (_iter_moved, _iter_var_owners) =
                    crate::profile::phase("tail:iter_vars", || {
                        rederive_settled_iter_vars(db, &mut context, moved_owners.as_ref())
                    });
                let returns_moved = crate::profile::phase("tail:inferred_returns", || {
                    rederive_settled_inferred_returns(db, &mut context)
                });
                let parent_closures_moved = crate::profile::phase("tail:parent_closures", || {
                    rederive_settled_parent_closures(db, &mut context)
                });
                if returns_moved || parent_closures_moved {
                    // Side effects only: this resolves the return targets the
                    // boundary snapshot covers, so the net diff below already
                    // sees the owners it moves.
                    crate::profile::phase("tail:call_site_consumers", || {
                        context.resolve_call_site_return_consumers(db)
                    });
                }
                // Assignments onto locals that read a decl the passes above
                // just moved (`selected = key` after the loop variable
                // settled): re-derived through the same source-order claim a
                // direct commit would have applied.
                let _assign_owners = crate::profile::phase("tail:assign_reads", || {
                    rederive_settled_assign_reads(db, &mut context)
                });
                // Everything that read a loop variable resolved while it still
                // held the answer the partial map gave, so moving one means
                // re-reading the initializers that could still take a better
                // answer -- and re-taking the keys the moved types were minted
                // into.
                let refresh_owners = moved_owners.as_ref().map(|previous| {
                    let mut owners = previous.clone();
                    owners.extend(global_owners.iter().cloned());
                    owners
                });
                let _initializer_owners = crate::profile::phase("tail:initializer_caches", || {
                    refresh_settled_initializer_caches(db, &mut context, refresh_owners.as_ref())
                });
                // The queued member attaches read the same settled types the
                // passes above refine: a prefix local typed by a call's
                // inferred return stays unresolved until that return lands,
                // and it can land inside this loop. A failed attach attempt
                // keeps its candidate queued, so re-drain the queue here --
                // this is the dependency edge from an inferred return to the
                // member write waiting on it. Placed candidates drop out;
                // failures stay for the next round.
                let attach_moved = crate::profile::phase("tail:attach_members", || {
                    attach_settled_index_expr_members(db, &mut context)
                });
                // The net boundary diff: owners whose cached type actually
                // differs across this round. Mid-round ping-pong cancels here.
                let net_moved = crate::profile::phase("tail:diff_boundary", || {
                    diff_settled_tail_boundary(db, &boundary_before)
                });
                crate::profile::phase("tail:queue_dependents", || {
                    queue_settled_decl_dependents(db, &mut context, &net_moved)
                });
                // Owner placements read the same settled types as the passes
                // above (prefix types through loop variables, globals, and
                // narrowed members), so they re-derive after them and before
                // the key re-take below: a moved owner re-homes the member
                // whose key is then re-minted.
                let owner_moved = crate::profile::phase("tail:index_expr_owners", || {
                    owner_settle::rederive_settled_index_expr_owners(
                        db,
                        &mut context,
                        moved_owners.as_ref(),
                    )
                });
                let member_keys_moved = crate::profile::phase("tail:member_keys", || {
                    refresh_settled_expr_type_member_keys(db, &mut context, moved_owners.as_ref())
                });
                guarded_bootstrap_candidates.extend(std::mem::take(
                    &mut context.settled_guarded_bootstrap_candidates,
                ));
                guarded_bootstrap_candidates.sort_by_key(|id| member_id_sort_key(*id));
                guarded_bootstrap_candidates.dedup();
                let guarded_bootstraps_moved = crate::profile::phase("tail:bootstraps", || {
                    lua::resettle_guarded_table_bootstraps(db, &guarded_bootstrap_candidates)
                });
                // Dynamic-field records, inferred signature returns, member
                // owners, member keys, and guarded bootstraps are not
                // type-owner state, so the boundary fingerprint below cannot
                // see them move. A recurring fingerprint amid that progress is
                // continued convergence, not a cycle: forget the sightings
                // instead of confirming an oscillation that never happened.
                let non_type_progressed = dynamic_fields_moved
                    || returns_moved
                    || parent_closures_moved
                    || member_keys_moved
                    || owner_moved
                    || attach_moved
                    || guarded_bootstraps_moved;
                if non_type_progressed {
                    fuse.reset_boundary_sightings();
                }
                if !net_moved.is_empty() {
                    // Exact cycle detection over the COMPLETE post-round
                    // movable-owner state, not just this round's net delta: a
                    // delta that recurs while another part of the boundary
                    // advances presents a different complete state every
                    // round and must not break the loop. The fingerprint is
                    // only a lookup accelerator; the fuse confirms exact
                    // canonical-state equality on a repeat sighting.
                    let canonical = crate::profile::phase("tail:canonical_state", || {
                        canonical_settled_tail_state(db, &context)
                    });
                    if fuse.observe_boundary(
                        fingerprint_settled_tail_state(&canonical),
                        false,
                        canonical.as_slice(),
                    ) {
                        break;
                    }
                }
                if net_moved.is_empty() && !non_type_progressed {
                    break;
                }
                moved_owners = Some(net_moved);
            }
            crate::profile::phase_report("settled_tail");
            context.settled_iter_var_candidates.clear();
            context.settled_global_read_candidates.clear();
            context.settled_multi_decl_global_read_candidates.clear();
            context.settled_dynamic_field_candidates.clear();
            context.settled_sibling_merge_read_candidates.clear();
            context.settled_decl_copy_candidates.clear();
            context.settled_assign_candidates.clear();
            context.settled_assign_candidate_keys.clear();
            context.settled_assign_claims.clear();
            context.settled_assign_candidate_counts.clear();
            context.settled_parent_closure_candidates.clear();
            context.settled_member_key_candidates = None;
            context.settled_owner_candidates = None;
        }

        // Net flows are collected last: the collector resolves wrappers through
        // signatures, receiver types and members, none of which exist yet when
        // the gmod pre-pass runs. See `GmodNetworkAnalysisPipeline`.
        run_analysis::<gmod::GmodNetworkAnalysisPipeline>(db, &mut context);

        context.persist_inferred_return_dependencies(db);

        for (consumer_file_id, owners) in context.infer_manager.drain_inferred_guard_dependencies()
        {
            context.add_inferred_guard_dependencies(consumer_file_id, owners);
        }
        for (consumer_file_id, owners) in std::mem::take(&mut context.inferred_guard_dependencies) {
            db.get_signature_index_mut()
                .set_inferred_guard_dependencies(consumer_file_id, owners);
        }
        let mut missed_member_reads = context
            .infer_manager
            .drain_missed_member_reads()
            .into_iter()
            .collect::<HashMap<_, _>>();
        // The per-candidate records carry the same reads the old file-level
        // union accumulated — but only for the three passes whose armed scopes
        // fed it (iter vars and the two initializer rewrites). The other
        // passes' records track staleness only: letting e.g. a global read's
        // `type_owners` into the union would draw the file's lazy readers into
        // the reindex expansion, re-analysing consumers whose answers do not
        // move. Only three fields of the union are consumed below, so it is
        // rebuilt here instead of cloned per candidate per round.
        let mut settled_read_sets =
            HashMap::<FileId, crate::db_index::read_set::InferenceReadSet>::default();
        for ((pass, candidate), (_, reads)) in &context.settled_candidate_reads {
            if !matches!(
                pass,
                SettledReadPass::Iter
                    | SettledReadPass::Decl
                    | SettledReadPass::DeclFull
                    | SettledReadPass::Member
                    | SettledReadPass::MemberFull
            ) {
                continue;
            }
            let file_reads = settled_read_sets.entry(candidate.file_id()).or_default();
            file_reads
                .type_owners
                .extend(reads.type_owners.iter().cloned());
            file_reads
                .signatures
                .extend(reads.signatures.iter().cloned());
            file_reads.missing_member_slots.extend(
                reads
                    .missing_member_slots
                    .iter()
                    .map(|(key, counts)| (key.clone(), *counts)),
            );
        }
        for reader_file_id in &workspace_file_ids {
            let reads = settled_read_sets.remove(reader_file_id).unwrap_or_default();
            missed_member_reads
                .entry(*reader_file_id)
                .or_default()
                .extend(reads.missing_member_slots.into_keys());
            db.get_member_index_mut().set_missed_member_reads(
                *reader_file_id,
                missed_member_reads
                    .remove(reader_file_id)
                    .unwrap_or_default(),
            );
            db.get_type_index_mut()
                .set_settled_reads(*reader_file_id, reads.type_owners);
            db.get_signature_index_mut()
                .set_settled_reads(*reader_file_id, reads.signatures);
            db.get_signature_index_mut()
                .snapshot_payload_refs(*reader_file_id);
        }
        if std::env::var_os("GLUALS_PROFILE").is_some() {
            eprintln!(
                "[profile] member_initializer_candidates={} guarded_member_read_candidates={}",
                context.member_initializer_reinfer_candidates.len(),
                context.guarded_member_read_candidates.len(),
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

    db.flush_inference_derived_state();
}

/// Retries the index-expression member attaches `set_index_expr_owner`
/// dropped. Runs twice before the settled tail and once per settled-tail
/// round: the types its prefixes read keep refining into the fixpoint, so a
/// candidate that fails here can place once the pass that lands its prefix
/// (typically an inferred call return) has run.
/// Whether any candidate was placed.
fn attach_settled_index_expr_members(db: &mut DbIndex, context: &mut AnalyzeContext) -> bool {
    let mut candidates = std::mem::take(&mut context.settled_member_attach_candidates);
    if candidates.is_empty() {
        return false;
    }
    let mut attached = false;
    candidates.sort_by_key(|candidate| (candidate.file_id, candidate.value.get_range().start()));
    candidates.dedup();
    let mut retry = Vec::new();
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);

    // A candidate whose recorded reads did not move fails the same way it did
    // on the attempt that recorded them, so it stays queued without paying
    // another attach attempt. No record means the candidate has never run.
    let mut pending = Vec::new();
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
        if context
            .settled_candidate_reads
            .get(&(
                SettledReadPass::Attach,
                SettledCandidate::Attach(file_id, candidate.value),
            ))
            .is_some_and(|(version, reads)| !version.is_stale_against(&read_version, db, reads))
        {
            retry.push(candidate);
            continue;
        }
        pending.push(candidate);
    }

    // Only the candidate files are re-inferred, so only their caches are stale.
    // Clearing the whole manager would also discard caches the passes that ran
    // before this one built for files this pass never touches.
    let candidate_files = pending
        .iter()
        .map(|candidate| candidate.file_id)
        .collect::<HashSet<_>>();
    context.infer_manager.clear_files(&candidate_files);

    let mut drained_reads = Vec::new();
    for candidate in pending {
        let file_id = candidate.file_id;
        let member_id = LuaMemberId::new(candidate.value, file_id);
        crate::db_index::read_set::arm_with_member_maps();
        let _drain = ReadDrain {
            slot: &mut drained_reads,
            key: SettledCandidate::Attach(file_id, candidate.value),
        };
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
        if unresolve::attempt_member(db, &mut context.infer_manager, &mut unresolve_member).is_err()
        {
            // The prefix still has not settled. Dropping it here is what made a
            // member's existence depend on analysis order: the passes that run
            // after this one go on refining the very types this retry needs, so
            // a candidate that fails now can succeed once they have. Keep it
            // queued for the next attempt instead.
            retry.push(candidate);
        } else {
            attached = true;
        }
    }
    for (candidate, reads) in drained_reads {
        context.record_settled_candidate_reads(
            SettledReadPass::Attach,
            candidate,
            read_version,
            reads,
        );
    }

    context.settled_member_attach_candidates = retry;
    attached
}

/// Re-derives the items the unresolve force wave settled.
///
/// The force wave reads each item against the members indexed by then. A
/// member the settled attach places afterwards is one those reads could not
/// see, so a type read through it settled to what a missing member reads as.
/// A re-index of the same file runs with that member in place, so a cold build
/// has to derive these once more with it in place too, or the two disagree.
fn requeue_force_finalized(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let unresolves = std::mem::take(&mut context.force_finalized);
    if unresolves.is_empty() && context.unresolves.is_empty() {
        return;
    }
    for unresolve in unresolves {
        if let unresolve::UnResolve::Return(return_) = &unresolve
            && let Some(signature) = db.get_signature_index_mut().get_mut(&return_.signature_id)
            && signature.resolve_return == crate::SignatureReturnStatus::InferResolve
        {
            signature.resolve_return = crate::SignatureReturnStatus::UnResolve;
        }
        context.add_unresolve(unresolve, InferFailReason::None);
    }
    // `clear` also drops unmaterialized pending template decls, and the
    // pipeline below materializes those at entry, so the other files' pending
    // decls must go here to match the blanket clear exactly.
    let _ = context.infer_manager.drain_pending_str_tpl_type_decls();
    // Only the files owning requeued items are attempted below, and each
    // attempt reads its own file's cache, so only their caches can be stale.
    // Moves within the run clear their file lazily via `pending_clears`.
    let files = context
        .unresolves
        .iter()
        .filter_map(|(unresolve, _)| unresolve.get_file_id())
        .collect::<HashSet<_>>();
    context.infer_manager.clear_files(&files);
    run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, context);
}

/// Re-resolves body-inferred returns after every type and member input settles.
///
/// A candidate re-derives when its recorded read set went stale, plus one
/// guaranteed attempt while its return is still unsettled: the walk-time
/// record can predate the dynamic-field index becoming visible, so reads it
/// could not make then only enter the record on the next derivation.
fn rederive_settled_inferred_returns(db: &mut DbIndex, context: &mut AnalyzeContext) -> bool {
    let mut changed = false;
    let mut fuse = common::FixpointFuse::new("rederive_settled_inferred_returns");
    loop {
        if fuse.trip() {
            break;
        }
        context.drain_inferred_return_reads();
        // The filter writes nothing, so one capture stands for every
        // record's staleness check this round.
        let now = crate::db_index::read_set::InferenceReadVersion::capture(db);
        let mut candidates = context
            .inferred_return_candidates
            .iter()
            .filter(|return_| {
                db.get_signature_index()
                    .get(&return_.signature_id)
                    .is_some_and(|signature| {
                        signature.resolve_return == crate::SignatureReturnStatus::InferResolve
                            && ((!context
                                .settled_return_attempts
                                .contains(&return_.signature_id)
                                && unresolve::is_unsettled_inferred_return(
                                    &signature.get_return_type(),
                                ))
                                || context.inferred_return_reads_are_stale(
                                    &now,
                                    db,
                                    return_.signature_id,
                                ))
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            break;
        }
        candidates.sort_by_key(|return_| (return_.file_id, return_.signature_id.get_position()));
        candidates.dedup_by(|left, right| left.signature_id == right.signature_id);
        for candidate in &candidates {
            context
                .infer_manager
                .clear_file_deferred_results(candidate.file_id);
        }

        let attempted = candidates
            .iter()
            .map(|candidate| candidate.signature_id)
            .collect::<Vec<_>>();
        let writes_before = db.get_signature_index().return_writes();
        let mut reasons = HashMap::default();
        let mut forced = candidates
            .into_iter()
            .map(UnResolve::from)
            .collect::<Vec<_>>();
        let mut settled_parent_closures = Vec::new();
        unresolve::settle_floored_fixpoint(
            db,
            &mut context.infer_manager,
            &mut reasons,
            &mut forced,
            &mut Vec::new(),
            &mut context.late_resolved_decls,
            &mut settled_parent_closures,
            &mut context.unresolve_read_records,
            false,
        );
        for candidate in settled_parent_closures {
            context.add_settled_parent_closure_candidate(candidate);
        }
        context.drain_inferred_return_reads();
        context.settled_return_attempts.extend(attempted);
        if db.get_signature_index().return_writes() == writes_before {
            break;
        }
        changed = true;
    }
    changed
}

/// Re-resolves parent-closure contracts installed while their source
/// signature's return was still partially unsettled. The copy stamps
/// `DocResolve`, which the ordinary return passes leave alone, so only the
/// explicitly queued candidates are retried here; a candidate stays queued
/// while the return it inherited is still partially unsettled.
fn rederive_settled_parent_closures(db: &mut DbIndex, context: &mut AnalyzeContext) -> bool {
    if context.settled_parent_closure_candidates.is_empty() {
        return false;
    }
    let mut candidates = context.settled_parent_closure_candidates.clone();
    candidates.sort_by_key(|candidate| (candidate.file_id, candidate.signature_id.get_position()));
    candidates.dedup_by(|left, right| left.signature_id == right.signature_id);
    let file_ids = candidates
        .iter()
        .map(|candidate| candidate.file_id)
        .collect::<HashSet<_>>();
    for file_id in file_ids {
        context.infer_manager.clear_file_deferred_results(file_id);
    }
    let writes_before =
        db.get_signature_index().return_writes() + db.get_signature_index().param_writes();
    let mut kept = Vec::new();
    for mut candidate in candidates {
        let previously_seen = candidate.last_source_return.clone();
        let _ = unresolve::attempt_closure_parent_params(
            db,
            &mut context.infer_manager,
            &mut candidate,
        );
        // A candidate leaves the queue only once the source return is settled,
        // unchanged since the last look, and mirrored by the copy. The source
        // can move through slot values that read as settled (a transient
        // `nil` slot), so a matching copy alone is not proof of convergence.
        let source_return = candidate
            .source_signature_id
            .and_then(|id| db.get_signature_index().get(&id))
            .map(|signature| signature.get_return_type());
        let child_return = db
            .get_signature_index()
            .get(&candidate.signature_id)
            .map(|signature| signature.get_return_type());
        let still_pending = match &source_return {
            Some(source_return) => {
                unresolve::is_unsettled_inferred_return(source_return)
                    || previously_seen.as_ref() != Some(source_return)
                    || child_return.as_ref() != Some(source_return)
            }
            None => child_return
                .as_ref()
                .is_some_and(unresolve::is_unsettled_inferred_return),
        };
        if still_pending {
            candidate.last_source_return = source_return;
            kept.push(candidate);
        }
    }
    context.settled_parent_closure_candidates = kept;
    (db.get_signature_index().return_writes() + db.get_signature_index().param_writes())
        != writes_before
}

/// The parallel re-derivation's per-file output: candidate type writes for the
/// settled iterator variables, plus the cache side effects inference produced.
struct IterVarRefreshResult {
    file_id: FileId,
    /// Each re-derived candidate's recorded reads, saved so the next round can
    /// skip candidates whose own reads did not move.
    candidate_reads: Vec<(
        SettledCandidate,
        crate::db_index::read_set::InferenceReadSet,
    )>,
    pending_type_decls: Vec<crate::PendingStrTplTypeDecl>,
    guard_dependencies: HashSet<crate::LuaInferredGuardOwner>,
    updates: Vec<unresolve::IterVarTypeUpdate>,
}

impl IterVarRefreshResult {
    fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            candidate_reads: Vec::new(),
            pending_type_decls: Vec::new(),
            guard_dependencies: HashSet::default(),
            updates: Vec::new(),
        }
    }
}

/// A settled iterator-variable candidate carried across the parallel boundary:
/// rowan-backed AST handles are not `Send`, so the closure re-reads the
/// expressions from the file's syntax tree by syntax id.
struct SettledIterVarCandidate {
    iter_expr_ids: Vec<LuaSyntaxId>,
    var_positions: Vec<rowan::TextSize>,
}

impl SettledIterVarCandidate {
    fn new(iter_var: &UnResolveIterVar) -> Self {
        Self {
            iter_expr_ids: iter_var
                .iter_exprs
                .iter()
                .map(LuaAstNode::get_syntax_id)
                .collect(),
            var_positions: iter_var
                .iter_vars
                .iter()
                .map(glua_parser::LuaAstToken::get_position)
                .collect(),
        }
    }

    fn iter_exprs(&self, root: &LuaSyntaxNode) -> Option<Vec<LuaExpr>> {
        self.iter_expr_ids
            .iter()
            .map(|id| LuaExpr::cast(id.to_node_from_root(root)?))
            .collect()
    }
}

/// Re-derives `for ... in pairs(t)` variable types that were read off `t`'s
/// member map or its declared field type.
///
/// The unresolve wave runs before the settled member passes, so the loop sees
/// only what had been attached by then and can fall back to `any` where the
/// field's own annotation says otherwise. Taking the answer again once every
/// member has landed is both the deterministic and the more faithful one.
fn rederive_settled_iter_vars(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    moved_owners: Option<&HashSet<LuaTypeOwner>>,
) -> (bool, HashSet<LuaTypeOwner>) {
    let read_pass = SettledReadPass::Iter;
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);
    // The first round has no records yet, so every candidate runs; later
    // rounds re-derive only the candidates whose own recorded reads moved.
    let rederive_all = moved_owners.is_none();
    // Kept, not taken: the settled tail runs to a fixpoint and each round
    // re-derives the same candidates against what the previous one landed.
    let mut candidates = context.settled_iter_var_candidates.clone();
    if candidates.is_empty() {
        return (false, HashSet::default());
    }
    candidates.sort_by_key(|iter_var| {
        (
            iter_var.file_id,
            iter_var
                .iter_vars
                .first()
                .map(glua_parser::LuaAstToken::get_position),
        )
    });

    let mut candidates_by_file = HashMap::<FileId, Vec<SettledIterVarCandidate>>::default();
    for iter_var in &candidates {
        let candidate = SettledIterVarCandidate::new(iter_var);
        candidates_by_file
            .entry(iter_var.file_id)
            .or_default()
            .push(candidate);
    }

    let mut file_ids = candidates_by_file.keys().copied().collect::<Vec<_>>();
    file_ids.sort_unstable();
    let candidate_reads = &context.settled_candidate_reads;

    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();
    let call_site_params_visible = context.infer_manager.call_site_params_visible();

    // Inference reads the settled indexes and records candidate type writes
    // without mutating the database. The results come back in `file_ids` order,
    // which is why it is sorted above: the writes are applied on the caller
    // thread in file and source order, never in the order the workers finished
    // or a hash map happened to yield.
    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let file_candidates = &candidates_by_file[&file_id];
        // The cache and syntax tree only serve a file that re-derives a
        // candidate this round — an all-fresh file early-outs on the same
        // per-candidate staleness check, paying only the scan.
        if !rederive_all
            && !file_candidates.iter().any(|candidate| {
                let key = SettledCandidate::IterVar(
                    file_id,
                    candidate.var_positions.first().copied().unwrap_or_default(),
                );
                candidate_reads
                    .get(&(read_pass, key))
                    .is_none_or(|(version, reads)| {
                        version.is_stale_against(&read_version, db, reads)
                    })
            })
        {
            return IterVarRefreshResult::new(file_id);
        }
        let mut infer_cache = crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
                call_site_params_visible,
            },
        );
        let mut result = IterVarRefreshResult::new(file_id);
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            return result;
        };
        for candidate in file_candidates {
            let key = SettledCandidate::IterVar(
                file_id,
                candidate.var_positions.first().copied().unwrap_or_default(),
            );
            // A candidate whose recorded reads did not move answers the same
            // as last round; one with no record has never run.
            let stale = rederive_all
                || candidate_reads
                    .get(&(read_pass, key))
                    .is_none_or(|(version, reads)| {
                        version.is_stale_against(&read_version, db, reads)
                    });
            if !stale {
                continue;
            }
            // Reads are armed per candidate so the next round re-derives this
            // loop alone rather than every candidate in the file. The guard
            // disarms on whichever path the body takes.
            crate::db_index::read_set::arm_with_member_maps();
            let _drain = ReadDrain {
                slot: &mut result.candidate_reads,
                key,
            };
            let Some(iter_exprs) = candidate.iter_exprs(&root) else {
                continue;
            };
            if let Ok(updates) = unresolve::resolve_settled_iter_var_readonly(
                db,
                &mut infer_cache,
                file_id,
                &iter_exprs,
                &candidate.var_positions,
            ) {
                result.updates.extend(updates);
            }
        }
        result.pending_type_decls = infer_cache.take_pending_str_tpl_type_decls();
        result.guard_dependencies = infer_cache.take_inferred_guard_dependencies();
        result
    });

    // Only a write that changes the cached type moved anything; landing the
    // answer already cached would otherwise re-derive its readers every round.
    let mut written = HashSet::default();
    for result in results {
        // A file whose candidates all skipped kept its iter-var mirrors;
        // one that ran any clears them before the fresh answers land.
        if !result.candidate_reads.is_empty() {
            context
                .infer_manager
                .clear_files_iter_var_results(&[result.file_id].into_iter().collect());
        }
        for (candidate, reads) in result.candidate_reads {
            context.record_settled_candidate_reads(read_pass, candidate, read_version, reads);
        }
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        for update in result.updates {
            let before = db
                .get_type_index()
                .get_type_cache(&update.owner)
                .map(|cache| cache.as_type().clone());
            common::write_type_cache(db, update.owner.clone(), update.cache.clone(), update.mode);
            if db
                .get_type_index()
                .get_type_cache(&update.owner)
                .map(|cache| cache.as_type().clone())
                != before
            {
                written.insert(update.owner.clone());
            }
            // The same answer serves the iter-var reads: mirror it into the
            // per-file iter-var cache the way the sequential re-derivation's
            // inference did.
            if let LuaTypeOwner::Decl(decl_id) = update.owner {
                let typ = update.cache.as_type().clone();
                context
                    .infer_manager
                    .get_infer_cache(decl_id.file_id)
                    .for_range_iter_var_type_cache
                    .insert(decl_id, crate::CacheEntry::Cache(typ));
            }
        }
    }
    (!written.is_empty(), written)
}

/// Cheap prefilter for [`refresh_settled_expr_type_member_keys`]: whether `member`
/// may hold a stale dynamic key, decided without reading any syntax tree.
///
/// An `ExprType` key is always minted from a key expression. A `Name`/`Integer`
/// key on an index or table-field node may instead be a const-folded one
/// (`keyed[kb]` filed as `Name("s")` while `kb` narrowed to that literal); the
/// per-member pass resolves the node and keeps only those whose key syntax
/// really is an expression, so static writes (`t.field`, `t["lit"]`, `{ key = v }`)
/// never pay for re-inference.
fn is_settled_member_rekey_candidate(member: &LuaMember) -> bool {
    let key = member.get_key();
    if key.is_expr() {
        return true;
    }
    if !(key.is_name() || key.is_integer()) {
        return false;
    }
    let kind = member.get_id().get_syntax_id().get_kind();
    LuaIndexExpr::can_cast(kind) || LuaTableField::can_cast(kind)
}

/// Re-files dynamically keyed writes under the key their key expression infers
/// to now.
///
/// `t[k] = v` is filed under the type `k` inferred to at the moment the walk
/// reached the write, so a key taken off a member map the walk had only partly
/// built names a slot that selects the wrong writers. The member's own id is
/// its identity, so once the settled passes have moved the types the key was
/// minted from, the member moves to the slot the settled key names instead of a
/// second key being minted and the first left orphaned.
///
/// A key that narrowed to a literal at the walk is filed const-folded — a
/// `Name`/`Integer` key minted from a key *expression* (`keyed[kb]` filed as
/// `Name("s")` while `kb` narrowed to that literal) — so candidacy is the key
/// syntax, not the current key variant: any member whose index key is an
/// expression is re-minted, whatever slot the walk filed it under.
///
/// Returns whether any member key moved. A rekeyed member-map reader may answer
/// differently now, so the settled tail continues the loop — and re-derives
/// every candidate unfiltered next round — when it does.
fn refresh_settled_expr_type_member_keys(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    moved_owners: Option<&HashSet<LuaTypeOwner>>,
) -> bool {
    let read_pass = SettledReadPass::MemberKey;
    let candidate_count = db.get_member_index().get_members_count();
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);
    if context
        .settled_member_key_candidates
        .as_ref()
        .is_none_or(|(count, _)| *count != candidate_count)
    {
        let mut file_ids: Vec<FileId> = context.analyzed_file_ids().into_iter().collect();
        file_ids.sort_unstable();
        let scanned = parallel::map_files_collect(db, &file_ids, |db, file_id| {
            let mut members = db
                .get_member_index()
                .get_file_members(file_id)
                .into_iter()
                .filter(|member| is_settled_member_rekey_candidate(member))
                .map(|member| member.get_id())
                .collect::<Vec<_>>();
            members.sort_unstable_by_key(|id| id.get_syntax_id().get_range().start());
            (file_id, members)
        });
        let candidates = scanned
            .into_iter()
            .filter(|(_, members)| !members.is_empty())
            .collect();
        context.settled_member_key_candidates = Some((candidate_count, candidates));
    }
    let Some((_, candidates)) = context.settled_member_key_candidates.as_ref() else {
        return false;
    };
    // The first round has no records yet, so every candidate runs; later
    // rounds re-derive only the members whose own recorded reads moved.
    let rederive_all = moved_owners.is_none();
    let candidate_reads = &context.settled_candidate_reads;
    let mut file_ids = candidates.keys().copied().collect::<Vec<_>>();
    file_ids.sort_unstable();
    if file_ids.is_empty() {
        return false;
    }

    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();
    let call_site_params_visible = context.infer_manager.call_site_params_visible();

    // Read-only per file, applied on the caller thread in file and source
    // order, like every other settled pass.
    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let file_candidates = &candidates[&file_id];
        // The cache and syntax tree only serve a file that re-derives a
        // member this round — an all-fresh file early-outs on the same
        // per-candidate staleness check, paying only the scan.
        if !rederive_all
            && !file_candidates.iter().any(|member_id| {
                candidate_reads
                    .get(&(read_pass, SettledCandidate::Member(*member_id)))
                    .is_none_or(|(version, reads)| {
                        version.is_stale_against(&read_version, db, reads)
                    })
            })
        {
            return (file_id, Vec::new(), Vec::new());
        }
        let mut infer_cache = crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
                call_site_params_visible,
            },
        );
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            return (file_id, Vec::new(), Vec::new());
        };

        let mut updates = Vec::new();
        // Reads are armed per member so the next round re-derives this member
        // alone rather than every candidate in the file. The guard disarms on
        // whichever path the body takes.
        let mut drained_reads = Vec::new();
        for &member_id in file_candidates {
            let key = SettledCandidate::Member(member_id);
            let stale = rederive_all
                || candidate_reads
                    .get(&(read_pass, key))
                    .is_none_or(|(version, reads)| {
                        version.is_stale_against(&read_version, db, reads)
                    });
            if !stale {
                continue;
            }
            crate::db_index::read_set::arm_with_member_maps();
            let _drain = ReadDrain {
                slot: &mut drained_reads,
                key,
            };
            let Some(member) = db.get_member_index().get_member(&member_id) else {
                continue;
            };
            let current_key = member.get_key();
            let Some(node) = member_id.get_syntax_id().to_node_from_root(&root) else {
                continue;
            };
            let index_key = if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
                index_expr.get_index_key()
            } else if let Some(table_field) = LuaTableField::cast(node) {
                table_field.get_field_key()
            } else {
                None
            };
            let Some(index_key) = index_key else {
                continue;
            };
            // A const-folded `Name`/`Integer` member shares its node kind with
            // static writes (`t.field` is an index expression too), and only an
            // expression key can have folded. Anything else keeps its slot
            // without paying for re-inference.
            if !current_key.is_expr() && !index_key.is_expr() {
                continue;
            }
            let Ok(settled_key) =
                LuaMemberKey::from_index_key_or_unknown(db, &mut infer_cache, &index_key)
            else {
                continue;
            };
            // Only ever a better-resolved key. A key expression that still
            // answers `nil`, `never`, an unbound template ref, or `unknown`
            // *beside a current unknown* has not settled -- it is a read that
            // has not resolved, and filing a write under it would move the
            // write off the wildcard key that dynamic reads do match and onto
            // one nothing selects. But `unknown` -> an informative key is the
            // core convergence: members the walk collapsed under
            // `ExprType(Unknown)` while their key expression read a settling
            // loop variable are re-minted once that variable settles --
            // replacing the collapsed key, never accreting beside it.
            let settled_is_unknown =
                matches!(&settled_key, LuaMemberKey::ExprType(typ) if typ.is_unknown());
            if &settled_key == current_key
                || (!settled_is_unknown
                    && matches!(&settled_key, LuaMemberKey::ExprType(typ)
                        if !crate::db_index::is_informative_type(typ) || typ.contain_tpl()))
                || (settled_is_unknown
                    && !matches!(current_key, LuaMemberKey::ExprType(typ) if typ.is_unknown()))
            {
                continue;
            }
            updates.push((member_id, settled_key));
        }
        (file_id, updates, drained_reads)
    });

    let mut moved = false;
    for (_file_id, updates, candidate_reads) in results {
        for (candidate, reads) in candidate_reads {
            context.record_settled_candidate_reads(read_pass, candidate, read_version, reads);
        }
        for (member_id, settled_key) in updates {
            if db
                .get_member_index_mut()
                .rekey_member(member_id, settled_key)
                .is_some()
            {
                moved = true;
            }
        }
    }
    moved
}

/// Re-derives `local x = SomeGlobal` reads.
///
/// A global's type is the merge of every file that writes it. A batch keeps the
/// values of the files it is not re-indexing while its own are empty until the
/// walk reaches them, so a read taken during the walk can see fewer writers than
/// a cold build does — the cold build defers the read instead, and by the time
/// it resolves every writer has landed. Taking the read again here gives both
/// the complete set.
fn rederive_settled_global_reads(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    moved_owners: Option<&HashSet<LuaTypeOwner>>,
) -> HashSet<LuaTypeOwner> {
    let read_pass = SettledReadPass::Global;
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);
    // The first round has no records yet, so every candidate runs; later
    // rounds re-derive only the candidates whose own recorded reads moved.
    let rederive_all = moved_owners.is_none();
    let candidate_stale = |context: &AnalyzeContext,
                           db: &DbIndex,
                           decl_id: LuaDeclId,
                           expr: &LuaExpr| {
        rederive_all
            || context
                .settled_candidate_reads
                .get(&(
                    read_pass,
                    SettledCandidate::Global(decl_id, expr.get_syntax_id()),
                ))
                .is_none_or(|(version, reads)| version.is_stale_against(&read_version, db, reads))
    };
    let standard = context
        .settled_global_read_candidates
        .iter()
        .filter(|(decl_id, expr)| candidate_stale(context, db, *decl_id, expr))
        .cloned()
        .collect::<Vec<_>>();
    let multi_decl = context
        .settled_multi_decl_global_read_candidates
        .iter()
        .filter(|(decl_id, expr)| candidate_stale(context, db, *decl_id, expr))
        .cloned()
        .collect::<Vec<_>>();
    if standard.is_empty() && multi_decl.is_empty() {
        return HashSet::default();
    }

    let files = standard
        .iter()
        .chain(multi_decl.iter())
        .map(|(decl_id, _)| decl_id.file_id)
        .collect::<HashSet<_>>();
    let before = standard
        .iter()
        .chain(multi_decl.iter())
        .map(|(decl_id, _)| LuaTypeOwner::Decl(*decl_id))
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|owner| {
            let typ = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|cache| cache.as_type().clone());
            (owner, typ)
        })
        .collect::<HashMap<_, _>>();
    let generic_initializer_candidates = context.settled_decl_initializer_candidates.clone();
    context.infer_manager.clear_files_deferred_results(&files);

    // What each candidate's re-derivation actually read, recorded so the next
    // round re-derives only the candidates whose own reads moved.
    let mut candidate_reads = Vec::<(
        SettledCandidate,
        crate::db_index::read_set::InferenceReadSet,
    )>::new();
    // `allow_unsubsumed_swap` is set for reads through a multi-declaration
    // global: those are one runtime table, so the read against the complete set
    // of backing tables replaces the walk's read against a subset even when the
    // two are not structurally related.
    for (mut candidates, allow_unsubsumed_swap) in [(standard, false), (multi_decl, true)] {
        candidates.sort_by_key(|(decl_id, _)| (decl_id.file_id, decl_id.position));
        for (decl_id, expr) in candidates {
            if generic_initializer_candidates.contains(&decl_id) {
                continue;
            }
            // Armed before the doc-cache check so that read stales the
            // candidate too; the guard disarms on whichever path follows.
            crate::db_index::read_set::arm_with_member_maps();
            let _drain = ReadDrain {
                slot: &mut candidate_reads,
                key: SettledCandidate::Global(decl_id, expr.get_syntax_id()),
            };
            let type_owner = LuaTypeOwner::Decl(decl_id);
            let existing = db.get_type_index().get_type_cache(&type_owner);
            if existing.is_some_and(|cached| cached.is_doc()) {
                continue;
            }
            let cached = existing.map(|cached| cached.as_type().clone());
            let may_improve = initializer_may_improve_after_resolve(&expr);
            let is_initializer = db
                .get_decl_index()
                .get_decl(&decl_id)
                .and_then(|decl| decl.get_initializer())
                .is_some_and(|initializer| {
                    initializer.get_expr_syntax_id() == LuaSyntaxId::from_node(expr.syntax())
                });
            let write = common::DeclWrite {
                position: expr.get_position(),
                may_improve_after_resolve: may_improve,
                reads_out_of_decl: lua::expr_reads_out_of_decl(db, decl_id.file_id, decl_id, &expr),
                may_narrow_uninformative: if is_initializer {
                    may_improve
                } else {
                    initializer_reads_through_call_or_index(&expr)
                },
                resolved_initializer: is_initializer && may_improve,
                fills_own_default: lua::expr_fills_own_default(db, decl_id.file_id, decl_id, &expr),
            };
            let cache = context.infer_manager.get_infer_cache(decl_id.file_id);
            crate::db_index::read_set::record(&type_owner);
            let inferred = crate::semantic::infer_expr(db, cache, expr);
            let Ok(settled) = inferred else {
                continue;
            };
            // Re-deriving may only add to what the walk found, never swap it. The
            // complete writer set is what a global read needs when the walk saw
            // none of it, and it is what puts the second writer of a two-file
            // table back after a re-index. But a global every file reassigns --
            // `PLUGIN_SHARED = PLUGIN` in each plugin -- settles to one arbitrary
            // writer, and taking that over the writer the reading file's own
            // include chain reaches would substitute an unrelated answer for a
            // right one. A multi-declaration global is exempt: its backing tables
            // are the same runtime table, so the complete read is authoritative.
            if !allow_unsubsumed_swap
                && let Some(cached) = &cached
                && !crate::is_undetermined_type(cached)
                && !settled_type_subsumes(cached, &settled)
            {
                continue;
            }
            if db
                .get_type_index()
                .get_type_cache(&type_owner)
                .is_some_and(|cached| cached.as_type() == &settled)
            {
                continue;
            }
            if allow_unsubsumed_swap && is_initializer {
                write_type_cache(
                    db,
                    type_owner,
                    LuaTypeCache::InferType(settled),
                    TypeCacheWriteMode::ForceOverwrite,
                );
            } else {
                common::bind_decl_write(db, decl_id, LuaTypeCache::InferType(settled), write);
            }
        }
    }
    // A decl+expr pair queued on both candidate lists drains twice under one
    // key; union so the record covers every read the pass actually made.
    let mut candidate_records =
        HashMap::<SettledCandidate, crate::db_index::read_set::InferenceReadSet>::default();
    for (candidate, reads) in candidate_reads {
        candidate_records
            .entry(candidate)
            .or_default()
            .extend(reads);
    }
    for (candidate, reads) in candidate_records {
        context.record_settled_candidate_reads(read_pass, candidate, read_version, reads);
    }
    let moved = before
        .into_iter()
        .filter_map(|(owner, before)| {
            let after = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|cache| cache.as_type());
            (after != before.as_ref()).then_some(owner)
        })
        .collect::<HashSet<_>>();
    moved
}

/// Every owner the settled tail can move in one round: the assignment,
/// global, iterator-variable, initializer, copy, member, and call-site
/// return-target candidates.
///
/// The tail converges on the net boundary state over this set (see
/// [`snapshot_settled_tail_boundary`]), so it must cover every owner any
/// pass of the round can write.
fn settled_tail_movable_owners(context: &AnalyzeContext) -> HashSet<LuaTypeOwner> {
    let mut owners = HashSet::default();
    owners.extend(
        context
            .settled_assign_candidates
            .iter()
            .map(|(decl_id, _, _)| LuaTypeOwner::Decl(*decl_id)),
    );
    owners.extend(
        context
            .settled_global_read_candidates
            .iter()
            .chain(context.settled_multi_decl_global_read_candidates.iter())
            .map(|(decl_id, _)| LuaTypeOwner::Decl(*decl_id)),
    );
    owners.extend(
        context
            .settled_iter_var_candidates
            .iter()
            .flat_map(|iter_var| {
                iter_var.iter_vars.iter().map(|var_name| {
                    LuaDeclId::new(
                        iter_var.file_id,
                        glua_parser::LuaAstToken::get_position(var_name),
                    )
                })
            })
            .map(LuaTypeOwner::Decl),
    );
    owners.extend(
        context
            .settled_decl_initializer_candidates
            .iter()
            .map(|decl_id| LuaTypeOwner::Decl(*decl_id)),
    );
    owners.extend(context.settled_decl_copy_candidates.iter().cloned());
    owners.extend(
        context
            .member_initializer_reinfer_candidates
            .iter()
            .chain(context.guarded_member_read_candidates.iter())
            .chain(context.settled_sibling_merge_read_candidates.iter())
            .map(|member_id| LuaTypeOwner::Member(*member_id)),
    );
    owners.extend(
        context
            .call_site_return_targets
            .iter()
            .map(|(_, owner, _, _)| owner.clone()),
    );
    owners
}

/// Snapshots the cached type of every owner the settled tail can move.
///
/// Taken at the top of each tail round; [`diff_settled_tail_boundary`] diffs
/// it at the bottom. Mid-round reports cancel in that diff, so the loop
/// converges on what actually changed across the boundary.
fn snapshot_settled_tail_boundary(
    db: &DbIndex,
    context: &AnalyzeContext,
) -> HashMap<LuaTypeOwner, Option<LuaType>> {
    settled_tail_movable_owners(context)
        .into_iter()
        .map(|owner| {
            let cached = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|cache| cache.as_type().clone());
            (owner, cached)
        })
        .collect()
}

/// Owners whose cached type differs from the round's opening snapshot.
///
/// Owners the round never wrote compare equal and drop out, and so does an
/// owner one pass moved and a later pass moved back: only the net boundary
/// state remains.
///
/// This is also the fixpoint's widening point. A self-referential counter
/// (`t[k] = (t[k] or 0) + 1`) re-derives `integer N` as `integer N+1` every
/// round, so the boundary never repeats and never empties — the loop would
/// climb `IntegerConst` forever. When the only change across the boundary is
/// numeric literals moving, the owner is rewritten to the widened primitive
/// (`integer`/`number`), which is the join the counter converges to.
fn diff_settled_tail_boundary(
    db: &mut DbIndex,
    before: &HashMap<LuaTypeOwner, Option<LuaType>>,
) -> HashSet<LuaTypeOwner> {
    before
        .iter()
        .filter(|(owner, cached)| {
            let now = db
                .get_type_index()
                .get_type_cache(owner)
                .map(|cache| cache.as_type().clone());
            if now.as_ref() == cached.as_ref() {
                return false;
            }
            if let (Some(new), Some(old)) = (now.as_ref(), cached.as_ref())
                && let Some(widened) = widen_numeric_literal_delta(new, old)
            {
                db.get_type_index_mut()
                    .widen_infer_type_cache(owner, widened);
            }
            true
        })
        .map(|(owner, _)| owner.clone())
        .collect()
}

/// The widened form of `new` when it differs from `old` only by numeric
/// literals having moved — the fixpoint widening operator for self-referential
/// counters. Returns `None` when the change is anything else, so a real type
/// change is never collapsed.
///
/// Only `IntegerConst`/`DocIntegerConst` (→ `integer`) and `FloatConst`
/// (→ `number`) widen: those are the literals arithmetic produces fresh each
/// round. String/boolean literals don't grow under a fixpoint, so a changed
/// one is a real change. Union arms widen pairwise when the arity matches.
fn widen_numeric_literal_delta(new: &LuaType, old: &LuaType) -> Option<LuaType> {
    match (new, old) {
        (LuaType::IntegerConst(_), LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_))
        | (LuaType::DocIntegerConst(_), LuaType::IntegerConst(_)) => Some(LuaType::Integer),
        (LuaType::FloatConst(_), LuaType::FloatConst(_)) => Some(LuaType::Number),
        (LuaType::Union(new_union), LuaType::Union(old_union)) => {
            let (LuaUnionType::Multi(new_arms), LuaUnionType::Multi(old_arms)) =
                (new_union.as_ref(), old_union.as_ref())
            else {
                return None;
            };
            if new_arms.len() != old_arms.len() {
                return None;
            }
            let mut widened_any = false;
            let arms = new_arms
                .iter()
                .zip(old_arms.iter())
                .map(|(new_arm, old_arm)| {
                    if new_arm == old_arm {
                        Some(new_arm.clone())
                    } else {
                        widen_numeric_literal_delta(new_arm, old_arm).inspect(|_| {
                            widened_any = true;
                        })
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            widened_any.then(|| LuaType::Union(Arc::new(LuaUnionType::from_vec(arms))))
        }
        _ => None,
    }
}

/// Canonical post-round state of every owner the settled tail can move:
/// sorted owner→type pairs.
///
/// [`FixpointFuse::observe_boundary`] takes this alongside the fingerprint.
/// The fingerprint (a u64 over this same state) is only a lookup accelerator:
/// on a repeat sighting the fuse confirms exact equality of this state before
/// calling it a cycle, so the same net delta recurring while another part of
/// the boundary advances — or a plain hash collision — can never break the
/// loop early. The sort keeps both forms deterministic across hash-map and
/// thread-pool orderings; neither form ever decides an answer, only whether
/// the loop has stopped converging.
fn canonical_settled_tail_state(
    db: &DbIndex,
    context: &AnalyzeContext,
) -> Vec<(LuaTypeOwner, Option<LuaType>)> {
    let mut entries = settled_tail_movable_owners(context)
        .into_iter()
        .map(|owner| {
            let cached = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|cache| cache.as_type().clone());
            (owner, cached)
        })
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|(left, _), (right, _)| left.stable_cmp(right));
    entries
}

/// Equality-only fingerprint of [`canonical_settled_tail_state`] for
/// [`FixpointFuse::observe_boundary`]: compared for equality only, never used
/// to order, index, or otherwise decide answers (see the fuse docs).
fn fingerprint_settled_tail_state(canonical: &[(LuaTypeOwner, Option<LuaType>)]) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    hasher.finish()
}

fn assert_send<T: Send>() {}

const _: fn() = assert_send::<crate::LuaInferCache>;

struct SettledAssignRefreshResult {
    file_id: FileId,
    settled: Vec<(usize, Option<LuaType>)>,
    /// Each re-derived candidate's recorded reads, saved so the next round can
    /// skip candidates whose own reads did not move.
    candidate_reads: Vec<(
        SettledCandidate,
        crate::db_index::read_set::InferenceReadSet,
    )>,
    infer_cache: crate::LuaInferCache,
}

impl SettledAssignRefreshResult {
    fn new(file_id: FileId, infer_cache: crate::LuaInferCache) -> Self {
        Self {
            file_id,
            settled: Vec::new(),
            candidate_reads: Vec::new(),
            infer_cache,
        }
    }
}

/// Re-derives local assignments whose right-hand side reads a local the
/// settled tail has since moved (`selected = key` after the loop variable
/// settled).
///
/// The walk bound these writes to whatever the source held when the file was
/// walked, and unlike `local x = <expr>` copies they read no initializer cache
/// of their own the read-set filter could see move.
///
/// Each write goes through [`bind_decl_write`](common::bind_decl_write), so
/// the replay keeps the source-order claim and write-authority semantics a
/// direct commit would have applied: an earlier writer still owns the slot,
/// and only this assignment's own right-hand side is re-read — never the
/// initializer alone, and never every write to a mutable local at once.
///
/// One round commits at most one write per declaration, picked from a stable
/// pre-round snapshot with the claim holder first and the earliest
/// widening/superseding write after it, so the same cache is never replaced
/// twice in a row. A commit lands when the settled answer structurally widens
/// what the round opened with (a union holding every cached arm, a primitive
/// collapsing the cached literals, or a merge covering both at once), when it
/// supersedes it by the cache's own replacement order, or through the
/// sideways/placeholder leg ([`settled_assign_write_committable`]): the claim
/// holder replaces its own contribution with any informative, leak-free
/// settled answer — even a structurally unrelated one, which is what a
/// multi-declaration global merge is; a floored placeholder (`unknown`, `nil`,
/// `never`) or a leaked template parameter takes any informative, leak-free
/// settled answer from any write; and an informative settled answer displaces
/// `any`. The settled answer must always say something leak-free: the walk
/// never trades an answer for `nil`, `unknown`, or `any`, and never seeds a
/// leak, so neither does the replay — in particular the holder cannot move
/// backward (`any -> nil`). Anything else keeps the walk-time answer, exactly
/// as builds without this pass do.
///
/// A trailing variadic right-hand side supplies one slot per extra target
/// (`first, second = echo(key)`): each queued target carries its return index
/// and selects that slot out of the settled answer, falling back to `nil`
/// exactly as the walk does. Extra slots over a non-variadic answer are
/// skipped: the walk answered those without reading the right-hand side's
/// type, so there is nothing to re-derive.
fn rederive_settled_assign_reads(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
) -> HashSet<LuaTypeOwner> {
    let read_pass = SettledReadPass::Assign;
    // Per-decl candidate counts: a candidate arriving or leaving changes the
    // arbitration group, so every candidate of that declaration re-derives.
    let mut decl_candidate_counts = HashMap::<LuaDeclId, usize>::default();
    for (decl_id, _, _) in &context.settled_assign_candidates {
        *decl_candidate_counts.entry(*decl_id).or_default() += 1;
    }
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);
    // The write-claim holder, the arbitration group, and the recorded reads
    // are the staleness inputs; the workers check all three in parallel.
    let mut candidates = context.settled_assign_candidates.clone();
    if candidates.is_empty() {
        return HashSet::default();
    }
    candidates.sort_by_key(|(decl_id, expr, ret_idx)| {
        (
            decl_id.file_id,
            decl_id.position,
            expr.get_range().start(),
            *ret_idx,
        )
    });

    // Stable pre-round snapshot: the claim holder and the cached answer every
    // commit decision below reads. Deciding against the live cache would let
    // an earlier commit of this same round elect the next winner, replacing
    // one cache twice in a row.
    let mut snapshots = HashMap::<LuaDeclId, (Option<LuaType>, Option<rowan::TextSize>)>::default();
    for (decl_id, _, _) in &candidates {
        snapshots.entry(*decl_id).or_insert_with(|| {
            let cached = db
                .get_type_index()
                .get_type_cache(&LuaTypeOwner::Decl(*decl_id))
                .map(|cache| cache.as_type().clone());
            let claim = db
                .get_type_index()
                .decl_write_claim(decl_id)
                .map(|(position, _)| position);
            (cached, claim)
        });
    }

    let files = candidates
        .iter()
        .map(|(decl_id, _, _)| decl_id.file_id)
        .collect::<HashSet<_>>();
    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();
    let call_site_params_visible = context.infer_manager.call_site_params_visible();

    let mut file_ids = files.iter().copied().collect::<Vec<_>>();
    file_ids.sort_unstable();
    let mut worker_caches_by_file = files
        .iter()
        .copied()
        .map(|file_id| {
            let infer_cache = context.infer_manager.take_file(file_id).unwrap_or_else(|| {
                crate::LuaInferCache::new(
                    file_id,
                    crate::CacheOptions {
                        analysis_phase,
                        dynamic_fields_visible,
                        building_dynamic_field_index: false,
                        call_site_params_visible,
                    },
                )
            });
            (file_id, infer_cache)
        })
        .collect::<HashMap<_, _>>();
    let db_ref: &DbIndex = &*db;
    let dispatch = parallel::dispatch_order(db_ref, &file_ids);
    let worker_caches = dispatch
        .into_iter()
        .map(|idx| {
            let file_id = file_ids[idx];
            (
                file_id,
                worker_caches_by_file
                    .remove(&file_id)
                    .expect("assignment cache exists for every file"),
            )
        })
        .collect::<Vec<_>>();
    let mut candidates_by_file =
        HashMap::<FileId, Vec<(usize, LuaDeclId, LuaSyntaxId, usize)>>::default();
    for (slot, (decl_id, expr, ret_idx)) in candidates.iter().enumerate() {
        candidates_by_file
            .entry(decl_id.file_id)
            .or_default()
            .push((slot, *decl_id, expr.get_syntax_id(), *ret_idx));
        context
            .settled_assign_claims
            .insert(*decl_id, snapshots[decl_id].1);
        context
            .settled_assign_candidate_counts
            .insert(*decl_id, decl_candidate_counts[decl_id]);
    }

    // Red syntax handles are not `Send`; workers reconstruct each expression
    // from its syntax id and return the complete file cache with the answer.
    let candidate_reads = &context.settled_candidate_reads;
    let assign_claims = &context.settled_assign_claims;
    let assign_counts = &context.settled_assign_candidate_counts;
    let run = |(file_id, infer_cache): (FileId, crate::LuaInferCache)| {
        glua_parser::with_syntax_resolvers(|| {
            let mut result = SettledAssignRefreshResult::new(file_id, infer_cache);
            let root = db_ref
                .get_vfs()
                .get_syntax_tree(&file_id)
                .map(|tree| tree.get_red_root());
            // Deferred and flow results only need flushing when this file
            // actually re-derives a candidate — the pass's writes are what
            // invalidates them.
            let mut cleared = false;
            for (slot, decl_id, expr_id, ret_idx) in &candidates_by_file[&file_id] {
                let key = SettledCandidate::Assign(*decl_id, *expr_id, *ret_idx);
                let claim = db_ref
                    .get_type_index()
                    .decl_write_claim(decl_id)
                    .map(|(position, _)| position);
                // A candidate with no record has never run; one whose
                // recorded reads, write claim, or arbitration group did not
                // move answers the same as last round.
                let stale = assign_claims.get(decl_id) != Some(&claim)
                    || assign_counts.get(decl_id).copied() != Some(decl_candidate_counts[decl_id])
                    || candidate_reads
                        .get(&(read_pass, key))
                        .is_none_or(|(version, reads)| {
                            version.is_stale_against(&read_version, db_ref, reads)
                        });
                if !stale {
                    continue;
                }
                if !cleared {
                    result.infer_cache.clear_deferred_inference_results();
                    result.infer_cache.clear_flow_results();
                    cleared = true;
                }
                // Reads are armed per candidate so the next round re-derives
                // this assignment alone rather than every candidate in the
                // file. The guard disarms on whichever path the body takes.
                crate::db_index::read_set::arm_with_member_maps();
                let _drain = ReadDrain {
                    slot: &mut result.candidate_reads,
                    key,
                };
                let type_owner = LuaTypeOwner::Decl(*decl_id);
                crate::db_index::read_set::record(&type_owner);
                let Some(expr) = root
                    .as_ref()
                    .and_then(|root| expr_id.to_node_from_root(root))
                    .and_then(LuaExpr::cast)
                else {
                    result.settled.push((*slot, None));
                    continue;
                };
                if db_ref
                    .get_type_index()
                    .get_type_cache(&type_owner)
                    .is_some_and(|cached| cached.is_doc())
                {
                    result.settled.push((*slot, None));
                    continue;
                }
                let Ok(inferred) =
                    crate::semantic::infer_expr(db_ref, &mut result.infer_cache, expr)
                else {
                    result.settled.push((*slot, None));
                    continue;
                };
                let settled = match inferred {
                    LuaType::Variadic(multi) => {
                        Some(multi.get_type(*ret_idx).cloned().unwrap_or(LuaType::Nil))
                    }
                    inferred if *ret_idx == 0 => Some(inferred),
                    _ => None,
                };
                result.settled.push((*slot, settled));
            }
            result
        })
    };
    let mut results = if parallel::runs_inline(worker_caches.len()) {
        worker_caches.into_iter().map(run).collect::<Vec<_>>()
    } else {
        parallel::init_pool();
        worker_caches.into_par_iter().map(run).collect::<Vec<_>>()
    };
    results.sort_unstable_by_key(|result| result.file_id);

    // The result vector is aligned to the sorted file list. Candidate slots
    // restore the pre-arbitration order used by the winner selection below.
    let mut settled = vec![None; candidates.len()];
    for result in results {
        for (slot, value) in result.settled {
            settled[slot] = value;
        }
        for (candidate, reads) in result.candidate_reads {
            context.record_settled_candidate_reads(read_pass, candidate, read_version, reads);
        }
        context
            .infer_manager
            .restore_file(result.file_id, result.infer_cache);
    }

    // At most one winning write per declaration: the claim holder first (the
    // only write allowed a sideways replace of its own contribution), then
    // the earliest committable write in source order.
    let mut winners = Vec::<(LuaDeclId, LuaExpr, LuaType)>::new();
    let mut group_start = 0;
    while group_start < candidates.len() {
        let group_decl = candidates[group_start].0;
        let mut group_end = group_start + 1;
        while group_end < candidates.len() && candidates[group_end].0 == group_decl {
            group_end += 1;
        }
        let (cached, claim) = &snapshots[&group_decl];
        let mut order: Vec<usize> = (group_start..group_end).collect();
        if let Some(claim) = claim
            && let Some(holder) = order
                .iter()
                .position(|idx| candidates[*idx].1.get_position() == *claim)
        {
            let holder = order.remove(holder);
            order.insert(0, holder);
        }
        for idx in order {
            let Some(settled_type) = &settled[idx] else {
                continue;
            };
            if cached.as_ref().is_some_and(|cached| cached == settled_type) {
                continue;
            }
            // An `unknown` arm is the read saying part of it had not landed
            // yet; committing it would cache a provisional answer as final.
            // Only a cache already carrying one may be replaced by the same
            // shape.
            if union_has_unsettled_arm(settled_type)
                && !cached.as_ref().is_some_and(union_has_unsettled_arm)
            {
                continue;
            }
            let (_, rhs, _) = &candidates[idx];
            let holder = claim
                .as_ref()
                .copied()
                .is_some_and(|claim| claim == rhs.get_position());
            // An assignment is never the declaration's own initializer, but
            // spell out the same write the file walk would have committed so
            // a later competing write faces identical arbitration.
            //
            // Settle-only monotonicity: the tail loops until no round moves,
            // so a replay must never trade sideways or backwards. Walk-time
            // arbitration can displace a settled answer and lose it back next
            // round (observed: the same owners alternating every round,
            // hanging cold builds), so only a replay that widens the cached
            // answer, supersedes it by the cache's own replacement order, or
            // passes the sideways/placeholder gate below may commit. See
            // [`settled_assign_write_committable`].
            let committable =
                settled_assign_write_committable(holder, settled_type, cached.as_ref());
            if committable {
                let (_, rhs, _) = &candidates[idx];
                winners.push((group_decl, rhs.clone(), settled_type.clone()));
                break;
            }
        }
        group_start = group_end;
    }

    for (decl_id, rhs, settled_type) in winners {
        let type_owner = LuaTypeOwner::Decl(decl_id);
        let may_improve = initializer_may_improve_after_resolve(&rhs);
        let write = common::DeclWrite {
            position: rhs.get_position(),
            may_improve_after_resolve: may_improve,
            reads_out_of_decl: lua::expr_reads_out_of_decl(db, decl_id.file_id, decl_id, &rhs),
            may_narrow_uninformative: initializer_reads_through_call_or_index(&rhs),
            resolved_initializer: false,
            fills_own_default: lua::expr_fills_own_default(db, decl_id.file_id, decl_id, &rhs),
        };
        let before_commit = db
            .get_type_index()
            .get_type_cache(&type_owner)
            .map(|cache| cache.as_type().clone());
        common::bind_decl_write(
            db,
            decl_id,
            LuaTypeCache::InferType(settled_type.clone()),
            write,
        );
        let refused = db
            .get_type_index()
            .get_type_cache(&type_owner)
            .map(|cache| cache.as_type().clone())
            == before_commit;
        if refused {
            // The ordered write refused the slot — a later holder owns it, or
            // the fallback only merges — but this round's arbitration picked
            // this write, so it replaces the previous contribution outright
            // instead of re-merging it.
            common::write_type_cache(
                db,
                type_owner,
                LuaTypeCache::InferType(settled_type),
                common::TypeCacheWriteMode::ForceOverwrite,
            );
        }
    }
    let moved = snapshots
        .into_iter()
        .filter_map(|(decl_id, (before, _))| {
            let owner = LuaTypeOwner::Decl(decl_id);
            let after = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|cache| cache.as_type());
            (after != before.as_ref()).then_some(owner)
        })
        .collect::<HashSet<_>>();
    moved
}

/// Commit gate for one settled assign-replay write: whether `settled_type`
/// may replace `cached` this round.
///
/// `None` seeds an empty slot, which always takes the read. Otherwise a write
/// commits when the settled answer structurally widens the cached one, when
/// it supersedes it by the cache's own replacement order, or through the
/// sideways/placeholder leg below.
pub(crate) fn settled_assign_write_committable(
    holder: bool,
    settled_type: &LuaType,
    cached: Option<&LuaType>,
) -> bool {
    let Some(cached) = cached else {
        return true;
    };
    union_widens_cached_type(settled_type, cached)
        || settled_widens_cached_literals(settled_type, cached)
        || settled_covers_cached_type(settled_type, cached)
        || LuaTypeCache::InferType(settled_type.clone())
            .supersedes(&LuaTypeCache::InferType(cached.clone()))
        || settled_assign_sideways_committable(holder, settled_type, cached)
}

/// Sideways/placeholder leg of the settled assign-replay commit gate.
///
/// Three explicit clauses, in order:
///
/// 1. Holder sideways: the write holds the declaration's current write claim,
///    so the slot keeps this write's own contribution. Any informative,
///    leak-free settled answer replaces it — even a structurally unrelated
///    one, which is what a multi-declaration global merge is. The informative
///    bar is what rejects backward moves: `any -> nil` cannot commit here
///    because `nil` says nothing about the value.
///
/// 2. Placeholder/leak replacement: a floored placeholder or a leaked template
///    parameter is not an answer at all, so the settled read takes it the way
///    the walk seeds an empty slot. A placeholder is `unknown`, `nil`, or
///    `never` ([`is_undetermined_type`](crate::db_index::is_undetermined_type))
///    — deliberately not bare "uninformative", so `any`, which states the
///    value is unconstrained, is never treated as an unfinished placeholder.
///    Any write may take these, which is what lets a deferred multi-target
///    (`first, second = echo(key)`) land its settled slot over the floored
///    placeholder — neither widening nor `supersedes` relates the two — and
///    what lets a leaked template resolve to concrete.
///
/// 3. Informative over `any`: `any` is authoritative, not unfinished, so only
///    a genuinely informative settled type displaces it. (`supersedes` ranks
///    inside the uninformative band but never lets an informative type beat
///    `any`; this clause is that ranking.)
fn settled_assign_sideways_committable(holder: bool, settled: &LuaType, cached: &LuaType) -> bool {
    // A settled answer that says nothing, or one carrying a leak, never
    // commits through this leg: the walk never trades an answer for `nil`,
    // `unknown`, or `any`, and never seeds a slot with a leak.
    if !crate::db_index::is_informative_type(settled)
        || crate::db_index::leaks_unsubstituted_tpl(settled)
    {
        return false;
    }
    if holder && !settled_covers_cached_type(cached, settled) {
        return true;
    }
    if crate::db_index::is_undetermined_type(cached)
        || crate::db_index::leaks_unsubstituted_tpl(cached)
    {
        return true;
    }
    !crate::db_index::is_informative_type(cached) && !crate::db_index::is_undetermined_type(cached)
}

/// Re-derives decls whose `panel:GetParent()` read was answered against a vgui
/// parent chain state that has since settled differently.
///
/// Both directions are batch artifacts. A read taken before the chains were
/// complete falls back to broad `Panel` where the finished chain names the
/// actual parent. A read taken while a chain was *transiently* complete — the
/// conflicting creation site's relations not yet re-resolved — binds a specific
/// panel the finished chain contradicts, and has to widen back. Only decls
/// whose cache holds a vgui panel type are touched, and only when the settled
/// read still answers a vgui panel type, so a decl typed by other means is
/// left alone.
pub(crate) fn rederive_vgui_parent_fallbacks(db: &mut DbIndex, context: &mut AnalyzeContext) {
    // Kept on the context: the deferred parent calls resolve their relations
    // after this first runs, and the same readers have to be re-derived again.
    let files = context.vgui_parent_fallback_files.clone();
    if files.is_empty() {
        return;
    }
    // The chains are complete now; drop the file's memoised inference so the
    // GetParent read is taken again. The flow answers have to go too: a value
    // read through a loop variable (`p = p:GetParent()`) is answered from the
    // flow cache, which survives the ordinary deferred clear.
    for file_id in &files {
        let cache = context.infer_manager.get_infer_cache(*file_id);
        cache.clear_deferred_inference_results();
        cache.clear_flow_results();
    }

    let mut files = files.into_iter().collect::<Vec<_>>();
    files.sort_by_key(|file_id| file_id.id);
    for file_id in files {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let mut decl_ids = db
            .get_type_index()
            .file_type_owners(file_id)
            .into_iter()
            .flatten()
            .filter_map(|owner| match owner {
                LuaTypeOwner::Decl(decl_id) => Some(*decl_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        decl_ids.sort_by_key(|decl_id| (decl_id.file_id, decl_id.position));
        for decl_id in decl_ids {
            let type_owner = LuaTypeOwner::Decl(decl_id);
            let Some(cached) = db.get_type_index().get_type_cache(&type_owner) else {
                continue;
            };
            if cached.is_doc() {
                continue;
            }
            let cached_type = cached.as_type().clone();
            let cached_exact_panel = is_exact_panel_type(&cached_type);
            if !cached_exact_panel && !is_more_specific_vgui_panel_type(db, &cached_type) {
                continue;
            }
            let Some((ret_idx, expr)) = local_initializer_expr(db, &root, decl_id) else {
                continue;
            };
            let cache = context.infer_manager.get_infer_cache(file_id);
            let Ok(settled) = crate::semantic::infer_expr(db, cache, expr) else {
                continue;
            };
            let settled = match &settled {
                LuaType::Variadic(multi) => {
                    multi.get_type(ret_idx).cloned().unwrap_or(LuaType::Unknown)
                }
                _ => settled,
            };
            let takes_settled = if cached_exact_panel {
                is_more_specific_vgui_panel_type(db, &settled)
            } else {
                settled != cached_type
                    && (is_exact_panel_type(&settled)
                        || is_more_specific_vgui_panel_type(db, &settled))
            };
            if takes_settled {
                db.get_type_index_mut()
                    .force_bind_type(type_owner, LuaTypeCache::InferType(settled));
            }
        }
    }
}

fn is_exact_panel_type(typ: &LuaType) -> bool {
    matches!(typ, LuaType::Ref(id) | LuaType::Def(id) if id.get_name() == "Panel")
}

fn is_more_specific_vgui_panel_type(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            id.get_name() != "Panel" && crate::semantic::type_decl_is_vgui_panel(db, id, 0)
        }
        _ => false,
    }
}

/// Whether `settled` is `cached` with more of the writer set folded in, rather
/// than a different answer.
///
/// A merge or a union the cached type is a component of says the walk saw part
/// of what has since landed; anything else says the two reads resolved to
/// different things, and the settled one carries no more authority for that
/// than the walk's.
fn settled_type_subsumes(cached: &LuaType, settled: &LuaType) -> bool {
    if cached == settled {
        return true;
    }
    match settled {
        LuaType::MergedTable(merged) => merged
            .get_types()
            .iter()
            .any(|component| settled_type_subsumes(cached, component)),
        LuaType::Union(union) => union
            .types()
            .any(|component| settled_type_subsumes(cached, component)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(component, _)| settled_type_subsumes(cached, component)),
        _ => false,
    }
}

/// Re-derives member assignment widenings that ran against an incomplete
/// set of sibling writers.
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

/// Which pass is re-reading the initializer caches.
#[derive(Clone, Copy)]
enum InitializerPass<'a> {
    /// The full pass, which reads every candidate once.
    Full,
    /// The settled tail, which only re-reads a candidate whose evidence moved.
    /// `moved` is `None` on the first round, before any read set exists.
    Settled {
        moved: Option<&'a HashSet<LuaTypeOwner>>,
    },
}

impl<'a> InitializerPass<'a> {
    /// Whether the index has settled, so an answer taken now is final rather
    /// than one the rest of the batch may still improve.
    fn is_settled(self) -> bool {
        matches!(self, InitializerPass::Settled { .. })
    }
}

fn refresh_initializer_caches(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let decl_copies = HashSet::default();
    let mut written =
        refresh_decl_initializer_caches(db, context, InitializerPass::Full, &decl_copies);
    written.extend(refresh_member_initializer_caches(
        db,
        context,
        InitializerPass::Full,
        &decl_copies,
    ));
    queue_settled_decl_dependents(db, context, &written);
}

/// [`refresh_initializer_caches`] for a late pass that moved a handful of types.
///
/// Only initializers whose cache could still take a better answer are re-read.
/// The blind-dynamic-field probe the full pass runs is not repeated: its verdict
/// is about whether the *first* answer was taken before the dynamic-field index
/// existed, which a later pass cannot change, and asking it again means
/// inferring every candidate in the workspace a second time.
fn refresh_settled_initializer_caches(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    moved_owners: Option<&HashSet<LuaTypeOwner>>,
) -> HashSet<LuaTypeOwner> {
    // Members first: a declaration that reads one of them takes its answer from
    // whatever the member holds when the read is taken.
    let pass = InitializerPass::Settled {
        moved: moved_owners,
    };
    let decl_copies = std::mem::take(&mut context.settled_decl_copy_candidates);
    let mut written = refresh_member_initializer_caches(db, context, pass, &decl_copies);
    written.extend(refresh_decl_initializer_caches(
        db,
        context,
        pass,
        &decl_copies,
    ));
    written
}

/// Queues the writes whose value reads a local the settled tail just moved.
///
/// `local time = ...` and the loop variable `d` are re-read against the
/// settled index above, but `self.ReloadDelay = time` and
/// `d.data = istable(d.data) and d.data or {}` were bound during the walk to
/// what the local held then, and read no cache of their own the read-set
/// filter could see move. A re-index binds them from the settled local
/// directly, so they are re-derived here for the next round to take the same
/// answer.
fn queue_settled_decl_dependents(
    db: &DbIndex,
    context: &mut AnalyzeContext,
    written: &HashSet<LuaTypeOwner>,
) {
    let mut decl_ids = written
        .iter()
        .filter_map(|owner| match owner {
            LuaTypeOwner::Decl(decl_id) => Some(*decl_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    decl_ids.sort_by_key(|decl_id| (decl_id.file_id, decl_id.position));
    for decl_id in decl_ids {
        let file_id = decl_id.file_id;
        let Some(references) = db
            .get_reference_index()
            .get_decl_references(&file_id, &decl_id)
        else {
            continue;
        };
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        for cell in references.cells.iter().filter(|cell| !cell.is_write) {
            let Some(name_expr) = root
                .covering_element(cell.range)
                .ancestors()
                .find(|node| node.text_range() == cell.range)
                .and_then(LuaNameExpr::cast)
            else {
                continue;
            };
            // The nearest statement the read sits in: a value expression of
            // an assignment or a local declaration.
            let Some(stat) = name_expr.syntax().ancestors().skip(1).find(|node| {
                glua_parser::LuaAssignStat::can_cast(node.kind().into())
                    || glua_parser::LuaLocalStat::can_cast(node.kind().into())
            }) else {
                continue;
            };
            let contains_read =
                |expr: &LuaExpr| expr.get_range().contains_range(name_expr.get_range());
            if let Some(assign_stat) = glua_parser::LuaAssignStat::cast(stat.clone()) {
                let (vars, exprs) = assign_stat.get_var_and_expr_list();
                let Some(idx) = exprs.iter().position(contains_read) else {
                    continue;
                };
                let rhs = exprs[idx].clone();
                // A trailing call supplies one slot per extra target
                // (`first, second = echo(key)`): every target the read's
                // right-hand side supplies queues with its return index, so
                // the replay selects that slot out of the settled variadic
                // answer instead of replaying only the paired target.
                let trailing_extras = (idx + 1 == exprs.len() && vars.len() > exprs.len())
                    .then(|| idx + 1..vars.len())
                    .into_iter()
                    .flatten();
                for var_pos in std::iter::once(idx).chain(trailing_extras) {
                    let ret_idx = var_pos - idx;
                    match vars.get(var_pos) {
                        Some(glua_parser::LuaVarExpr::IndexExpr(index_expr)) => {
                            let member_id = LuaMemberId::new(
                                LuaSyntaxId::from_node(index_expr.syntax()),
                                file_id,
                            );
                            if db.get_member_index().get_member(&member_id).is_none() {
                                continue;
                            }
                            context
                                .member_initializer_reinfer_candidates
                                .insert(member_id);
                            context
                                .settled_decl_copy_candidates
                                .insert(LuaTypeOwner::Member(member_id));
                        }
                        Some(glua_parser::LuaVarExpr::NameExpr(target)) => {
                            // An assignment onto a local reads no initializer cache
                            // of its own, so the read-set filter cannot see it move.
                            // Queue the write itself: the settled tail re-derives it
                            // through `bind_decl_write`, which keeps the source-order
                            // claim and write-authority semantics a direct commit
                            // would have applied.
                            //
                            // Globals have no local reference; fall back to the same
                            // identity `get_var_owner` binds the walk-time write to
                            // (a decl-index entry at the use-site position, else
                            // the use-site position itself) so the replay writes
                            // the owner the walk claimed.
                            let maybe_decl_id = LuaDeclId::new(file_id, target.get_position());
                            let target_decl_id =
                                if db.get_decl_index().get_decl(&maybe_decl_id).is_some() {
                                    maybe_decl_id
                                } else {
                                    db.get_reference_index()
                                        .get_local_reference(&file_id)
                                        .and_then(|refs| refs.get_decl_id(&target.get_range()))
                                        .unwrap_or(maybe_decl_id)
                                };
                            if target_decl_id == decl_id {
                                continue;
                            }
                            // A read out of the assigned decl never seeds it; the
                            // walk refuses such writes outright, so there is nothing
                            // to replay here either.
                            if lua::expr_reads_out_of_decl(db, file_id, target_decl_id, &rhs) {
                                continue;
                            }
                            context.request_settled_assign_candidate(
                                target_decl_id,
                                rhs.clone(),
                                ret_idx,
                            );
                        }
                        _ => {}
                    }
                }
            } else if let Some(local_stat) = glua_parser::LuaLocalStat::cast(stat) {
                let Some(value) = local_stat.get_value_exprs().find(contains_read) else {
                    continue;
                };
                let Some(local_name) = local_stat.get_local_name_by_value(value) else {
                    continue;
                };
                let copy_decl_id = LuaDeclId::new(file_id, local_name.get_position());
                // A local written again later holds the merge of every write;
                // re-reading the initializer alone would drop the others.
                if db
                    .get_reference_index()
                    .get_decl_references(&file_id, &copy_decl_id)
                    .is_some_and(|references| references.cells.iter().any(|cell| cell.is_write))
                {
                    continue;
                }
                context
                    .settled_decl_initializer_candidates
                    .insert(copy_decl_id);
                context
                    .settled_decl_copy_candidates
                    .insert(LuaTypeOwner::Decl(copy_decl_id));
            }
        }
    }
}

fn refresh_decl_initializer_caches(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    pass: InitializerPass<'_>,
    decl_copies: &HashSet<LuaTypeOwner>,
) -> HashSet<LuaTypeOwner> {
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);
    if context.settled_decl_initializer_candidates.is_empty() {
        return HashSet::default();
    }

    let mut candidates_by_file = HashMap::<FileId, Vec<LuaDeclId>>::default();
    for decl_id in &context.settled_decl_initializer_candidates {
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
    if file_ids.is_empty() {
        return HashSet::default();
    }
    // The settled tail's first round re-derives every candidate once: its
    // Full-pass records answer a different question (the settled filters take
    // different branches), so they cannot stand in for a settled run. A Full
    // pass instead checks its own records every call, skipping candidates
    // whose reads have not moved since the call that recorded them.
    let rederive_all = matches!(pass, InitializerPass::Settled { moved: None });
    let read_pass = if pass.is_settled() {
        SettledReadPass::Decl
    } else {
        SettledReadPass::DeclFull
    };
    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();
    let call_site_params_visible = context.infer_manager.call_site_params_visible();
    let pass_is_settled = pass.is_settled();
    let candidate_reads = &context.settled_candidate_reads;

    // A file whose candidates are all fresh needs no task: the scan below
    // would early-out on the same check, so an all-fresh file here costs a
    // lookup, not a dispatch. The first settled round records nothing yet,
    // so it always runs every candidate and skips the scan.
    if !rederive_all {
        file_ids.retain(|file_id| {
            candidates_by_file[file_id].iter().any(|decl_id| {
                decl_copies.contains(&LuaTypeOwner::Decl(*decl_id))
                    || candidate_reads
                        .get(&(read_pass, SettledCandidate::Decl(*decl_id)))
                        .is_none_or(|(version, reads)| {
                            version.is_stale_against(&read_version, db, reads)
                        })
            })
        });
        if file_ids.is_empty() {
            return HashSet::default();
        }
    }

    // Initializer inference reads the stabilized indexes and records candidate
    // cache writes without mutating the database. Process that read-only work
    // per file, then merge inference side effects and type writes in stable file
    // and source order on the caller thread.
    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        // A candidate whose recorded reads did not move answers what it
        // answered last round, so only candidates that have never run or whose
        // own reads moved are re-derived. The check runs before the tree is
        // rebuilt so an all-fresh file costs only the candidate scan.
        let stale_decls: Vec<LuaDeclId> = candidates_by_file[&file_id]
            .iter()
            .copied()
            .filter(|decl_id| {
                rederive_all
                    || decl_copies.contains(&LuaTypeOwner::Decl(*decl_id))
                    || candidate_reads
                        .get(&(read_pass, SettledCandidate::Decl(*decl_id)))
                        .is_none_or(|(version, reads)| {
                            version.is_stale_against(&read_version, db, reads)
                        })
            })
            .collect();
        if stale_decls.is_empty() {
            return InitializerRefreshResult::new(file_id);
        }
        let mut infer_cache = crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
                call_site_params_visible,
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
        for decl_id in &stale_decls {
            // Reads are armed per candidate so next round can re-derive this
            // decl alone rather than every decl in the file. The guard disarms
            // on whichever path the body takes, including its early `continue`s.
            crate::db_index::read_set::arm_with_member_maps();
            let _drain = ReadDrain {
                slot: &mut result.candidate_reads,
                key: SettledCandidate::Decl(*decl_id),
            };
            let type_owner = (*decl_id).into();
            let current_cache = db.get_type_index().get_type_cache(&type_owner).cloned();
            if current_cache.as_ref().is_some_and(LuaTypeCache::is_doc) {
                continue;
            }
            // A slot with no cache belongs to a deferred item that has not
            // resolved yet. Mid-batch a seed here would be read against whatever
            // the index held at this pass and then hold the item's real answer
            // out, so the full pass leaves it alone. Once the index has settled
            // there is no later answer to hold out, and skipping instead strands
            // the slot: `stabilize_unknown_locals` takes it for genuinely
            // unresolved and derives it from a use, so an initializer that
            // settled facts can answer loses to usage context.
            if current_cache.is_none() && !pass_is_settled {
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
            // A copy of a loop variable holds whatever that variable held when
            // the copy landed, and the settle has just moved it. What it holds
            // now is no evidence against re-reading it.
            let copies_settled_iter_var =
                pass.is_settled() && common::reads_settling_iter_var(db, file_id, &expr);
            let copies_settled_decl = pass.is_settled() && decl_copies.contains(&type_owner);
            if !copies_settled_iter_var
                && !copies_settled_decl
                && !initializer_reads_through_call_or_index(&expr)
            {
                continue;
            }
            // A slot whose writers had not all landed answers with a flat
            // union instead of the merge, and that answer was cached as if it
            // were final. See [`cached_provisional_slot_union`].
            let cached_a_provisional_slot_union = !current_is_uninformative
                && current_cache
                    .as_ref()
                    .is_some_and(|current| cached_provisional_slot_union(current.as_type()));
            // An `unknown` arm is the read saying it could not settle that part
            // of the union yet. See [`union_has_unsettled_arm`].
            let cached_an_unsettled_union = !current_is_uninformative
                && current_cache
                    .as_ref()
                    .is_some_and(|current| union_has_unsettled_arm(current.as_type()));
            if !copies_settled_iter_var
                && !copies_settled_decl
                && pass.is_settled()
                && !current_is_uninformative
                && !can_refine_nominal_type
                && !can_upgrade_authority
                && !cached_a_provisional_slot_union
                && !cached_an_unsettled_union
                && !matches!(
                    current_cache.as_ref().map(LuaTypeCache::as_type),
                    Some(LuaType::Tuple(_))
                )
            {
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
            let inferred_full_fact = infer_expr_fact_with_cache(db, &mut infer_cache, expr.clone());
            let resolved_variadic_tail =
                ret_idx > 0 && selects_resolved_result(inferred_full_fact.typ(), ret_idx);
            let inferred_fact = select_result_fact(inferred_full_fact, ret_idx);
            let inferred_fact = if copies_settled_iter_var {
                inferred_fact.with_runtime_type(widen_direct_literal_copy(inferred_fact.typ()))
            } else {
                inferred_fact
            };
            let inferred_type = inferred_fact.typ().clone();
            // An `unknown` arm says this read has not settled yet; it is no
            // evidence against an answer that has. A re-index walks the same
            // read once everything has landed and never caches the arm.
            if union_has_unsettled_arm(&inferred_type)
                && !current_is_uninformative
                && !cached_an_unsettled_union
                && !current_cache.as_ref().is_some_and(|current| {
                    union_widens_cached_type(&inferred_type, current.as_type())
                        || settled_widens_cached_literals(&inferred_type, current.as_type())
                })
            {
                continue;
            }

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
                            call_site_params_visible,
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
            let is_numeric_tuple_refinement = current_cache.as_ref().is_some_and(|current| {
                is_settled_numeric_tuple_refinement(&inferred_type, current.as_type())
            });
            let is_settled_widening = current_cache.as_ref().is_some_and(|current| {
                union_widens_cached_type(&inferred_type, current.as_type())
                    || settled_widens_cached_literals(&inferred_type, current.as_type())
            });
            // The mirror of `is_settled_widening`: the cache holds the union a
            // partial member set produced (`string` for a map whose keys had
            // not folded yet, `number` for a slot one literal had reached), and
            // the settled read narrows it to the arms the full set yields.
            // `settled_covers_cached_type` asks the settled answer to cover the
            // cache; here the cache must cover the settled answer — every
            // informative arm it holds is one the cache's arms already covered
            // or widened to. `nil`/`unknown` arms are absence markers the read
            // adds, not refinements, so they are excluded from the check.
            let is_settled_refinement = current_cache.as_ref().is_some_and(|current| {
                let settled_arms: Vec<LuaType> = match &inferred_type {
                    LuaType::Union(union) => known_arms(union)
                        .into_iter()
                        .filter(|arm| !matches!(arm, LuaType::Nil))
                        .collect(),
                    other => vec![other.clone()],
                };
                !settled_arms.is_empty()
                    && settled_arms
                        .iter()
                        .all(|settled| settled_covers_cached_type(current.as_type(), settled))
            });
            let cached_a_tpl_leak = current_cache
                .as_ref()
                .is_some_and(|current| crate::db_index::leaks_unsubstituted_tpl(current.as_type()));
            if !copies_settled_iter_var
                && !copies_settled_decl
                && !current_is_uninformative
                && !can_refine_nominal_type
                && !can_upgrade_authority
                && !cached_a_blind_dynamic_field_read
                && !cached_an_unsettled_union
                && !is_numeric_tuple_refinement
                && !cached_a_provisional_slot_union
                && !cached_a_tpl_leak
                && !is_settled_refinement
                && !is_settled_widening
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
                if copies_settled_decl
                    || (pass.is_settled()
                        && ret_idx == 0
                        && inferred_type.is_nil()
                        && current_cache
                            .as_ref()
                            .is_some_and(|current| current.as_type().is_unknown()))
                {
                    result.updates.push(InitializerCacheUpdate::Overwrite {
                        owner: type_owner,
                        fact: inferred_fact,
                    });
                } else if is_bottom(&inferred_type)
                    && current_cache
                        .as_ref()
                        .is_some_and(|current| is_bottom(current.as_type()))
                    && current_cache.as_ref().map(LuaTypeCache::as_type) != Some(&inferred_type)
                {
                    result.updates.push(InitializerCacheUpdate::Overwrite {
                        owner: type_owner,
                        fact: inferred_fact.with_runtime_type(LuaType::Unknown),
                    });
                } else if is_bottom(&inferred_type)
                    && resolved_variadic_tail
                    && current_cache
                        .as_ref()
                        .is_none_or(|current| current.as_type().is_unknown())
                {
                    // The walk cached `unknown` because the read had not
                    // resolved yet; the settled read resolves it to a bottom.
                    // A re-index walks the same read against the settled state
                    // and caches that bottom directly, so leaving `unknown`
                    // here would make the cache depend on which path derived
                    // it. `Bind` refuses an uninformative replacement, so this
                    // goes through the force path.
                    result.updates.push(InitializerCacheUpdate::Overwrite {
                        owner: type_owner,
                        fact: inferred_fact,
                    });
                } else if pass.is_settled()
                    && !is_bottom(&inferred_type)
                    && current_cache.as_ref().is_some_and(|current| {
                        inferred_type.is_any()
                            || (current_is_uninformative && !is_bottom(current.as_type()))
                    })
                {
                    // The settled read found a writer the walk had not seen
                    // yet, and that writer reads as `any`. A re-index walks
                    // the same read against the settled slot and caches `any`
                    // directly, so the walk's early answer has to go the same
                    // way here or the two builds disagree. `unknown` is the
                    // walk's placeholder for the same unfinished read.
                    result.updates.push(InitializerCacheUpdate::Overwrite {
                        owner: type_owner,
                        fact: inferred_fact,
                    });
                } else if pass.is_settled()
                    && current_cache.as_ref().is_some_and(|current| {
                        LuaTypeCache::InferType(inferred_type.clone()).supersedes(current)
                    })
                {
                    // Both answers carry no type information, but one of them
                    // admits more values — `any|nil` over `any`, the difference
                    // between reporting a nil check and not. Which one is cached
                    // otherwise comes down to how far the batch had run when the
                    // read was taken, so the settled one is taken here on the
                    // same rule the type index itself applies.
                    //
                    // Settled passes only. The rule is a one-way ratchet: it
                    // lets a more permissive answer replace the cache and never
                    // the reverse, so a full pass placing `any` outranks every
                    // settled round that answers `nil`, and the transient one
                    // stands. Which pass read the more complete index is a
                    // property of the batch — a warm re-index kept `any` for
                    // `report.data[1]` where a cold build kept `nil`.
                    result.updates.push(InitializerCacheUpdate::Bind {
                        owner: type_owner,
                        fact: inferred_fact,
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

            if current_is_uninformative && !cached_a_tpl_leak {
                result.updates.push(InitializerCacheUpdate::Bind {
                    owner: type_owner,
                    fact: inferred_fact,
                });
            } else if cached_a_tpl_leak
                || copies_settled_iter_var
                || copies_settled_decl
                || has_stronger_declared_authority
                || is_nominal_refinement
                || is_settled_widening
                || cached_a_blind_dynamic_field_read
                || cached_a_provisional_slot_union
                || cached_an_unsettled_union
                || is_numeric_tuple_refinement
                || is_settled_refinement
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
        for (candidate, reads) in result.candidate_reads {
            context.record_settled_candidate_reads(read_pass, candidate, read_version, reads);
        }
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        updates.extend(result.updates);
    }
    apply_initializer_cache_updates(db, updates)
}

/// Whether the cached answer is the flat union a member slot falls back to when
/// one of its writers had no type yet.
///
/// `resolve_member_type`'s `All` arm merges a slot's writers into one table only
/// when every one of them already reads as a table assignment; a sibling the
/// walk has not reached flips it to a plain union of the table literals
/// instead. What decided that is how far the batch had got, not the source, so
/// the read is provisional and worth taking again once every writer has landed.
///
/// A settled merge answers `MergedTable`, never this shape, so re-reading is
/// idempotent: the repaired cache no longer matches.
///
/// An arm carrying no type of its own is not one of the slot's writers: the
/// read adds `nil` for a branch it found no member on and `any` for a sibling
/// it could not resolve. Letting one veto the shape left the repair unable to
/// see the very unions it exists for — a double dynamic index over a table
/// several files write reads as `nil|any|<2052 literals>`.
fn cached_provisional_slot_union(typ: &LuaType) -> bool {
    let LuaType::Union(union) = typ else {
        return false;
    };
    let mut count = 0;
    for component in union.types() {
        if !crate::db_index::is_informative_type(component) {
            continue;
        }
        if !matches!(component, LuaType::TableConst(_)) {
            return false;
        }
        count += 1;
    }
    count > 1
}

/// Whether the cached answer is a union one of whose arms never settled.
///
/// An `unknown` arm is not a value the slot can hold; it is the read reporting
/// that the contributor behind that arm had not resolved when the answer was
/// taken. Whether it had is a property of how far the batch got, so the read is
/// provisional and worth taking again once everything has landed.
pub(crate) fn union_has_unsettled_arm(typ: &LuaType) -> bool {
    matches!(typ, LuaType::Union(union) if union.types().any(|arm| matches!(arm, LuaType::Unknown)))
}

/// Whether the re-derived type is a union that already contains everything the
/// cached one holds, plus more.
///
/// The cache then holds a subset snapshot taken before the other arms were
/// visible, so replacing it widens to the settled answer instead of guessing a
/// different one. A cached union is as much a subset snapshot as a cached single
/// arm is: both are decided by which contributors happened to be indexed first.
pub(crate) fn union_widens_cached_type(inferred: &LuaType, current: &LuaType) -> bool {
    let LuaType::Union(inferred_union) = inferred else {
        return false;
    };
    let inferred_arms = known_arms(inferred_union);
    // A cached arm is covered when the settled union holds it verbatim or holds
    // the primitive it widens to — `number` covers a cached `integer` the same
    // way it covers a cached `5`.
    let covered = |cached: &LuaType| {
        inferred_arms
            .iter()
            .any(|arm| arm == cached || crate::db_index::widens_primitive(arm, cached))
    };
    match current {
        LuaType::Union(current_union) => {
            let current_arms = known_arms(current_union);
            (current_arms.len() < inferred_arms.len()
                || (current_arms.len() == inferred_arms.len()
                    && current_union.types().count() < inferred_union.types().count()))
                && current_arms.iter().all(covered)
        }
        current => covered(current),
    }
}

/// Whether the settled answer is the primitive every arm of the cached union
/// widens to.
///
/// The cache then lists the literals this batch had reached when the read was
/// taken, and the settled read answers with the primitive the whole writer set
/// shares — `""|"Public Area"|"Public Property"` against `string` for a map
/// table several files write. Which literals were in it is decided by how far
/// the batch had run, so the collapsed answer replaces them.
/// [`union_widens_cached_type`] covers the same snapshot where the settled
/// answer is itself a union; this is that answer already collapsed.
///
/// Every arm has to be a literal the settled answer is the primitive *of*.
/// An arm carrying no type of its own is not one: `supersedes` would take a
/// `nil` arm as replaceable and the collapse would drop it, turning
/// `string|nil` into `string` and silencing every nil check on the slot.
fn settled_widens_cached_literals(inferred: &LuaType, current: &LuaType) -> bool {
    let LuaType::Union(union) = current else {
        return false;
    };
    let mut arms = union.types().peekable();
    arms.peek().is_some()
        && arms.all(|arm| {
            crate::db_index::is_informative_type(arm)
                && LuaTypeCache::InferType(inferred.clone())
                    .supersedes(&LuaTypeCache::InferType(arm.clone()))
        })
}

/// Whether the settled answer is the cached one with more of the writer set
/// folded in: every arm the cache holds is either an arm of the settled answer
/// or a literal that one of its arms widens to.
///
/// [`union_widens_cached_type`] and [`settled_widens_cached_literals`] each
/// cover one shape of this; a sibling merge produces both at once, `"a"|"b"`
/// against `string|number` when a computed-key writer joins widened literal
/// entries. A `nil` arm is covered only by a `nil` arm, so the collapse never
/// drops one.
fn settled_covers_cached_type(inferred: &LuaType, current: &LuaType) -> bool {
    fn arms(typ: &LuaType) -> Vec<LuaType> {
        match typ {
            LuaType::Union(union) => known_arms(union),
            other => vec![other.clone()],
        }
    }
    let inferred_arms = arms(inferred);
    let current_arms = arms(current);
    !current_arms.is_empty()
        && current_arms.iter().all(|cached| {
            inferred_arms.iter().any(|settled| {
                settled == cached || crate::db_index::widens_primitive(settled, cached)
            })
        })
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

fn widen_direct_literal_copy(typ: &LuaType) -> LuaType {
    if matches!(
        typ,
        LuaType::IntegerConst(_)
            | LuaType::FloatConst(_)
            | LuaType::StringConst(_)
            | LuaType::BooleanConst(_)
    ) {
        crate::widen_literal_type_for_assignment(typ)
    } else {
        typ.clone()
    }
}

fn refresh_member_initializer_caches(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    pass: InitializerPass<'_>,
    decl_copies: &HashSet<LuaTypeOwner>,
) -> HashSet<LuaTypeOwner> {
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);
    // A sibling merge is only worth re-reading once the writers it merged have
    // settled, which the full pass runs ahead of.
    let no_sibling_merges = HashSet::default();
    let sibling_merges = if pass.is_settled() {
        &context.settled_sibling_merge_read_candidates
    } else {
        &no_sibling_merges
    };
    if context.member_initializer_reinfer_candidates.is_empty()
        && context.guarded_member_read_candidates.is_empty()
        && sibling_merges.is_empty()
    {
        return HashSet::default();
    }

    let guarded = &context.guarded_member_read_candidates;
    let mut candidates_by_file = HashMap::<FileId, Vec<LuaMemberId>>::default();
    for member_id in context
        .member_initializer_reinfer_candidates
        .iter()
        .chain(guarded)
        .chain(sibling_merges)
    {
        candidates_by_file
            .entry(member_id.file_id)
            .or_default()
            .push(*member_id);
    }
    for candidates in candidates_by_file.values_mut() {
        candidates.sort_by_key(LuaMemberId::get_position);
        // The two candidate sets overlap: a guarded read can also be a nominal
        // one, and re-deriving it twice would only repeat the same answer.
        candidates.dedup();
    }
    let mut file_ids = candidates_by_file.keys().copied().collect::<Vec<_>>();
    file_ids.sort();
    if file_ids.is_empty() {
        return HashSet::default();
    }
    // See `refresh_decl_initializer_caches` for the pass-key split: the
    // settled tail's first round re-derives every candidate once, while a
    // Full pass re-derives only what its own recorded reads saw move.
    let rederive_all = matches!(pass, InitializerPass::Settled { moved: None });
    let read_pass = if pass.is_settled() {
        SettledReadPass::Member
    } else {
        SettledReadPass::MemberFull
    };
    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();
    let call_site_params_visible = context.infer_manager.call_site_params_visible();
    let candidate_reads = &context.settled_candidate_reads;

    // A file whose candidates are all fresh needs no task: the scan below
    // would early-out on the same check, so an all-fresh file here costs a
    // lookup, not a dispatch.
    if !rederive_all {
        file_ids.retain(|file_id| {
            candidates_by_file[file_id].iter().any(|member_id| {
                decl_copies.contains(&LuaTypeOwner::Member(*member_id))
                    || guarded.contains(member_id)
                    || sibling_merges.contains(member_id)
                    || candidate_reads
                        .get(&(read_pass, SettledCandidate::Member(*member_id)))
                        .is_none_or(|(version, reads)| {
                            version.is_stale_against(&read_version, db, reads)
                        })
            })
        });
        if file_ids.is_empty() {
            return HashSet::default();
        }
    }

    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        // A candidate whose recorded reads did not move answers what it
        // answered last round, so only candidates that have never run or whose
        // own reads moved are re-derived. The check runs before the tree is
        // rebuilt so an all-fresh file costs only the candidate scan.
        let stale_members: Vec<LuaMemberId> = candidates_by_file[&file_id]
            .iter()
            .copied()
            .filter(|member_id| {
                // Candidate-set membership is filter input, not an index read:
                // a member that joined the guarded or sibling-merge sets since
                // it last ran (or that copies a settled decl) takes different
                // filter branches, so it re-derives even with fresh reads.
                rederive_all
                    || decl_copies.contains(&LuaTypeOwner::Member(*member_id))
                    || guarded.contains(member_id)
                    || sibling_merges.contains(member_id)
                    || candidate_reads
                        .get(&(read_pass, SettledCandidate::Member(*member_id)))
                        .is_none_or(|(version, reads)| {
                            version.is_stale_against(&read_version, db, reads)
                        })
            })
            .collect();
        if stale_members.is_empty() {
            return InitializerRefreshResult::new(file_id);
        }
        let mut infer_cache = crate::LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
                call_site_params_visible,
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
        for member_id in &stale_members {
            // Reads are armed per candidate so next round can re-derive this
            // member alone rather than every member in the file. The guard
            // disarms on whichever path the body takes, including `continue`.
            crate::db_index::read_set::arm_with_member_maps();
            let _drain = ReadDrain {
                slot: &mut result.candidate_reads,
                key: SettledCandidate::Member(*member_id),
            };
            let type_owner = LuaTypeOwner::Member(*member_id);
            let Some(current_cache) = db.get_type_index().get_type_cache(&type_owner).cloned()
            else {
                continue;
            };
            let current_is_uninformative = type_is_uninformative(current_cache.as_type());
            if current_cache.is_doc() {
                continue;
            }
            let Some((expr, ret_idx)) = member_initializer_expr(&root, *member_id) else {
                continue;
            };
            // A member that copies a loop variable holds whatever that variable
            // held when the copy landed, and the settle has just moved it. What
            // it holds now is no evidence against re-reading it.
            let copies_settled_iter_var =
                pass.is_settled() && common::reads_settling_iter_var(db, file_id, &expr);
            let copies_settled_decl = pass.is_settled() && decl_copies.contains(&type_owner);
            let widens_mutable_local_literal = pass.is_settled()
                && common::mutable_local_name_read_decl(db, file_id, &expr).is_some()
                && matches!(
                    current_cache.as_type(),
                    LuaType::IntegerConst(_)
                        | LuaType::StringConst(_)
                        | LuaType::BooleanConst(_)
                        | LuaType::FloatConst(_)
                );
            let reads_field_exist_guard = guarded.contains(member_id);
            let reads_sibling_merge = sibling_merges.contains(member_id);
            // An `unknown` arm is the walk saying one writer of the slot it
            // read had not landed yet. See [`union_has_unsettled_arm`].
            let cached_an_unsettled_union =
                pass.is_settled() && union_has_unsettled_arm(current_cache.as_type());
            let widens_top_level_or_literal = pass.is_settled()
                && matches!(
                    current_cache.as_type(),
                    LuaType::IntegerConst(_)
                        | LuaType::StringConst(_)
                        | LuaType::BooleanConst(_)
                        | LuaType::FloatConst(_)
                )
                && lua::is_top_level_or_expr(&expr);
            if !widens_top_level_or_literal
                && !copies_settled_iter_var
                && !copies_settled_decl
                && !widens_mutable_local_literal
                && !reads_field_exist_guard
                && !reads_sibling_merge
                && !current_is_uninformative
                && !cached_an_unsettled_union
                && single_nominal_type_id(current_cache.as_type()).is_none()
            {
                continue;
            }
            let canonical_guarded_bootstrap =
                lua::canonical_guarded_table_bootstrap_type(db, *member_id, None);
            let inferred_type = if let Some(canonical) = &canonical_guarded_bootstrap {
                canonical.clone()
            } else {
                let Ok(inferred_type) = crate::infer_expr(db, &mut infer_cache, expr.clone())
                else {
                    continue;
                };
                // A tail var holds one return of the last expression, not the
                // whole multi-return value. Select it the way the deferred
                // write's own resolution does.
                let inferred_type = match &inferred_type {
                    LuaType::Variadic(multi) => {
                        multi.get_type(ret_idx).cloned().unwrap_or(LuaType::Unknown)
                    }
                    _ => inferred_type,
                };
                if copies_settled_iter_var {
                    widen_direct_literal_copy(&inferred_type)
                } else {
                    common::widen_mutable_local_name_copy(db, file_id, &expr, inferred_type)
                }
            };
            let inferred_type = lua::member_assignment_or_source_type_with_cache(
                db,
                &mut infer_cache,
                file_id,
                &type_owner,
                &expr,
                inferred_type,
            );
            if pass.is_settled() {
                let takes_inferred_type = !widens_top_level_or_literal
                    || settled_covers_cached_type(&inferred_type, current_cache.as_type());
                if takes_inferred_type && current_cache.as_type() != &inferred_type {
                    result.updates.push(InitializerCacheUpdate::Overwrite {
                        owner: type_owner,
                        fact: LuaTypeFact::certain(inferred_type),
                    });
                }
                continue;
            }
            let takes_inferred_type = if canonical_guarded_bootstrap.is_some()
                || copies_settled_iter_var
                || copies_settled_decl
                || widens_mutable_local_literal
                || cached_an_unsettled_union
            {
                true
            } else if reads_field_exist_guard
                && (union_widens_cached_type(&inferred_type, current_cache.as_type())
                    || (pass.is_settled() && inferred_type.is_any()))
            {
                // The guard's narrowing has since seen every type that owns the
                // key, so the settled answer is the cached one with the owners
                // the walk had not reached folded in. A settled `any` is that
                // answer too: the member set now holds a writer whose own type
                // never resolved, and `any` is what the read honestly reports.
                true
            } else if reads_sibling_merge
                && settled_covers_cached_type(&inferred_type, current_cache.as_type())
            {
                // The merge has since seen every writer, each holding its
                // settled type, so the settled answer is the cached snapshot
                // with the rest of the writer set folded in.
                true
            } else if current_is_uninformative {
                // A placeholder is not an answer: it only records that the
                // member's initializer had not been inferred yet when the write
                // landed. Re-inferring it against the settled index is the same
                // question, asked once the facts exist. A settled `any` is
                // that answer too: a re-index walks the same read against the
                // settled index and caches `any` directly.
                !type_is_uninformative(&inferred_type)
                    || (pass.is_settled()
                        && inferred_type.is_any()
                        && current_cache.as_type().is_unknown())
                    || (pass.is_settled()
                        && LuaTypeCache::InferType(inferred_type.clone())
                            .supersedes(&current_cache))
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
        for (candidate, reads) in result.candidate_reads {
            context.record_settled_candidate_reads(read_pass, candidate, read_version, reads);
        }
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        updates.extend(result.updates);
    }
    apply_initializer_cache_updates(db, updates)
}

struct InitializerRefreshResult {
    file_id: FileId,
    /// What each processed candidate actually read, in candidate order. The
    /// merge stores one record per candidate so the next round can skip the
    /// ones whose reads did not move instead of re-deriving the file.
    candidate_reads: Vec<(
        SettledCandidate,
        crate::db_index::read_set::InferenceReadSet,
    )>,
    pending_type_decls: Vec<crate::PendingStrTplTypeDecl>,
    guard_dependencies: HashSet<LuaInferredGuardOwner>,
    updates: Vec<InitializerCacheUpdate>,
}

impl InitializerRefreshResult {
    fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            candidate_reads: Vec::new(),
            pending_type_decls: Vec::new(),
            guard_dependencies: HashSet::default(),
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

impl InitializerCacheUpdate {
    fn owner(&self) -> &LuaTypeOwner {
        match self {
            InitializerCacheUpdate::Bind { owner, .. }
            | InitializerCacheUpdate::Overwrite { owner, .. }
            | InitializerCacheUpdate::ReplaceFact { owner, .. } => owner,
        }
    }
}

/// Applies the updates and returns the owners whose cache actually changed.
///
/// A re-derivation that lands the answer already cached moved nothing, and
/// counting it would re-derive its readers every round without end.
fn apply_initializer_cache_updates(
    db: &mut DbIndex,
    updates: Vec<InitializerCacheUpdate>,
) -> HashSet<LuaTypeOwner> {
    let owners = updates
        .iter()
        .map(InitializerCacheUpdate::owner)
        .cloned()
        .collect::<Vec<_>>();
    let cached_type = |db: &DbIndex, owner: &LuaTypeOwner| {
        db.get_type_index()
            .get_type_cache(owner)
            .map(|cache| cache.as_type().clone())
    };
    let before = owners
        .iter()
        .map(|owner| cached_type(db, owner))
        .collect::<Vec<_>>();
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
    owners
        .into_iter()
        .zip(before)
        .filter(|(owner, before)| cached_type(db, owner) != *before)
        .map(|(owner, _)| owner)
        .collect()
}

/// Whether selecting result `result_idx` out of a call answering `typ` reads a
/// value the call actually produced.
///
/// [`select_result_fact`] answers `nil` for any index past a non-variadic type.
/// Out of a resolved multi-return that `nil` is real — the call returns fewer
/// values than the statement binds. Out of an answer that never resolved to a
/// multi-return it is fabricated, and records only that the call did not
/// resolve, which is no evidence about the value.
fn selects_resolved_result(typ: &LuaType, result_idx: usize) -> bool {
    result_idx == 0 || matches!(typ, LuaType::Variadic(_))
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

fn member_initializer_expr(
    root: &LuaSyntaxNode,
    member_id: LuaMemberId,
) -> Option<(LuaExpr, usize)> {
    let node = member_id.get_syntax_id().to_node_from_root(root)?;
    if let Some(field) = LuaTableField::cast(node.clone()) {
        return field.get_value_expr().map(|expr| (expr, 0));
    }
    let index_expr = glua_parser::LuaIndexExpr::cast(node)?;
    let assign_stat = index_expr.get_parent::<glua_parser::LuaAssignStat>()?;
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let value_idx = vars
        .iter()
        .position(|var| var.syntax().text_range() == index_expr.get_range())?;
    // A var past the last right-hand side takes the next return of the last
    // expression (`a.b, t.c = f()` puts `t.c` on `f()`'s second return), so the
    // re-infer reads that expression and selects the element the var holds.
    match exprs.get(value_idx) {
        Some(expr) => Some((expr.clone(), 0)),
        None => exprs
            .last()
            .map(|expr| (expr.clone(), value_idx + 1 - exprs.len())),
    }
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

fn is_settled_numeric_tuple_refinement(inferred: &LuaType, current: &LuaType) -> bool {
    let (LuaType::Tuple(inferred_tuple), LuaType::Tuple(current_tuple)) = (inferred, current)
    else {
        return false;
    };
    if inferred_tuple.len() != current_tuple.len() {
        return false;
    }
    let mut refined = false;
    for (inf, cur) in inferred_tuple
        .get_types()
        .iter()
        .zip(current_tuple.get_types().iter())
    {
        if inf == cur {
            continue;
        }
        if inf.is_integer() && cur.is_number() {
            refined = true;
            continue;
        }
        return false;
    }
    refined
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
    !crate::db_index::is_informative_type(typ) || crate::db_index::leaks_unsubstituted_tpl(typ)
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

    let mut file_tree_map: HashMap<WorkspaceId, Vec<InFiled<LuaChunk>>> = HashMap::default();
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
    inferred_return_reads:
        HashMap<LuaSignatureId, crate::db_index::read_set::InferredReturnReadRecord>,
    pending_call_site_return_consumers: Vec<UnResolve>,
    pending_call_site_definition_refreshes: Vec<(LuaDefinitionId, LuaTypeOwner)>,
    /// Consumers already resolved once, kept so a later pass that settles a
    /// function return can have them re-resolved against it.
    call_site_return_targets: Vec<(FileId, LuaTypeOwner, LuaExpr, usize)>,
    call_site_return_definition_refreshes: HashMap<LuaTypeOwner, Vec<LuaDefinitionId>>,
    pending_unresolve_decl_ids: HashSet<LuaDeclId>,
    settled_decl_initializer_candidates: HashSet<LuaDeclId>,
    member_initializer_reinfer_candidates: HashSet<LuaMemberId>,
    /// Member writes whose value read through a field-exist guard. See
    /// [`AnalyzeContext::request_guarded_member_read_reinfer`].
    guarded_member_read_candidates: HashSet<LuaMemberId>,
    /// Members whose value was answered by merging a table's sibling members
    /// under a computed key. That merge reads whichever writers the batch had
    /// indexed, with the types they held at the time; the settled passes widen
    /// those writers and attach the cross-file ones, so the value is re-read
    /// once they have. Cleared with the other settled candidate sets.
    settled_sibling_merge_read_candidates: HashSet<LuaMemberId>,
    /// Writes whose value is a local the settled tail has since moved, by
    /// name. The walk bound them to what the local held then, so they take
    /// the settled re-read outright. See [`queue_settled_decl_copies`].
    settled_decl_copy_candidates: HashSet<LuaTypeOwner>,
    /// Local assignments whose right-hand side reads a local the settled tail
    /// has since moved (`selected = key`). Unlike member writes and immutable
    /// locals, these read no cache of their own, so the settled tail replays
    /// the write itself through `bind_decl_write`. Queued by
    /// [`queue_settled_decl_dependents`], re-derived by
    /// [`rederive_settled_assign_reads`]. Each entry carries the return index
    /// the target takes out of a trailing variadic right-hand side
    /// (`first, second = echo(key)` queues `(first, rhs, 0)` and
    /// `(second, rhs, 1)`); a lone right-hand side always queues index `0`.
    settled_assign_candidates: Vec<(LuaDeclId, LuaExpr, usize)>,
    /// Dedup keys for [`Self::settled_assign_candidates`]: the walk queues
    /// every call/index right-hand side onto a local, so membership cannot
    /// stay a linear scan.
    settled_assign_candidate_keys: HashSet<(LuaDeclId, rowan::TextRange, usize)>,
    settled_member_key_candidates: Option<(usize, HashMap<FileId, Vec<LuaMemberId>>)>,
    settled_owner_candidates: Option<(usize, HashMap<FileId, Vec<LuaMemberId>>)>,
    settled_return_attempts: HashSet<LuaSignatureId>,
    settled_assign_claims: HashMap<LuaDeclId, Option<rowan::TextSize>>,
    /// How many assignment candidates each declaration carried when the
    /// settled pass last ran it: a candidate arriving or leaving changes the
    /// arbitration group, so every candidate of that declaration re-derives.
    settled_assign_candidate_counts: HashMap<LuaDeclId, usize>,
    /// Per-candidate recorded reads for settled passes fine-grained enough to
    /// skip one candidate without re-deriving the rest of its file. Keyed by
    /// the pass and the candidate's own identity; a candidate absent here has
    /// never run and always reads as stale.
    settled_candidate_reads: HashMap<
        (SettledReadPass, SettledCandidate),
        (
            crate::db_index::read_set::InferenceReadVersion,
            crate::db_index::read_set::InferenceReadSet,
        ),
    >,
    /// What each surviving unresolve item's last `try_resolve` attempt read,
    /// kept across the wave pipelines and the settled tail's fixpoint calls so
    /// a retained item whose inputs never moved is not re-derived against
    /// identical state. Cleared when `set_force` flips attempt semantics:
    /// a pre-force record does not describe what a force attempt reads.
    pub(crate) unresolve_read_records:
        HashMap<unresolve::UnResolveIdentity, unresolve::FixpointReadRecord>,
    /// Locals the unresolve waves settled after the walk bound their readers.
    /// See [`queue_settled_decl_dependents`].
    pub(crate) late_resolved_decls: HashSet<LuaTypeOwner>,
    infer_manager: InferCacheManager,
    inferred_guard_dependencies: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_guard_candidates: Vec<InFiled<LuaSyntaxId>>,
    early_callable_signatures: Vec<(crate::LuaTypeOwner, LuaSignatureId)>,
    early_member_owner_candidates: Vec<(LuaMemberId, LuaDeclId)>,
    /// Index-expression member attaches that `set_index_expr_owner` dropped
    /// because the prefix carried no owner information yet. See
    /// [`attach_settled_index_expr_members`].
    settled_member_attach_candidates: Vec<InFiled<LuaSyntaxId>>,
    /// Items the unresolve force wave settled. Members the settled attach
    /// adds afterwards can change them, so they are re-derived once it has.
    /// See [`requeue_force_finalized`].
    pub(crate) force_finalized: Vec<unresolve::UnResolve>,
    /// Whether the unresolve pipeline may force and floor. Off, a run only
    /// settles what the index can answer and hands the rest back; on, the
    /// run is the terminal one and iterates its floors to a fixpoint.
    pub(crate) finalize_unresolves: bool,
    /// Guarded table bootstraps whose canonical writer was picked from an
    /// incomplete sibling set. See `resettle_guarded_table_bootstraps`.
    settled_guarded_bootstrap_candidates: Vec<LuaMemberId>,
    /// See [`AnalyzeContext::record_settled_iter_var_candidate`].
    settled_iter_var_candidates: Vec<UnResolveIterVar>,
    /// See [`AnalyzeContext::record_settled_global_read_candidate`].
    settled_global_read_candidates: Vec<(LuaDeclId, LuaExpr)>,
    /// Decls reading through a multi-declaration global. Unlike
    /// [`Self::settled_global_read_candidates`], the settled re-derivation is
    /// allowed to replace the walk's answer even when it does not structurally
    /// subsume it: a global declared once per realm is a single runtime table,
    /// so the read against the complete set of backing tables is authoritative
    /// over the read the walk took against whichever ones it had reached.
    settled_multi_decl_global_read_candidates: Vec<(LuaDeclId, LuaExpr)>,
    /// Dynamic-write sites whose prefix held no nameable table when the
    /// dynamic-field pass ran. See
    /// [`dynamic_field::rederive_settled_dynamic_fields`].
    pub(crate) settled_dynamic_field_candidates: Vec<(FileId, glua_parser::LuaSyntaxId)>,
    /// Files where a `panel:GetParent()` read resolved to the broad `Panel`
    /// fallback because the vgui parent chain was not complete yet. The chains
    /// are finished in the gmod-post pass, after which those reads (and anything
    /// derived from them) are re-derived; see `rederive_vgui_parent_fallbacks`.
    vgui_parent_fallback_files: HashSet<FileId>,
    /// Parent calls whose receiver had no type when the scripted-class scan ran,
    /// per file. See [`gmod::resolve_deferred_vgui_parent_calls`].
    deferred_vgui_parent_call_sites: HashMap<FileId, HashSet<LuaSyntaxId>>,
    call_site_return_invalidation_changed: bool,
    /// Parent-closure items whose inherited contract was installed while the
    /// source signature's return was still partially unsettled. Retried by
    /// [`rederive_settled_parent_closures`].
    settled_parent_closure_candidates: Vec<unresolve::UnResolveParentClosureParams>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SettledReadPass {
    Iter,
    MemberKey,
    Owner,
    Global,
    Decl,
    Member,
    Dynamic,
    Assign,
    /// The non-settled initializer passes (`InitializerPass::Full`). Their
    /// filter semantics differ from the settled rounds, so their read records
    /// live under separate keys: a decl that ran under Full has not run under
    /// Settled and vice versa.
    DeclFull,
    MemberFull,
    /// The settled member-attach retry. A candidate whose prefix reads have
    /// not moved fails the same way it did last round, so it stays queued
    /// without spending another attempt.
    Attach,
}

/// One re-derivation candidate's identity inside a settled pass, for passes
/// that record what each candidate — rather than each file — actually read.
/// A candidate whose recorded reads did not move answers the same as last
/// round, so the pass can skip it instead of re-deriving a whole file when
/// any one of its candidates' reads moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SettledCandidate {
    Decl(LuaDeclId),
    Member(LuaMemberId),
    /// One `for ... in` loop's iterator-variable candidate, keyed by its file
    /// and first loop variable position.
    IterVar(FileId, rowan::TextSize),
    /// One settled assignment re-derivation, keyed by the written decl, the
    /// value expression's site, and which return index it reads.
    Assign(LuaDeclId, glua_parser::LuaSyntaxId, usize),
    /// One settled global read re-derivation, keyed by the written decl and
    /// the read expression's site.
    Global(LuaDeclId, glua_parser::LuaSyntaxId),
    /// One settled dynamic-field site, keyed by file and index-expression
    /// site.
    Dynamic(FileId, glua_parser::LuaSyntaxId),
    /// One settled member-attach retry, keyed by file and the write's
    /// index-expression site.
    Attach(FileId, glua_parser::LuaSyntaxId),
}

impl SettledCandidate {
    fn file_id(&self) -> FileId {
        match *self {
            SettledCandidate::Decl(decl_id)
            | SettledCandidate::Assign(decl_id, ..)
            | SettledCandidate::Global(decl_id, _) => decl_id.file_id,
            SettledCandidate::Member(member_id) => member_id.file_id,
            SettledCandidate::IterVar(file_id, _)
            | SettledCandidate::Dynamic(file_id, _)
            | SettledCandidate::Attach(file_id, _) => file_id,
        }
    }
}

/// Collects the armed read set into `slot` when the enclosing iteration
/// scope ends, on every path including `continue`. Lets a worker arm reads
/// per candidate without restructuring the loop body's early exits.
struct ReadDrain<'a, K: Clone> {
    slot: &'a mut Vec<(K, crate::db_index::read_set::InferenceReadSet)>,
    key: K,
}

impl<K: Clone> Drop for ReadDrain<'_, K> {
    fn drop(&mut self) {
        self.slot
            .push((self.key.clone(), crate::db_index::read_set::disarm()));
    }
}

/// One file's context deltas from the parallel lua walk, as `Send`
/// descriptors. Every field mirrors a collection the walk can write;
/// drained on the worker by [`AnalyzeContext::take_walk_deltas`] and
/// replayed in file order by [`AnalyzeContext::apply_walk_deltas`], so the
/// merge reproduces the serial concatenation exactly.
#[derive(Default)]
pub(crate) struct WalkCtxDelta {
    unresolves: Vec<(unresolve::spec::ItemDesc, InferFailReason)>,
    inferred_returns: Vec<unresolve::spec::ItemDesc>,
    assign_candidates: Vec<(LuaDeclId, LuaSyntaxId, usize)>,
    member_attach_candidates: Vec<InFiled<LuaSyntaxId>>,
    decl_init_candidates: Vec<LuaDeclId>,
    member_init_candidates: Vec<LuaMemberId>,
    guarded_member_reads: Vec<LuaMemberId>,
    sibling_merge_reads: Vec<LuaMemberId>,
    vgui_fallback_files: Vec<FileId>,
    iter_var_candidates: Vec<unresolve::spec::ItemDesc>,
    global_read_candidates: Vec<(LuaDeclId, LuaSyntaxId)>,
    multi_decl_global_read_candidates: Vec<(LuaDeclId, LuaSyntaxId)>,
    guarded_bootstrap_candidates: Vec<LuaMemberId>,
}

impl AnalyzeContext {
    pub fn new() -> Self {
        Self {
            tree_list: Vec::new(),
            metas: HashSet::default(),
            scripted_scope_files: None,
            scripted_scope_infos: None,
            gmod_global_call_roles: None,
            unresolves: Vec::new(),
            inferred_return_candidates: Vec::new(),
            inferred_return_reads: HashMap::default(),
            pending_call_site_return_consumers: Vec::new(),
            pending_call_site_definition_refreshes: Vec::new(),
            call_site_return_targets: Vec::new(),
            call_site_return_definition_refreshes: HashMap::default(),
            pending_unresolve_decl_ids: HashSet::default(),
            settled_decl_initializer_candidates: HashSet::default(),
            member_initializer_reinfer_candidates: HashSet::default(),
            guarded_member_read_candidates: HashSet::default(),
            settled_sibling_merge_read_candidates: HashSet::default(),
            settled_decl_copy_candidates: HashSet::default(),
            settled_assign_candidates: Vec::new(),
            settled_assign_candidate_keys: HashSet::default(),
            settled_member_key_candidates: None,
            settled_owner_candidates: None,
            settled_candidate_reads: HashMap::default(),
            settled_assign_claims: HashMap::default(),
            settled_assign_candidate_counts: HashMap::default(),
            settled_return_attempts: HashSet::default(),
            unresolve_read_records: HashMap::default(),
            late_resolved_decls: HashSet::default(),
            infer_manager: InferCacheManager::new(),
            inferred_guard_dependencies: HashMap::default(),
            inferred_guard_candidates: Vec::new(),
            early_callable_signatures: Vec::new(),
            early_member_owner_candidates: Vec::new(),
            settled_member_attach_candidates: Vec::new(),
            force_finalized: Vec::new(),
            finalize_unresolves: true,
            settled_guarded_bootstrap_candidates: Vec::new(),
            settled_iter_var_candidates: Vec::new(),
            settled_global_read_candidates: Vec::new(),
            settled_multi_decl_global_read_candidates: Vec::new(),
            settled_dynamic_field_candidates: Vec::new(),
            vgui_parent_fallback_files: HashSet::default(),
            deferred_vgui_parent_call_sites: HashMap::default(),
            call_site_return_invalidation_changed: false,
            settled_parent_closure_candidates: Vec::new(),
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

    fn drain_inferred_return_reads(&mut self) {
        self.inferred_return_reads
            .extend(self.infer_manager.drain_inferred_return_reads());
    }

    fn persist_inferred_return_dependencies(&mut self, db: &mut DbIndex) {
        self.drain_inferred_return_reads();
        for (signature_id, record) in std::mem::take(&mut self.inferred_return_reads) {
            let source_file_id = signature_id.get_file_id();
            let mut dependencies =
                record
                    .reads
                    .type_owners
                    .into_iter()
                    .map(|owner| owner.get_file_id())
                    .chain(
                        record
                            .reads
                            .signatures
                            .into_iter()
                            .map(|signature| signature.get_file_id()),
                    )
                    .chain(
                        record
                            .reads
                            .params
                            .into_iter()
                            .map(|signature| signature.get_file_id()),
                    )
                    .chain(record.reads.modules)
                    .chain(
                        record
                            .reads
                            .special_calls
                            .into_iter()
                            .map(|(file_id, _)| file_id),
                    )
                    .chain(
                        record
                            .reads
                            .contribs
                            .into_iter()
                            .map(|(signature_id, _)| signature_id.get_file_id()),
                    )
                    .chain(record.reads.dynamic_fields.into_iter().flat_map(
                        |(owner, field_name)| {
                            let index = db.get_dynamic_field_index();
                            match field_name {
                                Some(field_name) => index
                                    .field_definitions(&owner, field_name.as_str())
                                    .iter()
                                    .map(|definition| definition.file_id)
                                    .collect::<Vec<_>>(),
                                None => index
                                    .get_wildcard_definitions(&owner)
                                    .iter()
                                    .map(|definition| definition.file_id)
                                    .collect::<Vec<_>>(),
                            }
                        },
                    ))
                    .collect::<HashSet<_>>();
            dependencies.remove(&source_file_id);
            db.get_signature_index_mut()
                .set_inferred_return_dependencies(signature_id, dependencies);
        }
    }

    fn inferred_return_reads_are_stale(
        &self,
        now: &crate::db_index::read_set::InferenceReadVersion,
        db: &DbIndex,
        signature_id: LuaSignatureId,
    ) -> bool {
        self.inferred_return_reads
            .get(&signature_id)
            .is_some_and(|record| record.is_stale_against(now, db))
    }

    fn record_settled_candidate_reads(
        &mut self,
        pass: SettledReadPass,
        candidate: SettledCandidate,
        version: crate::db_index::read_set::InferenceReadVersion,
        reads: crate::db_index::read_set::InferenceReadSet,
    ) {
        self.settled_candidate_reads
            .insert((pass, candidate), (version, reads));
    }

    pub(crate) fn analyzed_file_ids(&self) -> HashSet<FileId> {
        self.tree_list.iter().map(|tree| tree.file_id).collect()
    }

    /// Remembers an assignment whose widening skipped a sibling that had no type
    /// yet. The assigned type is kept as written, not as widened, so the settled
    /// pass can re-derive the merge instead of growing the partial answer.
    /// Remembers a `for ... in pairs(t)` whose variable types were read off
    /// `t`'s member map. Which members were attached when it ran is a property
    /// of how far the batch had got, so it is taken again once the settled
    /// member passes have finished attaching them.
    pub(crate) fn record_settled_iter_var_candidate(&mut self, iter_var: UnResolveIterVar) {
        self.settled_iter_var_candidates.push(iter_var);
    }

    /// Remembers a dynamic write whose prefix had no nameable table type yet,
    /// so its wildcard can be taken again once the batch has settled.
    pub(crate) fn record_settled_dynamic_field_candidate(
        &mut self,
        file_id: FileId,
        syntax_id: glua_parser::LuaSyntaxId,
    ) {
        self.settled_dynamic_field_candidates
            .push((file_id, syntax_id));
    }

    /// Remembers `local x = SomeGlobal`. A global's type is the merge of every
    /// file that writes it, and a batch that retains some writers while its own
    /// are still empty answers the read from a smaller set than a cold build
    /// sees. See `rederive_settled_global_reads`.
    pub(crate) fn record_settled_global_read_candidate(
        &mut self,
        decl_id: LuaDeclId,
        expr: LuaExpr,
    ) {
        self.settled_global_read_candidates.push((decl_id, expr));
    }

    pub(crate) fn record_settled_multi_decl_global_read_candidate(
        &mut self,
        decl_id: LuaDeclId,
        expr: LuaExpr,
    ) {
        self.settled_multi_decl_global_read_candidates
            .push((decl_id, expr));
    }

    pub(crate) fn record_vgui_parent_fallback_file(&mut self, file_id: FileId) {
        self.vgui_parent_fallback_files.insert(file_id);
    }

    pub(crate) fn record_deferred_vgui_parent_call(
        &mut self,
        file_id: FileId,
        syntax_id: LuaSyntaxId,
    ) {
        self.deferred_vgui_parent_call_sites
            .entry(file_id)
            .or_default()
            .insert(syntax_id);
    }

    pub(crate) fn record_settled_guarded_bootstrap_candidate(&mut self, member_id: LuaMemberId) {
        self.settled_guarded_bootstrap_candidates.push(member_id);
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

    fn add_settled_parent_closure_candidate(
        &mut self,
        candidate: unresolve::UnResolveParentClosureParams,
    ) {
        if self
            .settled_parent_closure_candidates
            .iter()
            .any(|existing| existing.signature_id == candidate.signature_id)
        {
            return;
        }
        self.settled_parent_closure_candidates.push(candidate);
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

    fn resolve_call_site_return_consumers(&mut self, db: &mut DbIndex) -> HashSet<LuaTypeOwner> {
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
            return HashSet::default();
        }

        // These consumers feed each other: one's expression can read a local, or
        // a function return, that another one settles. Inferring the whole set
        // against the pre-publish index leaves every such reader holding its
        // neighbour's *unresolved* value, and whether a neighbour is in this
        // batch or was already published by an earlier build is a property of
        // the batch rather than of the source. Iterate until publishing stops
        // moving anything, so a partial re-index and a cold build agree.
        let mut moved = false;
        let mut fuse = common::FixpointFuse::new("resolve_call_site_return_consumers");
        loop {
            if fuse.trip() {
                break;
            }
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
                let full = infer_expr_fact_with_cache(db, cache, expr.clone());
                // Publishing the fabricated `nil` would overwrite whatever the
                // walk derived with a record that this consumer's call is still
                // unresolved. Which consumers run at all depends on which
                // signatures this batch requeued, so that overwrite lands on a
                // warm re-index and not on a cold build. Leaving the slot alone
                // keeps both on the answer the walk reached.
                if !selects_resolved_result(full.typ(), *ret_idx) {
                    continue;
                }
                let fact = select_result_fact(full, *ret_idx);
                // Same reasoning one step further. A declaration whose slot an
                // ordered write claimed has had its writers arbitrated already:
                // `bind_decl_write` ranks `any` against the informative ones and
                // leaves two uninformative answers to the type index's own
                // order. A result that loses that ranking would overrule the
                // arbitration only on the runs where the batch happened to
                // requeue this consumer's signature.
                if !crate::db_index::is_informative_type(fact.typ())
                    && matches!(owner, LuaTypeOwner::Decl(decl_id)
                        if db.get_type_index().decl_write_claim(decl_id).is_some())
                    && db
                        .get_type_index()
                        .get_type_cache(owner)
                        .is_some_and(|current| {
                            !LuaTypeCache::InferType(fact.typ().clone()).supersedes(current)
                        })
                {
                    continue;
                }
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
            moved = true;
        }
        if moved {
            self.call_site_return_targets
                .iter()
                .map(|(_, owner, _, _)| owner.clone())
                .collect()
        } else {
            HashSet::default()
        }
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
    /// Queues an assignment onto a local for the settled replay.
    ///
    /// The write bound during the walk to whatever its right-hand side read
    /// then; a call or index answer that moves afterwards leaves the local's
    /// cache stale with nothing to report it. Re-derived through
    /// `bind_decl_write` once the batch settles.
    pub fn request_settled_assign_candidate(
        &mut self,
        decl_id: LuaDeclId,
        expr: LuaExpr,
        ret_idx: usize,
    ) {
        if self
            .settled_assign_candidate_keys
            .insert((decl_id, expr.get_range(), ret_idx))
        {
            self.settled_assign_candidates
                .push((decl_id, expr, ret_idx));
        }
    }

    pub fn request_settled_decl_initializer_reinfer(&mut self, decl_id: LuaDeclId) {
        self.settled_decl_initializer_candidates.insert(decl_id);
    }

    pub fn request_member_initializer_reinfer(&mut self, member_id: LuaMemberId) {
        self.member_initializer_reinfer_candidates.insert(member_id);
    }

    /// Queues a member write whose value read a member the enclosing `if`
    /// tested for existence.
    ///
    /// That guard narrows the receiver to the types owning the key, taken
    /// straight off the member index, so the read answers from however many of
    /// those types the batch had walked. Re-derived once they all stand.
    pub fn request_guarded_member_read_reinfer(&mut self, member_id: LuaMemberId) {
        self.guarded_member_read_candidates.insert(member_id);
    }

    /// Queues a member write whose value merged a table's sibling members
    /// under a computed key. See `settled_sibling_merge_read_candidates`.
    pub fn request_sibling_merge_read_reinfer(&mut self, member_id: LuaMemberId) {
        self.settled_sibling_merge_read_candidates.insert(member_id);
    }

    /// A context for a parallel lua-walk worker: the walk reads `metas`,
    /// `workspace_id` and `infer_manager` (shadowed) plus the delta
    /// collections [`take_walk_deltas`](Self::take_walk_deltas) drains.
    /// `tree_list` stays empty — red chunks cannot cross threads and the
    /// walk never reads it.
    pub(crate) fn worker_shadow_context(&self) -> Self {
        let mut context = Self::new();
        context.metas = self.metas.clone();
        context.workspace_id = self.workspace_id;
        context.infer_manager = self.infer_manager.worker_shadow();
        // Read-only Arc'd precomputes the walk may consult; copying them is
        // free and keeps a shadow from recomputing against its empty
        // `tree_list`.
        context.scripted_scope_files = self.scripted_scope_files.clone();
        context.scripted_scope_infos = self.scripted_scope_infos.clone();
        context.gmod_global_call_roles = self.gmod_global_call_roles.clone();
        context.finalize_unresolves = self.finalize_unresolves;
        context
    }

    /// Drains every context collection the lua walk can write, as `Send`
    /// descriptors. Called on the worker after each file's walk; replaying
    /// the delta on the canonical context through [`apply_walk_deltas`]
    /// reproduces the serial accumulation in order.
    pub(crate) fn take_walk_deltas(&mut self) -> WalkCtxDelta {
        // Re-derived by the replays below (`add_unresolve` re-inserts decl
        // ids, `request_settled_assign_candidate` re-keys dedup).
        self.pending_unresolve_decl_ids.clear();
        self.settled_assign_candidate_keys.clear();
        WalkCtxDelta {
            unresolves: self
                .unresolves
                .drain(..)
                .map(|(item, reason)| (unresolve::spec::describe_walk(&item), reason))
                .collect(),
            inferred_returns: self
                .inferred_return_candidates
                .drain(..)
                .map(|ret| unresolve::spec::describe_walk(&UnResolve::Return(Box::new(ret))))
                .collect(),
            assign_candidates: self
                .settled_assign_candidates
                .drain(..)
                .map(|(decl_id, expr, ret_idx)| {
                    (decl_id, LuaSyntaxId::from_node(expr.syntax()), ret_idx)
                })
                .collect(),
            member_attach_candidates: self.settled_member_attach_candidates.drain(..).collect(),
            decl_init_candidates: self.settled_decl_initializer_candidates.drain().collect(),
            member_init_candidates: self.member_initializer_reinfer_candidates.drain().collect(),
            guarded_member_reads: self.guarded_member_read_candidates.drain().collect(),
            sibling_merge_reads: self.settled_sibling_merge_read_candidates.drain().collect(),
            vgui_fallback_files: self.vgui_parent_fallback_files.drain().collect(),
            iter_var_candidates: self
                .settled_iter_var_candidates
                .drain(..)
                .map(|var| unresolve::spec::describe_walk(&UnResolve::IterDecl(Box::new(var))))
                .collect(),
            global_read_candidates: self
                .settled_global_read_candidates
                .drain(..)
                .map(|(decl_id, expr)| (decl_id, LuaSyntaxId::from_node(expr.syntax())))
                .collect(),
            multi_decl_global_read_candidates: self
                .settled_multi_decl_global_read_candidates
                .drain(..)
                .map(|(decl_id, expr)| (decl_id, LuaSyntaxId::from_node(expr.syntax())))
                .collect(),
            guarded_bootstrap_candidates: self
                .settled_guarded_bootstrap_candidates
                .drain(..)
                .collect(),
        }
    }

    /// Names of residual collections still holding entries after
    /// [`take_walk_deltas`](Self::take_walk_deltas) — diagnostics for the
    /// parallel walk's coverage audit.
    pub(crate) fn walk_delta_residual_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if !self.inferred_return_reads.is_empty() {
            fields.push("inferred_return_reads");
        }
        if !self.pending_call_site_return_consumers.is_empty() {
            fields.push("pending_call_site_return_consumers");
        }
        if !self.pending_call_site_definition_refreshes.is_empty() {
            fields.push("pending_call_site_definition_refreshes");
        }
        if !self.call_site_return_targets.is_empty() {
            fields.push("call_site_return_targets");
        }
        if !self.call_site_return_definition_refreshes.is_empty() {
            fields.push("call_site_return_definition_refreshes");
        }
        if !self.pending_unresolve_decl_ids.is_empty() {
            fields.push("pending_unresolve_decl_ids");
        }
        if !self.settled_assign_candidate_keys.is_empty() {
            fields.push("settled_assign_candidate_keys");
        }
        if self
            .settled_member_key_candidates
            .as_ref()
            .is_some_and(|(count, map)| *count != 0 || !map.is_empty())
        {
            fields.push("settled_member_key_candidates");
        }
        if self
            .settled_owner_candidates
            .as_ref()
            .is_some_and(|(count, map)| *count != 0 || !map.is_empty())
        {
            fields.push("settled_owner_candidates");
        }
        if !self.settled_return_attempts.is_empty() {
            fields.push("settled_return_attempts");
        }
        if !self.settled_assign_claims.is_empty() {
            fields.push("settled_assign_claims");
        }
        if !self.settled_assign_candidate_counts.is_empty() {
            fields.push("settled_assign_candidate_counts");
        }
        if !self.late_resolved_decls.is_empty() {
            fields.push("late_resolved_decls");
        }
        if !self.inferred_guard_dependencies.is_empty() {
            fields.push("inferred_guard_dependencies");
        }
        if !self.inferred_guard_candidates.is_empty() {
            fields.push("inferred_guard_candidates");
        }
        if !self.early_callable_signatures.is_empty() {
            fields.push("early_callable_signatures");
        }
        if !self.early_member_owner_candidates.is_empty() {
            fields.push("early_member_owner_candidates");
        }
        if !self.force_finalized.is_empty() {
            fields.push("force_finalized");
        }
        if !self.settled_guarded_bootstrap_candidates.is_empty() {
            fields.push("settled_guarded_bootstrap_candidates");
        }
        if !self.settled_iter_var_candidates.is_empty() {
            fields.push("settled_iter_var_candidates");
        }

        if !self.settled_global_read_candidates.is_empty() {
            fields.push("settled_global_read_candidates");
        }
        if !self.settled_multi_decl_global_read_candidates.is_empty() {
            fields.push("settled_multi_decl_global_read_candidates");
        }
        if !self.settled_dynamic_field_candidates.is_empty() {
            fields.push("settled_dynamic_field_candidates");
        }
        if !self.deferred_vgui_parent_call_sites.is_empty() {
            fields.push("deferred_vgui_parent_call_sites");
        }
        if !self.settled_parent_closure_candidates.is_empty() {
            fields.push("settled_parent_closure_candidates");
        }
        if !self.settled_decl_copy_candidates.is_empty() {
            fields.push("settled_decl_copy_candidates");
        }
        if self.call_site_return_invalidation_changed {
            fields.push("call_site_return_invalidation_changed");
        }
        fields
    }

    /// Whether a context collection the parallel walk does not model still
    /// holds entries after [`take_walk_deltas`](Self::take_walk_deltas).
    /// Non-empty means the file's walk wrote context state the merge cannot
    /// move — the file is re-run serially.
    pub(crate) fn walk_delta_residual_nonempty(&self) -> bool {
        !self.inferred_return_reads.is_empty()
            || !self.pending_call_site_return_consumers.is_empty()
            || !self.pending_call_site_definition_refreshes.is_empty()
            || !self.call_site_return_targets.is_empty()
            || !self.call_site_return_definition_refreshes.is_empty()
            || !self.pending_unresolve_decl_ids.is_empty()
            || !self.settled_assign_candidate_keys.is_empty()
            || self
                .settled_member_key_candidates
                .as_ref()
                .is_some_and(|(count, map)| *count != 0 || !map.is_empty())
            || self
                .settled_owner_candidates
                .as_ref()
                .is_some_and(|(count, map)| *count != 0 || !map.is_empty())
            || !self.settled_return_attempts.is_empty()
            || !self.settled_assign_claims.is_empty()
            || !self.settled_assign_candidate_counts.is_empty()
            || !self.late_resolved_decls.is_empty()
            || !self.inferred_guard_dependencies.is_empty()
            || !self.inferred_guard_candidates.is_empty()
            || !self.early_callable_signatures.is_empty()
            || !self.early_member_owner_candidates.is_empty()
            || !self.force_finalized.is_empty()
            || !self.settled_guarded_bootstrap_candidates.is_empty()
            || !self.settled_iter_var_candidates.is_empty()
            || !self.settled_global_read_candidates.is_empty()
            || !self.settled_multi_decl_global_read_candidates.is_empty()
            || !self.settled_dynamic_field_candidates.is_empty()
            || !self.deferred_vgui_parent_call_sites.is_empty()
            || !self.settled_parent_closure_candidates.is_empty()
            || !self.settled_decl_copy_candidates.is_empty()
            || self.call_site_return_invalidation_changed
    }

    /// Replays a worker's drained context deltas, in order, through the real
    /// context methods — dedup and bookkeeping (`pending_unresolve_decl_ids`,
    /// `settled_assign_candidate_keys`) re-derive exactly as the serial walk
    /// produced them. `false` when a descriptor cannot hydrate against
    /// `root`; the caller then re-runs the file serially.
    pub(crate) fn apply_walk_deltas(&mut self, delta: WalkCtxDelta, root: &LuaSyntaxNode) -> bool {
        // Hydrate every descriptor before applying anything: a failure must
        // leave the context untouched so the caller's serial re-run cannot
        // double-apply the leading entries.
        let mut unresolves = Vec::with_capacity(delta.unresolves.len());
        for (desc, reason) in delta.unresolves {
            let Some(item) = desc.hydrate(root) else {
                return false;
            };
            unresolves.push((item, reason));
        }
        let mut returns = Vec::with_capacity(delta.inferred_returns.len());
        for desc in delta.inferred_returns {
            let Some(UnResolve::Return(ret)) = desc.hydrate(root) else {
                return false;
            };
            returns.push(*ret);
        }
        let mut assigns = Vec::with_capacity(delta.assign_candidates.len());
        for (decl_id, syntax_id, ret_idx) in delta.assign_candidates {
            let Some(expr) = syntax_id.to_node_from_root(root).and_then(LuaExpr::cast) else {
                return false;
            };
            assigns.push((decl_id, expr, ret_idx));
        }
        let mut iter_vars = Vec::with_capacity(delta.iter_var_candidates.len());
        for desc in delta.iter_var_candidates {
            let Some(UnResolve::IterDecl(var)) = desc.hydrate(root) else {
                return false;
            };
            iter_vars.push(*var);
        }
        let mut global_reads = Vec::with_capacity(delta.global_read_candidates.len());
        for (decl_id, syntax_id) in delta.global_read_candidates {
            let Some(expr) = syntax_id.to_node_from_root(root).and_then(LuaExpr::cast) else {
                return false;
            };
            global_reads.push((decl_id, expr));
        }
        let mut multi_decl_global_reads =
            Vec::with_capacity(delta.multi_decl_global_read_candidates.len());
        for (decl_id, syntax_id) in delta.multi_decl_global_read_candidates {
            let Some(expr) = syntax_id.to_node_from_root(root).and_then(LuaExpr::cast) else {
                return false;
            };
            multi_decl_global_reads.push((decl_id, expr));
        }
        for (item, reason) in unresolves {
            self.add_unresolve(item, reason);
        }
        for ret in returns {
            self.add_inferred_return_candidate(ret);
        }
        for (decl_id, expr, ret_idx) in assigns {
            self.request_settled_assign_candidate(decl_id, expr, ret_idx);
        }
        for candidate in delta.member_attach_candidates {
            self.add_settled_member_attach_candidate(candidate);
        }
        for decl_id in delta.decl_init_candidates {
            self.request_settled_decl_initializer_reinfer(decl_id);
        }
        for member_id in delta.member_init_candidates {
            self.request_member_initializer_reinfer(member_id);
        }
        for member_id in delta.guarded_member_reads {
            self.request_guarded_member_read_reinfer(member_id);
        }
        for member_id in delta.sibling_merge_reads {
            self.request_sibling_merge_read_reinfer(member_id);
        }
        for file_id in delta.vgui_fallback_files {
            self.record_vgui_parent_fallback_file(file_id);
        }
        for var in iter_vars {
            self.record_settled_iter_var_candidate(var);
        }
        for (decl_id, expr) in global_reads {
            self.record_settled_global_read_candidate(decl_id, expr);
        }
        for (decl_id, expr) in multi_decl_global_reads {
            self.record_settled_multi_decl_global_read_candidate(decl_id, expr);
        }
        for member_id in delta.guarded_bootstrap_candidates {
            self.record_settled_guarded_bootstrap_candidate(member_id);
        }
        true
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
    use super::{
        cached_provisional_slot_union, settled_widens_cached_literals, union_has_unsettled_arm,
        union_widens_cached_type,
    };
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
    fn spots_a_provisional_slot_union_beside_an_uninformative_arm() {
        let literals = |extra: Vec<LuaType>| {
            let mut arms = vec![
                LuaType::TableConst(crate::InFiled::new(
                    crate::FileId::new(1),
                    rowan::TextRange::new(0.into(), 1.into()),
                )),
                LuaType::TableConst(crate::InFiled::new(
                    crate::FileId::new(1),
                    rowan::TextRange::new(2.into(), 3.into()),
                )),
            ];
            arms.extend(extra);
            union(arms)
        };
        // The read's own `nil`/`any` arms do not stop the rest being the
        // provisional flat-literal shape.
        assert!(cached_provisional_slot_union(&literals(vec![])));
        assert!(cached_provisional_slot_union(&literals(vec![LuaType::Nil])));
        assert!(cached_provisional_slot_union(&literals(vec![
            LuaType::Nil,
            LuaType::Any
        ])));
        // A real non-table arm still does.
        assert!(!cached_provisional_slot_union(&literals(vec![
            LuaType::String
        ])));
        // And one literal is not the shape, however many placeholders sit
        // beside it.
        assert!(!cached_provisional_slot_union(&union(vec![
            LuaType::TableConst(crate::InFiled::new(
                crate::FileId::new(1),
                rowan::TextRange::new(0.into(), 1.into()),
            )),
            LuaType::Nil,
            LuaType::Any,
        ])));
    }

    #[test]
    fn rejects_an_equal_union() {
        let settled = union(vec![LuaType::Number, LuaType::Any]);
        let cached = union(vec![LuaType::Number, LuaType::Any]);
        assert!(!union_widens_cached_type(&settled, &cached));
    }

    #[test]
    fn collapses_a_cached_literal_union_to_its_primitive() {
        let cached = union(vec![
            LuaType::StringConst(smol_str::SmolStr::new("").into()),
            LuaType::StringConst(smol_str::SmolStr::new("Public Area").into()),
        ]);
        assert!(settled_widens_cached_literals(&LuaType::String, &cached));
    }

    #[test]
    fn keeps_a_cached_union_the_settled_primitive_does_not_cover() {
        // A `nil` arm has no primitive to widen to, so collapsing would drop it.
        let with_nil = union(vec![
            LuaType::StringConst(smol_str::SmolStr::new("a").into()),
            LuaType::Nil,
        ]);
        assert!(!settled_widens_cached_literals(&LuaType::String, &with_nil));
        // A literal of another primitive is not covered either.
        let mixed = union(vec![
            LuaType::StringConst(smol_str::SmolStr::new("a").into()),
            LuaType::IntegerConst(1),
        ]);
        assert!(!settled_widens_cached_literals(&LuaType::String, &mixed));
        // And a non-union cache is never this shape.
        assert!(!settled_widens_cached_literals(
            &LuaType::String,
            &LuaType::StringConst(smol_str::SmolStr::new("a").into())
        ));
    }

    #[test]
    fn spots_the_unsettled_arm_of_a_cached_union() {
        assert!(union_has_unsettled_arm(&union(vec![
            LuaType::String,
            LuaType::Unknown
        ])));
        assert!(!union_has_unsettled_arm(&union(vec![
            LuaType::String,
            LuaType::Nil
        ])));
        assert!(!union_has_unsettled_arm(&LuaType::Unknown));
    }

    #[test]
    fn rejects_a_non_union_settled_type() {
        assert!(!union_widens_cached_type(
            &LuaType::Number,
            &LuaType::Number
        ));
    }
}
