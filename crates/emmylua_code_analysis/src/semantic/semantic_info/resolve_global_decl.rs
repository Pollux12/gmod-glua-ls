use std::sync::Arc;

use emmylua_parser::{LuaAstNode, LuaCallExpr, LuaNameExpr};

use crate::{
    DbIndex, GmodRealm, LuaDeclId, LuaInferCache, LuaType,
    semantic::overload_resolve::resolve_signature,
};

pub fn resolve_global_decl_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name: &str,
    name_expr: Option<&LuaNameExpr>,
) -> Option<LuaDeclId> {
    let module_index = db.get_module_index();
    let global_index = db.get_global_index();
    let priority_tiers = if let Some(current_workspace_id) =
        module_index.get_workspace_id(cache.get_file_id())
    {
        global_index.get_global_decl_id_priority_tiers(name, module_index, current_workspace_id)?
    } else {
        vec![(0, global_index.get_global_decl_ids(name)?.clone())]
    };

    let mut candidate_decl_ids = Vec::new();
    for (_, tier_decl_ids) in &priority_tiers {
        let tier_candidates = select_realm_compatible_decl_ids(db, cache, tier_decl_ids, name_expr);
        if !tier_candidates.is_empty() {
            candidate_decl_ids = tier_candidates;
            break;
        }
    }

    if candidate_decl_ids.is_empty()
        && let Some((_, decl_ids)) = priority_tiers.first()
    {
        candidate_decl_ids = decl_ids.clone();
    }

    if candidate_decl_ids.is_empty() {
        return None;
    }

    if candidate_decl_ids.len() == 1 {
        return candidate_decl_ids.first().copied();
    }

    if let Some(name_expr) = name_expr
        && let Some(call_expr) = name_expr.get_parent::<LuaCallExpr>()
    {
        return resolve_global_func_decl_id(db, cache, &candidate_decl_ids, call_expr);
    }

    let mut last_valid_decl_id = None;
    for decl_id in &candidate_decl_ids {
        let decl_type_cache = db.get_type_index().get_type_cache(&(*decl_id).into());
        if let Some(type_cache) = decl_type_cache {
            let typ = type_cache.as_type();
            if typ.is_def() || typ.is_ref() || typ.is_function() {
                return Some(*decl_id);
            }

            if type_cache.is_table() {
                last_valid_decl_id = Some(decl_id)
            }
        }
    }
    if last_valid_decl_id.is_none() && !candidate_decl_ids.is_empty() {
        return candidate_decl_ids.first().copied();
    }

    last_valid_decl_id.cloned()
}

fn resolve_global_func_decl_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl_ids: &[LuaDeclId],
    call_expr: LuaCallExpr,
) -> Option<LuaDeclId> {
    let mut overload_signature = vec![];
    for decl_id in decl_ids {
        let decl_type_cache = db.get_type_index().get_type_cache(&(*decl_id).into());
        if let Some(type_cache) = decl_type_cache {
            let typ = type_cache.as_type();
            if typ.is_def() || typ.is_ref() || typ.is_table() {
                return Some(*decl_id);
            }

            if let LuaType::Signature(signature) = typ {
                let signature = db.get_signature_index().get(signature)?;
                overload_signature.push((decl_id.clone(), signature.to_doc_func_type()));
            }
        }
    }

    let signature = resolve_signature(
        db,
        cache,
        overload_signature
            .iter()
            .map(|(_, doc_func)| doc_func.clone())
            .collect(),
        call_expr,
        false,
        None,
    );

    if let Ok(signature) = signature {
        for (decl_id, doc_func) in &overload_signature {
            if Arc::ptr_eq(&signature, doc_func) {
                return Some(decl_id.clone());
            }
        }
    }

    overload_signature.first().map(|(id, _)| id.clone())
}

fn select_realm_compatible_decl_ids(
    db: &DbIndex,
    cache: &LuaInferCache,
    decl_ids: &[LuaDeclId],
    name_expr: Option<&LuaNameExpr>,
) -> Vec<LuaDeclId> {
    if !db.get_emmyrc().gmod.enabled {
        return decl_ids.to_vec();
    }

    let Some(name_expr) = name_expr else {
        return decl_ids.to_vec();
    };

    let file_id = cache.get_file_id();
    let call_offset = name_expr.get_position();
    let infer_index = db.get_gmod_infer_index();
    let call_realm = infer_index.get_realm_at_offset(&file_id, call_offset);

    let mut compatible = Vec::new();
    for decl_id in decl_ids {
        let decl_realm = infer_index.get_realm_at_offset(&decl_id.file_id, decl_id.position);
        if is_realm_compatible(call_realm, decl_realm) {
            compatible.push(*decl_id);
        }
    }

    compatible
}

fn is_realm_compatible(call_realm: GmodRealm, decl_realm: GmodRealm) -> bool {
    !matches!(
        (call_realm, decl_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}
