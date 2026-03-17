use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaChunk, LuaExpr, LuaIndexExpr, LuaIndexKey,
    LuaLiteralToken, LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};

use crate::{
    CacheEntry, DbIndex, FlowId, FlowNode, FlowNodeKind, FlowTree, InferFailReason, LuaArrayType,
    LuaDeclId, LuaInferCache, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaSignatureId, LuaType,
    TypeOps, infer_expr,
    semantic::infer::{
        InferResult, VarRefId, infer_expr_list_value_type_at,
        narrow::{
            ResultTypeOrContinue,
            condition_flow::{InferConditionFlow, get_type_at_condition_flow},
            get_multi_antecedents, get_single_antecedent,
            get_type_at_cast_flow::get_type_at_cast_flow,
            get_var_ref_type, narrow_down_type,
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

pub fn get_type_at_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
) -> InferResult {
    let key = (var_ref_id.clone(), flow_id);
    if let Some(cache_entry) = cache.flow_node_cache.get(&key)
        && let CacheEntry::Cache(narrow_type) = cache_entry
    {
        return Ok(narrow_type.clone());
    }

    let result_type;
    let mut antecedent_flow_id = flow_id;
    loop {
        let flow_node = tree
            .get_flow_node(antecedent_flow_id)
            .ok_or(InferFailReason::None)?;

        match &flow_node.kind {
            FlowNodeKind::Start | FlowNodeKind::Unreachable => {
                result_type = get_var_ref_type(db, cache, var_ref_id)?;
                break;
            }
            FlowNodeKind::LoopLabel | FlowNodeKind::Break | FlowNodeKind::Return => {
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            }
            FlowNodeKind::BranchLabel | FlowNodeKind::NamedLabel(_) => {
                let multi_antecedents = get_multi_antecedents(tree, flow_node)?;

                let mut branch_result_type = LuaType::Unknown;
                for &flow_id in &multi_antecedents {
                    let branch_type = get_type_at_flow(db, tree, cache, root, var_ref_id, flow_id)?;
                    branch_result_type =
                        TypeOps::Union.apply(db, &branch_result_type, &branch_type);
                }
                result_type = branch_result_type;
                break;
            }
            FlowNodeKind::DeclPosition(position) => {
                if *position <= var_ref_id.get_position() {
                    match get_var_ref_type(db, cache, var_ref_id) {
                        Ok(var_type) => {
                            result_type = var_type;
                            break;
                        }
                        Err(err) => {
                            // 尝试推断声明位置的类型, 如果发生错误则返回初始错误, 否则返回当前推断错误
                            if let Some(init_type) =
                                try_infer_decl_initializer_type(db, cache, root, var_ref_id)?
                            {
                                result_type = init_type;
                                break;
                            }

                            return Err(err);
                        }
                    }
                } else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::Assignment(assign_ptr) => {
                let assign_stat = assign_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let result_or_continue = get_type_at_assign_stat(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    assign_stat,
                )?;

                if let ResultTypeOrContinue::Result(assign_type) = result_or_continue {
                    result_type = assign_type;
                    break;
                } else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::ImplFunc(func_ptr) => {
                let func_stat = func_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let Some(func_name) = func_stat.get_func_name() else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                };

                let Some(ref_id) = get_var_expr_var_ref_id(db, cache, func_name.to_expr()) else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                };

                if ref_id == *var_ref_id {
                    // Only use the func-stat's signature when the member isn't
                    // already declared (origin type is Nil). For Def types with
                    // @field annotations, let the flow continue so the declared
                    // type is preserved instead of being overridden by the
                    // implementation signature.
                    let is_undeclared = cache
                        .index_ref_origin_type_cache
                        .get(var_ref_id)
                        .is_some_and(|entry| matches!(entry, CacheEntry::Cache(t) if t.is_nil()));

                    if is_undeclared {
                        let Some(closure) = func_stat.get_closure() else {
                            return Err(InferFailReason::None);
                        };

                        result_type = LuaType::Signature(LuaSignatureId::from_closure(
                            cache.get_file_id(),
                            &closure,
                        ));
                        break;
                    }

                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                } else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::TrueCondition(condition_ptr) => {
                let condition = condition_ptr.to_node(root).ok_or(InferFailReason::None)?;
                // Errors in condition evaluation (e.g. method not found) must not
                // propagate and corrupt the type of unrelated variables.  Treat them
                // as "condition cannot be used for narrowing" and fall through.
                let result_or_continue = match get_type_at_condition_flow(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    condition,
                    InferConditionFlow::TrueCondition,
                ) {
                    Ok(r) => r,
                    Err(_) => ResultTypeOrContinue::Continue,
                };

                if let ResultTypeOrContinue::Result(condition_type) = result_or_continue {
                    result_type = condition_type;
                    break;
                } else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::FalseCondition(condition_ptr) => {
                let condition = condition_ptr.to_node(root).ok_or(InferFailReason::None)?;
                // Same defensive handling as TrueCondition above.
                let result_or_continue = match get_type_at_condition_flow(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    condition,
                    InferConditionFlow::FalseCondition,
                ) {
                    Ok(r) => r,
                    Err(_) => ResultTypeOrContinue::Continue,
                };

                if let ResultTypeOrContinue::Result(condition_type) = result_or_continue {
                    result_type = condition_type;
                    break;
                } else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::ForIStat(_) => {
                // todo check for `for i = 1, 10 do end`
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            }
            FlowNodeKind::TagCast(cast_ast_ptr) => {
                let tag_cast = cast_ast_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let cast_or_continue =
                    get_type_at_cast_flow(db, tree, cache, root, var_ref_id, flow_node, tag_cast)?;

                if let ResultTypeOrContinue::Result(cast_type) = cast_or_continue {
                    result_type = cast_type;
                    break;
                } else {
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
        }
    }

    cache
        .flow_node_cache
        .insert(key, CacheEntry::Cache(result_type.clone()));
    Ok(result_type)
}

fn get_type_at_assign_stat(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    assign_stat: LuaAssignStat,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    for (i, var) in vars.iter().cloned().enumerate() {
        if let Some(prefix_collection_type) = maybe_get_collection_append_assignment_type(
            db, tree, cache, root, var_ref_id, flow_node, &var, &exprs, i,
        )? {
            return Ok(ResultTypeOrContinue::Result(prefix_collection_type));
        }

        let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, var.to_expr()) else {
            continue;
        };

        if maybe_ref_id != *var_ref_id {
            // let typ = get_var_ref_type(db, cache, var_ref_id)?;
            continue;
        }

        // Check if there's an explicit type annotation (not just inferred type)
        let var_id = match var {
            LuaVarExpr::NameExpr(name_expr) => {
                Some(LuaDeclId::new(cache.get_file_id(), name_expr.get_position()).into())
            }
            LuaVarExpr::IndexExpr(index_expr) => {
                Some(LuaMemberId::new(index_expr.get_syntax_id(), cache.get_file_id()).into())
            }
        };

        let explicit_var_type = var_id
            .and_then(|id| db.get_type_index().get_type_cache(&id))
            .filter(|tc| tc.is_doc())
            .map(|tc| tc.as_type().clone());

        let expr_type = infer_expr_list_value_type_at(db, cache, &exprs, i)?;
        let Some(expr_type) = expr_type else {
            return Ok(ResultTypeOrContinue::Continue);
        };

        let source_type = if let Some(explicit) = explicit_var_type.clone() {
            explicit
        } else {
            let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?
        };

        let narrowed = if source_type == LuaType::Nil {
            None
        } else {
            let declared =
                get_var_ref_type(db, cache, var_ref_id)
                    .ok()
                    .and_then(|decl| match decl {
                        LuaType::Def(_) | LuaType::Ref(_) => Some(decl),
                        _ => None,
                    });

            narrow_down_type(db, source_type.clone(), expr_type.clone(), declared)
        };

        let result_type = narrowed.unwrap_or(explicit_var_type.unwrap_or(expr_type));

        return Ok(ResultTypeOrContinue::Result(result_type));
    }

    Ok(ResultTypeOrContinue::Continue)
}

#[allow(clippy::too_many_arguments)]
fn maybe_get_collection_append_assignment_type(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    var: &LuaVarExpr,
    exprs: &[LuaExpr],
    idx: usize,
) -> Result<Option<LuaType>, InferFailReason> {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return Ok(None);
    };
    if !is_collection_append_write(index_expr) {
        return Ok(None);
    }

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(None);
    };
    let LuaExpr::IndexExpr(prefix_index_expr) = prefix_expr else {
        return Ok(None);
    };
    let Some(prefix_var_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(prefix_index_expr.clone()))
    else {
        return Ok(None);
    };
    if prefix_var_ref_id != *var_ref_id {
        return Ok(None);
    }
    if !is_inferred_member_collection_expr(db, cache, &prefix_index_expr)? {
        return Ok(None);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let source_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    let Some(source_base) = infer_collection_base_type(db, &source_type) else {
        return Ok(None);
    };

    let value_type = infer_expr_list_value_type_at(db, cache, exprs, idx)?;
    let Some(value_type) = value_type else {
        return Ok(None);
    };

    let widened_base = TypeOps::Union.apply(db, &source_base, &value_type);
    Ok(Some(LuaType::Array(
        LuaArrayType::from_base_type(widened_base).into(),
    )))
}

fn is_collection_append_write(index_expr: &LuaIndexExpr) -> bool {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Some(LuaIndexKey::Expr(index_key_expr)) = index_expr.get_index_key() else {
        return false;
    };
    let LuaExpr::BinaryExpr(binary_expr) = index_key_expr else {
        return false;
    };
    if binary_expr
        .get_op_token()
        .is_none_or(|token| token.get_op() != BinaryOperator::OpAdd)
    {
        return false;
    }

    let Some((left, right)) = binary_expr.get_exprs() else {
        return false;
    };
    if !is_literal_integer_one(&right) {
        return false;
    }

    let LuaExpr::UnaryExpr(unary_expr) = left else {
        return false;
    };
    if unary_expr
        .get_op_token()
        .is_none_or(|token| token.get_op() != UnaryOperator::OpLen)
    {
        return false;
    }

    let Some(len_expr) = unary_expr.get_expr() else {
        return false;
    };
    expr_access_path(&prefix_expr) == expr_access_path(&len_expr)
}

fn is_inferred_member_collection_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> Result<bool, InferFailReason> {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(false);
    };
    let prefix_type = infer_expr(db, cache, prefix_expr)?;
    let Some(owner) = get_member_owner_for_prefix_type(prefix_type) else {
        return Ok(false);
    };
    let Some(index_key) = index_expr.get_index_key() else {
        return Ok(false);
    };
    let member_key = LuaMemberKey::from_index_key(db, cache, &index_key)?;
    let members = db
        .get_member_index()
        .get_members_for_owner_key(&owner, &member_key);
    if members.is_empty() {
        return Ok(false);
    }

    let mut saw_collection = false;
    for member in members {
        let Some(type_cache) = db.get_type_index().get_type_cache(&member.get_id().into()) else {
            continue;
        };
        if !type_cache.is_infer() {
            return Ok(false);
        }
        if normalize_infer_collection_type(type_cache.as_type()).is_none() {
            return Ok(false);
        }
        saw_collection = true;
    }

    Ok(saw_collection)
}

fn get_member_owner_for_prefix_type(prefix_type: LuaType) -> Option<LuaMemberOwner> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => Some(LuaMemberOwner::Element(in_file_range)),
        LuaType::Def(def_id) | LuaType::Ref(def_id) => Some(LuaMemberOwner::Type(def_id)),
        LuaType::Instance(instance) => Some(LuaMemberOwner::Element(instance.get_range().clone())),
        _ => None,
    }
}

fn normalize_infer_collection_type(typ: &LuaType) -> Option<()> {
    match typ {
        LuaType::Array(_) => Some(()),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => Some(()),
        _ => None,
    }
}

fn infer_collection_base_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Array(array) => Some(array.get_base().clone()),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => Some(tuple.cast_down_array_base(db)),
        _ => None,
    }
}

fn expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

fn is_literal_integer_one(expr: &LuaExpr) -> bool {
    let LuaExpr::LiteralExpr(literal_expr) = expr else {
        return false;
    };

    matches!(
        literal_expr.get_literal(),
        Some(LuaLiteralToken::Number(number))
            if matches!(number.get_number_value(), NumberResult::Int(1))
    )
}

fn try_infer_decl_initializer_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
) -> Result<Option<LuaType>, InferFailReason> {
    let Some(decl_id) = var_ref_id.get_decl_id_ref() else {
        return Ok(None);
    };

    let decl = db
        .get_decl_index()
        .get_decl(&decl_id)
        .ok_or(InferFailReason::None)?;

    let Some(value_syntax_id) = decl.get_value_syntax_id() else {
        return Ok(None);
    };

    let Some(node) = value_syntax_id.to_node_from_root(root.syntax()) else {
        return Ok(None);
    };

    let Some(expr) = LuaExpr::cast(node) else {
        return Ok(None);
    };

    let expr_type = infer_expr(db, cache, expr.clone())?;
    let init_type = match expr_type {
        LuaType::Variadic(variadic) => variadic.get_type(0).cloned(),
        ty => Some(ty),
    };

    Ok(init_type)
}
