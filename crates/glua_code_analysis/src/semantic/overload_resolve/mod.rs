mod resolve_signature_by_args;

use std::sync::Arc;

use glua_parser::LuaCallExpr;

use crate::db_index::{DbIndex, LuaFunctionType, LuaType};

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
    if call_expr.get_args_list().is_none() {
        return Err(InferFailReason::None);
    }
    let expr_types = infer_call_arg_expr_list_types(db, cache, call_expr.clone(), arg_count)
        .into_iter()
        .map(|(typ, _)| typ)
        .collect::<Vec<_>>();
    if is_generic {
        resolve_signature_by_generic(db, cache, overloads, call_expr, expr_types, arg_count)
    } else {
        resolve_signature_by_args(
            db,
            &overloads,
            &expr_types,
            call_expr.is_colon_call(),
            arg_count,
        )
    }
}

fn resolve_signature_by_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    overloads: Vec<Arc<LuaFunctionType>>,
    call_expr: LuaCallExpr,
    expr_types: Vec<LuaType>,
    arg_count: Option<usize>,
) -> InferCallFuncResult {
    let mut instantiate_funcs = Vec::new();
    for func in overloads {
        let instantiate_func = instantiate_func_generic(db, cache, &func, call_expr.clone())?;
        instantiate_funcs.push(Arc::new(instantiate_func));
    }
    resolve_signature_by_args(
        db,
        &instantiate_funcs,
        &expr_types,
        call_expr.is_colon_call(),
        arg_count,
    )
}
