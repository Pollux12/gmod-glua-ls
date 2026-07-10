mod infer_expr_semantic_decl;
mod resolve_global_decl;
mod semantic_decl_level;
mod semantic_guard;

use std::sync::Arc;

use crate::{
    DbIndex, InFiled, LuaDeclExtra, LuaDeclId, LuaInferenceConfidence, LuaInferenceEventId,
    LuaInferenceNodeId, LuaInferenceProvenanceKind, LuaInferenceStep, LuaMemberId,
    LuaSemanticDeclId, LuaType, LuaTypeCache, LuaTypeFact, LuaTypeOwner,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaDocNameType, LuaDocTag, LuaExpr, LuaLocalName, LuaParamName,
    LuaSyntaxId, LuaSyntaxKind, LuaSyntaxNode, LuaSyntaxToken, LuaTableField,
};
pub use infer_expr_semantic_decl::infer_expr_semantic_decl;
pub use resolve_global_decl::resolve_global_decl_id;
pub use semantic_decl_level::SemanticDeclLevel;
pub use semantic_guard::SemanticDeclGuard;

use super::infer::try_local_decl_initializer_fallback_type;
use super::{
    InferFailReason, LuaInferCache, infer_bind_value_type, infer_expr, infer_param_with_cache,
};

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticInfo {
    inference_fact: LuaTypeFact,
    pub semantic_decl: Option<LuaSemanticDeclId>,
    pub origin: SemanticInfoOrigin,
}

impl SemanticInfo {
    pub fn actual(typ: LuaType, semantic_decl: Option<LuaSemanticDeclId>) -> Self {
        Self::from_fact(
            LuaTypeFact::certain(typ),
            semantic_decl,
            SemanticInfoOrigin::Actual,
        )
    }

    fn from_fact(
        inference_fact: LuaTypeFact,
        semantic_decl: Option<LuaSemanticDeclId>,
        origin: SemanticInfoOrigin,
    ) -> Self {
        Self {
            inference_fact,
            semantic_decl,
            origin,
        }
    }

    pub fn contextual_expected(
        inference_fact: LuaTypeFact,
        semantic_decl: Option<LuaSemanticDeclId>,
    ) -> Self {
        Self::from_fact(
            inference_fact,
            semantic_decl,
            SemanticInfoOrigin::ContextualExpected,
        )
    }

    pub fn display_typ(&self) -> &LuaType {
        self.inference_fact.typ()
    }

    pub fn inference_fact(&self) -> &LuaTypeFact {
        &self.inference_fact
    }

    fn canonical(fact: LuaTypeFact, semantic_decl: Option<LuaSemanticDeclId>) -> Self {
        let origin = if fact.confidence() >= LuaInferenceConfidence::Certain {
            SemanticInfoOrigin::Actual
        } else {
            SemanticInfoOrigin::ContextualExpected
        };
        Self::from_fact(fact, semantic_decl, origin)
    }

    pub fn actual_typ(&self) -> Option<&LuaType> {
        matches!(self.origin, SemanticInfoOrigin::Actual).then_some(self.inference_fact.typ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticInfoOrigin {
    Actual,
    ContextualExpected,
}

pub fn infer_token_semantic_info(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    token: LuaSyntaxToken,
) -> Option<SemanticInfo> {
    let parent = token.parent()?;
    match parent.kind().into() {
        LuaSyntaxKind::ForStat | LuaSyntaxKind::ForRangeStat | LuaSyntaxKind::LocalName => {
            let file_id = cache.get_file_id();
            let decl_id = LuaDeclId::new(file_id, token.text_range().start());
            let type_owner = LuaTypeOwner::Decl(decl_id);
            let type_cache = db
                .get_type_index()
                .get_type_cache(&type_owner)
                .unwrap_or(&LuaTypeCache::InferType(LuaType::Unknown));
            let typ = type_cache.as_type().clone();
            let typ = try_local_decl_initializer_fallback_type(
                db,
                cache,
                decl_id,
                &typ,
                token.text_range().start(),
            )
            .unwrap_or(typ);
            let fact = db
                .get_inference_fact(&LuaInferenceNodeId::TypeOwner(type_owner))
                .unwrap_or_else(|| LuaTypeFact::certain(typ.clone()))
                .with_runtime_type(typ);
            Some(SemanticInfo::canonical(
                fact,
                Some(LuaSemanticDeclId::LuaDecl(decl_id)),
            ))
        }
        LuaSyntaxKind::ParamName => {
            let file_id = cache.get_file_id();
            let decl_id = LuaDeclId::new(file_id, token.text_range().start());
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            match &decl.extra {
                LuaDeclExtra::Param { .. } => {
                    let typ = infer_param_with_cache(db, cache, decl).ok()?;

                    Some(SemanticInfo::actual(
                        typ,
                        Some(LuaSemanticDeclId::LuaDecl(decl_id)),
                    ))
                }
                _ => None,
            }
        }
        _ => infer_node_semantic_info(db, cache, parent),
    }
}

pub fn infer_node_semantic_info(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    node: LuaSyntaxNode,
) -> Option<SemanticInfo> {
    match node {
        expr_node if LuaExpr::can_cast(expr_node.kind().into()) => {
            let expr = LuaExpr::cast(expr_node)?;
            let property_owner = infer_expr_semantic_decl(
                db,
                cache,
                expr.clone(),
                SemanticDeclGuard::default(),
                SemanticDeclLevel::NoTrace,
            );
            Some(infer_expr_semantic_info(db, cache, expr, property_owner))
        }
        table_field_node if LuaTableField::can_cast(table_field_node.kind().into()) => {
            let table_field = LuaTableField::cast(table_field_node)?;
            let member_id = LuaMemberId::new(table_field.get_syntax_id(), cache.get_file_id());
            let type_cache = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .unwrap_or(&LuaTypeCache::InferType(LuaType::Unknown));
            Some(SemanticInfo::actual(
                type_cache.as_type().clone(),
                Some(LuaSemanticDeclId::Member(member_id)),
            ))
        }
        name_type if LuaDocNameType::can_cast(name_type.kind().into()) => {
            let name_type = LuaDocNameType::cast(name_type)?;
            let name = name_type.get_name_text()?;
            let type_decl = db
                .get_type_index()
                .find_type_decl(cache.get_file_id(), &name)?;
            Some(SemanticInfo::actual(
                LuaType::Ref(type_decl.get_id()),
                LuaSemanticDeclId::TypeDecl(type_decl.get_id()).into(),
            ))
        }
        tags if LuaDocTag::can_cast(tags.kind().into()) => {
            let tag = LuaDocTag::cast(tags)?;
            match tag {
                LuaDocTag::Alias(alias) => {
                    type_def_tag_info(alias.get_name_token()?.get_name_text(), db, cache)
                }
                LuaDocTag::Class(class) => {
                    type_def_tag_info(class.get_name_token()?.get_name_text(), db, cache)
                }
                LuaDocTag::Enum(enum_) => {
                    type_def_tag_info(enum_.get_name_token()?.get_name_text(), db, cache)
                }
                LuaDocTag::Field(field) => {
                    let member_id = LuaMemberId::new(field.get_syntax_id(), cache.get_file_id());
                    let type_cache = db
                        .get_type_index()
                        .get_type_cache(&member_id.into())
                        .unwrap_or(&LuaTypeCache::InferType(LuaType::Unknown));
                    Some(SemanticInfo::actual(
                        type_cache.as_type().clone(),
                        Some(LuaSemanticDeclId::Member(member_id)),
                    ))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn infer_expr_semantic_info(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    semantic_decl: Option<LuaSemanticDeclId>,
) -> SemanticInfo {
    let contextual_fact = semantic_decl
        .as_ref()
        .and_then(semantic_decl_inference_node)
        .and_then(|node| db.get_inference_fact(&node))
        .filter(|fact| {
            fact.confidence() < LuaInferenceConfidence::Certain && !fact.typ().is_unknown()
        });

    let actual_result = infer_expr(db, cache, expr.clone());
    match actual_result {
        Ok(typ) if !typ.is_unknown() => contextual_fact
            .filter(|fact| fact.typ() == &typ)
            .map(|fact| SemanticInfo::canonical(fact, semantic_decl.clone()))
            .unwrap_or_else(|| actual_expr_semantic_info(db, typ, semantic_decl)),
        actual_result => infer_bind_value_type(db, cache, expr.clone())
            .filter(|typ| !typ.is_nil())
            .map(|typ| {
                let node = semantic_decl
                    .as_ref()
                    .and_then(semantic_decl_inference_node)
                    .unwrap_or_else(|| {
                        LuaInferenceNodeId::TypeOwner(LuaTypeOwner::SyntaxId(InFiled::new(
                            cache.get_file_id(),
                            expr.get_syntax_id(),
                        )))
                    });
                let fact = LuaTypeFact::new(
                    typ,
                    LuaInferenceConfidence::Anchored,
                    Arc::from([LuaInferenceStep {
                        event: LuaInferenceEventId {
                            node,
                            kind: LuaInferenceProvenanceKind::ContextualUnknown,
                            source: InFiled::new(cache.get_file_id(), expr.get_syntax_id()),
                        },
                        support: Arc::from([]),
                    }]),
                );
                SemanticInfo::contextual_expected(fact, semantic_decl.clone())
            })
            .unwrap_or_else(|| match actual_result {
                Ok(typ) => actual_expr_semantic_info(db, typ, semantic_decl),
                Err(InferFailReason::FieldNotFound) if matches!(expr, LuaExpr::IndexExpr(_)) => {
                    // Lua absent table field reads evaluate to nil when no contextual expected type applies.
                    SemanticInfo::actual(LuaType::Nil, semantic_decl)
                }
                Err(_) => SemanticInfo::actual(LuaType::Unknown, semantic_decl),
            }),
    }
}

fn actual_expr_semantic_info(
    db: &DbIndex,
    typ: LuaType,
    semantic_decl: Option<LuaSemanticDeclId>,
) -> SemanticInfo {
    let fact = semantic_decl
        .as_ref()
        .and_then(semantic_decl_inference_node)
        .and_then(|node| db.get_inference_fact(&node))
        .unwrap_or_else(|| LuaTypeFact::certain(typ.clone()))
        .with_runtime_type(typ);
    SemanticInfo::from_fact(fact, semantic_decl, SemanticInfoOrigin::Actual)
}

fn semantic_decl_inference_node(semantic_decl: &LuaSemanticDeclId) -> Option<LuaInferenceNodeId> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            Some(LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Decl(*decl_id)))
        }
        LuaSemanticDeclId::Member(member_id) => Some(LuaInferenceNodeId::TypeOwner(
            LuaTypeOwner::Member(*member_id),
        )),
        LuaSemanticDeclId::TypeDecl(_) | LuaSemanticDeclId::Signature(_) => None,
    }
}

fn type_def_tag_info(name: &str, db: &DbIndex, cache: &mut LuaInferCache) -> Option<SemanticInfo> {
    let type_decl = db
        .get_type_index()
        .find_type_decl(cache.get_file_id(), name)?;
    Some(SemanticInfo::actual(
        LuaType::Ref(type_decl.get_id()),
        LuaSemanticDeclId::TypeDecl(type_decl.get_id()).into(),
    ))
}

pub fn infer_token_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    token: LuaSyntaxToken,
    level: SemanticDeclLevel,
) -> Option<LuaSemanticDeclId> {
    let parent = token.parent()?;
    match parent.kind().into() {
        LuaSyntaxKind::ForStat
        | LuaSyntaxKind::ForRangeStat
        | LuaSyntaxKind::LocalName
        | LuaSyntaxKind::ParamName => {
            let file_id = cache.get_file_id();
            let decl_id = LuaDeclId::new(file_id, token.text_range().start());
            Some(LuaSemanticDeclId::LuaDecl(decl_id))
        }
        _ => infer_node_semantic_decl(db, cache, parent, level),
    }
}

pub fn infer_node_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    node: LuaSyntaxNode,
    level: SemanticDeclLevel,
) -> Option<LuaSemanticDeclId> {
    match node {
        expr_node if LuaExpr::can_cast(expr_node.kind().into()) => {
            // Only use the decl cache for the default trace level (Trace(10)).
            // Goto-definition calls find_decl with both Trace(10) and NoTrace,
            // which can return different results for the same node.
            let use_cache = level == SemanticDeclLevel::default();
            let syntax_id = LuaSyntaxId::from_node(&expr_node);
            if use_cache {
                if let Some(cached) = cache.decl_cache.get(&syntax_id) {
                    return cached.clone();
                }
            }
            let expr = LuaExpr::cast(expr_node)?;
            let result =
                infer_expr_semantic_decl(db, cache, expr, SemanticDeclGuard::default(), level);
            if use_cache {
                cache.decl_cache.insert(syntax_id, result.clone());
            }
            result
        }
        table_field_node if LuaTableField::can_cast(table_field_node.kind().into()) => {
            let table_field = LuaTableField::cast(table_field_node)?;
            let member_id = LuaMemberId::new(table_field.get_syntax_id(), cache.get_file_id());
            Some(LuaSemanticDeclId::Member(member_id))
        }
        name_type if LuaDocNameType::can_cast(name_type.kind().into()) => {
            let name_type = LuaDocNameType::cast(name_type)?;
            let name = name_type.get_name_text()?;
            let type_decl = db
                .get_type_index()
                .find_type_decl(cache.get_file_id(), &name)?;
            LuaSemanticDeclId::TypeDecl(type_decl.get_id()).into()
        }
        tags if LuaDocTag::can_cast(tags.kind().into()) => {
            let tag = LuaDocTag::cast(tags)?;
            match tag {
                LuaDocTag::Alias(alias) => {
                    type_def_tag_property_owner(alias.get_name_token()?.get_name_text(), db, cache)
                }
                LuaDocTag::Class(class) => {
                    type_def_tag_property_owner(class.get_name_token()?.get_name_text(), db, cache)
                }
                LuaDocTag::Enum(enum_) => {
                    type_def_tag_property_owner(enum_.get_name_token()?.get_name_text(), db, cache)
                }
                LuaDocTag::Field(field) => {
                    let member_id = LuaMemberId::new(field.get_syntax_id(), cache.get_file_id());
                    Some(LuaSemanticDeclId::Member(member_id))
                }
                _ => None,
            }
        }
        local_name if LuaLocalName::can_cast(local_name.kind().into()) => {
            let local_name = LuaLocalName::cast(local_name)?;
            let name_token = local_name.get_name_token()?;
            infer_token_semantic_decl(db, cache, name_token.syntax().clone(), level)
        }
        param_name if LuaParamName::can_cast(param_name.kind().into()) => {
            let param_name = LuaParamName::cast(param_name)?;
            let name_token = param_name.get_name_token()?;
            infer_token_semantic_decl(db, cache, name_token.syntax().clone(), level)
        }
        _ => None,
    }
}

fn type_def_tag_property_owner(
    name: &str,
    db: &DbIndex,
    cache: &mut LuaInferCache,
) -> Option<LuaSemanticDeclId> {
    let type_decl = db
        .get_type_index()
        .find_type_decl(cache.get_file_id(), name)?;
    LuaSemanticDeclId::TypeDecl(type_decl.get_id()).into()
}
