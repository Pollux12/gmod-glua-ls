use std::{ops::Deref, sync::Arc};

use glua_parser::{LuaCallExpr, LuaChunk, LuaExpr, LuaIndexKey, LuaIndexMemberExpr};

use crate::{
    DbIndex, FlowNode, FlowTree, InferFailReason, InferGuard, LuaAliasCallKind, LuaAliasCallType,
    LuaFunctionType, LuaInferCache, LuaSignatureCast, LuaSignatureId, LuaType, TypeOps,
    infer_call_expr_func, infer_expr,
    semantic::infer::{
        VarRefId,
        infer_index::infer_member_by_member_key,
        narrow::{
            ResultTypeOrContinue, condition_flow::InferConditionFlow, get_single_antecedent,
            get_type_at_cast_flow::cast_type, get_type_at_flow::get_type_at_flow,
            narrow_false_or_nil, remove_false_or_nil, var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

#[allow(clippy::too_many_arguments)]
pub fn get_type_at_call_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    // Keep references for potential IsValid fallback
    let call_expr_ref = call_expr.clone();
    let prefix_expr_ref = prefix_expr.clone();

    let maybe_func = infer_expr(db, cache, prefix_expr.clone())?;
    let result = match maybe_func {
        LuaType::DocFunction(f) => {
            let return_type = f.get_ret();
            match return_type {
                LuaType::TypeGuard(_) => get_type_at_call_expr_by_type_guard(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    call_expr,
                    f,
                    condition_flow,
                ),
                _ => {
                    // If the return type is not a type guard, we cannot infer the type cast.
                    Ok(ResultTypeOrContinue::Continue)
                }
            }
        }
        LuaType::Signature(signature_id) => {
            let Some(signature) = db.get_signature_index().get(&signature_id) else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            let ret = signature.get_return_type();
            match ret {
                LuaType::TypeGuard(_) => {
                    return get_type_at_call_expr_by_type_guard(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        signature.to_doc_func_type(),
                        condition_flow,
                    );
                }
                LuaType::Call(call) => {
                    return get_type_at_call_expr_by_call(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        &call,
                        condition_flow,
                    );
                }
                _ => {}
            }

            if let Some(signature_cast) = db.get_flow_index().get_signature_cast(&signature_id) {
                return match signature_cast.name.as_str() {
                    "self" => get_type_at_call_expr_by_signature_self(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        prefix_expr,
                        signature_cast,
                        signature_id,
                        condition_flow,
                    ),
                    name => get_type_at_call_expr_by_signature_param_name(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        signature_cast,
                        signature_id,
                        name,
                        condition_flow,
                    ),
                };
            }

            // No @cast annotation found — fall through so IsValid/isfunction
            // name-based narrowing can still run as a fallback.
            Ok(ResultTypeOrContinue::Continue)
        }
        _ => {
            // If the prefix expression is not a function, we cannot infer the type cast.
            Ok(ResultTypeOrContinue::Continue)
        }
    };

    // Fallback: check for IsValid pattern (Garry's Mod nil check) when normal
    // type-based narrowing didn't produce a result
    if let Ok(ResultTypeOrContinue::Continue) = result {
        if let Some(isfunction_type) = try_narrow_isfunction_member(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            &call_expr_ref,
            &prefix_expr_ref,
            condition_flow,
        )? {
            return Ok(ResultTypeOrContinue::Result(isfunction_type));
        }

        if let Some(isvalid_type) = try_narrow_isvalid(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            &call_expr_ref,
            &prefix_expr_ref,
            condition_flow,
        )? {
            return Ok(ResultTypeOrContinue::Result(isvalid_type));
        }
    }

    result
}

#[allow(clippy::too_many_arguments)]
fn try_narrow_isfunction_member(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<Option<LuaType>, InferFailReason> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return Ok(None);
    };

    if name_expr.get_name_text().as_deref() != Some("isfunction") {
        return Ok(None);
    }

    // Ignore shadowed local isfunction.
    if let Some(isfunction_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(name_expr.clone()))
        && let VarRefId::VarRef(isfunction_decl_id) = isfunction_ref_id
        && let Some(isfunction_decl) = db.get_decl_index().get_decl(&isfunction_decl_id)
        && isfunction_decl.is_local()
    {
        return Ok(None);
    }

    let Some(arg_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().next())
    else {
        return Ok(None);
    };

    let LuaExpr::IndexExpr(index_expr) = arg_expr else {
        return Ok(None);
    };
    let Some(prefix_obj_expr) = index_expr.get_prefix_expr() else {
        return Ok(None);
    };

    let Some(prefix_ref_id) = get_var_expr_var_ref_id(db, cache, prefix_obj_expr) else {
        return Ok(None);
    };
    if prefix_ref_id != *var_ref_id {
        return Ok(None);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    let Some(candidates) = collect_isfunction_narrow_candidates(db, &antecedent_type) else {
        return Ok(None);
    };

    let index_member = LuaIndexMemberExpr::IndexExpr(index_expr);
    let mut callable_candidates = Vec::new();
    for candidate in &candidates {
        let member_type = match infer_member_by_member_key(
            db,
            cache,
            candidate,
            index_member.clone(),
            &InferGuard::new(),
        ) {
            Ok(member_type) => member_type,
            Err(_) => continue,
        };

        if contains_callable_member_type(&member_type) {
            callable_candidates.push(candidate.clone());
        }
    }

    if callable_candidates.is_empty() {
        return Ok(None);
    }

    let narrowed_type = match condition_flow {
        InferConditionFlow::TrueCondition => LuaType::from_vec(callable_candidates),
        InferConditionFlow::FalseCondition => {
            let remaining_candidates = candidates
                .into_iter()
                .filter(|candidate| !callable_candidates.contains(candidate))
                .collect::<Vec<_>>();
            if remaining_candidates.is_empty() {
                return Ok(None);
            }
            LuaType::from_vec(remaining_candidates)
        }
    };

    Ok(Some(narrowed_type))
}

fn collect_isfunction_narrow_candidates(
    db: &DbIndex,
    antecedent_type: &LuaType,
) -> Option<Vec<LuaType>> {
    const MAX_CANDIDATES: usize = 128;

    match antecedent_type {
        LuaType::Union(union_type) => Some(union_type.into_vec().to_vec()),
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            let mut candidates = vec![LuaType::Ref(type_decl_id.clone())];
            let all_sub_types = db.get_type_index().get_all_sub_types(type_decl_id);
            if all_sub_types.len() > MAX_CANDIDATES {
                return None;
            }
            for sub_type in all_sub_types {
                candidates.push(LuaType::Ref(sub_type.get_id()));
            }
            Some(candidates)
        }
        LuaType::Instance(instance_type) => {
            collect_isfunction_narrow_candidates(db, instance_type.get_base())
        }
        _ => None,
    }
}

fn contains_callable_member_type(member_type: &LuaType) -> bool {
    match member_type {
        LuaType::Function | LuaType::Signature(_) | LuaType::DocFunction(_) => true,
        LuaType::Union(union_type) => union_type
            .into_vec()
            .iter()
            .any(contains_callable_member_type),
        LuaType::Intersection(intersection_type) => intersection_type
            .get_types()
            .iter()
            .any(contains_callable_member_type),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_type_guard(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    func_type: Arc<LuaFunctionType>,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(arg_list) = call_expr.get_args_list() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(first_arg) = arg_list.get_args().next() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, first_arg) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let mut return_type = func_type.get_ret().clone();
    if return_type.contain_tpl() {
        let call_expr_type = LuaType::DocFunction(func_type);
        let inst_func = infer_call_expr_func(
            db,
            cache,
            call_expr,
            call_expr_type,
            &InferGuard::new(),
            None,
        )?;

        return_type = inst_func.get_ret().clone();
    }

    let guard_type = match return_type {
        LuaType::TypeGuard(guard) => guard.deref().clone(),
        _ => return Ok(ResultTypeOrContinue::Continue),
    };

    match condition_flow {
        InferConditionFlow::TrueCondition => Ok(ResultTypeOrContinue::Result(guard_type)),
        InferConditionFlow::FalseCondition => {
            let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            let antecedent_type =
                get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
            Ok(ResultTypeOrContinue::Result(TypeOps::Remove.apply(
                db,
                &antecedent_type,
                &guard_type,
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_signature_self(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_prefix: LuaExpr,
    signature_cast: &LuaSignatureCast,
    signature_id: LuaSignatureId,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let LuaExpr::IndexExpr(call_prefix_index) = call_prefix else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(self_expr) = call_prefix_index.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(name_var_ref_id) = get_var_expr_var_ref_id(db, cache, self_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let Some(syntax_tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let signature_root = syntax_tree.get_chunk_node();

    // Choose the appropriate cast based on condition_flow and whether fallback exists
    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                return Ok(ResultTypeOrContinue::Continue);
            };
            cast_type(
                db,
                signature_id.get_file_id(),
                cast_op_type,
                antecedent_type,
                condition_flow,
            )?
        }
        InferConditionFlow::FalseCondition => {
            // Use fallback_cast if available, otherwise use the default behavior
            if let Some(fallback_cast_ptr) = &signature_cast.fallback_cast {
                let Some(fallback_op_type) = fallback_cast_ptr.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    fallback_op_type,
                    antecedent_type.clone(),
                    InferConditionFlow::TrueCondition, // Apply fallback as force cast
                )?
            } else {
                // Original behavior: remove the true type from antecedent
                let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    cast_op_type,
                    antecedent_type,
                    condition_flow,
                )?
            }
        }
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_signature_param_name(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    signature_cast: &LuaSignatureCast,
    signature_id: LuaSignatureId,
    name: &str,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let colon_call = call_expr.is_colon_call();
    let Some(arg_list) = call_expr.get_args_list() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(mut param_idx) = signature.find_param_idx(name) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let colon_define = signature.is_colon_define;
    match (colon_call, colon_define) {
        (true, false) => {
            if param_idx == 0 {
                return Ok(ResultTypeOrContinue::Continue);
            }

            param_idx -= 1;
        }
        (false, true) => {
            param_idx += 1;
        }
        _ => {}
    }

    let Some(expr) = arg_list.get_args().nth(param_idx) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(name_var_ref_id) = get_var_expr_var_ref_id(db, cache, expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let Some(syntax_tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let signature_root = syntax_tree.get_chunk_node();

    // Choose the appropriate cast based on condition_flow and whether fallback exists
    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                return Ok(ResultTypeOrContinue::Continue);
            };
            cast_type(
                db,
                signature_id.get_file_id(),
                cast_op_type,
                antecedent_type,
                condition_flow,
            )?
        }
        InferConditionFlow::FalseCondition => {
            // Use fallback_cast if available, otherwise use the default behavior
            if let Some(fallback_cast_ptr) = &signature_cast.fallback_cast {
                let Some(fallback_op_type) = fallback_cast_ptr.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    fallback_op_type,
                    antecedent_type.clone(),
                    InferConditionFlow::TrueCondition, // Apply fallback as force cast
                )?
            } else {
                // Original behavior: remove the true type from antecedent
                let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    cast_op_type,
                    antecedent_type,
                    condition_flow,
                )?
            }
        }
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(unused, clippy::too_many_arguments)]
fn get_type_at_call_expr_by_call(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    alias_call_type: &Arc<LuaAliasCallType>,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(maybe_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::CallExpr(call_expr.clone()))
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    if alias_call_type.get_call_kind() == LuaAliasCallKind::RawGet {
        let antecedent_type = infer_expr(db, cache, LuaExpr::CallExpr(call_expr))?;
        let result_type = match condition_flow {
            InferConditionFlow::FalseCondition => narrow_false_or_nil(db, antecedent_type),
            InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
        };
        return Ok(ResultTypeOrContinue::Result(result_type));
    };

    Ok(ResultTypeOrContinue::Continue)
}

/// Detect `IsValid(x)` or `x:IsValid()` calls and narrow the argument/self type
/// to remove nil/false in the true branch. This is essential for Garry's Mod where
/// IsValid is the standard nil/validity check.
#[allow(clippy::too_many_arguments)]
fn try_narrow_isvalid(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    condition_flow: InferConditionFlow,
) -> Result<Option<LuaType>, InferFailReason> {
    // Determine if this is an IsValid call and get the target expression to narrow
    let target_expr = match prefix_expr {
        // Global call: IsValid(x)
        LuaExpr::NameExpr(name_expr) => {
            if name_expr.get_name_text().as_deref() != Some("IsValid") {
                return Ok(None);
            }
            if let Some(isvalid_ref_id) =
                get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(name_expr.clone()))
            {
                let VarRefId::VarRef(isvalid_decl_id) = isvalid_ref_id else {
                    return Ok(None);
                };
                if let Some(isvalid_decl) = db.get_decl_index().get_decl(&isvalid_decl_id)
                    && isvalid_decl.is_local()
                {
                    return Ok(None);
                }
            }
            let arg_list = match call_expr.get_args_list() {
                Some(list) => list,
                None => return Ok(None),
            };
            match arg_list.get_args().next() {
                Some(first_arg) => first_arg,
                None => return Ok(None),
            }
        }
        // Method call: x:IsValid() (only colon syntax, not dot syntax)
        LuaExpr::IndexExpr(index_expr) => {
            if !call_expr.is_colon_call() {
                return Ok(None);
            }
            let is_isvalid = match index_expr.get_index_key() {
                Some(LuaIndexKey::Name(name_token)) => name_token.get_name_text() == "IsValid",
                _ => false,
            };
            if !is_isvalid {
                return Ok(None);
            }
            match index_expr.get_prefix_expr() {
                Some(self_expr) => self_expr,
                None => return Ok(None),
            }
        }
        _ => return Ok(None),
    };

    // Check if the target expression matches the variable we're narrowing
    let Some(target_ref_id) = get_var_expr_var_ref_id(db, cache, target_expr) else {
        return Ok(None);
    };
    if target_ref_id != *var_ref_id {
        return Ok(None);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
        InferConditionFlow::FalseCondition => antecedent_type,
    };

    Ok(Some(result_type))
}
