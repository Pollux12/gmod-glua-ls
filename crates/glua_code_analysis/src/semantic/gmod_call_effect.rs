use glua_parser::{LuaCallExpr, LuaExpr};

use crate::{
    DbIndex, GMOD_ATTR_WRITES_GLOBAL, LuaInferCache, LuaSignatureId, SemanticDeclGuard,
    SemanticDeclLevel, attribute_use_write_global_root,
    db_index::signature_writes_global_roots,
    semantic::{
        infer_expr_semantic_decl, local_name_closure_signature_id, semantic_decl_signature_id,
    },
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
        && let Some(signature_id) =
            local_name_closure_signature_id(db, cache.get_file_id(), name_expr)
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
        semantic_decl_signature_id(db, semantic_decl.clone())?,
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
