mod resolve_signature_by_args;

use std::sync::Arc;

use glua_parser::LuaCallExpr;

use rowan::TextRange;

use crate::db_index::{DbIndex, LuaFunctionType, LuaType, VariadicType};

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
    resolve_signature_with_index(db, cache, overloads, None, call_expr, is_generic, arg_count)
        .map(|(func, _)| func)
}

/// Resolves the candidate a call selects, with its index. `main_idx` names a
/// signature's main form among `overloads`: arguments that carry no type
/// evidence select it, since nothing in them picks an overload.
pub(crate) fn resolve_signature_with_index(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    overloads: Vec<Arc<LuaFunctionType>>,
    main_idx: Option<usize>,
    call_expr: LuaCallExpr,
    is_generic: bool,
    arg_count: Option<usize>,
) -> Result<(Arc<LuaFunctionType>, usize), InferFailReason> {
    if call_expr.get_args_list().is_none() {
        return Err(InferFailReason::None);
    }
    let expr_types = infer_call_arg_expr_list_types(db, cache, call_expr.clone(), arg_count);
    let candidates = if is_generic {
        instantiate_signatures(db, cache, overloads, call_expr.clone())?
    } else {
        overloads
    };
    if let Some(main_idx) = main_idx
        && let Some(main) = candidates.get(main_idx)
        && args_carry_no_evidence(main, &expr_types, call_expr.is_colon_call())
    {
        return Ok((main.clone(), main_idx));
    }
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

/// Whether the arguments past `main`'s receiver fit `main`'s arity and each of
/// them is `any` or `unknown`, so none of them tells one form of the signature
/// from another.
fn args_carry_no_evidence(
    main: &LuaFunctionType,
    expr_types: &[(LuaType, TextRange)],
    is_colon_call: bool,
) -> bool {
    let receiver_args = usize::from(main.is_colon_define() && !is_colon_call);
    let receiver_params = usize::from(!main.is_colon_define() && is_colon_call);
    let args = &expr_types[receiver_args.min(expr_types.len())..];
    let params = &main.get_params()[receiver_params.min(main.get_params().len())..];
    let takes_every_arg =
        args.len() <= params.len() || params.last().is_some_and(|param| param.0 == "...");
    let fills_every_required_param =
        params
            .iter()
            .enumerate()
            .skip(args.len())
            .all(|(idx, (name, param_type))| {
                name == "..."
                    || main.is_param_optional(idx + receiver_params)
                    || param_type
                        .as_ref()
                        .is_some_and(|param_type| param_type.is_nullable())
            });
    !args.is_empty()
        && takes_every_arg
        && fills_every_required_param
        && args.iter().all(|(arg_type, _)| {
            let arg_type = match arg_type {
                LuaType::Variadic(variadic) => match variadic.as_ref() {
                    VariadicType::Base(base) => base,
                    VariadicType::Multi(_) => arg_type,
                },
                _ => arg_type,
            };
            arg_type.is_any() || arg_type.is_unknown()
        })
}
