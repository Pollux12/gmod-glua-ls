mod docs;
mod exprs;
mod members;
mod stats;

use crate::{
    compilation::analyzer::AnalysisPipeline,
    db_index::{DbIndex, GmodScopedClassInfo, LegacyModuleEnv, LuaScopeKind},
    profile::Profile,
};

use super::{AnalyzeContext, gmod::ensure_scoped_class_type_decl_for_file};
use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaFuncStat, LuaSyntaxKind, LuaVarExpr};
use rowan::{TextRange, TextSize, WalkEvent};

use crate::{
    FileId,
    db_index::{
        GlobalId, LuaDecl, LuaDeclExtra, LuaDeclId, LuaDeclarationTree, LuaMember,
        LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaScopeId, LuaType,
        LuaTypeCache,
    },
};
use smol_str::SmolStr;

pub struct DeclAnalysisPipeline;

impl AnalysisPipeline for DeclAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("decl analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();

        let scripted_scope_infos = if db.get_emmyrc().gmod.enabled {
            Some(context.get_or_compute_scripted_scope_infos(db).clone())
        } else {
            None
        };

        for in_filed_tree in tree_list.iter() {
            // Detect scoped class once here and cache in GmodInferIndex for gmod_pre reuse.
            let scoped_class_info = if let Some(scripted_scope_infos) =
                scripted_scope_infos.as_ref()
                && let Some(info) = scripted_scope_infos.get(&in_filed_tree.file_id)
            {
                db.get_gmod_infer_index_mut()
                    .set_scoped_class_info(in_filed_tree.file_id, info.clone());
                Some(info.clone())
            } else {
                None
            };

            db.get_reference_index_mut()
                .create_local_reference(in_filed_tree.file_id);

            let mut analyzer = DeclAnalyzer::new(
                db,
                in_filed_tree.file_id,
                in_filed_tree.value.clone(),
                context,
                scoped_class_info,
            );
            analyzer.analyze();
            let decl_tree = analyzer.get_decl_tree();
            // Register the scoped class type (must happen during decl, before doc/flow phases)
            if let Some(scripted_scope_infos) = scripted_scope_infos.as_ref()
                && scripted_scope_infos.contains_key(&in_filed_tree.file_id)
            {
                ensure_scoped_class_type_decl_for_file(
                    db,
                    in_filed_tree.file_id,
                    in_filed_tree.value.syntax().text_range(),
                );
            }

            db.get_decl_index_mut().add_decl_tree(decl_tree);
        }
    }
}

fn walk_node_enter(analyzer: &mut DeclAnalyzer, node: LuaAst) {
    match node {
        LuaAst::LuaChunk(chunk) => {
            analyzer.create_scope(chunk.get_range(), LuaScopeKind::Normal);
            analyzer.maybe_seed_scoped_class_local_decl(chunk.get_range());
        }
        LuaAst::LuaBlock(block) => {
            analyzer.create_scope(block.get_range(), LuaScopeKind::Normal);
        }
        LuaAst::LuaLocalStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::LocalOrAssignStat);
            stats::analyze_local_stat(analyzer, stat);
        }
        LuaAst::LuaAssignStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::LocalOrAssignStat);
            stats::analyze_assign_stat(analyzer, stat);
        }
        LuaAst::LuaForStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::Normal);
            stats::analyze_for_stat(analyzer, stat);
        }
        LuaAst::LuaForRangeStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::ForRange);
            stats::analyze_for_range_stat(analyzer, stat);
        }
        LuaAst::LuaFuncStat(stat) => {
            if is_method_func_stat(&stat).unwrap_or(false) {
                analyzer.create_scope(stat.get_range(), LuaScopeKind::MethodStat);
            } else {
                analyzer.create_scope(stat.get_range(), LuaScopeKind::FuncStat);
            }
            let file_id = analyzer.get_file_id();
            analyzer
                .db
                .get_member_index_mut()
                .add_function_scope_range(file_id, stat.get_range());
            stats::analyze_func_stat(analyzer, stat);
        }
        LuaAst::LuaLocalFuncStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::FuncStat);
            let file_id = analyzer.get_file_id();
            analyzer
                .db
                .get_member_index_mut()
                .add_function_scope_range(file_id, stat.get_range());
            stats::analyze_local_func_stat(analyzer, stat);
        }
        LuaAst::LuaRepeatStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::Repeat);
        }
        LuaAst::LuaNameExpr(expr) => {
            exprs::analyze_name_expr(analyzer, expr);
        }
        LuaAst::LuaIndexExpr(expr) => {
            exprs::analyze_index_expr(analyzer, expr);
        }
        LuaAst::LuaClosureExpr(expr) => {
            analyzer.create_scope(expr.get_range(), LuaScopeKind::Normal);
            let file_id = analyzer.get_file_id();
            analyzer
                .db
                .get_member_index_mut()
                .add_function_scope_range(file_id, expr.get_range());
            exprs::analyze_closure_expr(analyzer, expr);
        }
        LuaAst::LuaTableExpr(expr) => {
            exprs::analyze_table_expr(analyzer, expr);
        }
        LuaAst::LuaLiteralExpr(expr) => {
            exprs::analyze_literal_expr(analyzer, expr);
        }
        LuaAst::LuaCallExpr(expr) => {
            exprs::analyze_call_expr(analyzer, expr);
        }
        LuaAst::LuaDocTagClass(doc_tag) => {
            docs::analyze_doc_tag_class(analyzer, doc_tag);
        }
        LuaAst::LuaDocTagEnum(doc_tag) => {
            docs::analyze_doc_tag_enum(analyzer, doc_tag);
        }
        LuaAst::LuaDocTagAlias(doc_tag) => {
            docs::analyze_doc_tag_alias(analyzer, doc_tag);
        }
        LuaAst::LuaDocTagAttribute(doc_tag) => {
            docs::analyze_doc_tag_attribute(analyzer, doc_tag);
        }
        LuaAst::LuaDocTagNamespace(doc_tag) => {
            docs::analyze_doc_tag_namespace(analyzer, doc_tag);
        }
        LuaAst::LuaDocTagUsing(doc_tag) => {
            docs::analyze_doc_tag_using(analyzer, doc_tag);
        }
        LuaAst::LuaDocTagMeta(doc_tag) => {
            docs::analyze_doc_tag_meta(analyzer, doc_tag);
        }
        _ => {}
    }
}

fn walk_node_leave(analyzer: &mut DeclAnalyzer, node: LuaAst) {
    if is_scope_owner(&node) {
        analyzer.pop_scope();
    }
}

fn is_scope_owner(node: &LuaAst) -> bool {
    matches!(
        node.syntax().kind().into(),
        LuaSyntaxKind::Chunk
            | LuaSyntaxKind::Block
            | LuaSyntaxKind::ClosureExpr
            | LuaSyntaxKind::RepeatStat
            | LuaSyntaxKind::ForRangeStat
            | LuaSyntaxKind::ForStat
            | LuaSyntaxKind::LocalStat
            | LuaSyntaxKind::FuncStat
            | LuaSyntaxKind::LocalFuncStat
            | LuaSyntaxKind::AssignStat
    )
}

#[derive(Debug)]
pub struct DeclAnalyzer<'a> {
    db: &'a mut DbIndex,
    root: LuaChunk,
    decl: LuaDeclarationTree,
    scoped_class_info: Option<GmodScopedClassInfo>,
    seeded_scoped_class_decl: bool,
    legacy_module_envs: Vec<LegacyModuleEnv>,
    scopes: Vec<LuaScopeId>,
    is_meta: bool,
    context: &'a mut AnalyzeContext,
}

impl<'a> DeclAnalyzer<'a> {
    pub fn new(
        db: &'a mut DbIndex,
        file_id: FileId,
        root: LuaChunk,
        context: &'a mut AnalyzeContext,
        scoped_class_info: Option<GmodScopedClassInfo>,
    ) -> DeclAnalyzer<'a> {
        DeclAnalyzer {
            db,
            root,
            decl: LuaDeclarationTree::new(file_id),
            scoped_class_info,
            seeded_scoped_class_decl: false,
            legacy_module_envs: Vec::new(),
            scopes: Vec::new(),
            is_meta: false,
            context,
        }
    }

    fn analyze(&mut self) {
        let root = self.root.clone();
        for walk_event in root.walk_descendants::<LuaAst>() {
            match walk_event {
                WalkEvent::Enter(node) => walk_node_enter(self, node),
                WalkEvent::Leave(node) => walk_node_leave(self, node),
            }
        }
    }

    pub fn get_file_id(&self) -> FileId {
        self.decl.file_id()
    }

    pub fn get_decl_tree(self) -> LuaDeclarationTree {
        self.decl
    }

    pub fn create_scope(&mut self, range: TextRange, kind: LuaScopeKind) {
        let scope_id = self.decl.create_scope(range, kind);
        if let Some(parent_scope_id) = self.scopes.last() {
            self.decl.add_child_scope(*parent_scope_id, scope_id);
        }

        self.scopes.push(scope_id);
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn add_decl_to_current_scope(&mut self, decl_id: LuaDeclId) {
        if let Some(scope_id) = self.scopes.last() {
            self.decl.add_decl_to_scope(*scope_id, decl_id);
        }
    }

    pub fn add_decl(&mut self, mut decl: LuaDecl) -> LuaDeclId {
        if let Some(scoped_class_info) = self.scoped_class_info.as_ref()
            && decl.get_name() == scoped_class_info.global_name
            && !scoped_class_info.is_global_singleton
            && let LuaDeclExtra::Global { kind } = decl.extra.clone()
        {
            decl.extra = LuaDeclExtra::Local { kind, attrib: None };
        }

        if let Some(module_env) = self.get_legacy_module_env_at(decl.get_position())
            && should_bind_decl_to_legacy_module(&decl, module_env)
            && let Some(kind) = decl_kind(&decl)
        {
            decl.extra = LuaDeclExtra::Module {
                kind,
                module_path: SmolStr::new(module_env.module_path.as_str()),
            };
        }

        let is_global = decl.is_global();
        let module_member_owner = decl
            .get_module_path()
            .map(|module_path| LuaMemberOwner::GlobalPath(GlobalId::new(module_path)));
        let file_id = decl.get_file_id();
        let name = decl.get_name().to_string();
        let syntax_id = decl.get_syntax_id();
        let id = self.decl.add_decl(decl);
        self.add_decl_to_current_scope(id);

        if is_global {
            self.db.get_global_index_mut().add_global_decl(&name, id);

            self.db
                .get_reference_index_mut()
                .add_global_reference(&name, file_id, syntax_id);
        }

        if let Some(owner) = module_member_owner {
            let member_id = LuaMemberId::new(syntax_id, file_id);
            let member = LuaMember::new(
                member_id,
                LuaMemberKey::Name(name.into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            );
            self.db.get_member_index_mut().add_member(owner, member);
            // Link the member's property to the same property as the decl so that
            // description/annotations stored on the decl (or its signature) are visible
            // when looking up the member (e.g. hover over a legacy-module member).
            self.db.get_property_index_mut().add_owner_map(
                crate::LuaSemanticDeclId::LuaDecl(id),
                crate::LuaSemanticDeclId::Member(member_id),
                file_id,
            );
        }

        id
    }

    pub fn find_decl(&self, name: &str, position: TextSize) -> Option<&LuaDecl> {
        let decl = self.decl.find_local_decl(name, position)?;
        let Some(module_env) = self.get_legacy_module_env_at(position) else {
            return Some(decl);
        };
        if decl.is_module_scoped()
            && decl.get_module_path() != Some(module_env.module_path.as_str())
        {
            return None;
        }

        Some(decl)
    }

    pub fn is_scoped_class_global_name(&self, name: &str) -> bool {
        self.scoped_class_info.as_ref().is_some_and(|info| {
            info.global_name == name
                || info.aliases.iter().any(|alias| alias == name)
                || info
                    .extra_scope_matches
                    .iter()
                    .any(|(_, global_name, _, _, _)| global_name == name)
        })
    }

    pub fn set_legacy_module_env(&mut self, legacy_module_env: LegacyModuleEnv) {
        self.project_legacy_module_chain_members(&legacy_module_env);
        let file_id = self.decl.file_id();
        self.db
            .get_module_index_mut()
            .set_legacy_module_env(file_id, legacy_module_env.clone());
        self.legacy_module_envs.push(legacy_module_env);
        self.legacy_module_envs
            .sort_by_key(|env| env.activation_position);
    }

    pub fn get_legacy_module_env_at(&self, position: TextSize) -> Option<&LegacyModuleEnv> {
        // Binary search: find the rightmost env where activation_position < position.
        // The vector is sorted by activation_position ascending.
        let envs = &self.legacy_module_envs;
        if envs.is_empty() {
            return None;
        }
        let pos_u32 = u32::from(position);
        let idx = envs.partition_point(|env| u32::from(env.activation_position) < pos_u32);
        if idx == 0 {
            return None;
        }
        Some(&envs[idx - 1])
    }

    fn maybe_seed_scoped_class_local_decl(&mut self, chunk_range: TextRange) {
        if self.seeded_scoped_class_decl {
            return;
        }

        let Some(scoped_class_info) = self.scoped_class_info.clone() else {
            return;
        };

        let synthetic_pos = chunk_range.start();
        let synthetic_range = TextRange::new(synthetic_pos, synthetic_pos);
        self.seed_scoped_class_decl_name(
            &scoped_class_info.global_name,
            scoped_class_info.is_global_singleton,
            synthetic_range,
        );

        if scoped_class_info.is_global_singleton {
            for alias in &scoped_class_info.aliases {
                self.seed_scoped_class_decl_name(alias, true, synthetic_range);
            }
        }

        for (_, extra_global_name, _, extra_is_singleton, _) in
            &scoped_class_info.extra_scope_matches
        {
            self.seed_scoped_class_decl_name(
                extra_global_name,
                *extra_is_singleton,
                synthetic_range,
            );
            if *extra_is_singleton {
                for alias in super::gmod::remap_scoped_alias(
                    extra_global_name,
                    &self.db.get_emmyrc().gmod.scripted_class_scopes,
                ) {
                    self.seed_scoped_class_decl_name(&alias, true, synthetic_range);
                }
            }
        }

        self.seeded_scoped_class_decl = true;
    }

    fn seed_scoped_class_decl_name(
        &mut self,
        global_name: &str,
        is_global_singleton: bool,
        synthetic_range: TextRange,
    ) {
        let file_id = self.get_file_id();
        let extra = if is_global_singleton {
            LuaDeclExtra::Global {
                kind: LuaSyntaxKind::NameExpr.into(),
            }
        } else {
            LuaDeclExtra::Local {
                kind: LuaSyntaxKind::NameExpr.into(),
                attrib: None,
            }
        };
        let mut decl = LuaDecl::new(global_name, file_id, synthetic_range, extra, None);
        if !is_global_singleton {
            decl.mark_seeded_class_local();
        }

        self.add_decl(decl);
    }

    fn project_legacy_module_chain_members(&mut self, legacy_module_env: &LegacyModuleEnv) {
        let parts = legacy_module_env
            .module_path
            .split('.')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if parts.len() < 2 {
            return;
        }

        let file_id = self.decl.file_id();
        let env_start = legacy_module_env.activation_position;
        for idx in 1..parts.len() {
            let owner_path = parts[..idx].join(".");
            let child_segment = parts[idx];
            let child_path = parts[..=idx].join(".");
            let synthetic_offset = TextSize::new(idx as u32);
            let synthetic_position = env_start + synthetic_offset;

            let member_id = LuaMemberId::new(
                glua_parser::LuaSyntaxId::new(
                    glua_parser::LuaSyntaxKind::CallExpr.into(),
                    TextRange::new(synthetic_position, synthetic_position),
                ),
                file_id,
            );
            let owner = LuaMemberOwner::GlobalPath(GlobalId::new(&owner_path));
            let member = LuaMember::new(
                member_id,
                LuaMemberKey::Name(child_segment.into()),
                LuaMemberFeature::FileFieldDecl,
                Some(GlobalId::new(&child_path)),
            );
            self.db.get_member_index_mut().add_member(owner, member);
            self.db.get_type_index_mut().bind_type(
                member_id.into(),
                LuaTypeCache::InferType(LuaType::Namespace(SmolStr::new(&child_path).into())),
            );
        }
    }
}

fn is_method_func_stat(stat: &LuaFuncStat) -> Option<bool> {
    let func_name = stat.get_func_name()?;
    if let LuaVarExpr::IndexExpr(index_expr) = func_name {
        return Some(index_expr.get_index_token()?.is_colon());
    }
    None
}

fn decl_kind(decl: &LuaDecl) -> Option<glua_parser::LuaKind> {
    match decl.extra {
        LuaDeclExtra::Local { kind, .. }
        | LuaDeclExtra::ImplicitSelf { kind }
        | LuaDeclExtra::Global { kind }
        | LuaDeclExtra::Module { kind, .. } => Some(kind),
        LuaDeclExtra::Param { .. } => None,
    }
}

fn should_bind_decl_to_legacy_module(decl: &LuaDecl, module_env: &LegacyModuleEnv) -> bool {
    matches!(decl.extra, LuaDeclExtra::Global { kind } if kind != LuaSyntaxKind::IndexExpr.into())
        && decl.get_position() > module_env.activation_position
}
