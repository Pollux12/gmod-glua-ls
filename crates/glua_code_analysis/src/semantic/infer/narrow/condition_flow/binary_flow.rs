use glua_parser::{
    BinaryOperator, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaChunk, LuaExpr, LuaIndexMemberExpr,
    LuaLiteralToken, UnaryOperator,
};
use rowan::TextSize;

use crate::{
    DbIndex, FlowNode, FlowTree, InferFailReason, InferGuard, LuaArrayLen, LuaArrayType,
    LuaInferCache, LuaType, TypeOps, infer_expr,
    semantic::infer::{
        VarRefId,
        infer_index::infer_member_by_member_key,
        narrow::{
            ResultTypeOrContinue,
            condition_flow::{
                InferConditionFlow, call_flow::get_type_at_call_expr, get_type_at_condition_flow,
            },
            get_single_antecedent,
            get_type_at_flow::get_type_at_flow,
            get_var_ref_type, narrow_down_type,
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
    semantic::type_check::is_sub_type_of,
};

#[allow(clippy::too_many_arguments)]
pub fn get_type_at_binary_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    binary_expr: LuaBinaryExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(op_token) = binary_expr.get_op_token() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some((left_expr, right_expr)) = binary_expr.get_exprs() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let dispatched = match op_token.get_op() {
        BinaryOperator::OpEq => try_get_at_eq_or_neq_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            left_expr.clone(),
            right_expr.clone(),
            condition_flow,
        ),
        BinaryOperator::OpNe => try_get_at_eq_or_neq_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            left_expr.clone(),
            right_expr.clone(),
            condition_flow.get_negated(),
        ),
        BinaryOperator::OpGt => try_get_at_gt_or_ge_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            left_expr.clone(),
            right_expr.clone(),
            condition_flow,
            true,
        ),
        BinaryOperator::OpGe => try_get_at_gt_or_ge_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            left_expr.clone(),
            right_expr.clone(),
            condition_flow,
            false,
        ),
        BinaryOperator::OpAnd => try_get_at_and_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            left_expr.clone(),
            right_expr.clone(),
            condition_flow,
        ),
        _ => Ok(ResultTypeOrContinue::Continue),
    }?;

    if matches!(dispatched, ResultTypeOrContinue::Result(_)) {
        return Ok(dispatched);
    }

    // Fallback: any binary expression that successfully evaluates implies its
    // index-typed operands have non-nil prefixes. For an undefined-global
    // prefix (Unknown base), keep the uncertainty as Unknown instead of
    // widening to Any after a pattern like `if tmysql.Version < 4.1 then`.
    // Comparison/equality ops that already matched a more-specific narrowing
    // return above; we only run this when dispatch produced Continue.
    //
    // Short-circuit operators (OpAnd / OpOr) need branch-aware gating: the
    // right operand is only guaranteed to evaluate in one branch, so widening
    // it in the other branch would falsely promote a possibly-nil prefix to
    // Any. The left operand always evaluates regardless of branch.
    let widen_right = match op_token.get_op() {
        BinaryOperator::OpAnd => matches!(condition_flow, InferConditionFlow::TrueCondition),
        BinaryOperator::OpOr => matches!(condition_flow, InferConditionFlow::FalseCondition),
        _ => true,
    };
    let operands: &[(&LuaExpr, bool)] = &[(&left_expr, true), (&right_expr, widen_right)];
    for (operand, eligible) in operands {
        if !*eligible {
            continue;
        }
        if let LuaExpr::IndexExpr(index_expr) = operand {
            if let Some(result) =
                try_unknown_prefix_widen(db, tree, cache, root, var_ref_id, flow_node, index_expr)?
            {
                return Ok(ResultTypeOrContinue::Result(result));
            }
        }
    }

    Ok(ResultTypeOrContinue::Continue)
}

/// If `index_expr`'s leftmost-name prefix matches `var_ref_id` and that var's
/// antecedent type is `Unknown`, return `Unknown`. Used as a fallback for binary
/// expressions where evaluating the index implies the prefix is non-nil.
#[allow(clippy::too_many_arguments)]
fn try_unknown_prefix_widen(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    index_expr: &glua_parser::LuaIndexExpr,
) -> Result<Option<LuaType>, InferFailReason> {
    let mut current = LuaExpr::IndexExpr(index_expr.clone());
    loop {
        match current {
            LuaExpr::IndexExpr(idx) => {
                let Some(next) = idx.get_prefix_expr() else {
                    return Ok(None);
                };
                current = next;
            }
            LuaExpr::NameExpr(_) => break,
            _ => return Ok(None),
        }
    }

    let Some(leftmost) = get_var_expr_var_ref_id(db, cache, current) else {
        return Ok(None);
    };
    if leftmost != *var_ref_id {
        return Ok(None);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let left_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    if matches!(left_type, LuaType::Unknown) {
        Ok(Some(LuaType::Any))
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn try_get_at_eq_or_neq_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let mut result_type = maybe_type_guard_binary(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        left_expr.clone(),
        right_expr.clone(),
        condition_flow,
    )?;
    if let ResultTypeOrContinue::Result(result_type) = result_type {
        return Ok(ResultTypeOrContinue::Result(result_type));
    }

    result_type = maybe_field_literal_eq_narrow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        left_expr.clone(),
        right_expr.clone(),
        condition_flow,
    )?;

    if let ResultTypeOrContinue::Result(result_type) = result_type {
        return Ok(ResultTypeOrContinue::Result(result_type));
    }

    result_type = maybe_type_name_literal_eq_narrow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        left_expr.clone(),
        right_expr.clone(),
        condition_flow,
    )?;

    if let ResultTypeOrContinue::Result(result_type) = result_type {
        return Ok(ResultTypeOrContinue::Result(result_type));
    }

    maybe_var_eq_narrow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        left_expr,
        right_expr,
        condition_flow,
    )
}

#[allow(clippy::too_many_arguments)]
fn try_get_at_gt_or_ge_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
    gt: bool,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    match left_expr {
        LuaExpr::UnaryExpr(unary_expr) => {
            let Some(op) = unary_expr.get_op_token() else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            match op.get_op() {
                UnaryOperator::OpLen => {}
                _ => return Ok(ResultTypeOrContinue::Continue),
            };

            let Some(expr) = unary_expr.get_expr() else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, expr) else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            if maybe_ref_id != *var_ref_id {
                // If the reference declaration ID does not match, we cannot narrow it
                return Ok(ResultTypeOrContinue::Continue);
            }

            let right_expr_type = infer_expr(db, cache, right_expr)?;
            let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            let antecedent_type =
                get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
            match (&antecedent_type, &right_expr_type) {
                (
                    LuaType::Array(array_type),
                    LuaType::IntegerConst(i) | LuaType::DocIntegerConst(i),
                ) => {
                    let add = if gt { 1 } else { 0 };
                    if condition_flow.is_true() {
                        let new_array_type = LuaArrayType::new(
                            array_type.get_base().clone(),
                            LuaArrayLen::Max(*i + add),
                        );
                        return Ok(ResultTypeOrContinue::Result(LuaType::Array(
                            new_array_type.into(),
                        )));
                    }
                }
                _ => return Ok(ResultTypeOrContinue::Continue),
            }

            Ok(ResultTypeOrContinue::Continue)
        }
        _ => Ok(ResultTypeOrContinue::Continue),
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_type_guard_binary(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let mut type_guard_expr: Option<LuaCallExpr> = None;
    let mut literal_string = String::new();
    if let LuaExpr::CallExpr(call_expr) = left_expr {
        if call_expr.is_type() {
            type_guard_expr = Some(call_expr);
            if let LuaExpr::LiteralExpr(literal_expr) = right_expr {
                match literal_expr.get_literal() {
                    Some(LuaLiteralToken::String(s)) => {
                        literal_string = s.get_value();
                    }
                    _ => return Ok(ResultTypeOrContinue::Continue),
                }
            }
        }
    } else if let LuaExpr::CallExpr(call_expr) = right_expr {
        if call_expr.is_type() {
            type_guard_expr = Some(call_expr);
            if let LuaExpr::LiteralExpr(literal_expr) = left_expr {
                match literal_expr.get_literal() {
                    Some(LuaLiteralToken::String(s)) => {
                        literal_string = s.get_value();
                    }
                    _ => return Ok(ResultTypeOrContinue::Continue),
                }
            }
        }
        // may ref a type value
    } else if let LuaExpr::NameExpr(name_expr) = left_expr
        && let LuaExpr::LiteralExpr(literal_expr) = right_expr
    {
        let Some(decl_id) = db
            .get_reference_index()
            .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
        else {
            return Ok(ResultTypeOrContinue::Continue);
        };

        let Some(expr_ptr) = tree.get_decl_ref_expr(&decl_id) else {
            return Ok(ResultTypeOrContinue::Continue);
        };

        let Some(expr) = expr_ptr.to_node(root) else {
            return Ok(ResultTypeOrContinue::Continue);
        };

        if let LuaExpr::CallExpr(call_expr) = expr {
            if call_expr.is_type() {
                type_guard_expr = Some(call_expr);
                match literal_expr.get_literal() {
                    Some(LuaLiteralToken::String(s)) => {
                        literal_string = s.get_value();
                    }
                    _ => return Ok(ResultTypeOrContinue::Continue),
                }
            }
        } else {
            return Ok(ResultTypeOrContinue::Continue);
        }
    }

    let Some(type_guard_expr) = type_guard_expr else {
        return Ok(ResultTypeOrContinue::Continue);
    };
    if literal_string.is_empty() {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let Some(arg_list) = type_guard_expr.get_args_list() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(arg) = arg_list.get_args().next() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(maybe_var_ref_id) = get_var_expr_var_ref_id(db, cache, arg) else {
        // If we cannot find a reference declaration ID, we cannot narrow it
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_var_ref_id != *var_ref_id {
        let guard_position = type_guard_expr.get_range().start();
        if !can_narrow_type_guard_through_decl_alias(
            db,
            cache,
            root,
            var_ref_id,
            &maybe_var_ref_id,
            guard_position,
        ) {
            return Ok(ResultTypeOrContinue::Continue);
        }
    }

    let anatecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, anatecedent_flow_id)?;

    let narrow = match literal_string.as_str() {
        "number" => LuaType::Number,
        "string" => LuaType::String,
        "boolean" => LuaType::Boolean,
        "table" => LuaType::Table,
        "function" => LuaType::Function,
        "thread" => LuaType::Thread,
        "userdata" => LuaType::Userdata,
        "nil" => LuaType::Nil,
        _ => {
            // If the type is not recognized, we cannot narrow it
            return Ok(ResultTypeOrContinue::Continue);
        }
    };

    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            narrow_down_type(db, antecedent_type.clone(), narrow.clone(), None).unwrap_or(narrow)
        }
        InferConditionFlow::FalseCondition => TypeOps::Remove.apply(db, &antecedent_type, &narrow),
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

fn can_narrow_type_guard_through_decl_alias(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    alias_var_ref_id: &VarRefId,
    guarded_var_ref_id: &VarRefId,
    guard_position: TextSize,
) -> bool {
    let (VarRefId::VarRef(alias_decl_id), VarRefId::VarRef(guarded_decl_id)) =
        (alias_var_ref_id, guarded_var_ref_id)
    else {
        return false;
    };

    let file_id = cache.get_file_id();
    if alias_decl_id.file_id != file_id || guarded_decl_id.file_id != file_id {
        return false;
    }

    let Some(alias_decl) = db.get_decl_index().get_decl(alias_decl_id) else {
        return false;
    };
    if !matches!(alias_decl.extra, crate::LuaDeclExtra::Local { .. }) {
        return false;
    }

    if alias_decl_id.position >= guard_position {
        return false;
    }

    let Some(value_syntax_id) = alias_decl.get_value_syntax_id() else {
        return false;
    };
    let Some(alias_expr_node) = value_syntax_id.to_node_from_root(root.syntax()) else {
        return false;
    };
    let Some(alias_expr) = LuaExpr::cast(alias_expr_node) else {
        return false;
    };
    if !matches!(alias_expr, LuaExpr::NameExpr(_)) {
        return false;
    }

    let Some(alias_source_var_ref_id) = get_var_expr_var_ref_id(db, cache, alias_expr) else {
        return false;
    };
    if alias_source_var_ref_id != *guarded_var_ref_id {
        return false;
    }

    if has_decl_write_in_open_interval(
        db,
        file_id,
        *alias_decl_id,
        alias_decl_id.position,
        guard_position,
    ) {
        return false;
    }

    if has_decl_write_in_open_interval(
        db,
        file_id,
        *guarded_decl_id,
        alias_decl_id.position,
        guard_position,
    ) {
        return false;
    }

    true
}

fn has_decl_write_in_open_interval(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: crate::LuaDeclId,
    start: TextSize,
    end: TextSize,
) -> bool {
    if start >= end {
        return false;
    }

    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .is_some_and(|decl_refs| {
            decl_refs
                .cells
                .iter()
                .any(|cell| cell.is_write && cell.range.start() > start && cell.range.start() < end)
        })
}

#[allow(clippy::too_many_arguments)]
fn maybe_var_eq_narrow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    // only check left as need narrow
    match left_expr {
        LuaExpr::NameExpr(left_name_expr) => {
            let Some(maybe_ref_id) =
                get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(left_name_expr.clone()))
            else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            if maybe_ref_id != *var_ref_id {
                // If the reference declaration ID does not match, we cannot narrow it
                return Ok(ResultTypeOrContinue::Continue);
            }

            let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            let left_type =
                get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
            let right_expr_type = infer_expr(db, cache, right_expr)?;

            let result_type = match condition_flow {
                InferConditionFlow::TrueCondition => {
                    // self 是特殊的, 我们删除其 nil 类型
                    if var_ref_id.is_self_ref() && !right_expr_type.is_nil() {
                        TypeOps::Remove.apply(db, &right_expr_type, &LuaType::Nil)
                    } else {
                        let left_maybe_type =
                            TypeOps::Intersect.apply(db, &left_type, &right_expr_type);

                        if left_maybe_type.is_never() {
                            left_type
                        } else {
                            left_maybe_type
                        }
                    }
                }
                InferConditionFlow::FalseCondition => {
                    TypeOps::Remove.apply(db, &left_type, &right_expr_type)
                }
            };
            Ok(ResultTypeOrContinue::Result(result_type))
        }
        LuaExpr::CallExpr(left_call_expr) => {
            if let LuaExpr::LiteralExpr(literal_expr) = right_expr {
                match literal_expr.get_literal() {
                    Some(LuaLiteralToken::Bool(b)) => {
                        let flow = if b.is_true() {
                            condition_flow
                        } else {
                            condition_flow.get_negated()
                        };

                        return get_type_at_call_expr(
                            db,
                            tree,
                            cache,
                            root,
                            var_ref_id,
                            flow_node,
                            left_call_expr,
                            flow,
                        );
                    }
                    _ => return Ok(ResultTypeOrContinue::Continue),
                }
            };

            Ok(ResultTypeOrContinue::Continue)
        }
        LuaExpr::IndexExpr(left_index_expr) => {
            let Some(maybe_ref_id) =
                get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(left_index_expr.clone()))
            else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            if maybe_ref_id != *var_ref_id {
                // If the reference declaration ID does not match, we cannot narrow it
                return Ok(ResultTypeOrContinue::Continue);
            }

            let right_expr_type = infer_expr(db, cache, right_expr)?;
            let result_type = match condition_flow {
                InferConditionFlow::TrueCondition => right_expr_type,
                InferConditionFlow::FalseCondition => {
                    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    let antecedent_type =
                        get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
                    TypeOps::Remove.apply(db, &antecedent_type, &right_expr_type)
                }
            };
            Ok(ResultTypeOrContinue::Result(result_type))
        }
        LuaExpr::UnaryExpr(unary_expr) => {
            let Some(op) = unary_expr.get_op_token() else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            match op.get_op() {
                UnaryOperator::OpLen => {}
                _ => return Ok(ResultTypeOrContinue::Continue),
            };

            let Some(expr) = unary_expr.get_expr() else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, expr) else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            if maybe_ref_id != *var_ref_id {
                // If the reference declaration ID does not match, we cannot narrow it
                return Ok(ResultTypeOrContinue::Continue);
            }

            let right_expr_type = infer_expr(db, cache, right_expr)?;
            let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            let antecedent_type =
                get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
            match (&antecedent_type, &right_expr_type) {
                (
                    LuaType::Array(array_type),
                    LuaType::IntegerConst(i) | LuaType::DocIntegerConst(i),
                ) => {
                    if condition_flow.is_true() {
                        let new_array_type =
                            LuaArrayType::new(array_type.get_base().clone(), LuaArrayLen::Max(*i));
                        return Ok(ResultTypeOrContinue::Result(LuaType::Array(
                            new_array_type.into(),
                        )));
                    }
                }
                _ => return Ok(ResultTypeOrContinue::Continue),
            }

            Ok(ResultTypeOrContinue::Continue)
        }
        _ => {
            // If the left expression is not a name or call expression, we cannot narrow it
            Ok(ResultTypeOrContinue::Continue)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_field_literal_eq_narrow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    // only check left as need narrow
    let syntax_id = left_expr.get_syntax_id();
    let (index_expr, literal_expr) = match (left_expr, right_expr) {
        (LuaExpr::IndexExpr(index_expr), LuaExpr::LiteralExpr(literal_expr)) => {
            (index_expr, literal_expr)
        }
        (LuaExpr::LiteralExpr(literal_expr), LuaExpr::IndexExpr(index_expr)) => {
            (index_expr, literal_expr)
        }
        _ => return Ok(ResultTypeOrContinue::Continue),
    };

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(maybe_var_ref_id) = get_var_expr_var_ref_id(db, cache, prefix_expr.clone()) else {
        // If we cannot find a reference declaration ID, we cannot narrow it
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_var_ref_id != *var_ref_id {
        if cache
            .narrow_by_literal_stop_position_cache
            .contains(&syntax_id)
            && var_ref_id.start_with(&maybe_var_ref_id)
        {
            return Ok(ResultTypeOrContinue::Result(get_var_ref_type(
                db, cache, var_ref_id,
            )?));
        }

        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let left_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    let LuaType::Union(union_type) = left_type else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    cache
        .narrow_by_literal_stop_position_cache
        .insert(syntax_id);

    let right_type = infer_expr(db, cache, LuaExpr::LiteralExpr(literal_expr))?;
    let index = LuaIndexMemberExpr::IndexExpr(index_expr);
    let mut opt_result = None;
    let mut union_types = union_type.into_vec();
    for (i, sub_type) in union_types.iter().enumerate() {
        let member_type = match infer_member_by_member_key(
            db,
            cache,
            sub_type,
            index.clone(),
            &InferGuard::new(),
        ) {
            Ok(member_type) => member_type,
            Err(_) => continue, // If we cannot infer the member type, skip this type
        };
        if const_type_eq(&member_type, &right_type) {
            // If the right type matches the member type, we can narrow it
            opt_result = Some(i);
        }
    }

    match condition_flow {
        InferConditionFlow::TrueCondition => {
            if let Some(i) = opt_result {
                return Ok(ResultTypeOrContinue::Result(union_types[i].clone()));
            }
        }
        InferConditionFlow::FalseCondition => {
            if let Some(i) = opt_result {
                union_types.remove(i);
                return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(union_types)));
            }
        }
    }

    Ok(ResultTypeOrContinue::Continue)
}

#[allow(clippy::too_many_arguments)]
fn maybe_type_name_literal_eq_narrow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let (call_expr, literal_expr) = match (left_expr, right_expr) {
        (LuaExpr::CallExpr(call_expr), LuaExpr::LiteralExpr(literal_expr)) => {
            (call_expr, literal_expr)
        }
        (LuaExpr::LiteralExpr(literal_expr), LuaExpr::CallExpr(call_expr)) => {
            (call_expr, literal_expr)
        }
        _ => return Ok(ResultTypeOrContinue::Continue),
    };

    let class_name = match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(s)) => s.get_value(),
        _ => return Ok(ResultTypeOrContinue::Continue),
    };

    let Some(target_type) = resolve_class_name_target_type(db, cache, &class_name) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(target_expr) = extract_type_name_guard_target_expr(&call_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(target_ref_id) = get_var_expr_var_ref_id(db, cache, target_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };
    if target_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let LuaType::Ref(target_type_id) = &target_type else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if !can_target_class_be_a_more_specific_narrowing(db, &antecedent_type, target_type_id) {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => target_type.clone(),
        InferConditionFlow::FalseCondition => {
            TypeOps::Remove.apply(db, &antecedent_type, &target_type)
        }
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

fn resolve_class_name_target_type(
    db: &DbIndex,
    cache: &LuaInferCache,
    class_name: &str,
) -> Option<LuaType> {
    let target_decl = db
        .get_type_index()
        .find_type_decl(cache.get_file_id(), class_name)?;

    target_decl
        .is_class()
        .then(|| LuaType::Ref(target_decl.get_id()))
}

fn can_target_class_be_a_more_specific_narrowing(
    db: &DbIndex,
    antecedent_type: &LuaType,
    target_type_id: &crate::LuaTypeDeclId,
) -> bool {
    antecedent_can_be_target_class(db, antecedent_type, target_type_id)
        && !antecedent_already_target_or_more_specific(db, antecedent_type, target_type_id)
}

fn antecedent_can_be_target_class(
    db: &DbIndex,
    antecedent_type: &LuaType,
    target_type_id: &crate::LuaTypeDeclId,
) -> bool {
    match antecedent_type {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            type_id == target_type_id || is_sub_type_of(db, target_type_id, type_id)
        }
        LuaType::Instance(instance_type) => {
            antecedent_can_be_target_class(db, instance_type.get_base(), target_type_id)
        }
        LuaType::Union(union_type) => union_type
            .into_vec()
            .iter()
            .any(|ty| antecedent_can_be_target_class(db, ty, target_type_id)),
        LuaType::MultiLineUnion(multi_line_union) => multi_line_union
            .get_unions()
            .iter()
            .any(|(ty, _)| antecedent_can_be_target_class(db, ty, target_type_id)),
        LuaType::Any | LuaType::Unknown => true,
        _ => false,
    }
}

fn antecedent_already_target_or_more_specific(
    db: &DbIndex,
    antecedent_type: &LuaType,
    target_type_id: &crate::LuaTypeDeclId,
) -> bool {
    match antecedent_type {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            type_id == target_type_id || is_sub_type_of(db, type_id, target_type_id)
        }
        LuaType::Instance(instance_type) => {
            antecedent_already_target_or_more_specific(db, instance_type.get_base(), target_type_id)
        }
        LuaType::Union(union_type) => union_type
            .into_vec()
            .iter()
            .all(|ty| antecedent_already_target_or_more_specific(db, ty, target_type_id)),
        LuaType::MultiLineUnion(multi_line_union) => multi_line_union
            .get_unions()
            .iter()
            .all(|(ty, _)| antecedent_already_target_or_more_specific(db, ty, target_type_id)),
        _ => false,
    }
}

fn extract_type_name_guard_target_expr(call_expr: &LuaCallExpr) -> Option<LuaExpr> {
    if !call_expr.is_colon_call() {
        return None;
    }

    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };

    let args = call_expr.get_args_list()?;
    if args.get_args().next().is_some() {
        return None;
    }

    index_expr.get_prefix_expr()
}

fn const_type_eq(left_type: &LuaType, right_type: &LuaType) -> bool {
    if left_type == right_type {
        return true;
    }

    match (left_type, right_type) {
        (
            LuaType::StringConst(l) | LuaType::DocStringConst(l),
            LuaType::StringConst(r) | LuaType::DocStringConst(r),
        ) => l == r,
        (LuaType::FloatConst(l), LuaType::FloatConst(r)) => l == r,
        (LuaType::BooleanConst(l), LuaType::BooleanConst(r)) => l == r,
        (
            LuaType::IntegerConst(l) | LuaType::DocIntegerConst(l),
            LuaType::IntegerConst(r) | LuaType::DocIntegerConst(r),
        ) => l == r,
        _ => false,
    }
}

/// Handle AND expressions (e.g., `if ctp and ctp.Disable then`)
/// In the true branch, both sides must be truthy, so we narrow each side
/// by removing false/nil from their types.
#[allow(clippy::too_many_arguments)]
fn try_get_at_and_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    left_expr: LuaExpr,
    right_expr: LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    // False branch of `a and b` is disjunctive: `!a OR (a AND !b)`.
    // The simple per-side narrowing below is only sound for the true branch.
    if !matches!(condition_flow, InferConditionFlow::TrueCondition) {
        return Ok(ResultTypeOrContinue::Continue);
    }

    // For AND expressions, we need to check if the current var_ref_id
    // appears in either the left or right side
    //
    // Example: `if ctp and ctp.Disable then`
    // - For `ctp`: left_expr is NameExpr("ctp") → narrow it
    // - For `ctp.Disable`: right_expr is IndexExpr("ctp", "Disable") → narrow `ctp`

    // Try to narrow based on left expression
    let left_result = get_type_at_condition_flow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        left_expr,
        condition_flow,
    )?;

    // Try to narrow based on right expression
    let right_result = get_type_at_condition_flow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        right_expr,
        condition_flow,
    )?;

    if !matches!(right_result, ResultTypeOrContinue::Continue) {
        return Ok(right_result);
    }

    Ok(left_result)
}
