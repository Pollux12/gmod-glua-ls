use glua_parser::{
    BinaryOperator, LuaAstNode, LuaExpr, LuaForStat, LuaIfStat, LuaIndexKey, LuaIndexMemberExpr,
    LuaNameExpr, LuaUnaryExpr, NumberResult, UnaryOperator,
};

use crate::{
    DbIndex, InferFailReason, LuaArrayLen, LuaArrayType, LuaInferCache, LuaType, TypeOps,
    infer_expr, semantic::infer::narrow::get_var_expr_var_ref_id,
};

pub fn infer_array_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    array_type: &LuaArrayType,
    index_member_expr: LuaIndexMemberExpr,
) -> Result<LuaType, InferFailReason> {
    let index_is_in_range = check_index_in_range(db, cache, &index_member_expr);
    if matches!(&index_member_expr, LuaIndexMemberExpr::TableField(_)) {
        return Ok(array_type.get_base().clone());
    }
    let key = index_member_expr
        .get_index_key()
        .ok_or(InferFailReason::None)?;

    match key {
        LuaIndexKey::Integer(i) => {
            if !db.get_emmyrc().strict.array_index {
                return Ok(array_type.get_base().clone());
            }

            let base_type = array_type.get_base();
            match array_type.get_len() {
                LuaArrayLen::None => {}
                LuaArrayLen::Max(max_len) => {
                    if let NumberResult::Int(index_value) = i.get_number_value() {
                        if index_value > 0 && index_value <= *max_len {
                            return Ok(base_type.clone());
                        }
                    }
                }
            }

            let result_type = match &base_type {
                LuaType::Any | LuaType::Unknown => base_type.clone(),
                _ => TypeOps::Union.apply(db, base_type, &LuaType::Nil),
            };

            Ok(result_type)
        }
        LuaIndexKey::Expr(expr) => {
            let expr_type = infer_expr(db, cache, expr.clone())?;
            // In Lua 5.1 / GLua there is no distinct integer type at runtime —
            // functions annotated as returning `number` are legitimate array indices.
            if expr_type.is_integer() || matches!(expr_type, LuaType::Number) {
                let base_type = array_type.get_base();
                match (array_type.get_len(), expr_type) {
                    (
                        LuaArrayLen::Max(max_len),
                        LuaType::IntegerConst(index_value) | LuaType::DocIntegerConst(index_value),
                    ) => {
                        if index_value > 0 && index_value <= *max_len {
                            return Ok(base_type.clone());
                        }
                    }
                    _ if index_is_in_range => {
                        return Ok(base_type.clone());
                    }
                    _ => {}
                }

                let result_type = match &base_type {
                    LuaType::Any | LuaType::Unknown => base_type.clone(),
                    _ => {
                        if db.get_emmyrc().strict.array_index {
                            TypeOps::Union.apply(db, base_type, &LuaType::Nil)
                        } else {
                            base_type.clone()
                        }
                    }
                };

                Ok(result_type)
            } else {
                Err(InferFailReason::FieldNotFound)
            }
        }
        _ => Err(InferFailReason::FieldNotFound),
    }
}

pub fn check_index_in_range(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_member_expr: &LuaIndexMemberExpr,
) -> bool {
    let Some(LuaIndexKey::Expr(index_expr)) = index_member_expr.get_index_key() else {
        return false;
    };
    let Some(prefix_expr) = index_member_expr.get_prefix_expr() else {
        return false;
    };

    if check_iter_var_range(db, cache, &index_expr, prefix_expr.clone()).unwrap_or(false) {
        return true;
    }

    let LuaExpr::NameExpr(index_name) = &index_expr else {
        return false;
    };
    if !numeric_for_index_has_positive_lower_bound(db, cache, index_name) {
        return false;
    }

    let Some(index_ref_id) = get_var_expr_var_ref_id(db, cache, index_expr) else {
        return false;
    };
    let Some(prefix_ref_id) = get_var_expr_var_ref_id(db, cache, prefix_expr) else {
        return false;
    };
    let index_range = index_member_expr.syntax().text_range();

    index_member_expr
        .syntax()
        .ancestors()
        .skip(1)
        .filter_map(LuaIfStat::cast)
        .any(|if_stat| {
            if !if_stat
                .get_block()
                .is_some_and(|block| block.syntax().text_range().contains_range(index_range))
            {
                return false;
            }

            if_stat.get_condition_expr().is_some_and(|condition| {
                condition_proves_index_at_most_len(
                    db,
                    cache,
                    condition,
                    &index_ref_id,
                    &prefix_ref_id,
                )
            })
        })
}

fn numeric_for_index_has_positive_lower_bound(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_name: &LuaNameExpr,
) -> bool {
    let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), index_name.get_range())
    else {
        return false;
    };
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    let decl_syntax_id = decl.get_syntax_id();
    if !decl_syntax_id.is_token() {
        return false;
    }

    let root = index_name.get_root();
    let Some(for_stat) = decl_syntax_id
        .to_token_from_root(&root)
        .and_then(|token| token.parent())
        .and_then(LuaForStat::cast)
    else {
        return false;
    };
    let iter_exprs = for_stat.get_iter_expr().collect::<Vec<_>>();
    if !(2..=3).contains(&iter_exprs.len())
        || !is_one_based_index_bound(db, cache, iter_exprs[0].clone()).unwrap_or(false)
    {
        return false;
    }

    iter_exprs.get(2).is_none_or(|step| {
        matches!(
            infer_expr(db, cache, step.clone()),
            Ok(LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value)) if value > 0
        )
    })
}

fn condition_proves_index_at_most_len(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    condition: LuaExpr,
    index_ref_id: &crate::semantic::infer::VarRefId,
    prefix_ref_id: &crate::semantic::infer::VarRefId,
) -> bool {
    let LuaExpr::BinaryExpr(binary) = condition else {
        return false;
    };
    let Some(op) = binary.get_op_token().map(|token| token.get_op()) else {
        return false;
    };
    let Some((left, right)) = binary.get_exprs() else {
        return false;
    };

    match op {
        BinaryOperator::OpLe => {
            expr_matches_ref(db, cache, left, index_ref_id)
                && len_expr_matches_ref(db, cache, right, prefix_ref_id)
        }
        BinaryOperator::OpGe => {
            len_expr_matches_ref(db, cache, left, prefix_ref_id)
                && expr_matches_ref(db, cache, right, index_ref_id)
        }
        _ => false,
    }
}

fn expr_matches_ref(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    expected: &crate::semantic::infer::VarRefId,
) -> bool {
    get_var_expr_var_ref_id(db, cache, expr).is_some_and(|actual| actual == *expected)
}

fn len_expr_matches_ref(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    expected: &crate::semantic::infer::VarRefId,
) -> bool {
    let LuaExpr::UnaryExpr(unary) = expr else {
        return false;
    };
    if !unary
        .get_op_token()
        .is_some_and(|token| token.get_op() == UnaryOperator::OpLen)
    {
        return false;
    }

    unary
        .get_expr()
        .and_then(|inner| get_var_expr_var_ref_id(db, cache, inner))
        .is_some_and(|actual| actual == *expected)
}

pub fn check_iter_var_range(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    may_iter_var: &LuaExpr,
    prefix_expr: LuaExpr,
) -> Option<bool> {
    match may_iter_var {
        LuaExpr::NameExpr(name_expr) => check_index_var_in_range(db, cache, name_expr, prefix_expr),
        LuaExpr::UnaryExpr(unary_expr) => check_is_len(db, cache, unary_expr, prefix_expr),
        _ => None,
    }
}

fn check_index_var_in_range(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    iter_var: &LuaNameExpr,
    prefix_expr: LuaExpr,
) -> Option<bool> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), iter_var.get_range())?;

    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let decl_syntax_id = decl.get_syntax_id();
    if !decl_syntax_id.is_token() {
        return None;
    }

    let root = prefix_expr.get_root();
    let token = decl_syntax_id.to_token_from_root(&root)?;
    let parent_node = token.parent()?;
    let for_stat = LuaForStat::cast(parent_node)?;
    let iter_exprs = for_stat.get_iter_expr().collect::<Vec<_>>();
    let test_len_expr = match iter_exprs.len() {
        2 => {
            if !is_one_based_index_bound(db, cache, iter_exprs[0].clone()).unwrap_or(false) {
                return None;
            }
            let LuaExpr::UnaryExpr(unary_expr) = iter_exprs[1].clone() else {
                return None;
            };
            unary_expr
        }
        3 => {
            let step_type = infer_expr(db, cache, iter_exprs[2].clone()).ok()?;
            let LuaType::IntegerConst(step_value) = step_type else {
                return None;
            };
            if step_value > 0 {
                if !is_one_based_index_bound(db, cache, iter_exprs[0].clone()).unwrap_or(false) {
                    return None;
                }
                let LuaExpr::UnaryExpr(unary_expr) = iter_exprs[1].clone() else {
                    return None;
                };
                unary_expr
            } else if step_value < 0 {
                if !is_one_based_index_bound(db, cache, iter_exprs[1].clone()).unwrap_or(false) {
                    return None;
                }
                let LuaExpr::UnaryExpr(unary_expr) = iter_exprs[0].clone() else {
                    return None;
                };
                unary_expr
            } else {
                return None;
            }
        }
        _ => return None,
    };

    let op = test_len_expr.get_op_token()?;
    if op.get_op() != UnaryOperator::OpLen {
        return None;
    }

    let len_expr = test_len_expr.get_expr()?;
    let len_expr_var_ref_id = get_var_expr_var_ref_id(db, cache, len_expr)?;
    let prefix_expr_var_ref_id = get_var_expr_var_ref_id(db, cache, prefix_expr)?;

    Some(len_expr_var_ref_id == prefix_expr_var_ref_id)
}

fn check_is_len(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    unary_expr: &LuaUnaryExpr,
    prefix_expr: LuaExpr,
) -> Option<bool> {
    let op = unary_expr.get_op_token()?;
    if op.get_op() != UnaryOperator::OpLen {
        return None;
    }

    let inner_var_expr = unary_expr.get_expr()?;
    let len_expr_var_ref_id = get_var_expr_var_ref_id(db, cache, inner_var_expr)?;
    let prefix_expr_var_ref_id = get_var_expr_var_ref_id(db, cache, prefix_expr)?;

    Some(len_expr_var_ref_id == prefix_expr_var_ref_id)
}

fn is_one_based_index_bound(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
) -> Option<bool> {
    let bound_type = infer_expr(db, cache, expr).ok()?;
    Some(matches!(
        bound_type,
        LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) if value >= 1
    ))
}
