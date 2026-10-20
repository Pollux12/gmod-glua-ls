use std::sync::Arc;

use rowan::TextRange;

use crate::{
    InferFailReason, check_type_compact,
    db_index::{DbIndex, LuaFunctionType, LuaType},
    semantic::{infer::InferCallFuncResult, is_nullable},
};

pub fn resolve_signature_by_args(
    db: &DbIndex,
    overloads: &[Arc<LuaFunctionType>],
    expr_types: &[(LuaType, TextRange)],
    is_colon_call: bool,
    arg_count: Option<usize>,
) -> InferCallFuncResult {
    let expr_len = expr_types.len();
    let arg_count = arg_count.unwrap_or(expr_len);
    let mut need_resolve_funcs = match overloads.len() {
        0 => return Err(InferFailReason::None),
        1 => return Ok(Arc::clone(&overloads[0])),
        _ => overloads
            .iter()
            .map(|it| Some(it.clone()))
            .collect::<Vec<_>>(),
    };

    let mut finite_funcs = overloads
        .iter()
        .filter(|func| !is_func_last_param_variadic(func))
        .peekable();
    if finite_funcs.peek().is_some()
        && finite_funcs.all(|func| arg_capacity(func, is_colon_call) < arg_count)
        && let Some(func) = resolve_by_leading_fit(db, overloads, expr_types, is_colon_call)
    {
        return Ok(func);
    }

    if expr_len == 0 {
        for overload in overloads {
            let param_len = overload.get_params().len();
            if param_len == 0 {
                return Ok(overload.clone());
            }
        }
    }

    let mut best_match_result = need_resolve_funcs[0]
        .clone()
        .expect("Match result should exist");
    let mut fits_only_union_components = vec![false; need_resolve_funcs.len()];
    for (arg_index, (expr_type, _)) in expr_types.iter().enumerate() {
        let mut current_match_result = ParamMatchResult::Not;
        let mut skipped_implicit_self = false;
        for (func_index, opt_func) in need_resolve_funcs.iter_mut().enumerate() {
            let func = match opt_func.as_ref() {
                None => continue,
                Some(func) => func,
            };
            let param_len = func.get_params().len();
            let effective_arg_count =
                effective_arg_count(arg_count, func.is_colon_define(), is_colon_call);
            if param_len < effective_arg_count && !is_func_last_param_variadic(func) {
                *opt_func = None;
                continue;
            }

            let colon_define = func.is_colon_define();
            let mut param_index = arg_index;
            match (colon_define, is_colon_call) {
                (true, false) => {
                    if param_index == 0 {
                        skipped_implicit_self = true;
                        continue;
                    }
                    param_index -= 1;
                }
                (false, true) => {
                    param_index += 1;
                }
                _ => {}
            }
            let param_type = if param_index < param_len {
                let param_info = func.get_params().get(param_index);
                param_info
                    .map(|it| it.1.clone().unwrap_or(LuaType::Any))
                    .unwrap_or(LuaType::Any)
            } else if let Some(last_param_info) = func.get_params().last() {
                if last_param_info.0 == "..." {
                    last_param_info.1.clone().unwrap_or(LuaType::Any)
                } else {
                    *opt_func = None;
                    continue;
                }
            } else {
                *opt_func = None;
                continue;
            };

            let match_result = classify_arg_match(db, &param_type, expr_type);

            if match_result > current_match_result {
                current_match_result = match_result;
                best_match_result = func.clone();
            }

            if match_result == ParamMatchResult::Not {
                *opt_func = None;
                continue;
            }
            if match_result == ParamMatchResult::UnionComponent {
                fits_only_union_components[func_index] = true;
            }

            if match_result >= ParamMatchResult::Exact
                && arg_index + 1 == expr_len
                && param_index + 1 == func.get_params().len()
            {
                return Ok(func.clone());
            }
        }

        if current_match_result == ParamMatchResult::Not && !skipped_implicit_self {
            break;
        }
    }

    let some_func_fits_every_arg = need_resolve_funcs
        .iter()
        .zip(&fits_only_union_components)
        .any(|(func, partial)| func.is_some() && !partial);
    let mut rest_need_resolve_funcs = need_resolve_funcs
        .iter()
        .zip(&fits_only_union_components)
        .filter(|(func, partial)| func.is_some() && !(some_func_fits_every_arg && **partial))
        .map(|(func, _)| func.clone())
        .collect::<Vec<_>>();

    let rest_len = rest_need_resolve_funcs.len();
    match rest_len {
        0 => return Ok(best_match_result),
        1 => {
            return Ok(rest_need_resolve_funcs[0]
                .clone()
                .expect("Resolve function should exist"));
        }
        _ => {}
    }

    let start_param_index = expr_len;
    let mut max_param_len = 0;
    for func in rest_need_resolve_funcs.iter().flatten() {
        let param_len = func.get_params().len();
        if param_len > max_param_len {
            max_param_len = param_len;
        }
    }

    for param_index in start_param_index..max_param_len {
        let mut current_match_result = ParamMatchResult::Not;
        for (i, opt_func) in rest_need_resolve_funcs.iter_mut().enumerate() {
            let func = match opt_func.as_ref() {
                None => continue,
                Some(func) => func,
            };
            let param_len = func.get_params().len();
            let colon_define = func.is_colon_define();
            let mut param_index = param_index;
            match (colon_define, is_colon_call) {
                (true, false) => {
                    if param_index == 0 {
                        continue;
                    }
                    param_index -= 1;
                }
                (false, true) => {
                    param_index += 1;
                }
                _ => {}
            }
            let param_type = if param_index < param_len {
                let param_info = func.get_params().get(param_index);
                param_info
                    .map(|it| it.1.clone().unwrap_or(LuaType::Any))
                    .unwrap_or(LuaType::Any)
            } else if let Some(last_param_info) = func.get_params().last() {
                if last_param_info.0 == "..." {
                    last_param_info.1.clone().unwrap_or(LuaType::Any)
                } else {
                    return Ok(func.clone());
                }
            } else {
                return Ok(func.clone());
            };

            let match_result = if param_type.is_any() {
                ParamMatchResult::Any
            } else if param_type.is_nullable() || func.is_param_optional(param_index) {
                ParamMatchResult::Type
            } else {
                ParamMatchResult::Not
            };

            if match_result > current_match_result {
                current_match_result = match_result;
                best_match_result = func.clone();
            }

            if match_result == ParamMatchResult::Not {
                *opt_func = None;
                continue;
            }

            if match_result >= ParamMatchResult::Any
                && i + 1 == rest_len
                && param_index + 1 == func.get_params().len()
            {
                return Ok(func.clone());
            }
        }

        if current_match_result == ParamMatchResult::Not {
            break;
        }
    }

    Ok(best_match_result)
}

fn is_func_last_param_variadic(func: &LuaFunctionType) -> bool {
    if let Some(last_param) = func.get_params().last() {
        last_param.0 == "..."
    } else {
        false
    }
}

/// Among candidates whose own capacity is filled by compatible arguments, the one consuming the most arguments.
/// Candidates with every required parameter supplied rank first, then those matching every argument in full
/// rather than only a union component.
fn resolve_by_leading_fit(
    db: &DbIndex,
    overloads: &[Arc<LuaFunctionType>],
    expr_types: &[(LuaType, TextRange)],
    is_colon_call: bool,
) -> Option<Arc<LuaFunctionType>> {
    let mut best: Option<((bool, bool, usize), &Arc<LuaFunctionType>)> = None;
    for func in overloads {
        let consumed = if is_func_last_param_variadic(func) {
            expr_types.len()
        } else {
            expr_types.len().min(arg_capacity(func, is_colon_call))
        };
        let supplied = required_params_supplied(db, func, expr_types.len(), is_colon_call);
        if best.is_some_and(|(best_rank, _)| best_rank >= (supplied, true, consumed)) {
            continue;
        }
        let Some(full) = leading_args_fit(db, func, &expr_types[..consumed], is_colon_call) else {
            continue;
        };
        let rank = (supplied, full, consumed);
        if best.is_none_or(|(best_rank, _)| rank > best_rank) {
            best = Some((rank, func));
        }
    }
    best.map(|(_, func)| Arc::clone(func))
}

/// A finite candidate is measured at its own capacity; a variadic one needs its required fixed parameters supplied.
fn required_params_supplied(
    db: &DbIndex,
    func: &LuaFunctionType,
    arg_count: usize,
    is_colon_call: bool,
) -> bool {
    if !is_func_last_param_variadic(func) {
        return true;
    }
    let supplied = effective_arg_count(arg_count, func.is_colon_define(), is_colon_call);
    let params = func.get_params();
    params[..params.len() - 1]
        .iter()
        .enumerate()
        .skip(supplied)
        .all(|(param_index, (_, param_type))| {
            func.is_param_optional(param_index)
                || param_type
                    .as_ref()
                    .is_none_or(|param_type| is_nullable(db, param_type))
        })
}

/// `None` when an argument does not fit; otherwise whether every argument fits in full.
fn leading_args_fit(
    db: &DbIndex,
    func: &LuaFunctionType,
    expr_types: &[(LuaType, TextRange)],
    is_colon_call: bool,
) -> Option<bool> {
    let mut full = true;
    for (arg_index, (expr_type, _)) in expr_types.iter().enumerate() {
        match leading_arg_match(db, func, arg_index, expr_type, is_colon_call) {
            ParamMatchResult::Not => return None,
            ParamMatchResult::UnionComponent => full = false,
            _ => {}
        }
    }
    Some(full)
}

fn leading_arg_match(
    db: &DbIndex,
    func: &LuaFunctionType,
    arg_index: usize,
    expr_type: &LuaType,
    is_colon_call: bool,
) -> ParamMatchResult {
    let param_index = match (func.is_colon_define(), is_colon_call) {
        (true, false) => match arg_index.checked_sub(1) {
            Some(index) => index,
            None => return ParamMatchResult::Type,
        },
        (false, true) => arg_index + 1,
        _ => arg_index,
    };
    let params = func.get_params();
    let param_type = match params.get(param_index).or_else(|| params.last()) {
        Some((name, ty)) if param_index < params.len() || name == "..." => {
            ty.clone().unwrap_or(LuaType::Any)
        }
        _ => return ParamMatchResult::Not,
    };
    classify_arg_match(db, &param_type, expr_type)
}

fn classify_arg_match(db: &DbIndex, param_type: &LuaType, expr_type: &LuaType) -> ParamMatchResult {
    if param_type.is_any() {
        ParamMatchResult::Any
    } else if expr_type.is_never() {
        // `Never` (bottom type) is the result of unreachable expressions such as
        // indexing a `nil` base; no such value exists at runtime, so it must not
        // reject an otherwise-matching signature (e.g. `Vector(nilVar[1], nilVar[2], nilVar[3])`).
        ParamMatchResult::Type
    } else if is_exact_param_match(param_type, expr_type) {
        ParamMatchResult::Exact
    } else if check_type_compact(db, param_type, expr_type).is_ok() {
        ParamMatchResult::Type
    } else if has_union_component_param_match(db, param_type, expr_type) {
        ParamMatchResult::UnionComponent
    } else {
        ParamMatchResult::Not
    }
}

/// How many call arguments `func` takes without a surplus.
fn arg_capacity(func: &LuaFunctionType, is_colon_call: bool) -> usize {
    let param_len = func.get_params().len();
    match (func.is_colon_define(), is_colon_call) {
        (true, false) => param_len.saturating_add(1),
        (false, true) => param_len.saturating_sub(1),
        _ => param_len,
    }
}

fn effective_arg_count(arg_count: usize, colon_define: bool, is_colon_call: bool) -> usize {
    match (colon_define, is_colon_call) {
        (true, false) => arg_count.saturating_sub(1),
        (false, true) => arg_count.saturating_add(1),
        _ => arg_count,
    }
}

fn is_exact_param_match(param_type: &LuaType, expr_type: &LuaType) -> bool {
    if param_type == expr_type {
        return true;
    }

    matches!(
        (param_type, expr_type),
        (LuaType::Number, LuaType::FloatConst(_))
            | (
                LuaType::Integer,
                LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)
            )
    )
}

fn has_union_component_param_match(
    db: &DbIndex,
    param_type: &LuaType,
    expr_type: &LuaType,
) -> bool {
    let LuaType::Union(union) = expr_type else {
        return false;
    };

    union.types().any(|component| {
        is_exact_param_match(param_type, component)
            || check_type_compact(db, param_type, component).is_ok()
    })
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum ParamMatchResult {
    Not,
    Any,
    UnionComponent,
    Type,
    Exact,
}
