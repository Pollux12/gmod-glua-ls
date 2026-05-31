mod infer_binary;
mod infer_call;
mod infer_doc_type;
mod infer_fail_reason;
mod infer_index;
mod infer_name;
mod infer_table;
mod infer_unary;
mod narrow;
mod test;

use std::{ops::Deref, sync::Arc};

use glua_parser::{
    LuaAst, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaLiteralExpr, LuaLiteralToken,
    LuaTableExpr, LuaVarExpr, NumberResult,
};
use infer_binary::infer_binary_expr;
use infer_call::infer_call_expr;
pub use infer_call::infer_call_expr_func;
pub use infer_doc_type::{DocTypeInferContext, infer_doc_type};
pub use infer_fail_reason::InferFailReason;
pub(crate) use infer_index::check_iter_var_range;
pub use infer_index::infer_index_expr;
pub(crate) use infer_index::infer_member_by_member_key;
pub(crate) use infer_index::resolve_decl_backed_global_path_member_type;
use infer_name::infer_name_expr;
pub(crate) use infer_name::resolve_scoped_scripted_global_type_decl_id;
pub(crate) use infer_name::try_local_decl_initializer_fallback_type;
pub use infer_name::{find_self_decl_or_member_id, infer_param};
use infer_table::infer_table_expr;
pub use infer_table::{infer_table_field_value_should_be, infer_table_should_be};
use infer_unary::infer_unary_expr;
pub use narrow::VarRefId;
pub(crate) use narrow::{contains_gmod_null_type, get_var_expr_var_ref_id, remove_false_or_nil};

use rowan::TextRange;
use smol_str::SmolStr;

use crate::{
    InFiled, InferGuard, LuaMemberKey, VariadicType,
    db_index::{DbIndex, LuaOperator, LuaOperatorMetaMethod, LuaSignatureId, LuaType},
};

use super::{CacheEntry, LuaInferCache, member::infer_raw_member_type};

pub type InferResult = Result<LuaType, InferFailReason>;
pub use infer_call::InferCallFuncResult;

pub fn infer_expr(db: &DbIndex, cache: &mut LuaInferCache, expr: LuaExpr) -> InferResult {
    cache.prof_infer_expr_calls += 1;
    let syntax_id = expr.get_syntax_id();
    let key = syntax_id;
    if let Some(cache_entry) = cache.expr_cache.get(&key) {
        cache.prof_infer_expr_hits += 1;
        match cache_entry {
            CacheEntry::Cache(ty) => return Ok(ty.clone()),
            CacheEntry::Error(reason) => {
                // If a cached UnResolveDeclType error is stale (the decl now
                // has a type), invalidate the entry and re-infer. This handles
                // cases where generic type resolution makes progress within
                // the same Lua pass.
                if let InferFailReason::UnResolveDeclType(decl_id) = reason {
                    if db
                        .get_type_index()
                        .get_type_cache(&(*decl_id).into())
                        .is_some()
                    {
                        cache.expr_cache.remove(&key);
                        cache.prof_infer_expr_hits -= 1; // not a real hit
                    // Fall through to re-infer below
                    } else {
                        return Err(reason.clone());
                    }
                } else {
                    return Err(reason.clone());
                }
            }
            CacheEntry::Ready => return Err(InferFailReason::RecursiveInfer),
        }
    }

    // for @as
    let file_id = cache.get_file_id();
    let in_filed_syntax_id = InFiled::new(file_id, syntax_id);
    if let Some(bind_type_cache) = db
        .get_type_index()
        .get_type_cache(&in_filed_syntax_id.into())
    {
        cache
            .expr_cache
            .insert(key, CacheEntry::Cache(bind_type_cache.as_type().clone()));
        return Ok(bind_type_cache.as_type().clone());
    }

    // Track recursion depth (manual decrement at each return after this point)
    cache.prof_infer_expr_depth += 1;
    if cache.prof_infer_expr_depth > 1 {
        cache.prof_infer_expr_recursive_calls += 1;
    }
    if cache.prof_infer_expr_depth > cache.prof_infer_expr_max_depth {
        cache.prof_infer_expr_max_depth = cache.prof_infer_expr_depth;
    }

    cache.expr_cache.insert(key, CacheEntry::Ready);
    cache.prof_unique_inferred += 1;

    // Profile sub-type timing (only when info logging is enabled)
    let profile_enabled = log::log_enabled!(log::Level::Info);
    let prof_start = profile_enabled.then(std::time::Instant::now);

    let result_type = match expr {
        LuaExpr::CallExpr(call_expr) => infer_call_expr(db, cache, call_expr),
        LuaExpr::TableExpr(table_expr) => infer_table_expr(db, cache, table_expr),
        LuaExpr::LiteralExpr(literal_expr) => infer_literal_expr(db, cache, literal_expr),
        LuaExpr::BinaryExpr(binary_expr) => infer_binary_expr(db, cache, binary_expr),
        LuaExpr::UnaryExpr(unary_expr) => infer_unary_expr(db, cache, unary_expr),
        LuaExpr::ClosureExpr(closure_expr) => infer_closure_expr(db, cache, closure_expr),
        LuaExpr::ParenExpr(paren_expr) => infer_expr(
            db,
            cache,
            paren_expr.get_expr().ok_or(InferFailReason::None)?,
        ),
        LuaExpr::NameExpr(name_expr) => infer_name_expr(db, cache, name_expr),
        LuaExpr::IndexExpr(index_expr) => infer_index_expr(db, cache, index_expr, true),
    };

    if let Some(start) = prof_start {
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        use glua_parser::LuaSyntaxKind;
        let node_kind = key.get_kind();
        if node_kind == LuaSyntaxKind::IndexExpr {
            cache.prof_infer_index_time_ns += elapsed_ns;
            cache.prof_infer_index_calls += 1;
        } else if node_kind == LuaSyntaxKind::CallExpr
            || node_kind == LuaSyntaxKind::RequireCallExpr
            || node_kind == LuaSyntaxKind::ErrorCallExpr
            || node_kind == LuaSyntaxKind::AssertCallExpr
            || node_kind == LuaSyntaxKind::TypeCallExpr
            || node_kind == LuaSyntaxKind::SetmetatableCallExpr
        {
            cache.prof_infer_call_time_ns += elapsed_ns;
            cache.prof_infer_call_calls += 1;
        } else if node_kind == LuaSyntaxKind::NameExpr {
            cache.prof_infer_name_time_ns += elapsed_ns;
            cache.prof_infer_name_calls += 1;
        } else if node_kind == LuaSyntaxKind::TableArrayExpr
            || node_kind == LuaSyntaxKind::TableObjectExpr
            || node_kind == LuaSyntaxKind::TableEmptyExpr
        {
            cache.prof_infer_table_time_ns += elapsed_ns;
            cache.prof_infer_table_calls += 1;
        } else {
            cache.prof_infer_other_time_ns += elapsed_ns;
        }
    }

    // During diagnostics, types are final — cache everything (including errors) to avoid
    // recomputation across diagnostic checkers. During analysis, unresolved errors are
    // removed so the unresolve phase can retry them.
    let is_diagnostics = cache.get_config().analysis_phase.is_diagnostics();

    match &result_type {
        Ok(result_type) => {
            cache
                .expr_cache
                .insert(key, CacheEntry::Cache(result_type.clone()));
        }
        Err(InferFailReason::None) | Err(InferFailReason::RecursiveInfer) => {
            cache.prof_infer_expr_depth -= 1;
            cache
                .expr_cache
                .insert(key, CacheEntry::Cache(LuaType::Unknown));
            return Ok(LuaType::Unknown);
        }
        Err(InferFailReason::FieldNotFound) => {
            cache.prof_err_field_not_found += 1;
            if cache.get_config().analysis_phase.is_force() {
                cache.prof_infer_expr_depth -= 1;
                cache
                    .expr_cache
                    .insert(key, CacheEntry::Cache(LuaType::Nil));
                return Ok(LuaType::Nil);
            } else if is_diagnostics {
                cache
                    .expr_cache
                    .insert(key, CacheEntry::Error(InferFailReason::FieldNotFound));
            } else {
                cache.prof_expr_cache_removals += 1;
                cache.expr_cache.remove(&key);
            }
        }
        Err(reason) => {
            cache.prof_err_other += 1;
            match reason {
                InferFailReason::UnResolveExpr(_) => cache.prof_err_unresolve_expr += 1,
                InferFailReason::UnResolveDeclType(_) => cache.prof_err_unresolve_decl_type += 1,
                InferFailReason::UnResolveMemberType(_) => {
                    cache.prof_err_unresolve_member_type += 1
                }
                InferFailReason::UnResolveTypeDecl(_) => cache.prof_err_unresolve_type_decl += 1,
                InferFailReason::UnResolveOperatorCall => cache.prof_err_unresolve_operator += 1,
                InferFailReason::UnResolveModuleExport(_) => cache.prof_err_unresolve_module += 1,
                InferFailReason::UnResolveSignatureReturn(_) => {
                    cache.prof_err_unresolve_sig_return += 1
                }
                _ => {}
            }
            if is_diagnostics {
                cache
                    .expr_cache
                    .insert(key, CacheEntry::Error(reason.clone()));
            } else if matches!(reason, InferFailReason::UnResolveDeclType(_)) {
                // Cache UnResolveDeclType as an error during analysis to prevent
                // O(N×M) re-inference cascade. The decl's type depends on its
                // initializer which already failed, so re-inference within the
                // same Lua pass won't help. The unresolve phase calls clear()
                // and re-infers from scratch. Other errors are still removed
                // so same-pass re-inference can pick up newly-added members.
                cache
                    .expr_cache
                    .insert(key, CacheEntry::Error(reason.clone()));
            } else {
                cache.prof_expr_cache_removals += 1;
                cache.expr_cache.remove(&key);
            }
        }
    }

    cache.prof_infer_expr_depth -= 1;
    result_type
}

fn infer_literal_expr(db: &DbIndex, config: &LuaInferCache, expr: LuaLiteralExpr) -> InferResult {
    match expr.get_literal().ok_or(InferFailReason::None)? {
        LuaLiteralToken::Nil(_) => Ok(LuaType::Nil),
        LuaLiteralToken::Bool(bool) => Ok(LuaType::BooleanConst(bool.is_true())),
        LuaLiteralToken::Number(num) => match num.get_number_value() {
            NumberResult::Int(i) => Ok(LuaType::IntegerConst(i)),
            NumberResult::Float(f) => Ok(LuaType::FloatConst(f)),
            _ => Ok(LuaType::Number),
        },
        LuaLiteralToken::String(str) => {
            Ok(LuaType::StringConst(SmolStr::new(str.get_value()).into()))
        }
        LuaLiteralToken::Dots(_) => {
            let file_id = config.get_file_id();
            let range = expr.get_range();

            let decl_id = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|file_ref| file_ref.get_decl_id(&range));

            let decl_type = match decl_id.and_then(|id| db.get_decl_index().get_decl(&id)) {
                Some(decl) if decl.is_global() => LuaType::Any,
                Some(decl) if decl.is_param() => {
                    let base = infer_param(db, decl).unwrap_or(LuaType::Unknown);
                    LuaType::Variadic(VariadicType::Base(base).into())
                }
                _ => LuaType::Variadic(VariadicType::Base(LuaType::Any).into()),
            };

            Ok(decl_type)
        }
        // unreachable
        _ => Ok(LuaType::Any),
    }
}

fn infer_closure_expr(_: &DbIndex, config: &LuaInferCache, closure: LuaClosureExpr) -> InferResult {
    let signature_id = LuaSignatureId::from_closure(config.get_file_id(), &closure);
    Ok(LuaType::Signature(signature_id))
}

fn get_custom_type_operator(
    db: &DbIndex,
    operand_type: LuaType,
    op: LuaOperatorMetaMethod,
) -> Option<Vec<&LuaOperator>> {
    if operand_type.is_custom_type() {
        let type_id = match operand_type {
            LuaType::Ref(type_id) => type_id,
            LuaType::Def(type_id) => type_id,
            _ => return None,
        };
        let op_ids = db.get_operator_index().get_operators(&type_id.into(), op)?;
        let operators = op_ids
            .iter()
            .filter_map(|id| db.get_operator_index().get_operator(id))
            .collect();

        Some(operators)
    } else {
        None
    }
}

pub fn infer_expr_list_value_type_at(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    exprs: &[LuaExpr],
    value_idx: usize,
) -> Result<Option<LuaType>, InferFailReason> {
    let exprs_len = exprs.len();
    if exprs_len == 0 {
        Ok(None)
    } else if value_idx < exprs_len {
        Ok(
            infer_expr_list_types(db, cache, &exprs[value_idx..], Some(1), infer_expr)?
                .first()
                .map(|(ty, _)| ty.clone()),
        )
    } else {
        let last_idx = exprs_len - 1;
        let offset = value_idx - last_idx;
        Ok(
            infer_expr_list_types(db, cache, &exprs[last_idx..], Some(offset + 1), infer_expr)?
                .get(offset)
                .map(|(ty, _)| ty.clone()),
        )
    }
}

pub fn infer_call_arg_expr_list_types(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: LuaCallExpr,
    var_count: Option<usize>,
) -> Vec<(LuaType, TextRange)> {
    let key = (call_expr.get_syntax_id(), var_count);
    if let Some(types) = cache.call_arg_types_cache.get(&key) {
        return types.as_ref().clone();
    }
    if let Some(var_count) = var_count {
        let full_key = (call_expr.get_syntax_id(), None);
        if let Some(types) = cache.call_arg_types_cache.get(&full_key)
            && types.len() >= var_count
        {
            return types[..var_count].to_vec();
        }
    }

    let types = call_expr
        .get_args_list()
        .and_then(|args| {
            let exprs = args.get_args().collect::<Vec<_>>();
            infer_expr_list_types(db, cache, &exprs, var_count, |db, cache, expr| {
                Ok(infer_expr(db, cache, expr).unwrap_or(LuaType::Unknown))
            })
            .ok()
        })
        .unwrap_or_default();
    cache
        .call_arg_types_cache
        .insert(key, Arc::new(types.clone()));
    types
}

pub fn infer_expr_list_types<F>(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    exprs: &[LuaExpr],
    var_count: Option<usize>,
    mut infer: F,
) -> Result<Vec<(LuaType, TextRange)>, InferFailReason>
where
    F: FnMut(&DbIndex, &mut LuaInferCache, LuaExpr) -> InferResult,
{
    let mut value_types = Vec::new();
    for (idx, expr) in exprs.iter().enumerate() {
        if let Some(var_count) = var_count
            && value_types.len() >= var_count
        {
            break;
        }

        let expr_type = infer(db, cache, expr.clone())?;
        match expr_type {
            LuaType::Variadic(variadic) => {
                if let Some(var_count) = var_count {
                    if idx < var_count {
                        for i in idx..var_count {
                            if let Some(typ) = variadic.get_type(i - idx) {
                                value_types.push((typ.clone(), expr.get_range()));
                            } else {
                                break;
                            }
                        }
                    }
                } else {
                    match variadic.deref() {
                        VariadicType::Base(base) => {
                            value_types.push((base.clone(), expr.get_range()));
                        }
                        VariadicType::Multi(vecs) => {
                            for typ in vecs {
                                value_types.push((typ.clone(), expr.get_range()));
                            }
                        }
                    }
                }

                break;
            }
            _ => value_types.push((expr_type, expr.get_range())),
        }
    }

    Ok(value_types)
}

/// 推断值已经绑定的类型(不是推断值的类型). 例如从右值推断左值类型, 从调用参数推断函数参数类型参数类型
pub fn infer_bind_value_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
) -> Option<LuaType> {
    let parent_node = expr.syntax().parent().and_then(LuaAst::cast)?;

    match parent_node {
        LuaAst::LuaAssignStat(assign) => {
            let (vars, exprs) = assign.get_var_and_expr_list();
            let mut typ = None;

            for (idx, var) in vars.iter().enumerate() {
                let var_expr: LuaExpr = var.clone().into();
                if expr != var_expr {
                    continue;
                }

                let Some(assign_expr) = exprs.get(idx) else {
                    return Some(LuaType::Nil);
                };
                return infer_expr(db, cache, assign_expr.clone()).ok();
            }

            for (idx, assign_expr) in exprs.iter().enumerate() {
                if expr == *assign_expr {
                    let var = vars.get(idx);
                    if let Some(var) = var {
                        if let LuaVarExpr::IndexExpr(index_expr) = var {
                            let prefix_expr = index_expr.get_prefix_expr()?;
                            let prefix_type = infer_expr(db, cache, prefix_expr).ok()?;
                            // 如果前缀类型是定义类型, 则不认为存在左值绑定
                            if let LuaType::Def(_) = prefix_type {
                                return None;
                            }
                        };
                        typ = Some(infer_expr(db, cache, var.clone().into()).ok()?);
                        break;
                    }
                }
            }
            typ
        }
        LuaAst::LuaTableField(table_field) => {
            let field_key = table_field.get_field_key()?;
            let table_expr = table_field.get_parent::<LuaTableExpr>()?;
            let table_type = infer_table_should_be(db, cache, table_expr.clone()).ok()?;
            let member_key = match LuaMemberKey::from_index_key(db, cache, &field_key) {
                Ok(key) => key,
                Err(_) => return None,
            };
            match infer_raw_member_type(db, &table_type, &member_key) {
                Ok(typ) => Some(typ),
                Err(InferFailReason::FieldNotFound) => None,
                Err(_) => Some(LuaType::Unknown),
            }
        }
        LuaAst::LuaCallArgList(call_arg_list) => {
            let call_expr = call_arg_list.get_parent::<LuaCallExpr>()?;
            // 获取调用位置
            let mut param_pos = 0;
            for (idx, arg) in call_arg_list.get_args().enumerate() {
                if arg == expr {
                    param_pos = idx;
                    break;
                }
            }
            let is_colon_call = call_expr.is_colon_call();

            let expr_type = infer_expr(db, cache, call_expr.get_prefix_expr()?).ok()?;
            let func_type = infer_call_expr_func(
                db,
                cache,
                call_expr.clone(),
                expr_type.clone(),
                &InferGuard::new(),
                None,
            )
            .ok()?;

            match (func_type.is_colon_define(), is_colon_call) {
                (true, false) => {
                    if param_pos == 0 {
                        return None;
                    }
                    param_pos -= 1;
                }
                (false, true) => {
                    param_pos += 1;
                }
                _ => {}
            }

            let param_info = func_type.get_params().get(param_pos)?;
            let mut param_type = param_info.1.clone();

            if let Some(typ) = &param_type {
                if matches!(typ, LuaType::Function | LuaType::Any) {
                    let file_id = cache.get_file_id();
                    if let Some(gmod_func) = crate::compilation::analyzer::unresolve::resolve_gmod_hook_add_callback_doc_function(
                        db,
                        &call_expr,
                        param_pos,
                        None,
                        file_id,
                    ) {
                        param_type = Some(LuaType::DocFunction(gmod_func));
                    }
                }
            }

            param_type
        }
        _ => None,
    }
}
