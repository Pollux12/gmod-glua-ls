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
    AsyncState, FileId, GmodScopedClassInfo, InFiled, InferFailReason, LuaDeclId, LuaFunctionType,
    LuaInferredGuardOwner, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaSignatureId,
    LuaType, LuaTypeCache, WorkspaceId,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{DbIndex, LuaMemberOwner},
    profile::Profile,
};
use glua_parser::LuaSyntaxId;
use glua_parser::{LuaAstNode, LuaChunk, LuaExpr};
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
        if db.get_emmyrc().gmod.enabled && db.get_emmyrc().gmod.infer_dynamic_fields {
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

        run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
        setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);

        run_analysis::<call_site_params::CallSiteParamAnalysisPipeline>(db, &mut context);

        let call_site_return_invalidation_changed = context.call_site_return_invalidation_changed;
        let local_inference_changed = local_inference::stabilize_unknown_locals(db, &mut context);
        let child_inference_changed =
            !local_inference::stabilize_unguarded_children(db, &mut context, false).is_empty();
        let late_guard_retries = context.inferred_guard_candidates.len();
        let late_guard_stats = stabilize_inferred_positive_guards(db, &mut context);
        let inferred_guard_changed = late_guard_stats.changed;
        let late_inference_changed = call_site_return_invalidation_changed
            || local_inference_changed
            || child_inference_changed
            || inferred_guard_changed;

        let infer_dynamic_fields =
            db.get_emmyrc().gmod.enabled && db.get_emmyrc().gmod.infer_dynamic_fields;
        if infer_dynamic_fields {
            run_analysis::<dynamic_field::DynamicFieldAnalysisPipeline>(db, &mut context);
            context.infer_manager.clear();
            run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
            setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);
            resolve_uninformative_local_decl_caches(db, &mut context);
        } else if late_inference_changed {
            // Late inference facts can unlock deferred locals even when the
            // optional dynamic-field pass is disabled. Retry that retained work
            // in-place instead of reindexing the entire file batch.
            context.infer_manager.clear();
            run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);
            setmetatable_factory::synthesize_setmetatable_factory_members(db, &workspace_file_ids);
            resolve_uninformative_local_decl_caches(db, &mut context);
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

fn resolve_uninformative_local_decl_caches(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if context.uninformative_local_decl_candidates.is_empty() {
        return;
    }

    for decl_id in std::mem::take(&mut context.uninformative_local_decl_candidates) {
        let type_owner = decl_id.into();
        let current_cache = db.get_type_index().get_type_cache(&type_owner);
        if !type_cache_is_uninformative(current_cache) {
            continue;
        }

        let Some((ret_idx, expr)) = local_initializer_expr(db, decl_id) else {
            continue;
        };
        if !matches!(expr, LuaExpr::CallExpr(_) | LuaExpr::IndexExpr(_)) {
            continue;
        }

        let cache = context.infer_manager.get_infer_cache(decl_id.file_id);
        let Ok(mut inferred_type) = crate::infer_expr(db, cache, expr) else {
            continue;
        };
        if let LuaType::Variadic(variadic) = inferred_type {
            inferred_type = variadic.get_type(ret_idx).cloned().unwrap_or(LuaType::Nil);
        } else if ret_idx != 0 {
            inferred_type = LuaType::Nil;
        }
        if type_is_uninformative(&inferred_type) {
            continue;
        }

        common::bind_resolved_type(db, type_owner, LuaTypeCache::InferType(inferred_type));
    }
}

fn local_initializer_expr(db: &DbIndex, decl_id: LuaDeclId) -> Option<(usize, LuaExpr)> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    let root = db
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let node = initializer.get_expr_syntax_id().to_node_from_root(&root)?;
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
    pending_unresolve_decl_ids: HashSet<LuaDeclId>,
    uninformative_local_decl_candidates: HashSet<LuaDeclId>,
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
            pending_unresolve_decl_ids: HashSet::new(),
            uninformative_local_decl_candidates: HashSet::new(),
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

    pub(crate) fn mark_call_site_return_invalidation(&mut self) {
        self.call_site_return_invalidation_changed = true;
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

    pub fn has_pending_decl_unresolve(&self, decl_id: LuaDeclId) -> bool {
        self.pending_unresolve_decl_ids.contains(&decl_id)
    }

    pub fn request_uninformative_local_decl_reinfer(&mut self, decl_id: LuaDeclId) {
        self.uninformative_local_decl_candidates.insert(decl_id);
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

fn return_point_contains_range(point: &LuaReturnPoint, range: rowan::TextRange) -> bool {
    match point {
        LuaReturnPoint::Expr(expr) => expr.get_range().contains_range(range),
        LuaReturnPoint::MuliExpr(exprs) => exprs
            .iter()
            .any(|expr| expr.get_range().contains_range(range)),
        LuaReturnPoint::Nil | LuaReturnPoint::Error => false,
    }
}
