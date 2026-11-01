use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaExpr, LuaIndexExpr, LuaLocalStat, LuaNameExpr,
    LuaSyntaxNode,
};

use crate::{
    CacheEntry, DbIndex, GlobalId, InferGuard, InferGuardRef, LuaDeclId, LuaInferCache, LuaType,
    TypeOps, check_type_compact,
    db_index::{LuaMemberIndexItem, LuaMemberKey, LuaMemberOwner, LuaTypeCache, LuaTypeDeclId},
    semantic::{
        generic::TypeSubstitutor,
        infer::{
            InferFailReason, InferResult,
            infer_index::{get_index_expr_var_ref_id, infer_index_expr},
            narrow::{infer_expr_narrow_type, remove_false_or_nil},
        },
        resolve_global_decl_id, unwrap_paren_to_name_expr,
    },
};

/// Checks if an empty table `{}` can satisfy the given type.
///
/// An empty table can satisfy a type only if the type has no required (non-optional) fields.
/// This includes checking all fields in the type and its parent classes.
///
/// Examples:
/// - `table` → true (plain table, no specific fields)
/// - `Opts` with `a?: string` → true (all fields optional)
/// - `MyClass` with `x: number` → false (has required field)
/// - `(Opts|MyClass)` → true (at least one type - Opts - can be satisfied)
fn can_empty_table_satisfy_type(db: &DbIndex, ty: &LuaType) -> bool {
    match ty {
        // Plain table types can always be satisfied by {}
        LuaType::Table | LuaType::TableConst(_) => true,

        // For class/ref types, check if all fields (including inherited) are optional
        LuaType::Ref(type_decl_id) => {
            // Collect this type and all its super types (includes inheritance)
            let all_types = type_decl_id.collect_super_types_with_self(db, ty.clone());

            // Check each type in the hierarchy for required fields
            for typ in all_types {
                if let LuaType::Ref(decl_id) = typ {
                    if has_required_fields(db, &decl_id) {
                        return false; // Found a required field somewhere in hierarchy
                    }
                }
            }

            true // No required fields found
        }

        // For unions, at least ONE type must be satisfiable by {}
        LuaType::Union(union_type) => {
            match union_type.nullable_inner() {
                // For Type?, check the inner type (nil is already removed)
                Some(inner) => can_empty_table_satisfy_type(db, inner),
                // At least one type in union must be satisfiable
                None => union_type
                    .types()
                    .any(|t| can_empty_table_satisfy_type(db, t)),
            }
        }

        // Other types (string, number, function, etc.) cannot be satisfied by empty table
        _ => false,
    }
}

/// Checks if a specific type declaration has any required (non-optional) fields.
/// Only checks direct members, not inherited ones (caller handles hierarchy).
fn has_required_fields(db: &DbIndex, type_decl_id: &LuaTypeDeclId) -> bool {
    let member_index = db.get_member_index();
    let type_index = db.get_type_index();

    // Get all direct members of this type
    let members = match member_index.get_members(&LuaMemberOwner::Type(type_decl_id.clone())) {
        Some(members) => members,
        None => return false, // No members = no required fields
    };

    // Check each member to see if it's required
    for member in members {
        let member_type = type_index
            .get_type_cache(&member.get_id().into())
            .unwrap_or(&LuaTypeCache::InferType(LuaType::Unknown))
            .as_type();

        // A field is required if it's NOT optional
        // is_optional() returns true for: nil, any, unknown, variadic, or unions containing these
        if !member_type.is_optional() {
            return true; // Found a required field
        }
    }

    false // No required fields in this type
}

pub fn try_bootstrap_or(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    left: &LuaExpr,
    right: &LuaExpr,
    err: &crate::semantic::InferFailReason,
) -> Option<LuaType> {
    if !err.is_need_resolve() {
        return None;
    }

    // Only the self-bootstrap `X = X or {}` is answered before its left read
    // resolves: that read is the slot being defined, so waiting on it would
    // wait forever. Any other `X or {}` is a read of `X`, and answering with
    // the fresh table alone commits the state of the index at the moment the
    // file was walked, so it waits for the read like any other operand.
    if !is_self_referential_bootstrap(db, cache, left) {
        return None;
    }

    if matches!(right, LuaExpr::TableExpr(table_expr) if table_expr.is_empty()) {
        return crate::semantic::infer_expr(db, cache, right.clone()).ok();
    }

    None
}

pub fn special_or_rule(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    left_type: &LuaType,
    right_type: &LuaType,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
) -> Option<LuaType> {
    if let LuaExpr::CallExpr(call_expr) = &right_expr
        && crate::calls_intrinsic_error(db, cache.get_file_id(), call_expr)
    {
        let narrowed = remove_false_or_nil(left_type.clone());
        return Some(narrowed);
    }

    // `c and a or b` with an always-truthy `a` yields `a` or `b` when `c` may go either way.
    if !left_type.is_always_truthy()
        && !left_type.is_always_falsy()
        && let LuaExpr::BinaryExpr(and_expr) = &left_expr
        && and_expr
            .get_op_token()
            .is_some_and(|token| token.get_op() == glua_parser::BinaryOperator::OpAnd)
        && let Some((_, truthy_expr)) = and_expr.get_exprs()
        && let Ok(truthy_type) = crate::infer_expr(db, cache, truthy_expr)
        && truthy_type.is_always_truthy()
    {
        return Some(TypeOps::Union.apply(db, &truthy_type, right_type));
    }

    if is_unresolved_global_unknown_name_expr(db, cache, &left_expr, left_type) {
        let effective_right =
            if is_unresolved_global_unknown_name_expr(db, cache, &right_expr, right_type) {
                LuaType::Nil
            } else {
                right_type.clone()
            };
        return Some(TypeOps::Union.apply(db, &LuaType::Nil, &effective_right));
    }

    if is_unresolved_global_unknown_name_expr(db, cache, &right_expr, right_type) {
        return Some(TypeOps::Union.apply(
            db,
            &remove_false_or_nil(left_type.clone()),
            &LuaType::Nil,
        ));
    }

    match right_expr {
        LuaExpr::TableExpr(table_expr) => {
            if table_expr.is_empty() {
                // A same-statement self-bootstrap keeps the fresh table owner.
                if is_self_referential_bootstrap(db, cache, &left_expr) {
                    return Some(right_type.clone());
                }

                // Opaque inputs remain possible unless all producers close this initializer cycle.
                let left_without_nil = remove_false_or_nil(left_type.clone());
                if left_without_nil.is_unknown()
                    && is_initializer_member_cycle(db, cache, &left_expr).unwrap_or(false)
                {
                    return Some(right_type.clone());
                }
                let opaque_left = (!left_type.is_always_falsy() && left_without_nil.is_unknown())
                    || (left_type.is_nil()
                        && is_missing_dynamic_member_read(db, cache, &left_expr));
                if opaque_left
                    && (member_read(&left_expr).is_some()
                        || unwrap_paren_to_name_expr(&left_expr)
                            .is_some_and(|name_expr| is_resolved_name_expr(db, cache, &name_expr)))
                {
                    return Some(TypeOps::Union.apply(db, &LuaType::Unknown, right_type));
                }

                // Bootstrap and falsy reads retain the fallback's concrete owner.
                if (left_type.is_unknown() || left_type.is_nil())
                    && (matches!(left_expr, LuaExpr::IndexExpr(_))
                        || is_unresolved_global_unknown_name_expr(db, cache, &left_expr, left_type))
                {
                    return Some(right_type.clone());
                }

                // Explicitly unknown locals remain dynamic after the fallback.
                let left_eval = if left_without_nil.is_unknown() {
                    LuaType::Any
                } else {
                    left_without_nil.clone()
                };
                if check_type_compact(db, &left_eval, &LuaType::Table).is_ok() {
                    if can_empty_table_satisfy_type(db, &left_without_nil) {
                        return Some(left_without_nil);
                    }
                }
            }
        }
        LuaExpr::LiteralExpr(_) => {
            match left_expr {
                LuaExpr::CallExpr(_) | LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_) => {}
                _ => return None,
            }

            // The answer below is the left arm's truthy half on its own, which
            // stands as the whole answer only while there is one: the
            // compatibility check has established the right arm adds nothing to
            // it. A left arm that is falsy throughout always evaluates to the
            // right arm, and its truthy half is empty, so answering with that
            // half drops the only operand the expression can return and hands
            // back a `nil` the source cannot produce. `and` already declines on
            // the same condition; leave this to the general rule, which returns
            // the right arm.
            if right_type.is_nil() || left_type.is_const() || left_type.is_always_falsy() {
                return None;
            }

            if check_type_compact(db, left_type, right_type).is_ok() {
                return Some(remove_false_or_nil(left_type.clone()));
            }
        }

        _ => {}
    }

    None
}

pub fn infer_binary_expr_or(db: &DbIndex, left: LuaType, right: LuaType) -> InferResult {
    if left.is_always_truthy() {
        return Ok(left);
    } else if left.is_always_falsy() {
        return Ok(right);
    }

    // Mirror of the `and` case: an unresolved left operand has no enumerable
    // truthy half, so it contributes nothing rather than widening to
    // `x|unknown`.
    let truthy_left = remove_false_or_nil(left);
    if truthy_left.is_unknown() {
        return Ok(right);
    }

    Ok(TypeOps::Union.apply(db, &truthy_left, &right))
}

/// True when `left_expr` is a field read that is also the target of the assignment
/// it feeds, e.g. `X.a[k] = X.a[k] or {}`. For a name target, only an unresolved
/// name qualifies: a resolved local, parameter, or declared global makes
/// `x = x or {}` a narrowing idiom over a declared type rather than a definition
/// of the table (unresolved globals qualify, matching the handling below).
pub(crate) fn is_self_referential_bootstrap(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    left_expr: &LuaExpr,
) -> bool {
    if !matches!(left_expr, LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_)) {
        return false;
    }

    let Some(binary_expr) = left_expr.get_parent::<LuaBinaryExpr>() else {
        return false;
    };
    let Some(assign_stat) = binary_expr.get_parent::<LuaAssignStat>() else {
        return false;
    };

    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let Some(index) = exprs
        .iter()
        .position(|expr| expr.get_range() == binary_expr.get_range())
    else {
        return false;
    };
    let Some(var) = vars.get(index) else {
        return false;
    };

    if significant_tokens(var.syntax()) != significant_tokens(left_expr.syntax()) {
        return false;
    }

    match left_expr {
        LuaExpr::IndexExpr(_) => true,
        LuaExpr::NameExpr(name_expr) => !is_resolved_name_expr(db, cache, name_expr),
        _ => false,
    }
}

fn member_read(left_expr: &LuaExpr) -> Option<LuaIndexExpr> {
    let mut operand = left_expr.clone();
    while let LuaExpr::ParenExpr(paren_expr) = &operand {
        operand = paren_expr.get_expr()?;
    }
    match operand {
        LuaExpr::IndexExpr(index) => Some(index),
        _ => None,
    }
}

fn is_missing_dynamic_member_read(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    left_expr: &LuaExpr,
) -> bool {
    let Some(index) = member_read(left_expr) else {
        return false;
    };
    let Some(prefix) = index.get_prefix_expr() else {
        return false;
    };
    let Ok(prefix_type) = crate::infer_expr(db, cache, prefix) else {
        return false;
    };
    if !receiver_may_have_dynamic_fields(db, &prefix_type, &InferGuard::new())
        || !matches!(
            infer_index_expr(db, cache, index.clone(), false),
            Err(InferFailReason::FieldNotFound)
        )
    {
        return false;
    }
    let Some(var_ref_id) = get_index_expr_var_ref_id(db, cache, &index) else {
        return true;
    };
    let previous_flow = cache.key_flow_cache(&var_ref_id, &LuaType::Unknown);
    let previous_origin =
        cache.replace_index_ref_origin_type_cache(&var_ref_id, CacheEntry::Cache(LuaType::Unknown));
    let result = infer_expr_narrow_type(db, cache, LuaExpr::IndexExpr(index), var_ref_id.clone());
    cache.restore_index_ref_origin_type_cache(&var_ref_id, previous_origin);
    cache.unkey_flow_cache(&var_ref_id, LuaType::Unknown, previous_flow);
    !matches!(result, Ok(typ) if typ.is_nil())
}

fn receiver_may_have_dynamic_fields(db: &DbIndex, typ: &LuaType, guard: &InferGuardRef) -> bool {
    let (type_id, substitutor) = match typ {
        LuaType::Ref(id) | LuaType::Def(id) => (id.clone(), None),
        LuaType::Generic(generic) => (
            generic.get_base_type_id(),
            Some(TypeSubstitutor::from_type_array(
                generic.get_params().clone(),
            )),
        ),
        LuaType::Instance(instance) => {
            return receiver_may_have_dynamic_fields(db, instance.get_base(), guard);
        }
        LuaType::TableOf(inner) => return receiver_may_have_dynamic_fields(db, inner, guard),
        LuaType::Union(union) => {
            return union
                .types()
                .any(|arm| receiver_may_have_dynamic_fields(db, arm, &guard.fork()));
        }
        _ => return false,
    };
    if guard.check(&type_id).is_err() {
        return false;
    }
    let Some(decl) = db.get_type_index().get_type_decl(&type_id) else {
        return false;
    };
    if decl.is_class() {
        return !decl.is_exact();
    }
    decl.get_alias_origin(db, substitutor.as_ref())
        .is_some_and(|origin| receiver_may_have_dynamic_fields(db, &origin, guard))
}

fn is_initializer_member_cycle(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    left_expr: &LuaExpr,
) -> Option<bool> {
    let binary_expr = left_expr.get_parent::<LuaBinaryExpr>()?;
    let local_stat = binary_expr.get_parent::<LuaLocalStat>()?;
    let index = local_stat
        .get_value_exprs()
        .position(|expr| expr.get_range() == binary_expr.get_range())?;
    let local_name = local_stat.get_local_name_list().nth(index)?;
    let file_id = cache.get_file_id();
    let decl_id = LuaDeclId::new(file_id, local_name.get_position());
    let references = db.get_reference_index().get_local_reference(&file_id)?;
    let mut operand = left_expr.clone();
    while let LuaExpr::ParenExpr(paren) = operand {
        operand = paren.get_expr()?;
    }
    let LuaExpr::IndexExpr(read) = &operand else {
        return None;
    };
    let prefix = read.get_prefix_expr()?;
    let members = db.get_member_index();
    let producers = if let Some(owner_path) =
        super::super::infer_index::global_expr_access_path(db, file_id, &prefix)
    {
        let path = super::super::infer_index::global_expr_access_path(db, file_id, &operand)?;
        LuaMemberIndexItem::Many(members.get_member_history_for_global_path(
            &LuaMemberOwner::GlobalPath(GlobalId::new(&owner_path)),
            &GlobalId::new(&path),
        ))
    } else {
        let owner = match crate::infer_expr(db, cache, prefix).ok()? {
            LuaType::TableConst(site) => LuaMemberOwner::Element(site),
            LuaType::Def(id) | LuaType::Ref(id) => LuaMemberOwner::Type(id),
            _ => return None,
        };
        let key = LuaMemberKey::from_index_key(db, cache, &read.get_index_key()?).ok()?;
        members.get_member_item(&owner, &key)?.clone()
    };
    let mut visible =
        producers.visible_member_ids_with_realm_at_offset(db, &file_id, read.get_position());
    if visible.is_empty() {
        // Inspect future producers only when the read has no visible writer.
        visible = producers.visible_member_ids_with_realm(db, &file_id);
    }
    if visible.is_empty() {
        return Some(false);
    }
    for member_id in visible {
        if member_id.file_id != file_id {
            return Some(false);
        }
        let root = db
            .get_vfs()
            .syntax_tree_around(&file_id, member_id.get_syntax_id().get_range())?
            .get_red_root();
        let node = member_id.get_syntax_id().to_node_from_root(&root)?;
        let assignment = node.ancestors().find_map(LuaAssignStat::cast)?;
        let (vars, exprs) = assignment.get_var_and_expr_list();
        let slot = vars
            .iter()
            .position(|var| var.get_range() == node.text_range())?;
        let name = unwrap_paren_to_name_expr(exprs.get(slot)?)?;
        if references.get_decl_id(&name.get_range()) != Some(decl_id)
            || assignment.get_position() < local_stat.get_range().end()
            || references
                .get_decl_references(&decl_id)
                .is_some_and(|refs| {
                    refs.cells.iter().any(|cell| {
                        cell.is_write
                            && cell.range.start() > decl_id.position
                            && cell.range.start() < assignment.get_position()
                    })
                })
        {
            return Some(false);
        }
    }
    Some(true)
}

/// True when the name has a local/param declaration or a declared global, i.e.
/// anything that makes the name a resolved read rather than an unresolved
/// bootstrap target. Mirrors `is_unresolved_global_unknown_name_expr`.
fn is_resolved_name_expr(db: &DbIndex, cache: &mut LuaInferCache, name_expr: &LuaNameExpr) -> bool {
    let file_id = cache.get_file_id();
    if db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .is_some()
    {
        return true;
    }

    let Some(name_text) = name_expr.get_name_text() else {
        return false;
    };

    if db
        .get_decl_index()
        .get_decl_tree(&file_id)
        .is_some_and(|tree| {
            tree.find_local_decl(name_text.as_str(), name_expr.get_position())
                .is_some()
        })
    {
        return true;
    }

    resolve_global_decl_id(db, cache, name_text.as_str(), Some(name_expr)).is_some()
}

fn significant_tokens(node: &LuaSyntaxNode) -> Vec<String> {
    node.descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !token.kind().to_token().is_trivia())
        .map(|token| token.text().to_string())
        .collect()
}

fn is_unresolved_global_unknown_name_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    if !expr_type.is_unknown() {
        return false;
    }

    let name_expr = unwrap_paren_to_name_expr(expr);
    let Some(name_expr) = name_expr else {
        return false;
    };

    let Some(name_text) = name_expr.get_name_text() else {
        return false;
    };

    let file_id = cache.get_file_id();
    let name_position = name_expr.get_position();
    if db
        .get_decl_index()
        .get_decl_tree(&file_id)
        .is_some_and(|tree| {
            tree.find_local_decl(name_text.as_str(), name_position)
                .is_some()
        })
    {
        return false;
    }

    resolve_global_decl_id(db, cache, name_text.as_str(), Some(&name_expr)).is_none()
}
