mod docs;
mod exprs;
mod members;
mod ops;
mod stats;

use rustc_hash::FxHashSet;

use crate::db_index::{
    DbIndex, FileReference, FileReferenceBatch, GmodScopedClassInfo, LegacyModuleEnv, LuaScopeKind,
};

use super::{ScriptedScopes, common::estimate_file_index_entries};
use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaBlock, LuaChunk, LuaFuncStat, LuaNameExpr, LuaSyntaxKind,
    LuaVarExpr,
};
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

pub(super) use ops::DeclWalk;
use ops::{ContextDelta, DeclOp};

/// Walks `file_id`'s declarations. Reads only the file's own syntax tree and
/// facts of its own that no other file's declarations write, so files walk
/// concurrently; [`DeclWalk::apply`] writes what the walk found to the index.
pub(super) fn walk_file(db: &DbIndex, file_id: FileId, scopes: &ScriptedScopes) -> DeclWalk {
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .expect("a live file has a syntax tree")
        .get_chunk_node();
    let scoped_class_info = if db.get_emmyrc().gmod.enabled {
        scopes.infos.get(&file_id).cloned()
    } else {
        None
    };
    let range = root.syntax().text_range();
    let mut analyzer = DeclAnalyzer::new(db, file_id, root, scoped_class_info.clone());
    analyzer.analyze();
    let assignment_sites = analyzer.take_assignment_sites();
    let DeclAnalyzer {
        decl,
        references,
        ops,
        delta,
        ..
    } = analyzer;
    let reassignments = collect_local_reassignments(
        references.file_reference(),
        Some(&decl),
        &assignment_sites,
        file_id,
    );
    DeclWalk {
        file_id,
        scoped_class_info,
        chunk_range: range,
        estimate: estimate_file_index_entries(usize::from(range.len())),
        references,
        ops,
        decl_tree: decl,
        reassignments,
        delta,
    }
}

/// Where each local declared in `file_id` is first reassigned. The assignment
/// sites are collected during the declaration walk and resolved after the
/// file's references and declaration tree are complete.
fn collect_local_reassignments(
    references: &FileReference,
    decl_tree: Option<&LuaDeclarationTree>,
    assignment_sites: &[LuaAssignStat],
    file_id: FileId,
) -> rustc_hash::FxHashMap<LuaDeclId, TextSize> {
    let mut positions = rustc_hash::FxHashMap::default();
    for assign_stat in assignment_sites {
        let position = assign_stat.get_position();

        let (vars, _) = assign_stat.get_var_and_expr_list();
        for var in vars {
            let LuaVarExpr::NameExpr(name_expr) = var else {
                continue;
            };

            let assigned_decl_id = references
                .get_decl_id(&name_expr.get_range())
                .or_else(|| assignment_name_decl_id(decl_tree, &name_expr));
            let Some(assigned_decl_id) = assigned_decl_id else {
                continue;
            };
            if assigned_decl_id.file_id != file_id || position <= assigned_decl_id.position {
                continue;
            }

            positions
                .entry(assigned_decl_id)
                .and_modify(|first| {
                    if position < *first {
                        *first = position;
                    }
                })
                .or_insert(position);
        }
    }

    positions
}

fn assignment_name_decl_id(
    decl_tree: Option<&LuaDeclarationTree>,
    name_expr: &LuaNameExpr,
) -> Option<LuaDeclId> {
    let name = name_expr.get_name_text()?;

    decl_tree
        .and_then(|tree| tree.find_local_decl(&name, name_expr.get_position()))
        .map(|decl| decl.get_id())
}

fn walk_node_enter(analyzer: &mut DeclAnalyzer, node: LuaAst) {
    match node {
        LuaAst::LuaChunk(chunk) => {
            analyzer.create_scope(chunk.get_range(), LuaScopeKind::Normal);
            analyzer.maybe_seed_scoped_class_decl(chunk.get_range());
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
            analyzer.assignment_sites.push(stat.clone());
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
            // A function statement's name is assigned where the statement runs,
            // so only its closure, not the whole statement, is a function scope.
            analyzer
                .ops
                .push(DeclOp::FunctionScopeRange(expr.get_range()));
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

pub struct DeclAnalyzer<'a> {
    db: &'a DbIndex,
    root: LuaChunk,
    decl: LuaDeclarationTree,
    scoped_class_info: Option<GmodScopedClassInfo>,
    scoped_class_global_names: Option<FxHashSet<String>>,
    seeded_scoped_class_decl: bool,
    legacy_module_envs: Vec<LegacyModuleEnv>,
    assignment_sites: Vec<LuaAssignStat>,
    /// Where each bare name is assigned in the file, built on first use.
    name_assignments: std::cell::OnceCell<rustc_hash::FxHashMap<SmolStr, Vec<TextSize>>>,
    scopes: Vec<LuaScopeId>,
    is_meta: bool,
    file_namespace: Option<String>,
    references: FileReferenceBatch,
    ops: Vec<DeclOp>,
    delta: ContextDelta,
}

impl<'a> DeclAnalyzer<'a> {
    pub fn new(
        db: &'a DbIndex,
        file_id: FileId,
        root: LuaChunk,
        scoped_class_info: Option<GmodScopedClassInfo>,
    ) -> DeclAnalyzer<'a> {
        let scoped_class_global_names = scoped_class_info.as_ref().map(|info| {
            let mut names =
                FxHashSet::with_capacity_and_hasher(1 + info.aliases.len(), Default::default());
            names.insert(info.global_name.clone());
            names.extend(info.aliases.iter().cloned());
            names
        });
        DeclAnalyzer {
            db,
            root,
            decl: LuaDeclarationTree::new(file_id),
            scoped_class_info,
            scoped_class_global_names,
            seeded_scoped_class_decl: false,
            legacy_module_envs: Vec::new(),
            assignment_sites: Vec::new(),
            name_assignments: std::cell::OnceCell::new(),
            scopes: Vec::new(),
            is_meta: false,
            file_namespace: None,
            references: FileReferenceBatch::new(file_id),
            ops: Vec::new(),
            delta: ContextDelta::default(),
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

    fn add_member(&mut self, owner: LuaMemberOwner, member: LuaMember) -> LuaMemberId {
        let id = member.get_id();
        self.ops.push(DeclOp::AddMember { owner, member });
        id
    }

    fn take_assignment_sites(&mut self) -> Vec<LuaAssignStat> {
        std::mem::take(&mut self.assignment_sites)
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

    pub fn add_decl(&mut self, decl: LuaDecl) -> LuaDeclId {
        self.insert_decl(decl, true)
    }

    /// Adds the global a `_G.name` write declares, leaving the name to a local
    /// of it still in scope.
    pub fn add_global_table_decl(&mut self, decl: LuaDecl) -> LuaDeclId {
        let shadowed_by_local = self
            .find_decl(decl.get_name(), decl.get_position())
            .is_some_and(LuaDecl::is_local);
        self.insert_decl(decl, !shadowed_by_local)
    }

    fn insert_decl(&mut self, mut decl: LuaDecl, in_scope: bool) -> LuaDeclId {
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
        if in_scope {
            self.add_decl_to_current_scope(id);
        }

        if is_global {
            self.references.add_global_reference(&name, syntax_id);
            self.ops.push(DeclOp::GlobalDecl {
                name: name.clone(),
                decl_id: id,
                file_order_key: self.db.get_vfs().file_order_key(&file_id),
            });
        }

        if let Some(owner) = module_member_owner {
            let member_id = LuaMemberId::new(syntax_id, file_id);
            let member = LuaMember::new(
                member_id,
                LuaMemberKey::Name(name.into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            );
            self.add_member(owner, member);
            // Link the member's property to the same property as the decl so that
            // description/annotations stored on the decl (or its signature) are visible
            // when looking up the member (e.g. hover over a legacy-module member).
            self.ops.push(DeclOp::OwnerMap {
                source: crate::LuaSemanticDeclId::LuaDecl(id),
                same: crate::LuaSemanticDeclId::Member(member_id),
            });
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

    /// Whether `decl` is a local that localises the global of its name, as
    /// `local string = string` does, and is never assigned again: what is
    /// written through it is written to the global.
    pub fn localises_global_of_its_name(&self, decl: &LuaDecl) -> bool {
        let (LuaDeclExtra::Local { .. }, Some(initializer)) = (&decl.extra, decl.get_initializer())
        else {
            return false;
        };
        if initializer.get_ret_idx() != 0 {
            return false;
        }
        let Some(value) = initializer
            .get_expr_syntax_id()
            .to_node_from_root(self.root.syntax())
            .and_then(LuaNameExpr::cast)
        else {
            return false;
        };
        value.get_name_text().as_deref() == Some(decl.get_name())
            && self
                .references
                .file_reference()
                .get_decl_id(&value.get_range())
                .and_then(|source| self.decl.get_decl(&source))
                .is_none_or(|source| source.is_global())
            && !self.is_assigned_in_its_block(decl)
    }

    /// Whether `decl`'s name is assigned after it anywhere in the block that
    /// declares it, including writes the walk has not reached yet.
    fn is_assigned_in_its_block(&self, decl: &LuaDecl) -> bool {
        let Some(block) = decl
            .get_syntax_id()
            .to_node_from_root(self.root.syntax())
            .and_then(|node| node.ancestors().find_map(LuaBlock::cast))
        else {
            return true;
        };
        let block = block.get_range();
        let name_assignments = self.name_assignments.get_or_init(|| {
            let mut sites = rustc_hash::FxHashMap::<SmolStr, Vec<TextSize>>::default();
            for assign_stat in self.root.descendants::<LuaAssignStat>() {
                for var in assign_stat.get_var_and_expr_list().0 {
                    if let LuaVarExpr::NameExpr(name_expr) = var
                        && let Some(name) = name_expr.get_name_text()
                    {
                        sites
                            .entry(name)
                            .or_default()
                            .push(name_expr.get_position());
                    }
                }
            }
            sites
        });
        name_assignments
            .get(decl.get_name())
            .is_some_and(|positions| {
                positions
                    .iter()
                    .any(|position| *position > decl.get_position() && block.contains(*position))
            })
    }

    pub fn is_scoped_class_global_name(&self, name: &str) -> bool {
        self.scoped_class_global_names
            .as_ref()
            .is_some_and(|global_names| global_names.contains(name))
    }

    fn maybe_seed_scoped_class_decl(&mut self, chunk_range: TextRange) {
        if self.seeded_scoped_class_decl {
            return;
        }

        let Some(scoped_class_info) = self.scoped_class_info.clone() else {
            return;
        };
        if !scoped_class_info.is_global_singleton {
            return;
        }

        let synthetic_base = chunk_range.start();
        let mut synthetic_offset = 0u32;
        let mut next_synthetic_range = || {
            let position = synthetic_base + TextSize::new(synthetic_offset);
            synthetic_offset += 1;
            TextRange::new(position, position)
        };

        self.seed_scoped_class_decl_name(
            &scoped_class_info.global_name,
            scoped_class_info.is_global_singleton,
            next_synthetic_range(),
        );
        if scoped_class_info.is_global_singleton {
            for alias in &scoped_class_info.aliases {
                self.seed_scoped_class_decl_name(alias, true, next_synthetic_range());
            }
        }
        self.seeded_scoped_class_decl = true;
    }

    fn seed_scoped_class_decl_name(
        &mut self,
        global_name: &str,
        is_global_singleton: bool,
        range: TextRange,
    ) {
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
        self.add_decl(LuaDecl::new(
            global_name,
            self.get_file_id(),
            range,
            extra,
            None,
        ));
    }

    pub fn set_legacy_module_env(&mut self, legacy_module_env: LegacyModuleEnv) {
        self.project_legacy_module_chain_members(&legacy_module_env);
        self.ops
            .push(DeclOp::LegacyModuleEnv(legacy_module_env.clone()));
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
            self.add_member(owner, member);
            self.ops.push(DeclOp::WriteTypeCache {
                owner: member_id.into(),
                cache: LuaTypeCache::InferType(LuaType::Namespace(
                    SmolStr::new(&child_path).into(),
                )),
            });
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
