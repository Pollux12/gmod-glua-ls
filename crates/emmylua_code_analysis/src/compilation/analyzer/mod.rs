mod common;
mod decl;
mod doc;
mod dynamic_field;
mod flow;
mod gmod;
mod infer_cache_manager;
mod lua;
pub(crate) mod unresolve;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    AsyncState, Emmyrc, FileId, InFiled, InferFailReason, LuaFunctionType, LuaMember,
    LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaType, LuaTypeCache, WorkspaceId,
    db_index::{DbIndex, LuaMemberOwner},
    profile::Profile,
};
use emmylua_parser::LuaChunk;
use infer_cache_manager::InferCacheManager;
use unresolve::UnResolve;

pub fn analyze(db: &mut DbIndex, need_analyzed_files: Vec<InFiled<LuaChunk>>, config: Arc<Emmyrc>) {
    if need_analyzed_files.is_empty() {
        return;
    }

    let contexts = module_analyze(db, need_analyzed_files, config);

    for (workspace_id, mut context) in contexts {
        context.workspace_id = Some(workspace_id);
        let profile_log = format!("analyze workspace {}", workspace_id);
        let _p = Profile::cond_new(&profile_log, context.tree_list.len() > 1);

        run_analysis::<decl::DeclAnalysisPipeline>(db, &mut context);
        run_analysis::<doc::DocAnalysisPipeline>(db, &mut context);
        run_analysis::<flow::FlowAnalysisPipeline>(db, &mut context);
        run_analysis::<lua::LuaAnalysisPipeline>(db, &mut context);

        if db.get_emmyrc().gmod.enabled {
            run_analysis::<gmod::GmodAnalysisPipeline>(db, &mut context);
        }

        synthesize_accessorfunc_members(db);
        run_analysis::<unresolve::UnResolveAnalysisPipeline>(db, &mut context);

        if db.get_emmyrc().gmod.enabled && db.get_emmyrc().gmod.infer_dynamic_fields {
            context.infer_manager.clear();
            run_analysis::<dynamic_field::DynamicFieldAnalysisPipeline>(db, &mut context);
        }
    }
}

fn synthesize_accessorfunc_members(db: &mut DbIndex) {
    let all_calls = db
        .get_accessor_func_call_index()
        .iter()
        .map(|(file_id, calls)| (*file_id, calls.clone()))
        .collect::<Vec<_>>();

    for (file_id, file_calls) in all_calls {
        for call in file_calls {
            if call.accessor_name.is_empty() {
                continue;
            }

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
            db.get_type_index_mut().bind_type(
                getter_member_id.into(),
                LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
            );

            let setter_name = format!("Set{}", call.accessor_name);
            let setter_func = LuaFunctionType::new(
                AsyncState::None,
                true,
                false,
                vec![("value".to_string(), Some(LuaType::Any))],
                LuaType::Nil,
            );
            let setter_member_id = LuaMemberId::new(call.syntax_id, file_id);
            let setter_member = LuaMember::new(
                setter_member_id,
                LuaMemberKey::Name(setter_name.as_str().into()),
                LuaMemberFeature::FileMethodDecl,
                None,
            );
            db.get_member_index_mut().add_member(owner, setter_member);
            db.get_type_index_mut().bind_type(
                setter_member_id.into(),
                LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
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
    config: Arc<Emmyrc>,
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
            let mut context = AnalyzeContext::new(config);
            context.add_tree_chunk(in_filed_tree);
            return vec![(workspace_id, context)];
        } else if db.get_vfs().is_remote_file(&file_id) {
            let mut context = AnalyzeContext::new(config);
            context.add_tree_chunk(in_filed_tree);
            return vec![(WorkspaceId::REMOTE, context)];
        };

        return vec![];
    }

    let _p = Profile::new("module analyze");
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
        let mut context = AnalyzeContext::new(config.clone());
        context.tree_list = std_lib;
        contexts.push((WorkspaceId::STD, context));
    }

    let mut main_vec = Vec::new();
    for (workspace_id, tree_list) in file_tree_map {
        let mut context = AnalyzeContext::new(config.clone());
        context.tree_list = tree_list;
        if db.get_module_index().is_library_workspace_id(workspace_id)
            || db.get_module_index().is_remote_workspace_id(workspace_id)
        {
            contexts.push((workspace_id, context));
        } else {
            main_vec.push((workspace_id, context));
        }
    }

    contexts.sort_by(|a, b| a.0.cmp(&b.0));
    main_vec.sort_by(|a, b| a.0.cmp(&b.0));

    contexts.extend(main_vec);
    contexts
}

#[derive(Debug)]
pub struct AnalyzeContext {
    tree_list: Vec<InFiled<LuaChunk>>,
    #[allow(unused)]
    config: Arc<Emmyrc>,
    metas: HashSet<FileId>,
    scripted_scope_files: Option<HashSet<FileId>>,
    unresolves: Vec<(UnResolve, InferFailReason)>,
    infer_manager: InferCacheManager,
    pub workspace_id: Option<WorkspaceId>,
}

impl AnalyzeContext {
    pub fn new(emmyrc: Arc<Emmyrc>) -> Self {
        Self {
            tree_list: Vec::new(),
            config: emmyrc,
            metas: HashSet::new(),
            scripted_scope_files: None,
            unresolves: Vec::new(),
            infer_manager: InferCacheManager::new(),
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
        self.unresolves.push((un_resolve, reason));
    }

    pub fn get_or_compute_scripted_scope_files(&mut self, db: &DbIndex) -> &HashSet<FileId> {
        if self.scripted_scope_files.is_none() {
            let file_ids = self
                .tree_list
                .iter()
                .map(|in_filed_tree| in_filed_tree.file_id)
                .collect::<Vec<_>>();
            self.scripted_scope_files =
                Some(lua::call::compute_scripted_class_files(db, &file_ids));
        }

        self.scripted_scope_files.as_ref().expect("set above")
    }
}
