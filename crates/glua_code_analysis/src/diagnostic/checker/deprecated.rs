use std::collections::HashSet;

use glua_parser::{LuaAst, LuaAstNode, LuaIndexExpr, LuaNameExpr};

use crate::{
    DiagnosticCode, LuaCommonProperty, LuaDeclId, LuaDeprecated, LuaMemberId, LuaSemanticDeclId,
    LuaType, SemanticDeclLevel, SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct DeprecatedChecker;

impl Checker for DeprecatedChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::Unused, DiagnosticCode::Deprecated];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let candidates = DeprecatedCandidates::new(context);
        if candidates.is_empty() {
            return;
        }

        for node in root.descendants::<LuaAst>() {
            match node {
                LuaAst::LuaNameExpr(name_expr) => {
                    check_name_expr(context, semantic_model, name_expr, &candidates);
                }
                LuaAst::LuaIndexExpr(index_expr) => {
                    check_index_expr(context, semantic_model, index_expr, &candidates);
                }
                _ => {}
            }
        }
    }
}

struct DeprecatedCandidates {
    names: HashSet<String>,
}

impl DeprecatedCandidates {
    fn new(context: &DiagnosticContext) -> Self {
        let db = context.db;
        let mut names = HashSet::new();
        for (owner_id, property) in db.get_property_index().iter_owner_properties() {
            if !property_can_report_deprecated(property) {
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

    fn should_check(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

fn property_can_report_deprecated(property: &LuaCommonProperty) -> bool {
    property.deprecated().is_some()
        || property.attribute_uses().is_some_and(|attribute_uses| {
            attribute_uses
                .iter()
                .any(|attribute_use| attribute_use.id.get_name() == "deprecated")
        })
}

fn check_name_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    name_expr: LuaNameExpr,
    candidates: &DeprecatedCandidates,
) -> Option<()> {
    let name_token = name_expr.get_name_token()?;
    if !candidates.should_check(&name_token.get_name_text()) {
        return Some(());
    }

    let semantic_decl = semantic_model.find_decl(
        rowan::NodeOrToken::Node(name_expr.syntax().clone()),
        SemanticDeclLevel::default(),
    )?;

    let decl_id = LuaDeclId::new(semantic_model.get_file_id(), name_expr.get_position());
    if let LuaSemanticDeclId::LuaDecl(id) = &semantic_decl
        && *id == decl_id
    {
        return Some(());
    }

    check_deprecated(
        context,
        semantic_model,
        &semantic_decl,
        name_expr.get_range(),
    );
    Some(())
}

fn check_index_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: LuaIndexExpr,
    candidates: &DeprecatedCandidates,
) -> Option<()> {
    let index_name_token = index_expr.get_index_name_token()?;
    if !candidates.should_check(index_name_token.text()) {
        return Some(());
    }

    let semantic_decl = semantic_model.find_decl(
        rowan::NodeOrToken::Node(index_expr.syntax().clone()),
        SemanticDeclLevel::default(),
    )?;
    let member_id = LuaMemberId::new(index_expr.get_syntax_id(), semantic_model.get_file_id());
    if let LuaSemanticDeclId::Member(id) = &semantic_decl
        && *id == member_id
    {
        return Some(());
    }
    let index_name_range = index_name_token.text_range();
    check_deprecated(context, semantic_model, &semantic_decl, index_name_range);
    Some(())
}

fn check_deprecated(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    semantic_decl: &LuaSemanticDeclId,
    range: rowan::TextRange,
) {
    let property = semantic_model
        .get_db()
        .get_property_index()
        .get_property(semantic_decl);
    let Some(property) = property else {
        return;
    };
    if let Some(deprecated) = property.deprecated() {
        let deprecated_message = match deprecated {
            LuaDeprecated::Deprecated => "deprecated".to_string(),
            LuaDeprecated::DeprecatedWithMessage(message) => message.to_string(),
        };

        context.add_diagnostic(DiagnosticCode::Deprecated, range, deprecated_message, None);
    }
    // 检查特性
    if let Some(attribute_uses) = property.attribute_uses() {
        for attribute_use in attribute_uses.iter() {
            if attribute_use.id.get_name() == "deprecated" {
                let deprecated_message =
                    match attribute_use.args.first().and_then(|(_, typ)| typ.as_ref()) {
                        Some(LuaType::DocStringConst(message)) => message.as_ref().to_string(),
                        _ => "deprecated".to_string(),
                    };
                context.add_diagnostic(DiagnosticCode::Deprecated, range, deprecated_message, None);
            }
        }
    }
}
