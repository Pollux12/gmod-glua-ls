use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaIndexExpr, LuaLiteralToken,
    LuaNameExpr, LuaStat, LuaSyntaxKind,
};

use crate::{
    DbIndex, GlobalId, InferFailReason, LuaDeclId, LuaInferCache, LuaInstanceType, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaType, LuaTypeDeclId,
    compilation::analyzer::gmod::name_expr_resolves_to_scoped_authoring_table,
    semantic::{
        infer::find_self_semantic_decl_id,
        member::{get_buildin_type_map_type_id, resolve_dynamic_field_member},
        semantic_info::resolve_global_decl_id,
    },
};

use super::{
    SemanticDeclLevel, infer_expr, infer_token_semantic_decl, semantic_guard::SemanticDeclGuard,
};

type DeclResult = Result<Option<LuaSemanticDeclId>, InferFailReason>;

/// `None` and `FieldNotFound` mean "looked, nothing there" — a settled answer.
/// Every other reason means "cannot answer yet" and must reach the caller so the
/// work is retried instead of being frozen as a miss.
fn terminal(reason: InferFailReason) -> DeclResult {
    match reason {
        InferFailReason::None | InferFailReason::FieldNotFound => Ok(None),
        other => Err(other),
    }
}

/// Request-time entry point: a stale miss is harmless for hover/goto.
/// Analysis-time callers must use [`try_infer_expr_semantic_decl`] instead.
pub fn infer_expr_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    semantic_guard: SemanticDeclGuard,
    level: SemanticDeclLevel,
) -> Option<LuaSemanticDeclId> {
    try_infer_expr_semantic_decl(db, cache, expr, semantic_guard, level).unwrap_or_default()
}

pub fn try_infer_expr_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    semantic_guard: SemanticDeclGuard,
    level: SemanticDeclLevel,
) -> DeclResult {
    let file_id = cache.get_file_id();
    let maybe_decl_id = LuaDeclId::new(file_id, expr.get_position());
    if db.get_decl_index().get_decl(&maybe_decl_id).is_some() {
        return Ok(Some(LuaSemanticDeclId::LuaDecl(maybe_decl_id)));
    };

    match expr {
        LuaExpr::NameExpr(name_expr) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_name_expr_semantic_decl(db, cache, name_expr, next_guard, level)
        }
        LuaExpr::IndexExpr(index_expr) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            let (found, pending) =
                match infer_index_expr_semantic_decl(db, cache, index_expr.clone(), next_guard) {
                    Ok(found) => (found, None),
                    Err(reason) => (None, Some(reason)),
                };
            match found.or_else(|| fallback_index_expr_member_decl(db, file_id, &index_expr)) {
                Some(found) => Ok(Some(found)),
                None => pending.map_or(Ok(None), Err),
            }
        }
        LuaExpr::ClosureExpr(closure_expr) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_closure_expr_semantic_decl(db, cache, closure_expr, next_guard, level)
        }
        LuaExpr::CallExpr(call_expr) if call_expr.is_require() => {
            Ok(infer_require_module_semantic_decl(db, cache, call_expr))
        }
        _ => {
            let member_id = LuaMemberId::new(expr.get_syntax_id(), file_id);
            if db.get_member_index().get_member(&member_id).is_some() {
                return Ok(Some(LuaSemanticDeclId::Member(member_id)));
            };

            Ok(None)
        }
    }
}

fn fallback_index_expr_member_decl(
    db: &DbIndex,
    file_id: crate::FileId,
    index_expr: &LuaIndexExpr,
) -> Option<LuaSemanticDeclId> {
    let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
    db.get_member_index()
        .get_member(&member_id)
        .map(|_| LuaSemanticDeclId::Member(member_id))
}

fn infer_name_expr_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: LuaNameExpr,
    semantic_guard: SemanticDeclGuard,
    level: SemanticDeclLevel,
) -> DeclResult {
    let Some(name_token) = name_expr.get_name_token() else {
        return Ok(None);
    };
    let name = name_token.get_name_text().to_string();
    if name == "self" {
        return Ok(find_self_semantic_decl_id(db, cache, &name_expr));
    }

    if let Some(type_decl_id) =
        name_expr_resolves_to_scoped_authoring_table(db, cache.get_file_id(), &name_expr)
    {
        return Ok(Some(LuaSemanticDeclId::TypeDecl(type_decl_id)));
    }

    let Some(decl_id) = get_name_decl_id(db, cache, &name, name_expr.clone()) else {
        return Ok(None);
    };
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return Ok(None);
    };
    if semantic_guard.reached_limit() || level.reached_limit() {
        return Ok(Some(LuaSemanticDeclId::LuaDecl(decl_id)));
    }

    if let Some(value_expr_id) = decl.get_value_syntax_id() {
        match value_expr_id.get_kind() {
            LuaSyntaxKind::NameExpr | LuaSyntaxKind::IndexExpr => {
                let file_id = decl.get_file_id();
                let (Some(tree), Some(next_guard), Some(next_level)) = (
                    db.get_vfs().get_syntax_tree(&file_id),
                    semantic_guard.next_level(),
                    level.next_level(),
                ) else {
                    return Ok(None);
                };
                // second infer
                let Some(value_expr) = value_expr_id.to_node(tree).and_then(LuaExpr::cast) else {
                    return Ok(None);
                };
                let semantic_id = if file_id == cache.get_file_id() {
                    try_infer_expr_semantic_decl(db, cache, value_expr, next_guard, next_level)?
                } else {
                    let mut value_cache = LuaInferCache::new(file_id, cache.get_config().clone());
                    try_infer_expr_semantic_decl(
                        db,
                        &mut value_cache,
                        value_expr,
                        next_guard,
                        next_level,
                    )?
                };
                if let Some(semantic_id) = semantic_id {
                    return Ok(Some(semantic_id));
                }
            }
            LuaSyntaxKind::RequireCallExpr => {
                let file_id = decl.get_file_id();
                let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
                    return Ok(None);
                };
                let call_expr = value_expr_id.to_node(tree).and_then(LuaCallExpr::cast);
                if let Some(call_expr) = call_expr
                    && call_expr.is_require()
                    && let Some(semantic_id) =
                        infer_require_module_semantic_decl(db, cache, call_expr)
                {
                    return Ok(Some(semantic_id));
                }
            }
            _ => {}
        }
    }

    Ok(Some(LuaSemanticDeclId::LuaDecl(decl_id)))
}

fn infer_require_module_semantic_decl(
    db: &DbIndex,
    cache: &LuaInferCache,
    call_expr: LuaCallExpr,
) -> Option<LuaSemanticDeclId> {
    let first_arg = call_expr.get_args_list()?.get_args().next()?;
    let module_path = match first_arg {
        LuaExpr::LiteralExpr(literal_expr) => {
            let literal_token = literal_expr.get_literal()?;
            match literal_token {
                LuaLiteralToken::String(string_token) => string_token.get_value(),
                _ => return None,
            }
        }
        _ => return None,
    };

    let module_info = db
        .get_module_index()
        .find_module_for_file(&module_path, cache.get_file_id())?;
    module_info.semantic_id.clone()
}

fn get_name_decl_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name: &str,
    name_expr: LuaNameExpr,
) -> Option<LuaDeclId> {
    let file_id = cache.get_file_id();
    let references_index = db.get_reference_index();
    let range = name_expr.get_range();
    let local_ref = references_index.get_local_reference(&file_id)?;
    let decl_id = local_ref.get_decl_id(&range);

    if let Some(decl_id) = decl_id {
        let decl = db.get_decl_index().get_decl(&decl_id)?;
        if decl.is_local() {
            return Some(decl_id);
        }
    }

    resolve_global_decl_id(db, cache, name, Some(&name_expr))
}

fn infer_index_expr_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: LuaIndexExpr,
    semantic_guard: SemanticDeclGuard,
) -> DeclResult {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(None);
    };
    let prefix_type = match infer_expr(db, cache, prefix_expr) {
        Ok(typ) => typ,
        Err(reason) => return terminal(reason),
    };
    let Some(index_key) = index_expr.get_index_key() else {
        return Ok(None);
    };
    let member_key = match LuaMemberKey::from_index_key(db, cache, &index_key) {
        Ok(key) => key,
        Err(reason) => return terminal(reason),
    };
    let Some(next_guard) = semantic_guard.next_level() else {
        return Ok(None);
    };
    infer_member_semantic_decl_by_member_key(
        db,
        cache,
        &prefix_type,
        &member_key,
        Some(index_expr.get_position()),
        next_guard,
    )
}

fn infer_closure_expr_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    closure_expr: LuaClosureExpr,
    semantic_guard: SemanticDeclGuard,
    level: SemanticDeclLevel,
) -> DeclResult {
    let Some(parent) = closure_expr.get_parent::<LuaStat>() else {
        return Ok(None);
    };
    match parent {
        LuaStat::LocalFuncStat(local_func_stat) => {
            let Some(name_token) = local_func_stat
                .get_local_name()
                .and_then(|name| name.get_name_token())
            else {
                return Ok(None);
            };
            Ok(infer_token_semantic_decl(
                db,
                cache,
                name_token.syntax().clone(),
                level,
            ))
        }
        LuaStat::FuncStat(func_stat) => {
            let (Some(func_name), Some(next_guard)) =
                (func_stat.get_func_name(), semantic_guard.next_level())
            else {
                return Ok(None);
            };
            try_infer_expr_semantic_decl(db, cache, func_name.into(), next_guard, level)
        }
        _ => Ok(None),
    }
}

fn infer_member_semantic_decl_by_member_key(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
    member_access_position: Option<rowan::TextSize>,
    semantic_guard: SemanticDeclGuard,
) -> DeclResult {
    match &prefix_type {
        LuaType::TableConst(id) => {
            let owner = LuaMemberOwner::Element(id.clone());
            Ok(infer_table_member_semantic_decl(
                db,
                cache,
                owner,
                member_key,
                member_access_position,
            ))
        }
        LuaType::String | LuaType::Io | LuaType::StringConst(_) | LuaType::DocStringConst(_) => {
            let Some(decl_id) = get_buildin_type_map_type_id(prefix_type) else {
                return Ok(None);
            };
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_custom_type_member_semantic_decl(
                db,
                cache,
                decl_id,
                member_key,
                member_access_position,
                next_guard,
            )
        }
        LuaType::Ref(decl_id) | LuaType::Def(decl_id) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_custom_type_member_semantic_decl(
                db,
                cache,
                decl_id.clone(),
                member_key,
                member_access_position,
                next_guard,
            )
        }
        LuaType::Union(union_type) => infer_composite_member_semantic_info(
            db,
            cache,
            union_type.types(),
            member_key,
            member_access_position,
            semantic_guard,
        ),
        LuaType::Generic(generic_type) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_custom_type_member_semantic_decl(
                db,
                cache,
                generic_type.get_base_type_id(),
                member_key,
                member_access_position,
                next_guard,
            )
        }
        LuaType::Instance(inst) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_instance_member_semantic_decl_by_member_key(
                db,
                cache,
                inst,
                member_key,
                member_access_position,
                next_guard,
            )
        }
        LuaType::Global => Ok(infer_global_member_semantic_decl_by_member_key(
            db, cache, member_key,
        )),
        LuaType::ModuleRef(file_id) => {
            let Some(module_info) = db.get_module_index().get_module(*file_id) else {
                return Ok(None);
            };
            let (Some(export_type), Some(next_guard)) =
                (&module_info.export_type, semantic_guard.next_level())
            else {
                return Ok(None);
            };
            infer_member_semantic_decl_by_member_key(
                db,
                cache,
                export_type,
                member_key,
                member_access_position,
                next_guard,
            )
        }
        LuaType::Intersection(intersection_type) => infer_composite_member_semantic_info(
            db,
            cache,
            intersection_type.get_types(),
            member_key,
            member_access_position,
            semantic_guard,
        ),
        LuaType::MergedTable(merged_table) => infer_composite_member_semantic_info(
            db,
            cache,
            merged_table.get_types(),
            member_key,
            member_access_position,
            semantic_guard,
        ),
        LuaType::TableOf(inner) => {
            let Some(next_guard) = semantic_guard.next_level() else {
                return Ok(None);
            };
            infer_member_semantic_decl_by_member_key(
                db,
                cache,
                inner,
                member_key,
                member_access_position,
                next_guard,
            )
        }
        LuaType::Namespace(ns) => Ok(infer_namespace_member_semantic_decl(
            db,
            cache,
            ns,
            member_key,
            member_access_position,
        )),
        _ => Ok(None),
    }
}

fn infer_table_member_semantic_decl(
    db: &DbIndex,
    cache: &LuaInferCache,
    owner: LuaMemberOwner,
    member_key: &LuaMemberKey,
    member_access_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let member_item = db.get_member_index().get_member_item(&owner, member_key)?;
    match member_access_position {
        Some(position) => member_item.resolve_semantic_decl_with_realm_at_offset(
            db,
            &cache.get_file_id(),
            position,
        ),
        None => member_item.resolve_semantic_decl_with_realm(db, &cache.get_file_id()),
    }
}

fn infer_custom_type_member_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type_id: LuaTypeDeclId,
    member_key: &LuaMemberKey,
    member_access_position: Option<rowan::TextSize>,
    semantic_guard: SemanticDeclGuard,
) -> DeclResult {
    let type_index = db.get_type_index();
    let Some(type_decl) = type_index.get_type_decl(&prefix_type_id) else {
        return Ok(None);
    };
    if type_decl.is_alias() {
        let Some(next_guard) = semantic_guard.next_level() else {
            return Ok(None);
        };
        let origin_type = type_decl
            .get_alias_origin(db, None)
            .unwrap_or(LuaType::String);
        return infer_member_semantic_decl_by_member_key(
            db,
            cache,
            &origin_type,
            member_key,
            member_access_position,
            next_guard,
        );
    }

    let owner = LuaMemberOwner::Type(prefix_type_id.clone());
    if let Some(member_item) = db.get_member_index().get_member_item(&owner, member_key) {
        return Ok(match member_access_position {
            Some(position) => member_item.resolve_semantic_decl_with_realm_at_offset(
                db,
                &cache.get_file_id(),
                position,
            ),
            None => member_item.resolve_semantic_decl_with_realm(db, &cache.get_file_id()),
        });
    }
    let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new(prefix_type_id.get_name()));
    if let Some(member_item) = db
        .get_member_index()
        .get_member_item(&global_owner, member_key)
    {
        return Ok(match member_access_position {
            Some(position) => member_item.resolve_semantic_decl_with_realm_at_offset(
                db,
                &cache.get_file_id(),
                position,
            ),
            None => member_item.resolve_semantic_decl_with_realm(db, &cache.get_file_id()),
        });
    }

    // An unresolved lookup must not short-circuit the remaining ones: a super
    // type may still hold the answer. Remember the reason and report it only if
    // nothing is found, so a real hit stays a hit and a real miss stays retryable.
    let mut pending = None;
    match resolve_dynamic_field_member(
        db,
        cache,
        &LuaType::Ref(prefix_type_id.clone()),
        member_key,
        member_access_position,
    ) {
        Ok(Some(dynamic_field)) => return Ok(dynamic_field.semantic_decl),
        Ok(None) => {}
        Err(reason) => pending = Some(reason),
    }

    if type_decl.is_class() {
        let Some(super_types) = type_index.get_super_types(&prefix_type_id) else {
            return pending.map_or(Ok(None), Err);
        };
        for super_type in super_types {
            let Some(next_guard) = semantic_guard.next_level() else {
                return pending.map_or(Ok(None), Err);
            };
            match infer_member_semantic_decl_by_member_key(
                db,
                cache,
                &super_type,
                member_key,
                member_access_position,
                next_guard,
            ) {
                Ok(Some(property)) => return Ok(Some(property)),
                Ok(None) => {}
                Err(reason) => pending = pending.or(Some(reason)),
            }
        }
    }

    pending.map_or(Ok(None), Err)
}

/// The first arm of a composite type that owns `member_key`.
///
/// The guard is forked per arm so a long miss streak (NULL / unrelated Entity
/// subclasses) cannot exhaust the depth budget and abort the whole scan.
fn infer_composite_member_semantic_info<'a>(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arms: impl IntoIterator<Item = &'a LuaType>,
    member_key: &LuaMemberKey,
    member_access_position: Option<rowan::TextSize>,
    semantic_guard: SemanticDeclGuard,
) -> DeclResult {
    let mut pending = None;
    for typ in arms {
        let Some(arm_guard) = semantic_guard.next_level() else {
            break;
        };
        match infer_member_semantic_decl_by_member_key(
            db,
            cache,
            typ,
            member_key,
            member_access_position,
            arm_guard,
        ) {
            Ok(Some(property_owner_id)) => return Ok(Some(property_owner_id)),
            Ok(None) => {}
            Err(reason) => pending = pending.or(Some(reason)),
        }
    }

    pending.map_or(Ok(None), Err)
}

fn infer_instance_member_semantic_decl_by_member_key(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    inst: &LuaInstanceType,
    member_key: &LuaMemberKey,
    member_access_position: Option<rowan::TextSize>,
    semantic_guard: SemanticDeclGuard,
) -> DeclResult {
    let range = inst.get_range();

    let origin_type = inst.get_base();
    let Some(next_guard) = semantic_guard.next_level() else {
        return Ok(None);
    };
    let pending = match infer_member_semantic_decl_by_member_key(
        db,
        cache,
        origin_type,
        member_key,
        member_access_position,
        next_guard,
    ) {
        Ok(Some(result)) => return Ok(Some(result)),
        Ok(None) => None,
        Err(reason) => Some(reason),
    };

    let owner = LuaMemberOwner::Element(range.clone());
    match infer_table_member_semantic_decl(db, cache, owner, member_key, member_access_position) {
        Some(result) => Ok(Some(result)),
        None => pending.map_or(Ok(None), Err),
    }
}

fn infer_global_member_semantic_decl_by_member_key(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_key: &LuaMemberKey,
) -> Option<LuaSemanticDeclId> {
    let name = member_key.get_name()?;
    resolve_global_decl_id(db, cache, name, None).map(LuaSemanticDeclId::LuaDecl)
}

fn infer_namespace_member_semantic_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    ns: &str,
    member_key: &LuaMemberKey,
    member_access_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let owner = LuaMemberOwner::GlobalPath(GlobalId::new(ns));
    infer_table_member_semantic_decl(db, cache, owner, member_key, member_access_position)
}
