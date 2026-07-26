mod resolve_signature_by_args;

use std::sync::Arc;

use glua_parser::LuaCallExpr;

use crate::db_index::{DbIndex, LuaFunctionType};

use super::{
    LuaInferCache,
    generic::instantiate_func_generic,
    infer::{InferCallFuncResult, InferFailReason, infer_call_arg_expr_list_types},
};

use resolve_signature_by_args::resolve_signature_by_args;

pub fn resolve_signature(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    overloads: Vec<Arc<LuaFunctionType>>,
    call_expr: LuaCallExpr,
    is_generic: bool,
    arg_count: Option<usize>,
) -> InferCallFuncResult {
    resolve_signature_with_index(db, cache, overloads, call_expr, is_generic, arg_count)
        .map(|(func, _)| func)
}

pub(crate) fn resolve_signature_with_index(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    overloads: Vec<Arc<LuaFunctionType>>,
    call_expr: LuaCallExpr,
    is_generic: bool,
    arg_count: Option<usize>,
) -> Result<(Arc<LuaFunctionType>, usize), InferFailReason> {
    if call_expr.get_args_list().is_none() {
        return Err(InferFailReason::None);
    }
    let expr_types = infer_call_arg_expr_list_types(db, cache, call_expr.clone(), arg_count)
        .into_iter()
        .map(|(typ, _)| typ)
        .collect::<Vec<_>>();
    let candidates = if is_generic {
        instantiate_signatures(db, cache, overloads, call_expr.clone())?
    } else {
        overloads
    };
    let resolved = resolve_signature_by_args(
        db,
        &candidates,
        &expr_types,
        call_expr.is_colon_call(),
        arg_count,
    )?;
    let selected_idx = candidates
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, &resolved))
        .ok_or(InferFailReason::None)?;
    Ok((resolved, selected_idx))
}

fn instantiate_signatures(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    overloads: Vec<Arc<LuaFunctionType>>,
    call_expr: LuaCallExpr,
) -> Result<Vec<Arc<LuaFunctionType>>, InferFailReason> {
    let mut instantiate_funcs = Vec::new();
    for func in overloads {
        let instantiate_func = instantiate_func_generic(db, cache, &func, call_expr.clone())?;
        instantiate_funcs.push(Arc::new(instantiate_func));
    }
    Ok(instantiate_funcs)
}
