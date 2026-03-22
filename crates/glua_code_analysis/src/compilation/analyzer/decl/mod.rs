mod docs;
mod exprs;
mod members;
mod stats;

use crate::{
    compilation::analyzer::AnalysisPipeline,
    db_index::{DbIndex, LuaScopeKind},
    profile::Profile,
};

use super::{
    AnalyzeContext,
    gmod::ensure_scoped_class_type_decl_for_file,
};
use crate::db_index::GmodScopedClassInfo;
use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaFuncStat, LuaSyntaxKind, LuaVarExpr};
use rowan::{TextRange, TextSize, WalkEvent};

use crate::{
    FileId,
    db_index::{LuaDecl, LuaDeclExtra, LuaDeclId, LuaDeclarationTree, LuaScopeId},
};

pub struct DeclAnalysisPipeline;

impl AnalysisPipeline for DeclAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("decl analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let scripted_scope_files = if db.get_emmyrc().gmod.enabled {
            Some(context.get_or_compute_scripted_scope_files(db).clone())
        } else {
            None
        };
        for in_filed_tree in tree_list.iter() {
            // Detect scoped class once here and cache in GmodInferIndex for gmod_pre reuse.
            let scoped_class_global_name = if let Some(scripted_scope_files) =
                scripted_scope_files.as_ref()
                && scripted_scope_files.contains(&in_filed_tree.file_id)
            {
                if let Some((class_name, global_name)) =
                    super::gmod::get_scripted_class_info_for_file(db, in_filed_tree.file_id)
                {
                    db.get_gmod_infer_index_mut()
                        .set_scoped_class_info(in_filed_tree.file_id, GmodScopedClassInfo {
                            class_name,
                            global_name: global_name.clone(),
                        });
                    Some(global_name)
                } else {
                    None
                }
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
                scoped_class_global_name,
            );
            analyzer.analyze();
            let decl_tree = analyzer.get_decl_tree();
            // Register the scoped class type (must happen during decl, before doc/flow phases)
            if let Some(scripted_scope_files) = scripted_scope_files.as_ref()
                && scripted_scope_files.contains(&in_filed_tree.file_id)
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
            stats::analyze_func_stat(analyzer, stat);
        }
        LuaAst::LuaLocalFuncStat(stat) => {
            analyzer.create_scope(stat.get_range(), LuaScopeKind::FuncStat);
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
    scoped_class_global_name: Option<String>,
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
        scoped_class_global_name: Option<String>,
    ) -> DeclAnalyzer<'a> {
        DeclAnalyzer {
            db,
            root,
            decl: LuaDeclarationTree::new(file_id),
            scoped_class_global_name,
            scopes: Vec::new(),
            is_meta: false,
            context,
        }
    }

    pub fn analyze(&mut self) {
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
        if let Some(scoped_class_global_name) = self.scoped_class_global_name.as_ref()
            && decl.get_name() == scoped_class_global_name
            && let LuaDeclExtra::Global { kind } = decl.extra.clone()
        {
            decl.extra = LuaDeclExtra::Local { kind, attrib: None };
        }

        let is_global = decl.is_global();
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

        id
    }

    pub fn find_decl(&self, name: &str, position: TextSize) -> Option<&LuaDecl> {
        self.decl.find_local_decl(name, position)
    }

    pub fn is_scoped_class_global_name(&self, name: &str) -> bool {
        self.scoped_class_global_name
            .as_ref()
            .is_some_and(|scoped_name| scoped_name == name)
    }
}

fn is_method_func_stat(stat: &LuaFuncStat) -> Option<bool> {
    let func_name = stat.get_func_name()?;
    if let LuaVarExpr::IndexExpr(index_expr) = func_name {
        return Some(index_expr.get_index_token()?.is_colon());
    }
    None
}
