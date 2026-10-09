use glua_parser::{LuaAstNode, LuaExpr, LuaNameExpr};

use crate::{
    DbIndex, FileId, LuaDeclId, LuaSemanticDeclId, LuaSignatureId, LuaType,
    semantic::get_member_value_expr,
};

pub(crate) fn decl_value_expr(db: &DbIndex, decl_id: LuaDeclId) -> Option<LuaExpr> {
    let value_syntax_id = db
        .get_decl_index()
        .get_decl(&decl_id)?
        .get_value_syntax_id()?;
    LuaExpr::cast(
        db.get_vfs()
            .syntax_node_around(&decl_id.file_id, value_syntax_id)?,
    )
}

pub(crate) fn semantic_decl_value(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<(FileId, LuaExpr)> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            Some((decl_id.file_id, decl_value_expr(db, decl_id)?))
        }
        LuaSemanticDeclId::Member(member_id) => {
            Some((member_id.file_id, get_member_value_expr(db, member_id)?))
        }
        LuaSemanticDeclId::Signature(_) | LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

pub(crate) fn semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaExpr> {
    semantic_decl_value(db, semantic_decl).map(|(_, value_expr)| value_expr)
}

/// The initializer expression of a local and the return index it binds.
pub(crate) fn local_initializer_expr(db: &DbIndex, decl_id: LuaDeclId) -> Option<(usize, LuaExpr)> {
    let initializer = db.get_decl_index().get_decl(&decl_id)?.get_initializer()?;
    let node = db
        .get_vfs()
        .syntax_node_around(&decl_id.file_id, initializer.get_expr_syntax_id())?;
    Some((initializer.get_ret_idx(), LuaExpr::cast(node)?))
}

/// The value expression of the declaration `name_expr` refers to in `file_id`.
pub(crate) fn local_name_value(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<(FileId, LuaExpr)> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())?;
    Some((decl_id.file_id, decl_value_expr(db, decl_id)?))
}

pub(crate) fn local_name_closure_signature_id(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<LuaSignatureId> {
    let (decl_file_id, LuaExpr::ClosureExpr(closure)) = local_name_value(db, file_id, name_expr)?
    else {
        return None;
    };
    Some(LuaSignatureId::from_closure(decl_file_id, &closure))
}

/// The signature the declaration is already known to own or be typed as.
pub(crate) fn known_signature_id(
    db: &DbIndex,
    semantic_decl: &LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(semantic_decl) {
        return Some(signature_id);
    }
    let type_cache = match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            db.get_type_index().get_type_cache(&(*decl_id).into())
        }
        LuaSemanticDeclId::Member(member_id) => {
            db.get_type_index().get_type_cache(&(*member_id).into())
        }
        LuaSemanticDeclId::Signature(signature_id) => return Some(*signature_id),
        LuaSemanticDeclId::TypeDecl(_) => return None,
    };
    match type_cache?.as_type() {
        LuaType::Signature(signature_id) => Some(*signature_id),
        _ => None,
    }
}

/// The signature of the closure the declaration's value expression is.
pub(crate) fn semantic_decl_closure_signature_id(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    if let LuaSemanticDeclId::Signature(signature_id) = semantic_decl {
        return Some(signature_id);
    }
    let (file_id, LuaExpr::ClosureExpr(closure)) = semantic_decl_value(db, semantic_decl)? else {
        return None;
    };
    Some(LuaSignatureId::from_closure(file_id, &closure))
}

pub(crate) fn semantic_decl_signature_id(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    known_signature_id(db, &semantic_decl)
        .or_else(|| semantic_decl_closure_signature_id(db, semantic_decl))
}
