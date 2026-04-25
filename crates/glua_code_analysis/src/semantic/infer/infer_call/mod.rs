use std::sync::Arc;

use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr, LuaSyntaxKind};
use rowan::TextRange;

use super::{
    super::{
        InferGuard, LuaInferCache, instantiate_type_generic, resolve_signature,
        resolve_signature_with_generic_params,
    },
    InferFailReason, InferResult,
};
use crate::compilation::analyzer::unresolve::get_wrapped_callable_target_expr;
use crate::{
    CacheEntry, DbIndex, InFiled, LuaArrayType, LuaFunctionType, LuaGenericType, LuaInstanceType,
    LuaIntersectionType, LuaOperatorMetaMethod, LuaOperatorOwner, LuaSignature, LuaSignatureId,
    LuaTupleType, LuaType, LuaTypeDeclId, LuaUnionType, ReturnTypeKind, VariadicType,
};
use crate::{
    InferGuardRef,
    semantic::{
        generic::{TypeSubstitutor, get_tpl_ref_extend_type, instantiate_doc_function},
        infer::narrow::get_type_at_call_expr_inline_cast,
        infer_expr_semantic_decl,
    },
};
use crate::{
    SemanticDeclGuard, SemanticDeclLevel, build_self_type, infer_self_type,
    instantiate_func_generic, instantiate_func_generic_with_params, semantic::infer_expr,
};
use infer_require::infer_require_call;
use infer_setmetatable::infer_setmetatable_call;

mod infer_require;
mod infer_setmetatable;

pub type InferCallFuncResult = Result<Arc<LuaFunctionType>, InferFailReason>;

pub fn infer_call_expr_func(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: LuaCallExpr,
    call_expr_type: LuaType,
    infer_guard: &InferGuardRef,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    let syntax_id = call_expr.get_syntax_id();
    let key = (syntax_id, args_count, call_expr_type.clone());
    if let Some(cache) = cache.call_cache.get(&key) {
        match cache {
            CacheEntry::Cache(ty) => return Ok(ty.clone()),
            _ => return Err(InferFailReason::RecursiveInfer),
        }
    }

    cache.call_cache.insert(key.clone(), CacheEntry::Ready);
    let result = match &call_expr_type {
        LuaType::DocFunction(func) => {
            infer_doc_function(db, cache, func, call_expr.clone(), args_count)
        }
        LuaType::Signature(signature_id) => {
            infer_signature_doc_function(db, cache, *signature_id, call_expr.clone(), args_count)
        }
        LuaType::Def(type_def_id) => infer_type_doc_function(
            db,
            cache,
            type_def_id.clone(),
            call_expr.clone(),
            &call_expr_type,
            infer_guard,
            args_count,
        ),
        LuaType::Ref(type_ref_id) => infer_type_doc_function(
            db,
            cache,
            type_ref_id.clone(),
            call_expr.clone(),
            &call_expr_type,
            infer_guard,
            args_count,
        ),
        LuaType::Generic(generic) => infer_generic_type_doc_function(
            db,
            cache,
            generic,
            call_expr.clone(),
            infer_guard,
            args_count,
        ),
        LuaType::Instance(inst) => infer_instance_type_doc_function(db, inst),
        LuaType::TableConst(meta_table) => infer_table_type_doc_function(db, meta_table.clone()),
        LuaType::TplRef(_) | LuaType::ConstTplRef(_) | LuaType::StrTplRef(_) => infer_tpl_ref_call(
            db,
            cache,
            call_expr.clone(),
            &call_expr_type,
            infer_guard,
            args_count,
        ),
        LuaType::Union(union) => {
            // 此时我们将其视为泛型实例化联合体
            if union
                .into_vec()
                .iter()
                .all(|t| matches!(t, LuaType::DocFunction(_)))
            {
                infer_generic_doc_function_union(db, cache, union, call_expr.clone(), args_count)
            } else {
                infer_union(db, cache, union, call_expr.clone(), args_count)
            }
        }
        _ => Err(InferFailReason::None),
    };
    let result = match result {
        Ok(func_ty) if wrapped_setmetatable_fallback_would_help(func_ty.as_ref()) => {
            infer_wrapped_setmetatable_call(
                db,
                cache,
                &call_expr,
                &call_expr_type,
                infer_guard,
                args_count,
            )
            .unwrap_or(Ok(func_ty))
        }
        Err(reason @ InferFailReason::None)
        | Err(reason @ InferFailReason::UnResolveOperatorCall) => infer_wrapped_setmetatable_call(
            db,
            cache,
            &call_expr,
            &call_expr_type,
            infer_guard,
            args_count,
        )
        .unwrap_or(Err(reason)),
        other => other,
    };

    let result = if let Ok(func_ty) = result {
        let func_ty = match func_ty.get_ret() {
            LuaType::Call(_) => {
                match instantiate_func_generic(db, cache, func_ty.as_ref(), call_expr.clone()) {
                    Ok(func_ty) => Arc::new(func_ty),
                    Err(_) => func_ty,
                }
            }
            _ => func_ty,
        };

        let func_ret = func_ty.get_ret();
        match func_ret {
            LuaType::TypeGuard(_) => Ok(func_ty),
            _ => unwrapp_return_type(db, cache, func_ret.clone(), call_expr).map(|new_ret| {
                LuaFunctionType::new(
                    func_ty.get_async_state(),
                    func_ty.is_colon_define(),
                    func_ty.is_variadic(),
                    func_ty.get_params().to_vec(),
                    new_ret,
                )
                .into()
            }),
        }
    } else {
        result
    };

    match &result {
        Ok(func_ty) => {
            cache
                .call_cache
                .insert(key, CacheEntry::Cache(func_ty.clone()));
        }
        Err(r) if r.is_need_resolve() => {
            cache.call_cache.remove(&key);
        }
        Err(InferFailReason::None) => {
            cache.call_cache.remove(&key);
        }
        _ => {}
    }

    result
}

fn infer_wrapped_setmetatable_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    call_expr_type: &LuaType,
    infer_guard: &InferGuardRef,
    args_count: Option<usize>,
) -> Option<InferCallFuncResult> {
    match call_expr_type {
        LuaType::TableConst(_) | LuaType::Instance(_) => {}
        _ => return None,
    }

    let prefix_expr = call_expr.get_prefix_expr()?;
    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;
    let target_expr = get_wrapped_callable_target_expr(db, semantic_decl)?;
    let target_type =
        normalize_wrapped_callable_target_type(db, infer_expr(db, cache, target_expr).ok()?);
    Some(infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        target_type,
        infer_guard,
        args_count,
    ))
}

fn normalize_wrapped_callable_target_type(db: &DbIndex, target_type: LuaType) -> LuaType {
    match target_type {
        LuaType::Signature(signature_id) => db
            .get_signature_index()
            .get(&signature_id)
            .map(|signature| {
                if signature.has_special_call_params() {
                    LuaType::Signature(signature_id)
                } else {
                    LuaType::DocFunction(signature.to_call_operator_func_type())
                }
            })
            .unwrap_or(LuaType::Signature(signature_id)),
        other => other,
    }
}

fn wrapped_setmetatable_fallback_would_help(func_ty: &LuaFunctionType) -> bool {
    matches!(func_ty.get_ret(), LuaType::Unknown | LuaType::Nil) || func_ty.get_ret().contain_tpl()
}

fn infer_tpl_ref_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: LuaCallExpr,
    call_expr_type: &LuaType,
    infer_guard: &InferGuardRef,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let extend_type = get_tpl_ref_extend_type(db, cache, call_expr_type, prefix_expr, 0)
        .ok_or(InferFailReason::None)?;
    if &extend_type == call_expr_type {
        return Err(InferFailReason::None);
    }
    infer_call_expr_func(db, cache, call_expr, extend_type, infer_guard, args_count)
}

fn infer_doc_function(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func: &LuaFunctionType,
    call_expr: LuaCallExpr,
    _: Option<usize>,
) -> InferCallFuncResult {
    if func.contain_tpl() {
        let result = instantiate_func_generic(db, cache, func, call_expr.clone())?;
        return Ok(Arc::new(result));
    }

    // Handle self-type substitution for functions with SelfInfer in return type
    // (e.g., tableof<self>). This covers cases like `local getTable = Entity.GetTable; getTable(self)`
    if func.contain_self() {
        let self_type = infer_self_type(db, cache, &call_expr).or_else(|| {
            // For regular function calls (not method calls), infer self type from first arg
            let args_list = call_expr.get_args_list()?;
            let first_arg = args_list.get_args().next()?;
            let arg_type = infer_expr(db, cache, first_arg).ok()?;
            Some(build_self_type(db, &arg_type))
        });
        if let Some(self_type) = self_type {
            let mut substitutor = crate::semantic::generic::TypeSubstitutor::new();
            substitutor.add_self_type(self_type);
            if let LuaType::DocFunction(f) = instantiate_doc_function(db, func, &substitutor) {
                return Ok(f);
            }
        }
    }

    Ok(func.clone().into())
}

fn infer_generic_doc_function_union(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    union: &LuaUnionType,
    call_expr: LuaCallExpr,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    let overloads = union
        .into_vec()
        .iter()
        .filter_map(|typ| match typ {
            LuaType::DocFunction(f) => Some(f.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    resolve_signature(db, cache, overloads, call_expr.clone(), false, args_count)
}

fn infer_signature_doc_function(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature_id: LuaSignatureId,
    call_expr: LuaCallExpr,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    let signature = db
        .get_signature_index()
        .get(&signature_id)
        .ok_or(InferFailReason::None)?;
    if !signature.is_resolve_return() {
        return Err(InferFailReason::UnResolveSignatureReturn(signature_id));
    }
    let is_generic = signature_is_generic(db, cache, &signature, &call_expr).unwrap_or(false);
    let overloads = &signature.overloads;
    if overloads.is_empty() {
        let mut fake_doc_function = LuaFunctionType::new(
            signature.async_state,
            signature.is_colon_define,
            signature.is_vararg,
            signature.get_type_params(),
            signature.get_return_type(),
        );
        if is_generic {
            fake_doc_function = instantiate_func_generic_with_params(
                db,
                cache,
                &fake_doc_function,
                call_expr,
                Some(&signature.generic_params),
            )?;
        }

        Ok(apply_signature_return_kinds_to_function(
            signature,
            &fake_doc_function,
        ))
    } else {
        let mut new_overloads = signature.overloads.clone();
        let fake_doc_function = LuaFunctionType::new(
            signature.async_state,
            signature.is_colon_define,
            signature.is_vararg,
            signature.get_type_params(),
            signature.get_return_type(),
        );
        new_overloads.push(apply_signature_return_kinds_to_function(
            signature,
            &fake_doc_function,
        ));

        resolve_signature_with_generic_params(
            db,
            cache,
            new_overloads,
            call_expr.clone(),
            is_generic,
            args_count,
            Some(&signature.generic_params),
        )
    }
}

fn infer_type_doc_function(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    type_id: LuaTypeDeclId,
    call_expr: LuaCallExpr,
    call_expr_type: &LuaType,
    infer_guard: &InferGuardRef,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    infer_guard.check(&type_id)?;
    let type_decl = db
        .get_type_index()
        .get_type_decl(&type_id)
        .ok_or_else(|| InferFailReason::UnResolveTypeDecl(type_id.clone()))?;
    if type_decl.is_alias() {
        let origin_type = type_decl
            .get_alias_origin(db, None)
            .ok_or(InferFailReason::None)?;
        return infer_call_expr_func(
            db,
            cache,
            call_expr,
            origin_type.clone(),
            infer_guard,
            args_count,
        );
    } else if type_decl.is_enum() {
        return Err(InferFailReason::None);
    }

    let operator_index = db.get_operator_index();
    let operator_ids = operator_index
        .get_operators(&type_id.clone().into(), LuaOperatorMetaMethod::Call)
        .ok_or(InferFailReason::UnResolveOperatorCall)?;
    let mut overloads = Vec::new();
    for overload_id in operator_ids {
        let operator = operator_index
            .get_operator(overload_id)
            .ok_or(InferFailReason::None)?;
        let func = operator.get_operator_func(db);
        match func {
            LuaType::DocFunction(f) => {
                if f.contain_self() {
                    let mut substitutor = TypeSubstitutor::new();
                    let self_type = build_self_type(db, call_expr_type);
                    substitutor.add_self_type(self_type);
                    if let LuaType::DocFunction(f) = instantiate_doc_function(db, &f, &substitutor)
                    {
                        overloads.push(f);
                    }
                } else if f.contain_tpl() {
                    let result = instantiate_func_generic(db, cache, &f, call_expr.clone())?;
                    overloads.push(Arc::new(result));
                } else {
                    overloads.push(f.clone());
                }
            }
            LuaType::Signature(signature_id) => {
                let signature = db
                    .get_signature_index()
                    .get(&signature_id)
                    .ok_or(InferFailReason::None)?;
                if !signature.is_resolve_return() {
                    return Err(InferFailReason::UnResolveSignatureReturn(signature_id));
                }

                overloads.push(apply_signature_return_kinds_to_function(
                    signature,
                    signature.to_call_operator_func_type().as_ref(),
                ));
            }
            _ => {}
        }
    }

    resolve_signature(db, cache, overloads, call_expr.clone(), false, args_count)
}

fn infer_generic_type_doc_function(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    generic: &LuaGenericType,
    call_expr: LuaCallExpr,
    infer_guard: &InferGuardRef,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    let type_id = generic.get_base_type_id();
    infer_guard.check(&type_id)?;
    let generic_params = generic.get_params();

    let type_decl = db
        .get_type_index()
        .get_type_decl(&type_id)
        .ok_or_else(|| InferFailReason::UnResolveTypeDecl(type_id.clone()))?;
    let substitutor = if type_decl.is_alias() {
        TypeSubstitutor::from_alias(db, generic_params.clone(), type_id.clone())
    } else {
        TypeSubstitutor::from_type_decl(db, generic_params.clone(), type_id.clone())
    };
    if type_decl.is_alias() {
        let origin_type = type_decl
            .get_alias_origin(db, Some(&substitutor))
            .ok_or(InferFailReason::None)?;
        return infer_call_expr_func(
            db,
            cache,
            call_expr,
            origin_type.clone(),
            infer_guard,
            args_count,
        );
    } else if type_decl.is_enum() {
        return Err(InferFailReason::None);
    }

    let operator_index = db.get_operator_index();
    let operator_ids = operator_index
        .get_operators(&type_id.into(), LuaOperatorMetaMethod::Call)
        .ok_or(InferFailReason::None)?;
    let mut overloads = Vec::new();
    for overload_id in operator_ids {
        let operator = operator_index
            .get_operator(overload_id)
            .ok_or(InferFailReason::None)?;
        let func = operator.get_operator_func(db);
        match func {
            LuaType::DocFunction(_) => {
                let new_f = instantiate_type_generic(db, &func, &substitutor);
                if let LuaType::DocFunction(f) = new_f {
                    overloads.push(f.clone());
                }
            }
            LuaType::Signature(signature_id) => {
                let signature = db
                    .get_signature_index()
                    .get(&signature_id)
                    .ok_or(InferFailReason::None)?;
                if !signature.is_resolve_return() {
                    return Err(InferFailReason::UnResolveSignatureReturn(signature_id));
                }

                let typ = LuaType::DocFunction(signature.to_call_operator_func_type());
                let new_f = instantiate_type_generic(db, &typ, &substitutor);
                if let LuaType::DocFunction(f) = new_f {
                    overloads.push(apply_signature_return_kinds_to_function(signature, &f));
                }
                // todo: support overload?
            }
            _ => {}
        }
    }

    resolve_signature(db, cache, overloads, call_expr.clone(), false, args_count)
}

fn infer_instance_type_doc_function(
    db: &DbIndex,
    instance: &LuaInstanceType,
) -> InferCallFuncResult {
    let base = instance.get_base();
    let base_table = match &base {
        LuaType::TableConst(meta_table) => meta_table.clone(),
        LuaType::Instance(inst) => {
            return infer_instance_type_doc_function(db, inst);
        }
        _ => return Err(InferFailReason::None),
    };

    infer_table_type_doc_function(db, base_table)
}

fn infer_table_type_doc_function(db: &DbIndex, table: InFiled<TextRange>) -> InferCallFuncResult {
    let meta_table = db
        .get_metatable_index()
        .get(&table)
        .ok_or(InferFailReason::None)?;

    let meta_table_owner = LuaOperatorOwner::Table(meta_table.clone());

    let call_operators = db
        .get_operator_index()
        .get_operators(&meta_table_owner, LuaOperatorMetaMethod::Call)
        .ok_or(InferFailReason::None)?;

    // only first one is valid
    for operator_id in call_operators {
        let operator = db
            .get_operator_index()
            .get_operator(operator_id)
            .ok_or(InferFailReason::None)?;
        let func = operator.get_operator_func(db);
        match func {
            LuaType::DocFunction(func) => {
                return Ok(func);
            }
            LuaType::Signature(signature_id) => {
                let signature = db
                    .get_signature_index()
                    .get(&signature_id)
                    .ok_or(InferFailReason::None)?;
                if !signature.is_resolve_return() {
                    return Err(InferFailReason::UnResolveSignatureReturn(signature_id));
                }

                return Ok(apply_signature_return_kinds_to_function(
                    signature,
                    signature.to_call_operator_func_type().as_ref(),
                ));
            }
            _ => {}
        }
    }

    Err(InferFailReason::None)
}

fn infer_union(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    union: &LuaUnionType,
    call_expr: LuaCallExpr,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    // 此时一般是 signature + doc_function 的联合体
    let mut all_overloads = Vec::new();
    let mut base_signatures = Vec::new();

    for ty in union.into_vec() {
        match ty {
            LuaType::Signature(signature_id) => {
                if let Some(signature) = db.get_signature_index().get(&signature_id) {
                    // 处理 overloads
                    let overloads = if signature.is_generic() {
                        signature
                            .overloads
                            .iter()
                            .map(|func| {
                                Ok(Arc::new(instantiate_func_generic(
                                    db,
                                    cache,
                                    func,
                                    call_expr.clone(),
                                )?))
                            })
                            .collect::<Result<Vec<_>, _>>()?
                    } else {
                        signature.overloads.clone()
                    };
                    all_overloads.extend(overloads);

                    // 处理 signature 本身的函数类型
                    let mut fake_doc_function = LuaFunctionType::new(
                        signature.async_state,
                        signature.is_colon_define,
                        signature.is_vararg,
                        signature.get_type_params(),
                        signature.get_return_type(),
                    );
                    if signature.is_generic() {
                        fake_doc_function = instantiate_func_generic(
                            db,
                            cache,
                            &fake_doc_function,
                            call_expr.clone(),
                        )?;
                    }
                    base_signatures.push(apply_signature_return_kinds_to_function(
                        signature,
                        &fake_doc_function,
                    ));
                }
            }
            LuaType::DocFunction(func) => {
                let func_to_push = if func.contain_tpl() {
                    Arc::new(instantiate_func_generic(
                        db,
                        cache,
                        &func,
                        call_expr.clone(),
                    )?)
                } else {
                    func.clone()
                };
                base_signatures.push(func_to_push);
            }
            _ => {}
        }
    }

    all_overloads.extend(base_signatures);
    if all_overloads.is_empty() {
        return Err(InferFailReason::None);
    }
    resolve_signature(db, cache, all_overloads, call_expr, false, args_count)
}

pub(crate) fn unwrapp_return_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_type: LuaType,
    call_expr: LuaCallExpr,
) -> InferResult {
    match &return_type {
        LuaType::Table => {
            let id = InFiled {
                file_id: cache.get_file_id(),
                value: call_expr.get_range(),
            };

            return Ok(LuaType::TableConst(id));
        }
        LuaType::TableConst(inst) => {
            if is_need_wrap_instance(cache, &call_expr, inst) {
                let id = InFiled {
                    file_id: cache.get_file_id(),
                    value: call_expr.get_range(),
                };

                return Ok(LuaType::Instance(
                    LuaInstanceType::new(return_type.clone(), id).into(),
                ));
            }

            return Ok(return_type);
        }
        LuaType::Instance(inst) => {
            if is_need_wrap_instance(cache, &call_expr, inst.get_range()) {
                let id = InFiled {
                    file_id: cache.get_file_id(),
                    value: call_expr.get_range(),
                };

                return Ok(LuaType::Instance(
                    LuaInstanceType::new(return_type.clone(), id).into(),
                ));
            }

            return Ok(return_type);
        }

        LuaType::Variadic(variadic) => {
            if is_last_call_expr(&call_expr) {
                return Ok(return_type);
            }

            return match variadic.get_type(0) {
                Some(ty) => Ok(ty.clone()),
                None => Ok(LuaType::Nil),
            };
        }
        LuaType::SelfInfer => {
            if let Some(self_type) = infer_self_type(db, cache, &call_expr) {
                return Ok(self_type);
            }
        }
        LuaType::TableOf(inner) if matches!(inner.as_ref(), LuaType::SelfInfer) => {
            if let Some(self_type) = infer_self_type(db, cache, &call_expr) {
                return Ok(LuaType::TableOf(self_type.into()));
            }
        }
        LuaType::TypeGuard(_) => return Ok(LuaType::Boolean),
        _ => {}
    }

    Ok(return_type)
}

fn is_need_wrap_instance(
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    inst: &InFiled<TextRange>,
) -> bool {
    if cache.get_file_id() != inst.file_id {
        return true;
    }

    !call_expr.get_range().contains(inst.value.start())
}

fn is_last_call_expr(call_expr: &LuaCallExpr) -> bool {
    let mut opt_parent = call_expr.syntax().parent();
    while let Some(parent) = &opt_parent {
        match parent.kind().into() {
            LuaSyntaxKind::AssignStat
            | LuaSyntaxKind::LocalStat
            | LuaSyntaxKind::ReturnStat
            | LuaSyntaxKind::TableArrayExpr
            | LuaSyntaxKind::CallArgList => {
                let next_expr = call_expr.syntax().next_sibling();
                return next_expr.is_none();
            }
            LuaSyntaxKind::TableFieldValue => {
                opt_parent = parent.parent();
            }
            LuaSyntaxKind::ForRangeStat => return true,
            _ => return false,
        }
    }

    false
}

pub fn infer_call_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: LuaCallExpr,
) -> InferResult {
    if call_expr.is_require() {
        return infer_require_call(db, cache, call_expr);
    } else if call_expr.is_setmetatable() {
        return infer_setmetatable_call(db, cache, call_expr);
    }

    check_can_infer(db, cache, &call_expr)?;

    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let prefix_type = infer_expr(db, cache, prefix_expr)?;
    let ret_type = infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        prefix_type,
        &InferGuard::new(),
        None,
    )?
    .get_ret()
    .clone();

    if let Some(tree) = db.get_flow_index().get_flow_tree(&cache.get_file_id())
        && let Some(flow_id) = tree.get_flow_id(call_expr.get_syntax_id())
        && let Some(flow_ret_type) =
            get_type_at_call_expr_inline_cast(db, cache, tree, call_expr, flow_id, ret_type.clone())
    {
        return Ok(flow_ret_type);
    }

    Ok(ret_type)
}

fn check_can_infer(
    db: &DbIndex,
    cache: &LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Result<(), InferFailReason> {
    let call_args = call_expr
        .get_args_list()
        .ok_or(InferFailReason::None)?
        .get_args();
    for arg in call_args {
        if let LuaExpr::ClosureExpr(closure) = arg {
            let sig_id = LuaSignatureId::from_closure(cache.get_file_id(), &closure);
            let signature = db
                .get_signature_index()
                .get(&sig_id)
                .ok_or(InferFailReason::None)?;
            if !signature.is_resolve_return() {
                return Err(InferFailReason::UnResolveSignatureReturn(sig_id));
            }
        }
    }

    Ok(())
}

fn signature_is_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature: &LuaSignature,
    call_expr: &LuaCallExpr,
) -> Option<bool> {
    if signature.is_generic() {
        return Some(true);
    }
    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    let prefix_type = infer_expr(db, cache, index_expr.get_prefix_expr()?).ok()?;
    match prefix_type {
        // 对于 Generic 直接认为是泛型
        LuaType::Generic(_) => Some(true),
        _ => Some(prefix_type.contain_tpl()),
    }
}

fn apply_signature_return_kinds_to_function(
    signature: &LuaSignature,
    function: &LuaFunctionType,
) -> Arc<LuaFunctionType> {
    let return_type = apply_signature_return_kinds(signature, function.get_ret().clone());

    Arc::new(LuaFunctionType::new(
        function.get_async_state(),
        function.is_colon_define(),
        function.is_variadic(),
        function.get_params().to_vec(),
        return_type,
    ))
}

fn apply_signature_return_kinds(signature: &LuaSignature, return_type: LuaType) -> LuaType {
    match signature.return_docs.len() {
        0 => return_type,
        1 => apply_return_kind(signature.return_docs[0].return_kind, return_type),
        _ => match return_type {
            LuaType::Variadic(variadic) => match variadic.as_ref() {
                VariadicType::Base(base) => LuaType::Variadic(
                    VariadicType::Base(apply_return_kind(
                        signature.return_docs[0].return_kind,
                        base.clone(),
                    ))
                    .into(),
                ),
                VariadicType::Multi(types) => LuaType::Variadic(
                    VariadicType::Multi(
                        types
                            .iter()
                            .enumerate()
                            .map(|(idx, typ)| {
                                let kind = signature
                                    .return_docs
                                    .get(idx)
                                    .map(|info| info.return_kind)
                                    .unwrap_or(ReturnTypeKind::Reference);
                                apply_return_kind(kind, typ.clone())
                            })
                            .collect(),
                    )
                    .into(),
                ),
            },
            _ => apply_return_kind(signature.return_docs[0].return_kind, return_type),
        },
    }
}

fn apply_return_kind(return_kind: ReturnTypeKind, return_type: LuaType) -> LuaType {
    match return_kind {
        ReturnTypeKind::Definition => apply_definition_return_type(return_type),
        ReturnTypeKind::Instance | ReturnTypeKind::Reference => return_type,
    }
}

fn apply_definition_return_type(return_type: LuaType) -> LuaType {
    match return_type {
        LuaType::Ref(type_id) => LuaType::Def(type_id),
        LuaType::Array(array) => LuaType::Array(
            LuaArrayType::from_base_type(apply_definition_return_type(array.get_base().clone()))
                .into(),
        ),
        LuaType::Tuple(tuple) => LuaType::Tuple(
            LuaTupleType::new(
                tuple
                    .get_types()
                    .iter()
                    .cloned()
                    .map(apply_definition_return_type)
                    .collect(),
                tuple.status,
            )
            .into(),
        ),
        LuaType::Instance(instance) => LuaType::Instance(
            LuaInstanceType::new(
                apply_definition_return_type(instance.get_base().clone()),
                instance.get_range().clone(),
            )
            .into(),
        ),
        LuaType::TypeGuard(inner) => {
            LuaType::TypeGuard(apply_definition_return_type(inner.as_ref().clone()).into())
        }
        LuaType::TableOf(inner) => {
            LuaType::TableOf(apply_definition_return_type(inner.as_ref().clone()).into())
        }
        LuaType::Union(union) => LuaType::from_vec(
            union
                .into_vec()
                .iter()
                .cloned()
                .map(apply_definition_return_type)
                .collect(),
        ),
        LuaType::Intersection(intersection) => LuaType::Intersection(
            LuaIntersectionType::new(
                intersection
                    .get_types()
                    .iter()
                    .cloned()
                    .map(apply_definition_return_type)
                    .collect(),
            )
            .into(),
        ),
        LuaType::MultiLineUnion(multi_union) => LuaType::from_vec(
            multi_union
                .get_unions()
                .iter()
                .map(|(typ, _)| apply_definition_return_type(typ.clone()))
                .collect(),
        ),
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Base(base) => LuaType::Variadic(
                VariadicType::Base(apply_definition_return_type(base.clone())).into(),
            ),
            VariadicType::Multi(types) => LuaType::Variadic(
                VariadicType::Multi(
                    types
                        .iter()
                        .cloned()
                        .map(apply_definition_return_type)
                        .collect(),
                )
                .into(),
            ),
        },
        _ => return_type,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        InferFailReason, InferGuard, LuaType, VirtualWorkspace, semantic::infer_call_expr_func,
    };

    #[test]
    fn test_call_cache_non_callable_not_sticky() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def("local i = 1\n i()\n");
        let call_expr = ws.get_node::<glua_parser::LuaCallExpr>(file_id);
        let semantic_model = ws.analysis.compilation.get_semantic_model(file_id).unwrap();
        let db = semantic_model.get_db();
        let mut cache = semantic_model.get_cache().borrow_mut();
        let call_expr_type = LuaType::IntegerConst(1);

        let _ = infer_call_expr_func(
            db,
            &mut cache,
            call_expr.clone(),
            call_expr_type.clone(),
            &InferGuard::new(),
            None,
        );
        let second = infer_call_expr_func(
            db,
            &mut cache,
            call_expr,
            call_expr_type,
            &InferGuard::new(),
            None,
        );

        assert!(!matches!(second, Err(InferFailReason::RecursiveInfer)));
    }
}
