mod condition_flow;
mod get_type_at_cast_flow;
mod get_type_at_flow;
mod narrow_type;
mod var_ref_id;

use crate::{
    CacheEntry, DbIndex, FlowAntecedent, FlowId, FlowNode, FlowNodeKind, FlowTree, InferFailReason,
    LuaInferCache, LuaType, infer_param,
    semantic::infer::{
        InferResult,
        infer_name::{find_decl_member_type, infer_global_type},
    },
};
pub use get_type_at_cast_flow::get_type_at_call_expr_inline_cast;
use glua_parser::{LuaAstNode, LuaChunk, LuaExpr};
pub use narrow_type::{narrow_down_type, narrow_false_or_nil, remove_false_or_nil};
pub use var_ref_id::{VarRefId, get_var_expr_var_ref_id};

pub fn infer_expr_narrow_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    var_ref_id: VarRefId,
) -> InferResult {
    let file_id = cache.get_file_id();
    let Some(flow_tree) = db.get_flow_index().get_flow_tree(&file_id) else {
        return get_var_ref_type(db, cache, &var_ref_id);
    };

    let Some(flow_id) = flow_tree.get_flow_id(expr.get_syntax_id()) else {
        return get_var_ref_type(db, cache, &var_ref_id);
    };

    let root = LuaChunk::cast(expr.get_root()).ok_or(InferFailReason::None)?;
    let query_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, expr.get_position());
    let previous_query_realm = cache.flow_query_realm.replace(query_realm);
    let result =
        get_type_at_flow::get_type_at_flow(db, flow_tree, cache, &root, &var_ref_id, flow_id);
    cache.flow_query_realm = previous_query_realm;
    result
}

pub fn get_var_ref_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_ref_id: &VarRefId,
) -> InferResult {
    if let Some(decl_id) = var_ref_id.get_decl_id_ref() {
        let decl = db
            .get_decl_index()
            .get_decl(&decl_id)
            .ok_or(InferFailReason::None)?;

        // Parameter declarations carry their canonical type in signature metadata.
        // Flow/assignment analysis may also create a decl type cache entry for params,
        // but that inferred cache must not replace the declared parameter type.
        if decl.is_param() {
            if let Ok(param_type) = infer_param(db, decl) {
                return Ok(param_type);
            }

            if let Some(type_cache) = db.get_type_index().get_type_cache(&decl.get_id().into()) {
                return Ok(type_cache.as_type().clone());
            }

            return Err(InferFailReason::UnResolveDeclType(decl.get_id()));
        }

        if decl.is_global() {
            if let Some(type_cache) = db.get_type_index().get_type_cache(&decl.get_id().into()) {
                let typ = type_cache.as_type();
                return if typ.contain_tpl() {
                    Ok(LuaType::Unknown)
                } else {
                    Ok(typ.clone())
                };
            }

            let name = decl.get_name();
            return infer_global_type(db, Some(cache.get_file_id()), None, name);
        }

        if let Some(type_cache) = db.get_type_index().get_type_cache(&decl.get_id().into()) {
            let result = type_cache.as_type().clone();
            // 不要在此阶段展开泛型别名, 必须让后续的泛型匹配阶段基于声明形态完成推断
            return Ok(result);
        }

        Err(InferFailReason::UnResolveDeclType(decl.get_id()))
    } else if let Some(member_id) = var_ref_id.get_member_id_ref() {
        find_decl_member_type(db, member_id)
    } else if let VarRefId::GlobalName(name, _) = var_ref_id {
        Ok(
            infer_global_type(db, Some(cache.get_file_id()), None, name.as_str())
                .unwrap_or(LuaType::Unknown),
        )
    } else {
        if let Some(type_cache) = cache.index_ref_origin_type_cache.get(var_ref_id)
            && let CacheEntry::Cache(ty) = type_cache
        {
            return Ok(ty.clone());
        }

        Err(InferFailReason::None)
    }
}

fn get_single_antecedent(tree: &FlowTree, flow: &FlowNode) -> Result<FlowId, InferFailReason> {
    match &flow.antecedent {
        Some(antecedent) => match antecedent {
            FlowAntecedent::Single(id) => Ok(*id),
            FlowAntecedent::Multiple(multi_id) => {
                let multi_flow = tree
                    .get_multi_antecedents(*multi_id)
                    .ok_or(InferFailReason::None)?;
                if !multi_flow.is_empty() {
                    if let Some(preferred) = multi_flow.iter().copied().find(|flow_id| {
                        tree.get_flow_node(*flow_id).is_some_and(|flow_node| {
                            !matches!(
                                flow_node.kind,
                                FlowNodeKind::Unreachable
                                    | FlowNodeKind::Return
                                    | FlowNodeKind::Break
                            )
                        })
                    }) {
                        Ok(preferred)
                    } else {
                        Ok(multi_flow[0])
                    }
                } else {
                    Err(InferFailReason::None)
                }
            }
        },
        None => Err(InferFailReason::None),
    }
}

fn get_multi_antecedents(tree: &FlowTree, flow: &FlowNode) -> Result<Vec<FlowId>, InferFailReason> {
    match &flow.antecedent {
        Some(antecedent) => match antecedent {
            FlowAntecedent::Single(id) => Ok(vec![*id]),
            FlowAntecedent::Multiple(multi_id) => {
                let multi_flow = tree
                    .get_multi_antecedents(*multi_id)
                    .ok_or(InferFailReason::None)?;
                Ok(multi_flow.to_vec())
            }
        },
        None => Err(InferFailReason::None),
    }
}

#[derive(Debug)]
pub enum ResultTypeOrContinue {
    Result(LuaType),
    Continue,
}
