use glua_parser::{LuaAstNode, LuaExpr};

use crate::{
    DbIndex, LuaInferCache, LuaType, LuaUnionType, TypeOps, check_type_compact,
    db_index::{LuaMemberOwner, LuaTypeCache, LuaTypeDeclId},
    semantic::{
        infer::{InferResult, narrow::remove_false_or_nil},
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
            match union_type.as_ref() {
                LuaUnionType::Nullable(inner) => {
                    // For Type?, check the inner type (nil is already removed)
                    can_empty_table_satisfy_type(db, inner)
                }
                LuaUnionType::Multi(types) => {
                    // At least one type in union must be satisfiable
                    types.iter().any(|t| can_empty_table_satisfy_type(db, t))
                }
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

pub fn special_or_rule(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    left_type: &LuaType,
    right_type: &LuaType,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
) -> Option<LuaType> {
    if let LuaExpr::CallExpr(call_expr) = &right_expr
        && call_expr.is_error()
    {
        return Some(remove_false_or_nil(left_type.clone()));
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
                // When left is Unknown (an unresolved global), `x or {}` should
                // produce `{} | nil` (i.e. `{}?`), not `Any`. Fall through to
                // the general `infer_binary_expr_or` which handles this correctly.
                if left_type.is_unknown() {
                    return None;
                }

                // Remove nil/false from left type and check if result is table-compatible
                let left_without_nil = remove_false_or_nil(left_type.clone());
                if check_type_compact(db, &left_without_nil, &LuaType::Table).is_ok() {
                    // Only narrow if empty table can actually satisfy the type
                    // (i.e., the type has no required fields)
                    if can_empty_table_satisfy_type(db, &left_without_nil) {
                        return Some(left_without_nil);
                    }
                    // Otherwise, fall through to regular OR logic which will create a union
                }
            }
        }
        LuaExpr::LiteralExpr(_) => {
            match left_expr {
                LuaExpr::CallExpr(_) | LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_) => {}
                _ => return None,
            }

            if right_type.is_nil() || left_type.is_const() {
                return None;
            }

            // When left is Unknown (an unresolved global), fall through to
            // general or logic which produces `nil | right_type` (nullable).
            if left_type.is_unknown() {
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

    Ok(TypeOps::Union.apply(db, &remove_false_or_nil(left), &right))
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
