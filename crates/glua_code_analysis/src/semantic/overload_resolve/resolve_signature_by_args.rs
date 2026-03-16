use std::sync::Arc;

use crate::{
    InferFailReason, check_type_compact,
    db_index::{DbIndex, LuaFunctionType, LuaType},
    semantic::infer::InferCallFuncResult,
};

pub fn resolve_signature_by_args(
    db: &DbIndex,
    overloads: &[Arc<LuaFunctionType>],
    expr_types: &[LuaType],
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
    for (arg_index, expr_type) in expr_types.iter().enumerate() {
        let mut current_match_result = ParamMatchResult::Not;
        let mut skipped_implicit_self = false;
        for opt_func in &mut need_resolve_funcs {
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

            let match_result = if param_type.is_any() {
                ParamMatchResult::Any
            } else if is_exact_param_match(&param_type, expr_type) {
                ParamMatchResult::Exact
            } else if check_type_compact(db, &param_type, expr_type).is_ok() {
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

    let mut rest_need_resolve_funcs = need_resolve_funcs
        .iter()
        .filter_map(|it| it.clone())
        .map(Some)
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
            } else if param_type.is_nullable() {
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

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum ParamMatchResult {
    Not,
    Any,
    Type,
    Exact,
}
