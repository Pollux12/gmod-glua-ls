use crate::{
    TypeSubstitutor,
    db_index::{
        LuaFunctionType, LuaOperatorMetaMethod, LuaSignatureId, LuaType, LuaTypeDeclId,
        VariadicType,
    },
    semantic::type_check::type_check_context::TypeCheckContext,
};

use super::{
    TypeCheckResult, check_general_type_compact, type_check_fail_reason::TypeCheckFailReason,
    type_check_guard::TypeCheckGuard,
};

pub fn check_doc_func_type_compact(
    context: &mut TypeCheckContext,
    source_func: &LuaFunctionType,
    compact_type: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    // TODO: 缓存以提高性能
    // 如果是泛型+不包含模板参数+alias, 那么尝试实例化再检查
    if let LuaType::Generic(generic) = compact_type {
        if !generic.contain_tpl() {
            let base_id = generic.get_base_type_id();
            if let Some(decl) = context.db.get_type_index().get_type_decl(&base_id)
                && decl.is_alias()
            {
                let substitutor =
                    TypeSubstitutor::from_alias(generic.get_params().clone(), base_id.clone());
                if let Some(alias_origin) = decl.get_alias_origin(context.db, Some(&substitutor)) {
                    return check_general_type_compact(
                        context,
                        &LuaType::DocFunction(source_func.clone().into()),
                        &alias_origin,
                        check_guard.next_level()?,
                    );
                }
            }
        }
    }
    match compact_type {
        LuaType::DocFunction(compact_func) => {
            check_doc_func_type_compact_for_params(context, source_func, compact_func, check_guard)
        }
        LuaType::Signature(signature_id) => check_doc_func_type_compact_for_signature(
            context,
            source_func,
            signature_id,
            check_guard,
        ),
        LuaType::Ref(type_id) => {
            check_doc_func_type_compact_for_custom_type(context, source_func, type_id, check_guard)
        }
        LuaType::Def(type_id) => {
            check_doc_func_type_compact_for_custom_type(context, source_func, type_id, check_guard)
        }
        LuaType::Union(union) => {
            for union_type in union.types() {
                check_doc_func_type_compact(
                    context,
                    source_func,
                    union_type,
                    check_guard.next_level()?,
                )?;
            }

            Ok(())
        }
        LuaType::Function => Ok(()),
        _ => Err(TypeCheckFailReason::TypeNotMatch),
    }
}

fn check_doc_func_type_compact_for_params(
    context: &mut TypeCheckContext,
    source_func: &LuaFunctionType,
    compact_func: &LuaFunctionType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    let mut source_params: Vec<(String, Option<LuaType>)> = source_func.get_params().to_vec();
    let mut compact_params: Vec<(String, Option<LuaType>)> = compact_func.get_params().to_vec();

    // colon-defined methods have an implicit `self` not stored in params; expand it for comparison
    if source_func.is_colon_define() {
        source_params.insert(0, ("self".to_string(), None));
    }

    if compact_func.is_colon_define() {
        compact_params.insert(0, ("self".to_string(), None));
    }

    let compact_len = compact_params.len();

    for i in 0..compact_len {
        let source_param = match source_params.get(i) {
            Some(p) => p,
            None => {
                break;
            }
        };
        let compact_param = &compact_params[i];

        let source_param_type = &source_param.1;
        // too many complex session to handle varargs
        if source_param.0 == "..." {
            check_doc_func_type_compact_for_varargs(
                context,
                source_param_type,
                &compact_params[i..],
                check_guard.next_level()?,
            )?;
        }

        if compact_param.0 == "..." {
            break;
        }

        let compact_param_type = &compact_param.1;

        if let (Some(source_type), Some(compact_type)) = (source_param_type, compact_param_type) {
            match check_general_type_compact(
                context,
                source_type,
                compact_type,
                check_guard.next_level()?,
            ) {
                Ok(()) => {}
                Err(e) if e.is_type_not_match() => {
                    if i == 0 && source_type.is_self_infer() && compact_param.0 == "self" {
                        continue;
                    }
                    // add error message
                    return Err(e);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    check_doc_func_returns_compact(
        context,
        source_func.get_ret(),
        compact_func.get_ret(),
        check_guard.next_level()?,
    )?;

    Ok(())
}

fn check_doc_func_returns_compact(
    context: &mut TypeCheckContext,
    expected: &LuaType,
    actual: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    if actual.is_never() || expected.is_nil() {
        return Ok(());
    }

    match expected {
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Base(expected_type) => check_actual_return_tail(
                context,
                expected_type,
                actual,
                0,
                check_guard.next_level()?,
            ),
            VariadicType::Multi(expected_types) => check_expected_return_types(
                context,
                expected_types,
                actual,
                check_guard.next_level()?,
            ),
        },
        _ => check_expected_return_types(
            context,
            std::slice::from_ref(expected),
            actual,
            check_guard.next_level()?,
        ),
    }
}

fn check_expected_return_types(
    context: &mut TypeCheckContext,
    expected_types: &[LuaType],
    actual: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    for (index, expected_type) in expected_types.iter().enumerate() {
        if let LuaType::Variadic(variadic) = expected_type
            && let VariadicType::Base(expected_type) = variadic.as_ref()
        {
            return check_actual_return_tail(
                context,
                expected_type,
                actual,
                index,
                check_guard.next_level()?,
            );
        }

        match actual_return_slot(actual, index) {
            ActualReturnSlot::Missing => {
                if !expected_type.is_optional() {
                    return Err(TypeCheckFailReason::TypeNotMatch);
                }
            }
            ActualReturnSlot::Guaranteed(actual_type) => {
                check_general_type_compact(
                    context,
                    expected_type,
                    actual_type,
                    check_guard.next_level()?,
                )?;
            }
            ActualReturnSlot::Variadic(actual_type) => {
                if !expected_type.is_optional() {
                    return Err(TypeCheckFailReason::TypeNotMatch);
                }
                check_general_type_compact(
                    context,
                    expected_type,
                    actual_type,
                    check_guard.next_level()?,
                )?;
            }
        }
    }

    Ok(())
}

enum ActualReturnSlot<'a> {
    Missing,
    Guaranteed(&'a LuaType),
    Variadic(&'a LuaType),
}

fn actual_return_slot(actual: &LuaType, index: usize) -> ActualReturnSlot<'_> {
    match actual {
        LuaType::Nil => ActualReturnSlot::Missing,
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Base(actual_type) => ActualReturnSlot::Variadic(actual_type),
            VariadicType::Multi(actual_types) => actual_return_slot_from_types(actual_types, index),
        },
        _ if index == 0 => ActualReturnSlot::Guaranteed(actual),
        _ => ActualReturnSlot::Missing,
    }
}

fn actual_return_slot_from_types(actual_types: &[LuaType], index: usize) -> ActualReturnSlot<'_> {
    if let Some(actual_type) = actual_types.get(index) {
        return match actual_type {
            LuaType::Variadic(variadic) => match variadic.as_ref() {
                VariadicType::Base(actual_type) => ActualReturnSlot::Variadic(actual_type),
                VariadicType::Multi(actual_types) => actual_return_slot_from_types(actual_types, 0),
            },
            _ => ActualReturnSlot::Guaranteed(actual_type),
        };
    }

    let tail_start = actual_types.len().saturating_sub(1);
    match actual_types.last() {
        Some(LuaType::Variadic(variadic)) => match variadic.as_ref() {
            VariadicType::Base(actual_type) => ActualReturnSlot::Variadic(actual_type),
            VariadicType::Multi(nested_types) => {
                let nested_index = index.saturating_sub(tail_start);
                actual_return_slot_from_types(nested_types, nested_index)
            }
        },
        _ => ActualReturnSlot::Missing,
    }
}

fn check_actual_return_tail(
    context: &mut TypeCheckContext,
    expected_type: &LuaType,
    actual: &LuaType,
    start: usize,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    match actual {
        LuaType::Nil => Ok(()),
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Base(actual_type) => check_general_type_compact(
                context,
                expected_type,
                actual_type,
                check_guard.next_level()?,
            ),
            VariadicType::Multi(actual_types) => check_actual_return_types_tail(
                context,
                expected_type,
                actual_types,
                start,
                check_guard.next_level()?,
            ),
        },
        _ if start == 0 => {
            check_general_type_compact(context, expected_type, actual, check_guard.next_level()?)
        }
        _ => Ok(()),
    }
}

fn check_actual_return_types_tail(
    context: &mut TypeCheckContext,
    expected_type: &LuaType,
    actual_types: &[LuaType],
    start: usize,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    for actual_type in actual_types.iter().skip(start) {
        match actual_type {
            LuaType::Variadic(variadic) => match variadic.as_ref() {
                VariadicType::Base(actual_type) => {
                    return check_general_type_compact(
                        context,
                        expected_type,
                        actual_type,
                        check_guard.next_level()?,
                    );
                }
                VariadicType::Multi(actual_types) => {
                    check_actual_return_types_tail(
                        context,
                        expected_type,
                        actual_types,
                        0,
                        check_guard.next_level()?,
                    )?;
                }
            },
            _ => check_general_type_compact(
                context,
                expected_type,
                actual_type,
                check_guard.next_level()?,
            )?,
        }
    }

    Ok(())
}

fn check_doc_func_type_compact_for_varargs(
    context: &mut TypeCheckContext,
    varargs: &Option<LuaType>,
    compact_params: &[(String, Option<LuaType>)],
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    if let Some(varargs_type) = varargs {
        for compact_param in compact_params {
            let compact_param_type = &compact_param.1;
            if let Some(compact_param_type) = compact_param_type {
                check_general_type_compact(
                    context,
                    varargs_type,
                    compact_param_type,
                    check_guard.next_level()?,
                )?;
            }
        }
    }

    Ok(())
}

fn check_doc_func_type_compact_for_signature(
    context: &mut TypeCheckContext,
    source_func: &LuaFunctionType,
    signature_id: &LuaSignatureId,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    let signature = context
        .db
        .get_signature_index()
        .get(signature_id)
        .ok_or(TypeCheckFailReason::TypeNotMatch)?;

    // dotnot check generic method
    if signature.is_generic() {
        return Ok(());
    }

    for overload_func in &signature.overloads {
        match check_doc_func_type_compact_for_params(
            context,
            source_func,
            overload_func,
            check_guard.next_level()?,
        ) {
            Ok(()) => {
                return Ok(());
            }
            Err(e) if e.is_type_not_match() => {
                // continue to check next overload
                continue;
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    let fake_doc_func = signature.to_doc_func_type();

    check_doc_func_type_compact_for_params(
        context,
        source_func,
        &fake_doc_func,
        check_guard.next_level()?,
    )
}

// check type is callable
fn check_doc_func_type_compact_for_custom_type(
    context: &mut TypeCheckContext,
    source_func: &LuaFunctionType,
    custom_type_id: &LuaTypeDeclId,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    let type_decl = context
        .db
        .get_type_index()
        .get_type_decl(custom_type_id)
        .ok_or(TypeCheckFailReason::TypeNotMatch)?;

    if type_decl.is_class() {
        let call_operators = context
            .db
            .get_operator_index()
            .get_operators(&custom_type_id.clone().into(), LuaOperatorMetaMethod::Call)
            .ok_or(TypeCheckFailReason::TypeNotMatch)?;
        for operator_id in call_operators {
            let operator = context
                .db
                .get_operator_index()
                .get_operator(operator_id)
                .ok_or(TypeCheckFailReason::TypeNotMatch)?;
            let call_type = operator.get_operator_func(context.db);
            match call_type {
                LuaType::DocFunction(doc_func) => {
                    match check_doc_func_type_compact_for_params(
                        context,
                        source_func,
                        &doc_func,
                        check_guard.next_level()?,
                    ) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.is_type_not_match() => continue,
                        Err(e) => return Err(e),
                    }
                }
                LuaType::Signature(signature_id) => {
                    let signature = context
                        .db
                        .get_signature_index()
                        .get(&signature_id)
                        .ok_or(TypeCheckFailReason::TypeNotMatch)?;
                    let doc_f = signature.to_call_operator_func_type();
                    match check_doc_func_type_compact_for_params(
                        context,
                        source_func,
                        &doc_f,
                        check_guard.next_level()?,
                    ) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.is_type_not_match() => continue,
                        Err(e) => return Err(e),
                    }
                }
                _ => {}
            }
        }
    }

    Err(TypeCheckFailReason::TypeNotMatch)
}

pub fn check_sig_type_compact(
    context: &mut TypeCheckContext,
    sig_id: &LuaSignatureId,
    compact_type: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    let signature = context
        .db
        .get_signature_index()
        .get(sig_id)
        .ok_or(TypeCheckFailReason::TypeNotMatch)?;

    // cannot check generic method
    if signature.is_generic() {
        return Ok(());
    }

    let fake_doc_func = signature.to_doc_func_type();

    check_doc_func_type_compact(
        context,
        &fake_doc_func,
        compact_type,
        check_guard.next_level()?,
    )
}
