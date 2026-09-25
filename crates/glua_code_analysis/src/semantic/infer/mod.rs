mod infer_binary;
mod infer_call;
mod infer_doc_type;
mod infer_fail_reason;
mod infer_index;
mod infer_name;
mod infer_table;
mod infer_unary;
pub(crate) mod narrow;
mod test;

use std::{ops::Deref, sync::Arc};

use glua_parser::{
    LuaAst, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaLiteralExpr, LuaLiteralToken,
    LuaTableExpr, LuaVarExpr, NumberResult,
};
use infer_binary::infer_binary_expr;
pub(crate) use infer_call::get_prefix_expr_signature_id;
use infer_call::infer_call_expr;
pub use infer_call::infer_call_expr_func;
pub(crate) use infer_call::signature_call_selects_declared_overload;
pub use infer_doc_type::{DocTypeInferContext, infer_doc_type};
pub use infer_fail_reason::InferFailReason;
pub(crate) use infer_index::check_iter_var_range;
pub(crate) use infer_index::global_expr_access_path;
pub use infer_index::infer_index_expr;
pub(crate) use infer_index::infer_member_by_member_key;
pub(crate) use infer_index::pairs_iter_value_registry_path;
pub(crate) use infer_index::resolve_decl_backed_global_path_member_type;
pub(crate) use infer_index::{keep_access_paths, stop_keeping_access_paths};
pub(crate) use infer_name::find_self_ref_id;
pub(crate) use infer_name::infer_authoritative_method_self_type;
pub(crate) use infer_name::infer_enclosing_self_type;
use infer_name::infer_name_expr;
pub(crate) use infer_name::is_authoritative_self_receiver_type;
pub(crate) use infer_name::is_concrete_self_receiver_type;
pub(crate) use infer_name::try_local_decl_initializer_fallback_type;
pub(crate) use infer_name::type_decl_is_vgui_panel;
pub(crate) use infer_name::{ParamInferenceSource, infer_param_is_weak};
pub use infer_name::{infer_param, infer_param_with_cache};
use infer_table::infer_table_expr;
pub use infer_table::{infer_table_field_value_should_be, infer_table_should_be};
use infer_unary::infer_unary_expr;
pub use narrow::{SelfRefId, VarRefId, VarRefRootId};
pub(crate) use narrow::{contains_gmod_null_type, get_var_expr_var_ref_id, remove_false_or_nil};

use rowan::TextRange;
use smol_str::SmolStr;

use crate::{
    InFiled, InferGuard, LuaMemberKey, VariadicType, analysis_stack_exhausted,
    db_index::{DbIndex, LuaOperator, LuaOperatorMetaMethod, LuaSignatureId, LuaType},
};

use super::{CacheEntry, LuaInferCache, member::infer_raw_member_type_with_cache};

pub type InferResult = Result<LuaType, InferFailReason>;
pub use infer_call::InferCallFuncResult;

pub fn infer_expr(db: &DbIndex, cache: &mut LuaInferCache, expr: LuaExpr) -> InferResult {
    let key = expr.get_syntax_id();
    if let Some(mark) = cache.expr_in_progress.get(&key) {
        crate::db_index::derivation::consult_cycle(*mark);
        return Err(InferFailReason::RecursiveInfer);
    }
    let lookup = cache.lookup();
    if crate::db_index::derivation::check_tsc() {
        crate::db_index::derivation::take_reused_across();
    }
    if let Some(cache_entry) = cache.expr_cache.get(&key, lookup) {
        match cache_entry {
            CacheEntry::Cache(ty) => {
                let ty = ty.clone();
                if crate::db_index::derivation::check_tsc()
                    && crate::db_index::derivation::take_reused_across()
                {
                    let shared_reads = cache
                        .expr_cache
                        .provenance(&key)
                        .map(crate::db_index::derivation::describe_reads)
                        .unwrap_or_default();
                    let mut fresh = cache.fork_fresh();
                    let frame = crate::db_index::derivation::open_frame();
                    let recomputed = infer_expr(db, &mut fresh, expr.clone());
                    let fresh_reads = fresh
                        .close_frame(frame)
                        .map(|provenance| crate::db_index::derivation::describe_reads(&provenance))
                        .unwrap_or_default();
                    if recomputed.as_ref().ok() != Some(&ty) {
                        let shared_keys = shared_reads
                            .iter()
                            .map(|(key, _)| key.as_str())
                            .collect::<std::collections::HashSet<_>>();
                        let only_fresh = fresh_reads
                            .iter()
                            .filter(|(key, _)| !shared_keys.contains(key.as_str()))
                            .map(|(key, _)| key.as_str())
                            .collect::<Vec<_>>();
                        let written = shared_reads
                            .iter()
                            .filter(|(_, written)| *written)
                            .map(|(key, _)| key.as_str())
                            .collect::<Vec<_>>();
                        if let Some(provenance) = cache.expr_cache.provenance(&key) {
                            let (origin, keys) =
                                crate::db_index::derivation::provenance_keys(provenance);
                            if let Some(origin) = origin {
                                let seen = db.reader_graph().recorded_slots(origin);
                                let moved = keys
                                    .iter()
                                    .filter_map(|(slot, hashed)| {
                                        let (_, then) =
                                            seen.iter().find(|(read, _)| read == slot)?;
                                        let now = db.fact_fingerprint(hashed);
                                        (*then != now).then(|| format!("{:?}", &**hashed))
                                    })
                                    .collect::<Vec<_>>();
                                eprintln!("[tsc] origin {origin:?} moved-unmarked={moved:?}");
                            }
                        }
                        let gmod_class = shared_reads
                            .iter()
                            .filter(|(key, _)| key.contains("GmodClass"))
                            .collect::<Vec<_>>();
                        eprintln!("[tsc] gmod-class reads {gmod_class:?}");
                        eprintln!(
                            "[tsc] reads shared={} fresh={} only_fresh={only_fresh:?} written={written:?}",
                            shared_reads.len(),
                            fresh_reads.len(),
                        );
                        eprintln!(
                            "[tsc] mismatch {:?} {:?} {:?} `{}` shared={ty:?} fresh={recomputed:?}",
                            db.get_vfs()
                                .get_uri(&cache.get_file_id())
                                .map(|uri| uri.to_string()),
                            expr.get_range(),
                            crate::db_index::derivation::current(),
                            expr.syntax().text(),
                        );
                    }
                }
                return Ok(ty);
            }
            CacheEntry::Error(reason) => {
                // If a cached UnResolveDeclType error is stale (the decl now
                // has a type), invalidate the entry and re-infer. This handles
                // cases where generic type resolution makes progress within
                // the same Lua pass.
                if let InferFailReason::UnResolveDeclType(decl_id) = reason {
                    let decl_id = *decl_id;
                    if db
                        .get_type_index()
                        .get_type_cache(&decl_id.into())
                        .is_some()
                    {
                        cache.expr_cache.remove(&key);
                    // Fall through to re-infer below
                    } else {
                        return Err(InferFailReason::UnResolveDeclType(decl_id));
                    }
                } else {
                    return Err(reason.clone());
                }
            }
            CacheEntry::Ready => return Err(InferFailReason::RecursiveInfer),
        }
    }

    crate::profile::work::computed(
        crate::profile::work::Work::Infer,
        (cache.get_file_id(), key),
    );
    let frame = crate::db_index::derivation::open_frame();
    let (result, stored) = infer_expr_miss(db, cache, expr, key);
    let provenance = cache.close_frame(frame);
    match stored {
        Some(Some(entry)) => {
            cache.expr_in_progress.remove(&key);
            cache.expr_cache.insert(key, entry, provenance);
        }
        Some(None) => {
            cache.expr_in_progress.remove(&key);
            cache.expr_cache.remove(&key);
        }
        None => {}
    }
    result
}

/// Infers `expr` on a memo miss. Returns the result and what to store for
/// it: an entry, `Some(None)` to drop the key, or `None` to leave it as is.
#[allow(clippy::option_option)]
fn infer_expr_miss(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    key: glua_parser::LuaSyntaxId,
) -> (InferResult, Option<Option<CacheEntry<LuaType>>>) {
    // for @as
    let file_id = cache.get_file_id();
    let in_filed_syntax_id = InFiled::new(file_id, key);
    if let Some(bind_type_cache) = db
        .get_type_index()
        .get_type_cache(&in_filed_syntax_id.into())
    {
        let ty = bind_type_cache.as_type().clone();
        return (Ok(ty.clone()), Some(Some(CacheEntry::Cache(ty))));
    }

    // The reserve check sits below the cache fast paths: cache hits return
    // without recursing, so they need no stack.
    if analysis_stack_exhausted() {
        crate::db_index::derivation::taint();
        return (Err(InferFailReason::RecursiveInfer), None);
    }

    cache
        .expr_in_progress
        .insert(key, crate::db_index::derivation::scope_mark());

    let result_type = match expr {
        LuaExpr::CallExpr(call_expr) => infer_call_expr(db, cache, call_expr),
        LuaExpr::TableExpr(table_expr) => infer_table_expr(db, cache, table_expr),
        LuaExpr::LiteralExpr(literal_expr) => infer_literal_expr(db, cache, literal_expr),
        LuaExpr::BinaryExpr(binary_expr) => infer_binary_expr(db, cache, binary_expr),
        LuaExpr::UnaryExpr(unary_expr) => infer_unary_expr(db, cache, unary_expr),
        LuaExpr::ClosureExpr(closure_expr) => infer_closure_expr(db, cache, closure_expr),
        LuaExpr::ParenExpr(paren_expr) => match paren_expr.get_expr() {
            Some(inner) => infer_expr(db, cache, inner),
            // The key keeps its `Ready` marker.
            None => {
                crate::db_index::derivation::taint();
                return (Err(InferFailReason::None), None);
            }
        },
        LuaExpr::NameExpr(name_expr) => infer_name_expr(db, cache, name_expr),
        LuaExpr::IndexExpr(index_expr) => infer_index_expr(db, cache, index_expr, true),
    };

    // During diagnostics, types are final — cache everything (including errors) to avoid
    // recomputation across diagnostic checkers. During analysis, unresolved errors are
    // removed so the unresolve phase can retry them.
    let is_diagnostics = cache.get_config().analysis_phase.is_diagnostics();
    let is_force = cache.get_config().analysis_phase.is_force();

    match result_type {
        Ok(result_type) => {
            let stored = CacheEntry::Cache(result_type.clone());
            (Ok(result_type), Some(Some(stored)))
        }
        Err(InferFailReason::None) | Err(InferFailReason::RecursiveInfer) => (
            Ok(LuaType::Unknown),
            Some(Some(CacheEntry::Cache(LuaType::Unknown))),
        ),
        Err(InferFailReason::FieldNotFound) => {
            if is_force {
                (
                    Ok(LuaType::Nil),
                    Some(Some(CacheEntry::Cache(LuaType::Nil))),
                )
            } else if is_diagnostics {
                (
                    Err(InferFailReason::FieldNotFound),
                    Some(Some(CacheEntry::Error(InferFailReason::FieldNotFound))),
                )
            } else {
                (Err(InferFailReason::FieldNotFound), Some(None))
            }
        }
        Err(reason) if reason.is_need_resolve() && is_force => {
            // A fact that has not landed reads as unknown: the read is
            // recorded, so its reader is evaluated again once it lands.
            (
                Ok(LuaType::Unknown),
                Some(Some(CacheEntry::Cache(LuaType::Unknown))),
            )
        }
        Err(reason) => {
            if is_diagnostics {
                let stored = CacheEntry::Error(reason.clone());
                (Err(reason), Some(Some(stored)))
            } else if matches!(reason, InferFailReason::UnResolveDeclType(_)) {
                // Cache UnResolveDeclType as an error during analysis to prevent
                // O(N×M) re-inference cascade. The decl's type depends on its
                // initializer which already failed, so re-inference within the
                // same Lua pass won't help. The unresolve phase calls clear()
                // and re-infers from scratch. Other errors are still removed
                // so same-pass re-inference can pick up newly-added members.
                let stored = CacheEntry::Error(reason.clone());
                (Err(reason), Some(Some(stored)))
            } else {
                (Err(reason), Some(None))
            }
        }
    }
}

fn infer_literal_expr(
    db: &DbIndex,
    config: &mut LuaInferCache,
    expr: LuaLiteralExpr,
) -> InferResult {
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
                    let base = infer_param_with_cache(db, config, decl).unwrap_or(LuaType::Unknown);
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
) -> Arc<Vec<(LuaType, TextRange)>> {
    let key = (call_expr.get_syntax_id(), var_count);
    if let Some(types) = cache.tracked_get(|cache| &mut cache.call_arg_types_cache, &key) {
        return types;
    }
    if let Some(var_count) = var_count {
        let full_key = (call_expr.get_syntax_id(), None);
        if let Some(types) = cache.tracked_get(|cache| &mut cache.call_arg_types_cache, &full_key)
            && types.len() >= var_count
        {
            return Arc::new(types[..var_count].to_vec());
        }
    }

    let frame = crate::db_index::derivation::open_frame();
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
    let shared = Arc::new(types);
    cache.tracked_insert(
        |cache| &mut cache.call_arg_types_cache,
        key,
        shared.clone(),
        frame,
    );
    shared
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
                        // The last expression in the list forwards every value
                        // it produces: `f(g())`, `f(...)` and `return f()` all
                        // pass the whole pack on. In earlier positions Lua
                        // truncates the expression to its first value.
                        VariadicType::Base(base) if idx + 1 != exprs.len() => {
                            value_types.push((base.clone(), expr.get_range()));
                        }
                        VariadicType::Base(_) => {
                            value_types
                                .push((LuaType::Variadic(variadic.clone()), expr.get_range()));
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
            // A union that carries a multi-return still spreads, slot by
            // slot. An `unknown` arm - what an unannotated recursive function
            // leaves behind, since its own return cannot inform itself - says
            // nothing about arity or about any slot, so the informative arms
            // decide both.
            LuaType::Union(ref union)
                if union.types().any(|typ| matches!(typ, LuaType::Variadic(_))) =>
            {
                let arms = union
                    .types()
                    .filter_map(|typ| match typ {
                        LuaType::Variadic(variadic) => Some(variadic.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                // A `Base` arm answers every slot, so its arity bounds nothing.
                // That is only safe while the caller has asked for a fixed
                // number of values; with no arity to fill it contributes one
                // value, exactly as a bare `Variadic` does.
                let slots = arms
                    .iter()
                    .map(|variadic| match variadic.deref() {
                        VariadicType::Multi(types) => types.len(),
                        VariadicType::Base(_) if var_count.is_some() => usize::MAX,
                        VariadicType::Base(_) => 1,
                    })
                    .max()
                    .unwrap_or(0);
                let wanted = match var_count {
                    Some(var_count) => var_count.saturating_sub(value_types.len()),
                    None => slots,
                };
                for slot in 0..wanted.min(slots) {
                    let slot_type = arms
                        .iter()
                        .filter_map(|variadic| variadic.get_type(slot).cloned())
                        .reduce(|left, right| crate::TypeOps::Union.apply(db, &left, &right));
                    let Some(slot_type) = slot_type else {
                        break;
                    };
                    value_types.push((slot_type, expr.get_range()));
                }

                break;
            }
            LuaType::Unknown if matches!(expr, LuaExpr::CallExpr(_)) && var_count.is_some() => {
                let remaining = var_count
                    .unwrap_or(value_types.len())
                    .saturating_sub(value_types.len());
                value_types.extend(std::iter::repeat_n(
                    (LuaType::Unknown, expr.get_range()),
                    remaining,
                ));
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
                            // An inferred container's element type is the merge
                            // of every store into the slot -- this one included
                            // -- plus the slot's own bootstrap placeholder, so
                            // it reads back as `Union({}, local, ...)`. That is
                            // the local deriving itself, not evidence of its
                            // type, so it cannot pin the stored local.
                            if matches!(prefix_type, LuaType::TableConst(_)) {
                                return None;
                            }
                        }
                        typ = Some(infer_expr(db, cache, var.clone().into()).ok()?);
                        break;
                    }
                }
            }
            typ
        }
        LuaAst::LuaFuncStat(func_stat) => {
            let func_name = func_stat.get_func_name()?;
            let func_expr: LuaExpr = func_name.into();
            if expr != func_expr {
                return None;
            }

            let signature_id =
                LuaSignatureId::from_closure(cache.get_file_id(), &func_stat.get_closure()?);
            Some(LuaType::Signature(signature_id))
        }
        LuaAst::LuaTableField(table_field) => {
            let field_key = table_field.get_field_key()?;
            let table_expr = table_field.get_parent::<LuaTableExpr>()?;
            let table_type = infer_table_should_be(db, cache, table_expr.clone()).ok()?;
            let member_key = match LuaMemberKey::from_index_key(db, cache, &field_key) {
                Ok(key) => key,
                Err(_) => return None,
            };
            match infer_raw_member_type_with_cache(db, cache, &table_type, &member_key) {
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
