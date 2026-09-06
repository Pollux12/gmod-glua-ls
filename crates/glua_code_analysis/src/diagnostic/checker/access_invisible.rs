use std::collections::HashSet;
use std::sync::Arc;

use glua_parser::{LuaAst, LuaAstNode, LuaAstToken, LuaIndexExpr, LuaNameExpr, VisibilityKind};
use rowan::TextRange;

use crate::{
    DiagnosticCode, Emmyrc, LuaCommonProperty, LuaDeclId, LuaMemberId, LuaSemanticDeclId,
    SemanticDeclLevel, SemanticModel,
};

use super::{
    Checker, DiagnosticContext, PrecomputedPropertyNameCandidates,
    precompute_property_name_candidates,
};

pub struct AccessInvisibleChecker;

impl Checker for AccessInvisibleChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::AccessInvisible];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let candidates = AccessInvisibleCandidates::new(context, semantic_model.get_db());
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

fn check_name_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    name_expr: LuaNameExpr,
    candidates: &AccessInvisibleCandidates,
) -> Option<()> {
    let name_token = name_expr.get_name_token()?;
    if !candidates.should_check_name(&name_token.get_name_text()) {
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

    if !semantic_model.is_semantic_visible(name_token.syntax().clone(), semantic_decl.clone()) {
        let emmyrc = semantic_model.get_emmyrc();
        report_reason(context, emmyrc, name_token.get_range(), semantic_decl);
    }
    Some(())
}

fn check_index_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: LuaIndexExpr,
    candidates: &AccessInvisibleCandidates,
) -> Option<()> {
    let index_token = index_expr.get_index_name_token()?;
    if !candidates.should_check_member_name(index_token.text()) {
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

    if !semantic_model.is_semantic_visible(index_token.clone(), semantic_decl.clone()) {
        let emmyrc = semantic_model.get_emmyrc();
        report_reason(context, emmyrc, index_token.text_range(), semantic_decl);
    }

    Some(())
}

struct AccessInvisibleCandidates {
    candidates: Arc<PrecomputedPropertyNameCandidates>,
    private_name_patterns: Vec<String>,
}

impl AccessInvisibleCandidates {
    fn new(context: &DiagnosticContext, db: &crate::DbIndex) -> Self {
        Self {
            candidates: context
                .get_shared_data_arc()
                .map(|shared_data| shared_data.property_name_candidates.clone())
                .unwrap_or_else(|| Arc::new(precompute_property_name_candidates(db))),
            private_name_patterns: db.get_emmyrc().doc.private_name.clone(),
        }
    }

    fn explicit_names(&self) -> &HashSet<String> {
        &self.candidates.access_invisible
    }

    fn is_empty(&self) -> bool {
        self.explicit_names().is_empty() && self.private_name_patterns.is_empty()
    }

    fn should_check_name(&self, name: &str) -> bool {
        self.explicit_names().contains(name)
    }

    fn should_check_member_name(&self, name: &str) -> bool {
        self.explicit_names().contains(name) || self.matches_private_name_pattern(name)
    }

    fn matches_private_name_pattern(&self, name: &str) -> bool {
        self.private_name_patterns.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                name.starts_with(prefix)
            } else if let Some(suffix) = pattern.strip_prefix('*') {
                name.ends_with(suffix)
            } else {
                name == pattern
            }
        })
    }
}

pub(super) fn property_can_report_access_invisible(property: &LuaCommonProperty) -> bool {
    !matches!(property.visibility, VisibilityKind::Public) || property.version_conds().is_some()
}

fn report_reason(
    context: &mut DiagnosticContext,
    emmyrc: &Emmyrc,
    range: TextRange,
    property_owner_id: LuaSemanticDeclId,
) -> Option<()> {
    let property = context
        .db
        .get_property_index()
        .get_property(&property_owner_id)?;

    if let Some(version_conds) = &property.version_conds() {
        let version_number = emmyrc.runtime.version.to_lua_version_number();
        let visible = version_conds.iter().any(|cond| cond.check(&version_number));
        if !visible {
            let message = format!(
                "The current Lua version {version} is not accessible; expected {conds}.",
                version = version_number,
                conds = version_conds
                    .iter()
                    .map(|it| format!("{}", it))
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            context.add_diagnostic(
                DiagnosticCode::AccessInvisible,
                range,
                message.to_string(),
                None,
            );
            return Some(());
        }
    }

    let message = match property.visibility {
        VisibilityKind::Protected => {
            "The property is protected and cannot be accessed outside its subclasses.".to_string()
        }
        VisibilityKind::Private => {
            "The property is private and cannot be accessed outside the class.".to_string()
        }
        VisibilityKind::Package => {
            "The property is package-private and cannot be accessed outside the package."
                .to_string()
        }
        VisibilityKind::Internal => {
            "The property is internal and cannot be accessed outside the module.".to_string()
        }
        _ => {
            return None;
        }
    };

    context.add_diagnostic(
        DiagnosticCode::AccessInvisible,
        range,
        message.to_string(),
        None,
    );

    Some(())
}
