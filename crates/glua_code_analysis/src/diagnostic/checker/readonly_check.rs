use std::collections::HashSet;

use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaExpr, LuaIndexKey, LuaSyntaxId, LuaSyntaxKind,
};
use rowan::{NodeOrToken, TextRange};

use crate::{
    DiagnosticCode, LuaCommonProperty, LuaDeclId, LuaMemberId, LuaSemanticDeclId,
    PropertyDeclFeature, SemanticDeclLevel, SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct ReadOnlyChecker;

impl Checker for ReadOnlyChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::ReadOnly];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let candidates = ReadOnlyCandidates::new(context);
        if candidates.is_empty() {
            return;
        }

        for ast_node in root.descendants::<LuaAst>() {
            match ast_node {
                LuaAst::LuaAssignStat(assign_stat) => {
                    check_assign_stat(context, semantic_model, &assign_stat, &candidates);
                }
                // need check?
                LuaAst::LuaFuncStat(_) => {}
                // we need known function is readonly
                LuaAst::LuaCallExpr(_) => {}
                _ => {}
            }
        }
    }
}

struct ReadOnlyCandidates {
    names: HashSet<String>,
}

impl ReadOnlyCandidates {
    fn new(context: &DiagnosticContext) -> Self {
        let db = context.db;
        let mut names = HashSet::new();
        for (owner_id, property) in db.get_property_index().iter_owner_properties() {
            if !property_can_report_readonly(property) {
                continue;
            }

            match owner_id {
                LuaSemanticDeclId::LuaDecl(decl_id) => {
                    if let Some(decl) = db.get_decl_index().get_decl(decl_id) {
                        names.insert(decl.get_name().to_string());
                    }
                }
                LuaSemanticDeclId::Member(member_id) => {
                    if let Some(member) = db.get_member_index().get_member(member_id)
                        && let Some(name) = member.get_key().get_name()
                    {
                        names.insert(name.to_string());
                    }
                }
                LuaSemanticDeclId::TypeDecl(type_decl_id) => {
                    names.insert(type_decl_id.get_name().to_string());
                    names.insert(type_decl_id.get_simple_name().to_string());
                }
                LuaSemanticDeclId::Signature(_) => {}
            }
        }

        Self { names }
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn should_check_expr(&self, expr: &LuaExpr) -> bool {
        let mut current = expr.clone();
        loop {
            match current {
                LuaExpr::NameExpr(name_expr) => {
                    return name_expr
                        .get_name_text()
                        .is_some_and(|name| self.names.contains(name.as_ref() as &str));
                }
                LuaExpr::IndexExpr(index_expr) => {
                    if let Some(index_key) = index_expr.get_index_key()
                        && self.should_check_index_key(&index_key)
                    {
                        return true;
                    }
                    let Some(prefix) = index_expr.get_prefix_expr() else {
                        return false;
                    };
                    current = prefix;
                }
                _ => return false,
            }
        }
    }

    fn should_check_index_key(&self, index_key: &LuaIndexKey) -> bool {
        match index_key {
            LuaIndexKey::Name(name) => {
                let name = name.get_name_text();
                self.names.contains(name)
            }
            LuaIndexKey::String(string) => {
                let value = string.get_value();
                self.names.contains(value.as_str())
            }
            LuaIndexKey::Integer(_) | LuaIndexKey::Idx(_) | LuaIndexKey::Expr(_) => false,
        }
    }
}

fn property_can_report_readonly(property: &LuaCommonProperty) -> bool {
    property
        .decl_features
        .has_feature(PropertyDeclFeature::ReadOnly)
}

fn check_and_report_semantic_id(
    context: &mut DiagnosticContext,
    range: TextRange,
    semantic_decl_id: LuaSemanticDeclId,
) -> Option<()> {
    match semantic_decl_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let self_decl_id = LuaDeclId::new(context.file_id, range.start());
            if decl_id == self_decl_id {
                return None;
            }
        }
        LuaSemanticDeclId::Member(member_id) => {
            let syntax_id = LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range);
            let self_member_id = LuaMemberId::new(syntax_id, context.file_id);
            if member_id == self_member_id {
                return None;
            }
        }
        _ => {}
    }

    // TODO filter self
    let property_index = context.db.get_property_index();
    if let Some(property) = property_index.get_property(&semantic_decl_id) {
        if property
            .decl_features
            .has_feature(PropertyDeclFeature::ReadOnly)
        {
            context.add_diagnostic(
                DiagnosticCode::ReadOnly,
                range,
                t!("The variable is marked as readonly and cannot be assigned to.").to_string(),
                None,
            );
        }
    }

    Some(())
}

fn check_assign_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    assign_stat: &LuaAssignStat,
    candidates: &ReadOnlyCandidates,
) -> Option<()> {
    let (vars, _) = assign_stat.get_var_and_expr_list();
    for var in vars {
        let mut var = LuaExpr::cast(var.syntax().clone())?;
        if !candidates.should_check_expr(&var) {
            continue;
        }

        loop {
            let node_or_token = NodeOrToken::Node(var.syntax().clone());
            let semantic_decl_id =
                semantic_model.find_decl(node_or_token, SemanticDeclLevel::default());
            if let Some(semantic_decl_id) = semantic_decl_id {
                check_and_report_semantic_id(context, var.get_range(), semantic_decl_id);
            }
            match var {
                LuaExpr::IndexExpr(index_expr) => {
                    var = index_expr.get_prefix_expr()?;
                }
                _ => {
                    break;
                }
            }
        }
    }

    Some(())
}
