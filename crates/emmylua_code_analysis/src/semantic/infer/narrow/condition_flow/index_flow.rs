use emmylua_parser::{LuaChunk, LuaExpr, LuaIndexExpr, LuaIndexMemberExpr};

use crate::{
    DbIndex, FlowNode, FlowTree, InferFailReason, InferGuard, LuaInferCache, LuaType,
    semantic::infer::{
        VarRefId,
        infer_index::infer_member_by_member_key,
        narrow::{
            ResultTypeOrContinue, condition_flow::InferConditionFlow, get_single_antecedent,
            get_type_at_flow::get_type_at_flow, narrow_false_or_nil, remove_false_or_nil,
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

#[allow(clippy::too_many_arguments)]
pub fn get_type_at_index_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    index_expr: LuaIndexExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(name_var_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(index_expr.clone()))
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return maybe_field_exist_narrow(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            index_expr,
            condition_flow,
        );
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let result_type = match condition_flow {
        InferConditionFlow::FalseCondition => narrow_false_or_nil(db, antecedent_type),
        InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(clippy::too_many_arguments)]
fn maybe_field_exist_narrow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    index_expr: LuaIndexExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(maybe_var_ref_id) = get_var_expr_var_ref_id(db, cache, prefix_expr.clone()) else {
        // If we cannot find a reference declaration ID, we cannot narrow it
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let left_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    let Some(candidates) = collect_field_exist_narrow_candidates(db, &left_type) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let index = LuaIndexMemberExpr::IndexExpr(index_expr);
    let mut result = vec![];
    for sub_type in &candidates {
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
        // donot use always true
        if !member_type.is_always_falsy() {
            result.push(sub_type.clone());
        }
    }

    match condition_flow {
        InferConditionFlow::TrueCondition => {
            if !result.is_empty() {
                return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(result)));
            }
        }
        InferConditionFlow::FalseCondition => {
            if !result.is_empty() {
                let remaining = candidates
                    .into_iter()
                    .filter(|candidate| !result.contains(candidate))
                    .collect::<Vec<_>>();
                if !remaining.is_empty() {
                    return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(remaining)));
                }
            }
        }
    }

    Ok(ResultTypeOrContinue::Continue)
}

fn collect_field_exist_narrow_candidates(
    db: &DbIndex,
    left_type: &LuaType,
) -> Option<Vec<LuaType>> {
    const MAX_CANDIDATES: usize = 128;

    match left_type {
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
            collect_field_exist_narrow_candidates(db, instance_type.get_base())
        }
        _ => None,
    }
}
