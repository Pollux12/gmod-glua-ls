use std::{collections::HashSet, ops::Deref, sync::Arc};

use glua_parser::{LuaAstNode, LuaCallExpr, LuaChunk, LuaDocTypeList, LuaExpr, LuaNameExpr};
use internment::ArcIntern;
use smol_str::SmolStr;

use crate::{
    DocTypeInferContext, FileId, GenericTpl, GenericTplId, LuaDocDefaultValue, LuaFunctionType,
    LuaGenericType, LuaSemanticDeclId, TypeVisitTrait,
    db_index::{DbIndex, LuaType},
    infer_doc_type,
    semantic::{
        LuaInferCache,
        generic::{
            instantiate_type::instantiate_doc_function,
            tpl_context::TplContext,
            tpl_pattern::{
                multi_param_tpl_pattern_match_multi_return, tpl_pattern_match,
                variadic_tpl_pattern_match,
            },
        },
        infer::{
            InferFailReason,
            narrow::get_type_at_flow::{
                explicit_param_string_default_reaches_flow, inferred_string_default_reaches_flow,
            },
        },
        infer_enclosing_self_type, infer_expr,
    },
};
use crate::{LuaMemberOwner, SemanticDeclLevel, infer_node_semantic_decl};

use super::TypeSubstitutor;

/// Resolve a flow-valid inferred string default for a call argument expression.
///
/// When the arg is a local variable with an inferred string default (from
/// `x = x or "literal"`), and the self-coalescing assignment is the last
/// write to that variable that dominates the call site, returns
/// `Some(LuaType::StringConst(value))`.
///
/// Only returns a value when:
/// - `arg_type` is exactly `LuaType::String`
/// - `param_type` actually contains a `StrTplRef`
/// - exactly one candidate default is flow-valid at the use site
/// - no explicit `---@param` default takes precedence
fn resolve_str_default_from_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    param_type: &LuaType,
    arg_expr: &LuaExpr,
) -> Option<LuaType> {
    // Quick gate: only when param contains a StrTplRef.
    if !param_type.contain_tpl() {
        return None;
    }
    let mut has_str_tpl = false;
    param_type.visit_type(&mut |t| {
        if matches!(t, LuaType::StrTplRef(_)) {
            has_str_tpl = true;
        }
    });
    if !has_str_tpl {
        return None;
    }

    // Resolve the argument's declaration.
    let name_expr = LuaNameExpr::cast(arg_expr.syntax().clone())?;
    let file_id = cache.get_file_id();
    let range = name_expr.get_range();
    let local_ref = db.get_reference_index().get_local_reference(&file_id)?;
    let decl_id = local_ref.get_decl_id(&range)?;

    // Seed the cache with the use-site realm so that flow-reachability
    // checks evaluate from the call argument's realm context (not the
    // declaration position fallback).
    let use_site_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, range.start());
    let previous_realm = cache.flow_query_realm.replace(use_site_realm);

    let result = resolve_str_default_from_arg_inner(db, cache, param_type, arg_expr, decl_id);

    cache.flow_query_realm = previous_realm;
    result
}

/// Inner helper for `resolve_str_default_from_arg` — separated so the
/// caller can seed/restore `flow_query_realm` around the call.
fn resolve_str_default_from_arg_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    _param_type: &LuaType,
    arg_expr: &LuaExpr,
    decl_id: crate::LuaDeclId,
) -> Option<LuaType> {
    let file_id = cache.get_file_id();

    // ── Explicit default precedence ───────────────────────────────────
    // If the variable is a function parameter with an explicit `---@param`
    // default (e.g. `---@param x string = "foo"`), prefer that over any
    // inferred `x = x or "foo"` default — but ONLY when:
    // 1. the default is a `String` (this resolver is for string-template binding),
    // 2. the explicit default is still flow-valid at the use site.
    if let Some(decl) = db.get_decl_index().get_decl(&decl_id) {
        if let crate::LuaDeclExtra::Param {
            idx: param_idx_in_sig,
            signature_id,
            ..
        } = &decl.extra
        {
            if let Some(default_val) = db
                .get_signature_index()
                .get(signature_id)
                .and_then(|sig| sig.get_param_info_by_id(*param_idx_in_sig))
                .and_then(|info| info.default_value.as_ref())
            {
                // Only String defaults participate in string-template binding.
                if let LuaDocDefaultValue::String(s) = default_val {
                    // Flow-validity: the explicit default must still reach
                    // the use site (not killed by a non-coalescing reassignment).
                    let flow_tree = db.get_flow_index().get_flow_tree(&file_id);
                    let root = LuaChunk::cast(arg_expr.get_root());
                    let flow_valid = match (&flow_tree, &root) {
                        (Some(tree), Some(root)) => tree
                            .get_flow_id(arg_expr.get_syntax_id())
                            .is_some_and(|use_flow_id| {
                                explicit_param_string_default_reaches_flow(
                                    db,
                                    tree,
                                    cache,
                                    root,
                                    decl_id,
                                    use_flow_id,
                                )
                            }),
                        _ => false,
                    };

                    if flow_valid {
                        return Some(LuaType::StringConst(SmolStr::new(s.as_str()).into()));
                    }
                }
                // Non-String explicit defaults (Boolean, Number, Nil) are
                // ignored by this resolver — they don't bind string templates.
            }
        }
    }

    // Get inferred string default candidates from the side-map.
    let candidates = db
        .get_property_index()
        .get_inferred_string_defaults(&decl_id)?;
    if candidates.is_empty() {
        return None;
    }

    // Get the flow tree and the flow ID for the argument expression (use site).
    // Using the arg expression's syntax ID (not the call expression's) is
    // consistent with how existing narrow-flow querying works in
    // `semantic/infer/narrow/mod.rs`.
    let flow_tree = db.get_flow_index().get_flow_tree(&file_id)?;
    let use_flow_id = flow_tree.get_flow_id(arg_expr.get_syntax_id())?;
    let root = LuaChunk::cast(arg_expr.get_root())?;

    // Check each candidate for flow-validity.  Exactly one must be valid.
    let mut valid_value: Option<SmolStr> = None;
    for candidate in candidates {
        let reaches = inferred_string_default_reaches_flow(
            db,
            flow_tree,
            cache,
            &root,
            decl_id,
            use_flow_id,
            candidate.source_range,
        );
        if reaches {
            if valid_value.is_some() {
                // Multiple valid candidates — ambiguous, don't bind.
                return None;
            }
            valid_value = Some(candidate.value.clone());
        }
    }

    valid_value.map(|v| LuaType::StringConst(v.into()))
}

pub fn instantiate_func_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func: &LuaFunctionType,
    call_expr: LuaCallExpr,
) -> Result<LuaFunctionType, InferFailReason> {
    let file_id = cache.get_file_id().clone();
    let mut generic_tpls = HashSet::new();
    let mut contain_self = false;
    func.visit_type(&mut |t| match t {
        LuaType::TplRef(generic_tpl) | LuaType::ConstTplRef(generic_tpl) => {
            let tpl_id = generic_tpl.get_tpl_id();
            if tpl_id.is_func() {
                generic_tpls.insert(tpl_id);
            }
        }
        LuaType::StrTplRef(str_tpl) => {
            generic_tpls.insert(str_tpl.get_tpl_id());
        }
        LuaType::SelfInfer => {
            contain_self = true;
        }
        _ => {}
    });

    let origin_params = func.get_params();
    let mut func_params: Vec<_> = origin_params
        .iter()
        .map(|(name, t)| (name.clone(), t.clone().unwrap_or(LuaType::Unknown)))
        .collect();

    let arg_exprs = call_expr
        .get_args_list()
        .ok_or(InferFailReason::None)?
        .get_args()
        .collect::<Vec<_>>();
    let mut substitutor = TypeSubstitutor::new();
    let mut context = TplContext {
        db,
        cache,
        substitutor: &mut substitutor,
        call_expr: Some(call_expr.clone()),
    };
    if !generic_tpls.is_empty() {
        context.substitutor.add_need_infer_tpls(generic_tpls);

        if let Some(type_list) = call_expr.get_call_generic_type_list() {
            // 如果使用了`obj:abc--[[@<string>]]("abc")`强制指定了泛型, 那么我们只需要直接应用
            apply_call_generic_type_list(db, file_id, &mut context, &type_list);
        } else {
            // 如果没有指定泛型, 则需要从调用参数中推断
            infer_generic_types_from_call(
                db,
                &mut context,
                func,
                &call_expr,
                &mut func_params,
                &arg_exprs,
            )?;
        }
    }

    if contain_self && let Some(self_type) = infer_self_type(db, cache, &call_expr) {
        substitutor.add_self_type(self_type);
    }

    if let LuaType::DocFunction(f) = instantiate_doc_function(db, func, &substitutor) {
        Ok(f.deref().clone())
    } else {
        Ok(func.clone())
    }
}

fn apply_call_generic_type_list(
    db: &DbIndex,
    file_id: FileId,
    context: &mut TplContext,
    type_list: &LuaDocTypeList,
) {
    let doc_ctx = DocTypeInferContext::new(db, file_id);
    for (i, doc_type) in type_list.get_types().enumerate() {
        let typ = infer_doc_type(doc_ctx, &doc_type);
        context
            .substitutor
            .insert_type(GenericTplId::Func(i as u32), typ, true);
    }
}

fn infer_generic_types_from_call(
    db: &DbIndex,
    context: &mut TplContext,
    func: &LuaFunctionType,
    call_expr: &LuaCallExpr,
    func_params: &mut Vec<(String, LuaType)>,
    arg_exprs: &[LuaExpr],
) -> Result<(), InferFailReason> {
    let colon_call = call_expr.is_colon_call();
    let colon_define = func.is_colon_define();
    match (colon_define, colon_call) {
        (true, false) => {
            func_params.insert(0, ("self".to_string(), LuaType::Any));
        }
        (false, true) => {
            if !func_params.is_empty() {
                func_params.remove(0);
            }
        }
        _ => {}
    }

    let mut unresolve_tpls = vec![];
    for i in 0..func_params.len() {
        if i >= arg_exprs.len() {
            break;
        }

        if context.substitutor.is_infer_all_tpl() {
            break;
        }

        let (_, func_param_type) = &func_params[i];
        let call_arg_expr = &arg_exprs[i];
        if !func_param_type.contain_tpl() {
            continue;
        }

        if !func_param_type.is_variadic()
            && check_expr_can_later_infer(context, func_param_type, call_arg_expr)?
        {
            // 如果参数不能被后续推断, 那么我们先不处理
            unresolve_tpls.push((func_param_type.clone(), call_arg_expr.clone()));
            continue;
        }

        let arg_type = match infer_expr(db, context.cache, call_arg_expr.clone()) {
            Ok(t) => t,
            Err(InferFailReason::FieldNotFound) => LuaType::Nil, // 对于未找到的字段, 我们认为是 nil 以执行后续推断
            Err(e) => return Err(e),
        };
        match (func_param_type, &arg_type) {
            (LuaType::Variadic(variadic), _) => {
                let mut arg_types = vec![];
                for arg_expr in &arg_exprs[i..] {
                    let arg_type = infer_expr(db, context.cache, arg_expr.clone())?;
                    arg_types.push(arg_type);
                }
                variadic_tpl_pattern_match(context, variadic, &arg_types)?;
                break;
            }
            (_, LuaType::Variadic(variadic)) => {
                let func_param_types = func_params[i..]
                    .iter()
                    .map(|(_, t)| t)
                    .cloned()
                    .collect::<Vec<_>>();
                multi_param_tpl_pattern_match_multi_return(context, &func_param_types, variadic)?;
                break;
            }
            _ => {
                // Try to bind from a registered string default when the
                // inferred arg_type is plain `String` and the param has a
                // StrTplRef.  This covers `x = x or "literal"` patterns where
                // the canonical type stays `string` but the declaration carries
                // an auxiliary default-value metadata.
                let effective_type = if matches!(arg_type, LuaType::String) {
                    resolve_str_default_from_arg(db, context.cache, func_param_type, call_arg_expr)
                        .unwrap_or_else(|| arg_type.clone())
                } else {
                    arg_type.clone()
                };
                tpl_pattern_match(context, func_param_type, &effective_type)?;
            }
        }
    }

    if !context.substitutor.is_infer_all_tpl() {
        for (func_param_type, call_arg_expr) in unresolve_tpls {
            let closure_type = infer_expr(db, context.cache, call_arg_expr)?;

            tpl_pattern_match(context, &func_param_type, &closure_type)?;
        }
    }

    Ok(())
}

pub fn build_self_type(db: &DbIndex, self_type: &LuaType) -> LuaType {
    match self_type {
        LuaType::Def(id) | LuaType::Ref(id) => {
            if let Some(generic) = db.get_type_index().get_generic_params(id) {
                let mut params = Vec::new();
                for (i, generic_param) in generic.iter().enumerate() {
                    if let Some(t) = &generic_param.type_constraint {
                        params.push(t.clone());
                    } else {
                        params.push(LuaType::TplRef(Arc::new(GenericTpl::new(
                            GenericTplId::Type(i as u32),
                            ArcIntern::new(generic_param.name.clone()),
                            None,
                        ))));
                    }
                }
                let generic = LuaGenericType::new(id.clone(), params);
                return LuaType::Generic(Arc::new(generic));
            }
        }
        _ => {}
    };
    self_type.clone()
}

pub fn infer_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaType> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    match prefix_expr {
        LuaExpr::IndexExpr(index) => {
            let self_expr = index.get_prefix_expr()?;
            let self_type = infer_expr(db, cache, self_expr).ok()?;
            let self_type = build_self_type(db, &self_type);
            return Some(self_type);
        }
        LuaExpr::NameExpr(name) => {
            let semantic_decl_id = infer_node_semantic_decl(
                db,
                cache,
                name.syntax().clone(),
                SemanticDeclLevel::default(),
            )?;
            if let LuaSemanticDeclId::Member(member_id) = semantic_decl_id {
                if let Some(first_arg) = call_expr.get_args_list()?.get_args().next()
                    && let Some(arg_type) = infer_alias_self_arg_type(db, cache, first_arg)
                {
                    let self_type = build_self_type(db, &arg_type);
                    return Some(self_type);
                }

                let owner = db.get_member_index().get_current_owner(&member_id)?;
                if let LuaMemberOwner::Type(id) = owner {
                    let typ = LuaType::Ref(id.clone());
                    let self_type = build_self_type(db, &typ);
                    return Some(self_type);
                }
                return None;
            }
        }
        _ => {}
    }

    None
}

fn infer_alias_self_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: LuaExpr,
) -> Option<LuaType> {
    if let LuaExpr::NameExpr(name_expr) = &arg_expr
        && name_expr.get_name_text()? == "self"
        && is_implicit_self_name(db, cache, name_expr)
        && let Some(self_type) = infer_enclosing_self_type(db, cache, name_expr)
    {
        return Some(self_type);
    }

    infer_expr(db, cache, arg_expr).ok()
}

fn is_implicit_self_name(db: &DbIndex, cache: &LuaInferCache, name_expr: &LuaNameExpr) -> bool {
    db.get_decl_index()
        .get_decl_tree(&cache.get_file_id())
        .and_then(|tree| tree.find_local_decl("self", name_expr.get_position()))
        .is_some_and(|decl| decl.is_implicit_self())
}

fn check_expr_can_later_infer(
    context: &mut TplContext,
    func_param_type: &LuaType,
    call_arg_expr: &LuaExpr,
) -> Result<bool, InferFailReason> {
    let doc_function = match func_param_type {
        LuaType::DocFunction(doc_func) => doc_func.clone(),
        LuaType::Signature(sig_id) => {
            let sig = context
                .db
                .get_signature_index()
                .get(sig_id)
                .ok_or(InferFailReason::None)?;

            sig.to_doc_func_type()
        }
        _ => return Ok(false),
    };

    if let LuaExpr::ClosureExpr(_) = call_arg_expr {
        return Ok(true);
    }

    let doc_params = doc_function.get_params();
    let variadic_count = doc_params
        .iter()
        .filter_map(|(_, t)| {
            if let Some(LuaType::Variadic(_)) = t {
                Some(())
            } else {
                None
            }
        })
        .count();

    Ok(variadic_count > 1)
}
