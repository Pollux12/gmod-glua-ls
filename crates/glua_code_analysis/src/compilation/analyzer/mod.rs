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
mod parallel;
mod setmetatable_factory;
pub(crate) mod unresolve;

pub(crate) use lua::infer_for_range_iter_expr_func;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    AsyncState, FileId, GmodScopedClassInfo, InFiled, InferFailReason, LuaDeclId, LuaDefinitionId,
    LuaFunctionType, LuaInferredGuardOwner, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaSignatureId, LuaType, LuaTypeCache, LuaTypeDeclId, LuaTypeFact, LuaTypeOwner, WorkspaceId,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{DbIndex, LuaMemberOwner},
    profile::Profile,
};
use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaChunk, LuaClosureExpr, LuaExpr, LuaNameExpr, LuaSyntaxId,
    LuaSyntaxNode,
};
use infer_cache_manager::InferCacheManager;
use lua::LuaReturnPoint;
use unresolve::{UnResolve, UnResolveReturn};

pub fn analyze(db: &mut DbIndex, need_analyzed_files: Vec<InFiled<LuaChunk>>) {
    if need_analyzed_files.is_empty() {
        return;
    }

    let contexts = module_analyze(db, need_analyzed_files);

    for (workspace_id, mut context) in contexts {
        context.workspace_id = Some(workspace_id);
        let profile_log = format!("analyze workspace {}", workspace_id);
        let _p = Profile::cond_new(&profile_log, context.tree_list.len() > 1);
        let workspace_file_ids = context
            .tree_list
            .iter()
            .map(|in_filed_tree| in_filed_tree.file_id)
            .collect::<Vec<_>>();

        run_analysis::<decl::DeclAnalysisPipeline>(db, &mut context);
        run_analysis::<doc::DocAnalysisPipeline>(db, &mut context);
        run_analysis::<gmod::GmodPreAnalysisPipeline>(db, &mut context);
        let early_signature_owners = publish_callable_signatures(db, &context);
        run_analysis::<flow::FlowAnalysisPipeline>(db, &mut context);

        let early_member_owners = resolve_early_member_owners(db, &mut context);
        local_inference::prepare_inferred_positive_guards(db, &context);
        let guard_candidates = context.inferred_guard_candidates.len();
        let early_guard_stats = stabilize_inferred_positive_guards(db, &mut context);

        run_analysis::<lua::LuaAnalysisPipeline>(db, &mut context);

        // Gmod post-analysis: synthesize members that depend on metadata collected
        // during lua_analyze (AccessorFunc, NetworkVar, VGUI register calls).
        run_analysis::<gmod::GmodPostAnalysisPipeline>(db, &mut context);

        synthesize_accessorfunc_members(db, &workspace_file_ids);
        let infer_dynamic_fields =
            db.get_emmyrc().gmod.enabled && db.get_emmyrc().gmod.infer_dynamic_fields;
        if infer_dynamic_fields {
            // Special-call resolution needs dynamic fields that point at outparam
            // tables, while some dynamic fields need unresolve-refined aliases.
            // Seed only declared-member table fields before unresolve; the full
            // dynamic pass still runs afterward.
            run_analysis::<dynamic_field::EarlyDynamicFieldAnalysisPipeline>(db, &mut context);
        }

        // Seed direct parent-to-child evidence before function returns are
        // resolved. The later pass still captures evidence unlocked by unresolve.
        let early_child_sources =
            local_inference::stabilize_unguarded_children(db, &mut context, true);
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
        let late_child_sources =
            local_inference::stabilize_unguarded_children(db, &mut context, false);
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

    // Initializer inference reads the stabilized indexes and records candidate
    // cache writes without mutating the database. Process that read-only work
    // per file, then merge inference side effects and type writes in stable file
    // and source order on the caller thread.
    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let mut infer_cache =
            crate::LuaInferCache::new(file_id, crate::CacheOptions { analysis_phase });
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
            let can_refine_nominal_type = !current_is_uninformative
                && db.get_emmyrc().gmod.enabled
                && current_cache
                    .as_ref()
                    .is_some_and(|current| single_nominal_type_id(current.as_type()).is_some())
                && db
                    .get_reference_index()
                    .get_decl_references(&decl_id.file_id, decl_id)
                    .is_none_or(|references| !references.mutable);
            if !current_is_uninformative && !can_refine_nominal_type {
                continue;
            }

            let Some((ret_idx, expr)) = local_initializer_expr(db, &root, *decl_id) else {
                continue;
            };
            if !matches!(expr, LuaExpr::CallExpr(_) | LuaExpr::IndexExpr(_)) {
                continue;
            }
            let Ok(mut inferred_type) = crate::infer_expr(db, &mut infer_cache, expr) else {
                continue;
            };
            if let LuaType::Variadic(variadic) = inferred_type {
                inferred_type = variadic.get_type(ret_idx).cloned().unwrap_or(LuaType::Nil);
            } else if ret_idx != 0 {
                inferred_type = LuaType::Nil;
            }
            if type_is_uninformative(&inferred_type)
                || current_cache
                    .as_ref()
                    .is_some_and(|current| current.as_type() == &inferred_type)
            {
                continue;
            }

            if current_is_uninformative {
                result.updates.push(InitializerCacheUpdate::Bind {
                    owner: type_owner,
                    inferred_type,
                });
            } else if can_refine_nominal_type
                && current_cache.as_ref().is_some_and(|current| {
                    is_strict_nominal_refinement(db, &inferred_type, current.as_type())
                })
            {
                result.updates.push(InitializerCacheUpdate::Overwrite {
                    owner: type_owner,
                    inferred_type,
                });
            }
        }
        result.pending_type_decls = infer_cache.take_pending_str_tpl_type_decls();
        result.guard_dependencies = infer_cache.take_inferred_guard_dependencies();
        result
    });

    for result in results {
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        apply_initializer_cache_updates(db, result.updates);
    }
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

    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let mut infer_cache =
            crate::LuaInferCache::new(file_id, crate::CacheOptions { analysis_phase });
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
            if current_cache.is_doc() || single_nominal_type_id(current_cache.as_type()).is_none() {
                continue;
            }
            let Some(expr) = member_initializer_expr(&root, *member_id) else {
                continue;
            };
            let Ok(inferred_type) = crate::infer_expr(db, &mut infer_cache, expr) else {
                continue;
            };
            if inferred_type == *current_cache.as_type()
                || !is_strict_nominal_refinement(db, &inferred_type, current_cache.as_type())
            {
                continue;
            }

            result.updates.push(InitializerCacheUpdate::Overwrite {
                owner: type_owner,
                inferred_type,
            });
        }
        result.pending_type_decls = infer_cache.take_pending_str_tpl_type_decls();
        result.guard_dependencies = infer_cache.take_inferred_guard_dependencies();
        result
    });

    for result in results {
        context.infer_manager.merge_inference_side_effects(
            result.file_id,
            result.pending_type_decls,
            result.guard_dependencies,
        );
        apply_initializer_cache_updates(db, result.updates);
    }
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
        inferred_type: LuaType,
    },
    Overwrite {
        owner: LuaTypeOwner,
        inferred_type: LuaType,
    },
}

fn apply_initializer_cache_updates(db: &mut DbIndex, updates: Vec<InitializerCacheUpdate>) {
    for update in updates {
        match update {
            InitializerCacheUpdate::Bind {
                owner,
                inferred_type,
            } => {
                common::bind_resolved_type(db, owner, LuaTypeCache::InferType(inferred_type));
            }
            InitializerCacheUpdate::Overwrite {
                owner,
                inferred_type,
            } => {
                common::write_type_cache(
                    db,
                    owner,
                    LuaTypeCache::InferType(inferred_type),
                    common::TypeCacheWriteMode::ForceOverwrite,
                );
            }
        }
    }
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

fn local_initializer_expr(
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
    T::analyze(db, context);
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
    unresolves: Vec<(UnResolve, InferFailReason)>,
    inferred_return_candidates: Vec<UnResolveReturn>,
    pending_call_site_return_consumers: Vec<UnResolve>,
    pending_call_site_definition_refreshes: Vec<(LuaDefinitionId, LuaTypeOwner)>,
    pending_unresolve_decl_ids: HashSet<LuaDeclId>,
    uninformative_local_decl_candidates: HashSet<LuaDeclId>,
    member_initializer_reinfer_candidates: HashSet<LuaMemberId>,
    infer_manager: InferCacheManager,
    inferred_guard_dependencies: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_guard_candidates: Vec<InFiled<LuaSyntaxId>>,
    early_callable_signatures: Vec<(crate::LuaTypeOwner, LuaSignatureId)>,
    early_member_owner_candidates: Vec<(LuaMemberId, LuaDeclId)>,
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
            unresolves: Vec::new(),
            inferred_return_candidates: Vec::new(),
            pending_call_site_return_consumers: Vec::new(),
            pending_call_site_definition_refreshes: Vec::new(),
            pending_unresolve_decl_ids: HashSet::new(),
            uninformative_local_decl_candidates: HashSet::new(),
            member_initializer_reinfer_candidates: HashSet::new(),
            infer_manager: InferCacheManager::new(),
            inferred_guard_dependencies: HashMap::new(),
            inferred_guard_candidates: Vec::new(),
            early_callable_signatures: Vec::new(),
            early_member_owner_candidates: Vec::new(),
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

    fn resolve_call_site_return_consumers(&mut self, db: &mut DbIndex) -> usize {
        let consumers = std::mem::take(&mut self.pending_call_site_return_consumers);
        let count = consumers.len();
        if count == 0 {
            self.pending_call_site_definition_refreshes.clear();
            return 0;
        }

        let mut definition_refreshes = HashMap::<LuaTypeOwner, Vec<LuaDefinitionId>>::new();
        for (definition, owner) in std::mem::take(&mut self.pending_call_site_definition_refreshes)
        {
            definition_refreshes
                .entry(owner)
                .or_default()
                .push(definition);
        }
        self.infer_manager.clear();

        for consumer in consumers {
            let (file_id, owner, expr, ret_idx) = match consumer {
                UnResolve::Decl(decl) => (
                    decl.file_id,
                    LuaTypeOwner::Decl(decl.decl_id),
                    decl.expr,
                    decl.ret_idx,
                ),
                UnResolve::Member(member) => {
                    let Some(expr) = member.expr else {
                        continue;
                    };
                    (
                        member.file_id,
                        LuaTypeOwner::Member(member.member_id),
                        expr,
                        member.ret_idx,
                    )
                }
                _ => continue,
            };
            let cache = self.infer_manager.get_infer_cache(file_id);
            let Ok(mut typ) = crate::infer_expr(db, cache, expr) else {
                continue;
            };
            if let LuaType::Variadic(variadic) = typ {
                typ = variadic.get_type(ret_idx).cloned().unwrap_or(LuaType::Nil);
            } else if ret_idx != 0 {
                typ = LuaType::Nil;
            }
            db.get_type_index_mut()
                .force_bind_type(owner.clone(), LuaTypeCache::InferType(typ.clone()));
            if let Some(definitions) = definition_refreshes.get(&owner) {
                for definition in definitions {
                    db.get_type_index_mut()
                        .bind_definition_fact(*definition, LuaTypeFact::certain(typ.clone()));
                }
            }
        }
        count
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
