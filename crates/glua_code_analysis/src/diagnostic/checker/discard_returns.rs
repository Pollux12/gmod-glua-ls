use std::{collections::HashSet, sync::Arc};

use glua_parser::{LuaAstNode, LuaCallExpr, LuaCallExprStat, LuaExpr, LuaIndexKey};
use rowan::NodeOrToken;

use crate::{
    DbIndex, DiagnosticCode, LuaNoDiscard, LuaSemanticDeclId, LuaType, LuaTypeOwner,
    SemanticDeclLevel, SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct DiscardReturnsChecker;

#[derive(Debug, Default)]
pub struct PrecomputedNoDiscardCandidates {
    callee_names: HashSet<String>,
}

impl PrecomputedNoDiscardCandidates {
    fn is_empty(&self) -> bool {
        self.callee_names.is_empty()
    }

    fn should_check_call(&self, call_expr: &LuaCallExpr) -> bool {
        match static_call_name(call_expr) {
            StaticCallName::Static(name) => self.callee_names.contains(name.as_str()),
            StaticCallName::Unknown => true,
        }
    }
}

pub fn precompute_nodiscard_candidates(db: &DbIndex) -> PrecomputedNoDiscardCandidates {
    let mut candidates = PrecomputedNoDiscardCandidates::default();
    for (owner, type_cache) in db.get_type_index().iter_type_caches() {
        let LuaType::Signature(signature_id) = type_cache.as_type() else {
            continue;
        };
        let Some(signature) = db.get_signature_index().get(signature_id) else {
            continue;
        };
        if signature.nodiscard.is_none() {
            continue;
        }
        if let Some(name) = owner_name(db, owner) {
            candidates.callee_names.insert(name);
        }
    }
    candidates
}

impl Checker for DiscardReturnsChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::DiscardReturns];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let candidates = context
            .get_shared_data_arc()
            .map(|shared_data| shared_data.nodiscard_candidates.clone())
            .unwrap_or_else(|| Arc::new(precompute_nodiscard_candidates(semantic_model.get_db())));
        if candidates.is_empty() {
            return;
        }

        for call_expr_stat in root.descendants::<LuaCallExprStat>() {
            check_call_expr(context, semantic_model, call_expr_stat, &candidates);
        }
    }
}

enum StaticCallName {
    Static(String),
    Unknown,
}

fn static_call_name(call_expr: &LuaCallExpr) -> StaticCallName {
    match call_expr.get_prefix_expr() {
        Some(LuaExpr::NameExpr(name_expr)) => name_expr
            .get_name_token()
            .map(|token| StaticCallName::Static(token.get_name_text().to_string()))
            .unwrap_or(StaticCallName::Unknown),
        Some(LuaExpr::IndexExpr(index_expr)) => match index_expr.get_index_key() {
            Some(LuaIndexKey::Name(name)) => {
                StaticCallName::Static(name.get_name_text().to_string())
            }
            Some(LuaIndexKey::String(name)) => StaticCallName::Static(name.get_value()),
            _ => StaticCallName::Unknown,
        },
        _ => StaticCallName::Unknown,
    }
}

fn owner_name(db: &DbIndex, owner: &LuaTypeOwner) -> Option<String> {
    match owner {
        LuaTypeOwner::Decl(decl_id) => db
            .get_decl_index()
            .get_decl(decl_id)
            .map(|decl| decl.get_name().to_string()),
        LuaTypeOwner::Member(member_id) => db
            .get_member_index()
            .get_member(member_id)
            .and_then(|member| member.get_key().get_name())
            .map(str::to_string),
        LuaTypeOwner::SyntaxId(_) => None,
    }
}

fn check_call_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr_stat: LuaCallExprStat,
    candidates: &PrecomputedNoDiscardCandidates,
) -> Option<()> {
    let call_expr = call_expr_stat.get_call_expr()?;
    if !candidates.should_check_call(&call_expr) {
        return Some(());
    }

    let prefix_node = call_expr.get_prefix_expr()?.syntax().clone();
    let semantic_decl = semantic_model.find_decl(
        NodeOrToken::Node(prefix_node.clone()),
        SemanticDeclLevel::default(),
    )?;

    let signature_id = match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let type_cache = semantic_model
                .get_db()
                .get_type_index()
                .get_type_cache(&decl_id.into());
            if let Some(type_cache) = type_cache {
                if let LuaType::Signature(signature_id) = type_cache.as_type() {
                    *signature_id
                } else {
                    return Some(());
                }
            } else {
                return Some(());
            }
        }
        LuaSemanticDeclId::Member(member_id) => {
            let type_cache = semantic_model
                .get_db()
                .get_type_index()
                .get_type_cache(&member_id.into());
            if let Some(type_cache) = type_cache {
                if let LuaType::Signature(signature_id) = type_cache.as_type() {
                    *signature_id
                } else {
                    return Some(());
                }
            } else {
                return Some(());
            }
        }
        LuaSemanticDeclId::Signature(signature_id) => signature_id,
        _ => return Some(()),
    };

    let signature = semantic_model
        .get_db()
        .get_signature_index()
        .get(&signature_id)?;
    if let Some(nodiscard) = &signature.nodiscard {
        let nodiscard_message = match nodiscard {
            LuaNoDiscard::NoDiscard => "no discard".to_string(),
            LuaNoDiscard::NoDiscardWithMessage(message) => message.to_string(),
        };

        context.add_diagnostic(
            DiagnosticCode::DiscardReturns,
            prefix_node.text_range(),
            nodiscard_message,
            None,
        );
    }

    Some(())
}
