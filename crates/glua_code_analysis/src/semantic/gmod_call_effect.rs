use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr, LuaNameExpr};

use crate::{
    DbIndex, GMOD_ATTR_WRITES_GLOBAL, LuaInferCache, LuaSignatureId, LuaType, SemanticDeclGuard,
    SemanticDeclLevel, attribute_use_write_global_root,
    db_index::signature_writes_global_roots,
    semantic::{get_member_value_expr, infer_expr_semantic_decl},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GmodCallWriteEffect {
    Unknown,
    Globals(Vec<String>),
}

pub(crate) fn gmod_call_write_effect(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> GmodCallWriteEffect {
    let Some((signature_id, semantic_decl)) = call_effect_signature_and_decl(db, cache, call_expr)
    else {
        return GmodCallWriteEffect::Unknown;
    };
    let mut roots = Vec::new();
    if let Some(signature_roots) = signature_writes_global_roots(db, signature_id) {
        roots.extend(signature_roots);
    }
    if let Some(semantic_roots) = semantic_decl_writes_global_roots(db, semantic_decl) {
        roots.extend(semantic_roots);
    }
    if roots.is_empty() {
        GmodCallWriteEffect::Unknown
    } else {
        roots.sort();
        roots.dedup();
        GmodCallWriteEffect::Globals(roots)
    }
}

fn call_effect_signature_and_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<(LuaSignatureId, crate::LuaSemanticDeclId)> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    if let LuaExpr::NameExpr(name_expr) = &prefix_expr
        && let Some(signature_id) = get_local_name_signature_id(db, cache, name_expr)
    {
        return Some((
            signature_id,
            crate::LuaSemanticDeclId::Signature(signature_id),
        ));
    }

    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;
    Some((
        get_signature_id_from_semantic_decl_value_expr(db, semantic_decl.clone())?,
        semantic_decl,
    ))
}

fn semantic_decl_writes_global_roots(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<Vec<String>> {
    let property = db.get_property_index().get_property(&semantic_decl)?;
    let mut roots = Vec::new();
    for attribute_use in property.attribute_uses()?.iter() {
        if attribute_use.id.get_name() != GMOD_ATTR_WRITES_GLOBAL {
            continue;
        }
        roots.push(attribute_use_write_global_root(attribute_use)?);
    }
    if roots.is_empty() { None } else { Some(roots) }
}

fn get_local_name_signature_id(
    db: &DbIndex,
    cache: &LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaSignatureId> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
    let closure = LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)?;
    let LuaExpr::ClosureExpr(closure) = closure else {
        return None;
    };
    Some(LuaSignatureId::from_closure(decl.get_file_id(), &closure))
}

fn get_signature_id_from_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return Some(signature_id);
    }
    let file_id = match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
            decl_id.file_id
        }
        crate::LuaSemanticDeclId::Member(member_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
            member_id.file_id
        }
        crate::LuaSemanticDeclId::Signature(signature_id) => return Some(signature_id),
        crate::LuaSemanticDeclId::TypeDecl(_) => return None,
    };
    let LuaExpr::ClosureExpr(closure) = get_semantic_decl_value_expr(db, semantic_decl)? else {
        return None;
    };
    Some(LuaSignatureId::from_closure(file_id, &closure))
}

fn get_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<LuaExpr> {
    match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            let value_syntax_id = decl.get_value_syntax_id()?;
            let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
            LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)
        }
        crate::LuaSemanticDeclId::Member(member_id) => get_member_value_expr(db, member_id),
        crate::LuaSemanticDeclId::Signature(_) | crate::LuaSemanticDeclId::TypeDecl(_) => None,
    }
}
