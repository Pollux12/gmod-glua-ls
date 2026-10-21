use std::{ops::Deref, sync::Arc};

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaExpr, LuaIndexMemberExpr, LuaLiteralToken, LuaTableExpr, LuaVarExpr,
};
use rustc_hash::FxHashSet;
use smol_str::SmolStr;

use crate::{
    DbIndex, GlobalId, GmodHookKind, InferFailReason, InferGuard, InferGuardRef, LuaDocParamInfo,
    LuaDocReturnInfo, LuaFunctionType, LuaInferCache, LuaMemberKey, LuaMemberOwner, LuaSignature,
    LuaSignatureId, LuaType, LuaTypeDeclId, RenderLevel, ReturnTypeKind, SignatureReturnStatus,
    TypeOps, VariadicType, get_real_type, humanize_type, infer_call_expr_func, infer_expr,
    infer_table_should_be, instantiate_outside_signature,
};

use super::{
    ResolveResult, UnResolveCallClosureParams, UnResolveClosureReturn, UnResolveParentAst,
    UnResolveParentClosureParams, UnResolveReturn, find_decl_function::find_decl_function_type,
    resolve::try_resolve_return_point, writes::ResolveWrites,
};

pub fn try_resolve_call_closure_params(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    closure_params: &mut UnResolveCallClosureParams,
) -> ResolveResult {
    let Some(callback_func) = callback_contract_for_arg(
        db,
        cache,
        &closure_params.call_expr,
        closure_params.param_idx,
        closure_params.signature_id,
        closure_params.file_id,
    )?
    else {
        return Ok(());
    };
    let async_state = callback_func.get_async_state();
    let params_to_insert = callback_func.get_params().to_vec();

    let signature_id = closure_params.signature_id;
    if db.get_signature_index().get(&signature_id).is_none() {
        return Err(InferFailReason::None);
    }

    // The mutation reads the signature's live state when it lands, so the
    // deferred closure replays it verbatim against whatever the slot holds
    // then — the same state the serial attempt would have seen.
    writes.push(move |db, _| {
        let Some(signature) = db.get_signature_index_mut().get_mut(&signature_id) else {
            return;
        };

        let params_before = signature.params.len();
        let param_docs_before = signature.param_docs.len();
        let async_before = signature.async_state;
        let signature_params = &mut signature.param_docs;
        for (idx, (name, type_ref)) in params_to_insert.iter().enumerate() {
            if signature_params.contains_key(&idx) {
                continue;
            }

            signature_params.insert(
                idx,
                LuaDocParamInfo {
                    name: name.clone(),
                    type_ref: type_ref.clone().unwrap_or(LuaType::Any),
                    default_value: None,
                    description: None,
                    nullable: false,
                    attributes: None,
                },
            );
        }
        if signature.params.len() < params_to_insert.len() {
            let missing_start = signature.params.len();
            for (name, _) in params_to_insert.iter().skip(missing_start) {
                signature.params.push(SmolStr::new(name));
            }
        }

        signature.async_state = async_state;
        // Re-settling an identical contract moves nothing observable: only a
        // novel fact may bump the param epoch, or the fixpoint filter degrades
        // to a full requeue.
        if signature.param_docs.len() != param_docs_before
            || signature.params.len() != params_before
            || signature.async_state != async_before
        {
            db.get_signature_index_mut().note_param_write(signature_id);
        }
    });

    Ok(())
}

pub fn try_resolve_closure_return(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    closure_return: &mut UnResolveClosureReturn,
) -> ResolveResult {
    let call_expr = closure_return.call_expr.clone();
    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let call_expr_type = infer_expr(db, cache, prefix_expr)?;
    let mut param_idx = closure_return.param_idx;
    let call_doc_func = infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        call_expr_type,
        &InferGuard::new(),
        None,
    )?;

    let colon_define = call_doc_func.is_colon_define();
    let colon_call = call_expr.is_colon_call();
    match (colon_define, colon_call) {
        (true, false) => {
            if param_idx == 0 {
                return Ok(());
            }
            param_idx -= 1
        }
        (false, true) => {
            param_idx += 1;
        }
        _ => {}
    }

    let Some(callback_func) = resolve_call_param_doc_function(
        db,
        &call_expr,
        &call_doc_func,
        param_idx,
        closure_return.signature_id,
        closure_return.file_id,
    ) else {
        return Ok(());
    };
    let ret_type = callback_func.get_ret().clone();

    let signature_id = closure_return.signature_id;
    if db.get_signature_index().get(&signature_id).is_none() {
        return Err(InferFailReason::None);
    }

    // An `unknown`/`any` contextual return carries no information, but taking it
    // would clear the body-derived return below and stamp `DocResolve` over the
    // result, which every later repair pass then refuses to touch. Fall back to
    // the body exactly as an unbound template return does.
    if is_uninformative_callback_return(&ret_type) {
        return try_convert_to_func_body_infer(db, writes, cache, closure_return);
    }

    // The slot's `resolve_return` is read when the write lands, matching the
    // state the serial attempt would have mutated in place.
    writes.push(move |db, _| {
        let Some(signature) = db.get_signature_index_mut().get_mut(&signature_id) else {
            return;
        };

        match signature.resolve_return {
            SignatureReturnStatus::UnResolve => {}
            SignatureReturnStatus::InferResolve => {
                signature.return_docs.clear();
            }
            _ => return,
        }

        signature.return_docs.push(LuaDocReturnInfo {
            name: None,
            type_ref: ret_type.clone(),
            default_value: None,
            description: None,
            attributes: None,
            return_kind: ReturnTypeKind::default(),
        });

        signature.resolve_return = SignatureReturnStatus::DocResolve;
        // A DocResolve install is a novel return fact for the signature's
        // return readers.
        db.get_signature_index_mut().note_return_write(signature_id);
    });
    Ok(())
}
fn is_uninformative_callback_return(typ: &LuaType) -> bool {
    if typ.contain_tpl() || typ.is_unknown() || typ.is_any() {
        return true;
    }
    match typ {
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            crate::VariadicType::Base(base) => is_uninformative_callback_return(base),
            crate::VariadicType::Multi(multi) => multi.iter().all(is_uninformative_callback_return),
        },
        _ => false,
    }
}

fn try_convert_to_func_body_infer(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    closure_return: &mut UnResolveClosureReturn,
) -> ResolveResult {
    let mut unresolve = UnResolveReturn {
        file_id: closure_return.file_id,
        signature_id: closure_return.signature_id,
        return_points: closure_return.return_points.clone(),
    };

    try_resolve_return_point(db, writes, cache, &mut unresolve)
}

/// The function type the callee of `call_expr` declares for its argument at
/// `arg_idx`: the contract a callback passed there is called with. `origin`
/// is the callback's own signature, which a hook contract is chosen for.
pub(crate) fn callback_contract_for_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    arg_idx: usize,
    origin: LuaSignatureId,
    call_file_id: crate::FileId,
) -> Result<Option<Arc<LuaFunctionType>>, InferFailReason> {
    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let call_expr_type = infer_expr(db, cache, prefix_expr)?;
    let call_doc_func = infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        call_expr_type,
        &InferGuard::new(),
        None,
    )?;
    let param_idx = match (call_expr.is_colon_call(), call_doc_func.is_colon_define()) {
        (true, false) => arg_idx + 1,
        (false, true) if arg_idx == 0 => return Ok(None),
        (false, true) => arg_idx - 1,
        _ => arg_idx,
    };
    Ok(resolve_call_param_doc_function(
        db,
        call_expr,
        &call_doc_func,
        param_idx,
        origin,
        call_file_id,
    ))
}

fn resolve_call_param_doc_function(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    call_doc_func: &LuaFunctionType,
    param_idx: usize,
    origin_signature_id: LuaSignatureId,
    call_file_id: crate::FileId,
) -> Option<Arc<LuaFunctionType>> {
    if let Some(param_type) = call_doc_func.get_params().get(param_idx) {
        let param_type = get_real_type(db, param_type.1.as_ref().unwrap_or(&LuaType::Any))?;
        match param_type {
            LuaType::DocFunction(func) => return Some(func.clone()),
            LuaType::Union(union_types) => {
                if let Some(LuaType::DocFunction(func)) = union_types
                    .types()
                    .filter(|typ| matches!(typ, LuaType::DocFunction(_)))
                    .min_by_key(|typ| stable_type_selection_key(db, typ))
                {
                    return Some(func.clone());
                }
            }
            _ => {}
        }
    }

    resolve_gmod_hook_add_callback_doc_function(
        db,
        call_expr,
        param_idx,
        Some(origin_signature_id),
        call_file_id,
    )
}

fn stable_type_selection_key(db: &DbIndex, typ: &LuaType) -> String {
    humanize_type(db, typ, RenderLevel::Detailed)
}
/// Counts hook-candidate params carrying an informative (non-placeholder)
/// type. Used to rank same-hook candidates by provenance quality instead of
/// batch order: an annotated contract outranks a peer closure whose params
/// this pass copied, and a return-only candidate never outranks either.
fn informative_hook_param_count(func: &LuaFunctionType) -> usize {
    func.get_params()
        .iter()
        .filter(|(_, ty)| match ty {
            Some(t) => !(t.is_any() || t.is_unknown()),
            None => false,
        })
        .count()
}

pub fn resolve_gmod_hook_add_callback_doc_function(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    param_idx: usize,
    origin_signature_id: Option<LuaSignatureId>,
    call_file_id: crate::FileId,
) -> Option<Arc<LuaFunctionType>> {
    resolve_gmod_hook_callback_doc_function(
        db,
        call_expr,
        param_idx,
        origin_signature_id,
        call_file_id,
    )
    .map(|resolved| resolved.function)
}

pub struct GmodHookCallbackDocFunction {
    pub hook_name: String,
    pub function: Arc<LuaFunctionType>,
}

pub fn resolve_gmod_hook_callback_doc_function(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    param_idx: usize,
    origin_signature_id: Option<LuaSignatureId>,
    call_file_id: crate::FileId,
) -> Option<GmodHookCallbackDocFunction> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }

    let hook_site = db
        .get_gmod_infer_index()
        .get_hook_file_metadata(&call_file_id)?
        .sites
        .iter()
        .find(|site| {
            site.syntax_id == call_expr.get_syntax_id()
                && site.kind == GmodHookKind::Add
                && site.callback_arg_idx == Some(param_idx)
        })?;
    let hook_name = hook_site.hook_name.as_ref()?.clone();
    let member_key = LuaMemberKey::Name(hook_name.clone().into());
    let mut candidates = Vec::new();
    let mut seen_member_ids = FxHashSet::default();
    for owner_name in iter_hook_owner_names(db) {
        for owner in [
            LuaMemberOwner::Type(LuaTypeDeclId::global(&owner_name)),
            LuaMemberOwner::GlobalPath(GlobalId::new(&owner_name)),
        ] {
            let Some(item) = db.get_member_index().get_member_item(&owner, &member_key) else {
                continue;
            };

            for member_id in item.visible_member_ids_with_realm_at_offset(
                db,
                &call_file_id,
                call_expr.get_range().start(),
            ) {
                if !seen_member_ids.insert(member_id) {
                    continue;
                }
                let Some(type_cache) = db.get_type_index().get_type_cache(&member_id.into()) else {
                    continue;
                };
                let Some(function_types) = filter_signature_type(
                    db,
                    type_cache.as_type(),
                    origin_signature_id.as_ref(),
                    true,
                    false,
                ) else {
                    continue;
                };

                candidates.extend(function_types);
            }
        }
    }

    // Several hook owners can declare the same hook (the annotated `GM`
    // method plus a peer closure filling the slot): prefer the candidate
    // with the most informative params so the pick is a function of settled
    // inputs, not of which sibling the batch resolved first. A return-only
    // candidate (no annotated params) still serves hover when it is the only
    // one, but never outranks an annotated contract. Ties break on the
    // candidate content so identically-informative candidates stay stable
    // across orders.
    candidates.sort_by(|a, b| {
        informative_hook_param_count(a)
            .cmp(&informative_hook_param_count(b))
            .reverse()
            .then_with(|| {
                stable_type_selection_key(db, &LuaType::DocFunction(Arc::clone(a))).cmp(
                    &stable_type_selection_key(db, &LuaType::DocFunction(Arc::clone(b))),
                )
            })
    });
    let function = candidates.into_iter().next()?;
    Some(GmodHookCallbackDocFunction {
        hook_name,
        function,
    })
}

fn iter_hook_owner_names(db: &DbIndex) -> Vec<String> {
    let mut names = vec![
        "GM".to_string(),
        "GAMEMODE".to_string(),
        "SANDBOX".to_string(),
        "PLUGIN".to_string(),
    ];
    for scoped_hook_owner in db
        .get_emmyrc()
        .gmod
        .scripted_class_scopes
        .hook_owner_globals()
    {
        if !names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&scoped_hook_owner))
        {
            names.push(scoped_hook_owner);
        }
    }
    for configured_prefix in &db.get_emmyrc().gmod.hook_mappings.method_prefixes {
        let normalized = configured_prefix
            .trim()
            .trim_end_matches([':', '.'])
            .to_string();
        if normalized.is_empty() {
            continue;
        }

        if !names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&normalized))
        {
            names.push(normalized);
        }
    }

    names
}

pub fn extract_hook_name(call_expr: &LuaCallExpr) -> Option<String> {
    let args = call_expr.get_args_list()?;
    let first_arg = args.get_args().next()?;
    let LuaExpr::LiteralExpr(literal_expr) = first_arg else {
        return None;
    };
    let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
        return None;
    };

    let hook_name = string_token.get_value();
    let trimmed = hook_name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn try_resolve_closure_parent_params(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    closure_params: &mut UnResolveParentClosureParams,
) -> ResolveResult {
    let origin_sig_id = closure_params.signature_id;
    let signature = db
        .get_signature_index()
        .get(&closure_params.signature_id)
        .ok_or(InferFailReason::None)?;
    if !signature.param_docs.is_empty()
        && !signature.params_filled_from_slot
        && !signature.return_filled_from_slot
    {
        return Ok(());
    }
    let self_type;
    let member_type = match &closure_params.parent_ast {
        UnResolveParentAst::LuaFuncStat(func_stat) => {
            let func_name = func_stat.get_func_name().ok_or(InferFailReason::None)?;
            match func_name {
                LuaVarExpr::IndexExpr(index_expr) => {
                    let prefix_expr = index_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
                    let prefix_type = infer_expr(db, cache, prefix_expr)?;
                    self_type = Some(prefix_type.clone());
                    find_best_function_type(
                        db,
                        cache,
                        &prefix_type,
                        LuaIndexMemberExpr::IndexExpr(index_expr),
                        signature,
                        Some(&origin_sig_id),
                    )
                    .ok_or(InferFailReason::None)?
                }
                _ => return Ok(()),
            }
        }
        UnResolveParentAst::LuaTableField(table_field) => {
            let parnet_table_expr = table_field
                .get_parent::<LuaTableExpr>()
                .ok_or(InferFailReason::None)?;
            let parent_table_type = infer_table_should_be(db, cache, parnet_table_expr)?;
            self_type = Some(parent_table_type.clone());
            find_best_function_type(
                db,
                cache,
                &parent_table_type,
                LuaIndexMemberExpr::TableField(table_field.clone()),
                signature,
                Some(&origin_sig_id),
            )
            .ok_or(InferFailReason::None)?
        }
        UnResolveParentAst::LuaAssignStat(assign) => {
            let (vars, exprs) = assign.get_var_and_expr_list();
            let position = closure_params.signature_id.get_position();
            let idx = exprs
                .iter()
                .position(|expr| expr.get_position() == position)
                .ok_or(InferFailReason::None)?;
            let var = vars.get(idx).ok_or(InferFailReason::None)?;
            match var {
                LuaVarExpr::IndexExpr(index_expr) => {
                    let prefix_expr = index_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
                    let prefix_expr_type = infer_expr(db, cache, prefix_expr)?;
                    self_type = Some(prefix_expr_type.clone());
                    find_best_function_type(
                        db,
                        cache,
                        &prefix_expr_type,
                        LuaIndexMemberExpr::IndexExpr(index_expr.clone()),
                        signature,
                        Some(&origin_sig_id),
                    )
                    .ok_or(InferFailReason::None)?
                }
                _ => return Ok(()),
            }
        }
    };

    resolve_closure_member_type(
        db,
        writes,
        closure_params,
        &member_type,
        self_type,
        &InferGuard::new(),
    )
}

/// Defers one `resolve_doc_function` write. The signature's existence is the
/// only thing the attempt can fail on, so it is checked up front; every other
/// read the mutation makes is replayed inside the closure against the live
/// slot, which is exactly the state the serial attempt would have seen.
fn defer_resolve_doc_function(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    signature_id: LuaSignatureId,
    doc_func: &LuaFunctionType,
    self_type: Option<LuaType>,
) -> ResolveResult {
    if db.get_signature_index().get(&signature_id).is_none() {
        return Err(InferFailReason::None);
    }
    let doc_func = doc_func.clone();
    writes.push(move |db, _| {
        let _ = resolve_doc_function(db, signature_id, &doc_func, self_type);
    });
    Ok(())
}

fn resolve_closure_member_type(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    closure_params: &mut UnResolveParentClosureParams,
    member_type: &LuaType,
    self_type: Option<LuaType>,
    infer_guard: &InferGuardRef,
) -> ResolveResult {
    match &member_type {
        LuaType::DocFunction(doc_func) => {
            let target = db
                .get_signature_index()
                .get(&closure_params.signature_id)
                .ok_or(InferFailReason::None)?;
            defer_resolve_doc_function(
                db,
                writes,
                closure_params.signature_id,
                &contract_for_target(db, target, doc_func),
                self_type,
            )
        }
        LuaType::Signature(id) => {
            if id == &closure_params.signature_id {
                return Ok(());
            }
            let signature = db.get_signature_index().get(id);

            if let Some(signature) = signature {
                // An empty `return_docs` renders as `-> nil`, so a base whose
                // return has not settled yet hands out a contract claiming it
                // returns nothing. `resolve_doc_function` stamps that as
                // `DocResolve`, which no later pass reopens, so the override
                // would keep whichever answer the base happened to hold when
                // this ran. Wait for the base to settle instead.
                if !signature.is_resolve_return() {
                    return Err(InferFailReason::UnResolveSignatureReturn(*id));
                }
                // See `filter_signature_type`: params this pass copied off a
                // sibling's receiver are that sibling's answer, not the slot's.
                // A slot-filled flag with no docs at all carries no params to
                // inherit, so it must not suppress a typed union member.
                if signature.params_filled_from_slot && !signature.param_docs.is_empty() {
                    return Ok(());
                }
                // A return inferred from the base's body describes that body, not
                // a contract its overrides keep, so only a documented one carries.
                // The base's signature read is recorded, so an unsettled return
                // re-runs this copy once the base settles.
                let documented = signature.resolve_return == SignatureReturnStatus::DocResolve;
                let source_return = if documented {
                    signature.get_return_type()
                } else {
                    LuaType::Unknown
                };
                let target = db
                    .get_signature_index()
                    .get(&closure_params.signature_id)
                    .ok_or(InferFailReason::None)?;
                let source_contract = Arc::new(contract_outside_signature(
                    db,
                    signature,
                    source_return.clone(),
                ));
                defer_resolve_doc_function(
                    db,
                    writes,
                    closure_params.signature_id,
                    &contract_for_target(db, target, &source_contract),
                    self_type,
                )
            } else {
                Ok(())
            }
        }
        LuaType::Union(union_types) => {
            let signature = db
                .get_signature_index()
                .get(&closure_params.signature_id)
                .ok_or(InferFailReason::None)?;
            let mut multi_function_type = Vec::new();
            for typ in union_types.into_vec() {
                match typ {
                    LuaType::DocFunction(func) => {
                        multi_function_type.push(func.clone());
                    }
                    LuaType::Ref(ref_id) => {
                        if infer_guard.check(&ref_id).is_err() {
                            continue;
                        }
                        let type_decl = db
                            .get_type_index()
                            .get_type_decl(&ref_id)
                            .ok_or(InferFailReason::None)?;

                        if let Some(origin) = type_decl.get_alias_origin(db, None)
                            && let LuaType::DocFunction(f) = origin
                        {
                            multi_function_type.push(f);
                        }
                    }
                    _ => {}
                };
            }

            let final_params = merge_contract_params(
                db,
                signature.get_type_params(),
                signature.is_colon_define,
                &multi_function_type,
                ParamMerge::Union,
            );
            let mut final_ret = LuaType::Never;
            for doc_func in &multi_function_type {
                final_ret = TypeOps::Union.apply(db, &final_ret, doc_func.get_ret());
            }

            // Nothing contributed a return type: pin the escape value instead of
            // letting the seed reach the synthesized signature.
            if final_ret.is_never() {
                final_ret = LuaType::Unknown;
            }

            defer_resolve_doc_function(
                db,
                writes,
                closure_params.signature_id,
                &LuaFunctionType::new(
                    signature.async_state,
                    signature.is_colon_define,
                    signature.is_vararg,
                    final_params,
                    final_ret,
                ),
                self_type,
            )
        }
        LuaType::Ref(ref_id) => {
            infer_guard.check(ref_id)?;
            let type_decl = db
                .get_type_index()
                .get_type_decl(ref_id)
                .ok_or(InferFailReason::None)?;

            if type_decl.is_alias()
                && let Some(origin) = type_decl.get_alias_origin(db, None)
            {
                return resolve_closure_member_type(
                    db,
                    writes,
                    closure_params,
                    &origin,
                    self_type,
                    infer_guard,
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn resolve_doc_function(
    db: &mut DbIndex,
    signature_id: LuaSignatureId,
    doc_func: &LuaFunctionType,
    self_type: Option<LuaType>,
) -> ResolveResult {
    let signature = db
        .get_signature_index_mut()
        .get_mut(&signature_id)
        .ok_or(InferFailReason::None)?;

    let async_before = signature.async_state;
    signature.async_state = doc_func.get_async_state();

    let mut doc_params = doc_func.get_params().to_vec();
    // doc_func is the valid signature traced upwards, signature is the unresolved signature
    match (doc_func.is_colon_define(), signature.is_colon_define) {
        (true, false) => {
            // The original signature is colon-defined but the unresolved one is not, so the first parameter must be inserted
            doc_params.insert(0, ("self".to_string(), Some(LuaType::SelfInfer)));
        }
        (false, true) => {
            if !doc_params.is_empty() {
                doc_params.remove(0);
            }
        }
        _ => {}
    }

    let receiver_filled_from_slot = !signature.is_colon_define
        && doc_params
            .first()
            .is_some_and(|(_, typ)| typ.as_ref().is_some_and(LuaType::is_self_infer));
    if let Some(self_type) = self_type
        && let Some((_, Some(typ))) = doc_params.first()
        && typ.is_self_infer()
    {
        doc_params[0].1 = Some(self_type);
    }

    let params_before = signature.param_docs.clone();
    let filled_from_slot_before = signature.params_filled_from_slot;
    let receiver_from_slot_before = signature.receiver_filled_from_slot;
    signature.params_filled_from_slot |= !doc_params.is_empty();
    signature.receiver_filled_from_slot = receiver_filled_from_slot;
    for (index, param) in doc_params.iter().enumerate() {
        let name = signature
            .params
            .get(index)
            .map_or(param.0.as_str(), SmolStr::as_str);
        signature.param_docs.insert(
            index,
            LuaDocParamInfo {
                name: name.to_string(),
                type_ref: param.1.clone().unwrap_or(LuaType::Any),
                default_value: None,
                description: None,
                nullable: false,
                attributes: None,
            },
        );
    }
    // Overwriting an identical contract moves nothing observable: only a
    // novel fact may bump the param epoch. The tick fires below, after the
    // signature borrow ends.
    let params_changed = signature.param_docs != params_before
        || signature.params_filled_from_slot != filled_from_slot_before
        || signature.receiver_filled_from_slot != receiver_from_slot_before
        || signature.async_state != async_before;

    let return_before = (
        signature.resolve_return,
        signature.return_docs.clone(),
        signature.return_filled_from_slot,
    );

    // An unknown contract return says nothing about the result, so the
    // closure keeps the return its own body gives.
    let doc_return = doc_func.get_ret();
    let constrains_return = !doc_return.is_unknown();
    let inferred_tail = (constrains_return
        && signature.resolve_return == SignatureReturnStatus::InferResolve)
        .then(|| {
            inferred_return_tail_matching_documented_first(
                doc_return,
                &signature.get_return_type(),
                |slot| signature.return_correlation_implies(0, slot),
            )
        })
        .flatten();
    let should_apply_return = constrains_return
        && match signature.resolve_return {
            SignatureReturnStatus::UnResolve => true,
            SignatureReturnStatus::InferResolve => {
                should_doc_return_override_inferred(doc_return, &signature.get_return_type())
            }
            SignatureReturnStatus::DocResolve => signature.return_filled_from_slot,
        };

    if let Some(inferred_tail) = inferred_tail {
        signature.resolve_return = SignatureReturnStatus::DocResolve;
        signature.return_filled_from_slot = true;
        signature.return_docs.clear();
        signature.return_docs.push(LuaDocReturnInfo {
            name: None,
            type_ref: doc_return.clone(),
            default_value: None,
            description: None,
            attributes: None,
            return_kind: ReturnTypeKind::default(),
        });
        signature
            .return_docs
            .extend(inferred_tail.into_iter().map(|type_ref| LuaDocReturnInfo {
                name: None,
                type_ref,
                default_value: None,
                description: None,
                attributes: None,
                return_kind: ReturnTypeKind::default(),
            }));
    } else if should_apply_return {
        let slot_refresh = signature.resolve_return == SignatureReturnStatus::DocResolve;
        signature.resolve_return = SignatureReturnStatus::DocResolve;
        signature.return_filled_from_slot = true;
        if slot_refresh {
            if let Some(first) = signature.return_docs.first_mut() {
                first.type_ref = doc_return.clone();
            } else {
                signature.return_docs.push(LuaDocReturnInfo {
                    name: None,
                    type_ref: doc_return.clone(),
                    default_value: None,
                    description: None,
                    attributes: None,
                    return_kind: ReturnTypeKind::default(),
                });
            }
        } else {
            signature.return_docs.clear();
            signature.return_docs.push(LuaDocReturnInfo {
                name: None,
                type_ref: doc_return.clone(),
                default_value: None,
                description: None,
                attributes: None,
                return_kind: ReturnTypeKind::default(),
            });
        }
    }
    // A DocResolve install is a novel return fact for the signature's return
    // readers, routed through the return epoch like every other return write.
    let return_changed = signature.resolve_return != return_before.0
        || signature.return_docs != return_before.1
        || signature.return_filled_from_slot != return_before.2;
    if params_changed {
        db.get_signature_index_mut().note_param_write(signature_id);
    }
    if return_changed {
        db.get_signature_index_mut().note_return_write(signature_id);
    }

    Ok(())
}

/// Retains body-inferred extra Lua results when a contextual function contract
/// describes only the same first result. Exact first-slot agreement prevents a
/// different contract from widening the documented return, while informative
/// tail checks keep unresolved inference from leaking into callers.
pub(super) fn inferred_return_tail_matching_documented_first<F>(
    documented_first: &LuaType,
    inferred_return: &LuaType,
    correlation_implies_non_nil: F,
) -> Option<Vec<LuaType>>
where
    F: Fn(usize) -> bool,
{
    if documented_first.is_variadic() {
        return None;
    }

    let LuaType::Variadic(variadic) = inferred_return else {
        return None;
    };
    let VariadicType::Multi(inferred_slots) = variadic.deref() else {
        return None;
    };
    if inferred_slots.len() <= 1
        || inferred_slots
            .iter()
            .skip(1)
            .any(|typ| typ.is_any() || typ.is_unknown())
    {
        return None;
    }

    let inferred_first = exact_non_nil_type(inferred_slots.first()?);
    let documented_first_non_nil = exact_non_nil_type(documented_first);
    if inferred_first != documented_first_non_nil {
        return None;
    }

    let mut inferred_tail = inferred_slots[1..].to_vec();
    if documented_first.is_always_truthy() {
        for (tail_idx, typ) in inferred_tail.iter_mut().enumerate() {
            if correlation_implies_non_nil(tail_idx + 1) {
                *typ = exact_non_nil_type(typ);
            }
        }
    }
    Some(inferred_tail)
}

fn exact_non_nil_type(typ: &LuaType) -> LuaType {
    match typ {
        LuaType::Union(union) => LuaType::from_vec(
            union
                .types()
                .filter(|component| !component.is_nil())
                .cloned()
                .collect(),
        ),
        _ => typ.clone(),
    }
}

fn should_doc_return_override_inferred(doc_return: &LuaType, inferred_return: &LuaType) -> bool {
    !(doc_return.is_any() || doc_return.is_unknown())
        || inferred_return.is_any()
        || inferred_return.is_unknown()
}

#[cfg(test)]
mod inferred_return_tail_tests {
    use super::inferred_return_tail_matching_documented_first;
    use crate::{LuaType, VariadicType};

    #[test]
    fn matching_documented_first_preserves_only_informative_tail_slots() {
        let inferred = LuaType::Variadic(
            VariadicType::Multi(vec![
                LuaType::from_vec(vec![LuaType::String, LuaType::Nil]),
                LuaType::from_vec(vec![LuaType::Number, LuaType::Nil]),
            ])
            .into(),
        );

        assert_eq!(
            inferred_return_tail_matching_documented_first(&LuaType::String, &inferred, |_| true),
            Some(vec![LuaType::Number])
        );
    }

    #[test]
    fn incompatible_documented_first_rejects_inferred_tail() {
        let inferred =
            LuaType::Variadic(VariadicType::Multi(vec![LuaType::Number, LuaType::Boolean]).into());

        assert_eq!(
            inferred_return_tail_matching_documented_first(&LuaType::String, &inferred, |_| false),
            None
        );
    }

    #[test]
    fn uninformative_inferred_tail_is_not_exposed() {
        let inferred =
            LuaType::Variadic(VariadicType::Multi(vec![LuaType::String, LuaType::Unknown]).into());

        assert_eq!(
            inferred_return_tail_matching_documented_first(&LuaType::String, &inferred, |_| false),
            None
        );
    }

    #[test]
    fn nullable_documented_first_does_not_make_correlated_tail_non_nil() {
        let nullable_string = LuaType::from_vec(vec![LuaType::String, LuaType::Nil]);
        let nullable_number = LuaType::from_vec(vec![LuaType::Number, LuaType::Nil]);
        let inferred = LuaType::Variadic(
            VariadicType::Multi(vec![nullable_string.clone(), nullable_number.clone()]).into(),
        );

        assert_eq!(
            inferred_return_tail_matching_documented_first(&nullable_string, &inferred, |_| true),
            Some(vec![nullable_number])
        );
    }
}

/// `sig`'s params with `ret`, as a contract for a closure outside `sig`'s
/// generic context, merged over all of `sig`'s forms.
fn contract_outside_signature(db: &DbIndex, sig: &LuaSignature, ret: LuaType) -> LuaFunctionType {
    let instantiate_params = |params: &[(String, Option<LuaType>)]| -> Vec<_> {
        params
            .iter()
            .map(|(name, typ)| {
                (
                    name.clone(),
                    typ.as_ref()
                        .map(|typ| instantiate_outside_signature(db, sig, typ)),
                )
            })
            .collect()
    };
    let overloads: Vec<_> = sig
        .overloads
        .iter()
        .map(|overload| {
            Arc::new(LuaFunctionType::new(
                overload.get_async_state(),
                overload.is_colon_define(),
                overload.is_variadic(),
                instantiate_params(overload.get_params()),
                LuaType::Unknown,
            ))
        })
        .collect();
    let params = merge_contract_params(
        db,
        instantiate_params(&sig.get_type_params()),
        sig.is_colon_define,
        &overloads,
        ParamMerge::Agreement,
    );
    LuaFunctionType::new(
        sig.async_state,
        sig.is_colon_define,
        sig.is_vararg,
        params,
        instantiate_outside_signature(db, sig, &ret),
    )
    .with_optional_params(sig.get_param_optional_flags())
}

/// `contract` as `target` takes it, by `target`'s own params.
fn contract_for_target(
    db: &DbIndex,
    target: &LuaSignature,
    contract: &Arc<LuaFunctionType>,
) -> LuaFunctionType {
    let target_params = target
        .params
        .iter()
        .map(|name| (name.to_string(), None))
        .collect();
    LuaFunctionType::new(
        contract.get_async_state(),
        target.is_colon_define,
        target.is_vararg,
        merge_contract_params(
            db,
            target_params,
            target.is_colon_define,
            std::slice::from_ref(contract),
            ParamMerge::Agreement,
        ),
        contract.get_ret().clone(),
    )
}

/// How [`merge_contract_params`] settles a position its contracts type
/// differently.
#[derive(Clone, Copy)]
enum ParamMerge {
    /// The closure's own forms: the position takes any of their types.
    Union,
    /// A base's forms, for an override that declares none: the position is
    /// `any`, as a `...` is over later positions that differ.
    Agreement,
}

/// Merges `contracts` into the `params` of a closure defined with
/// `is_colon_define`. A `...` takes the type of the contract's own `...`, and
/// is `any` where the contract has a position there instead.
fn merge_contract_params(
    db: &DbIndex,
    mut params: Vec<(String, Option<LuaType>)>,
    is_colon_define: bool,
    contracts: &[Arc<LuaFunctionType>],
    merge: ParamMerge,
) -> Vec<(String, Option<LuaType>)> {
    let mut variadic_type = LuaType::Unknown;
    for contract in contracts {
        let mut contract_params = contract.get_params().to_vec();
        match (contract.is_colon_define(), is_colon_define) {
            (true, false) => {
                // The original signature is colon-defined but the unresolved one is not, so the first parameter must be inserted
                contract_params.insert(0, ("self".to_string(), Some(LuaType::SelfInfer)));
            }
            (false, true) => {
                // The original signature is not colon-defined but the unresolved one is, so the first parameter must be removed
                if !contract_params.is_empty() {
                    contract_params.remove(0);
                }
            }
            _ => {}
        }

        for (idx, param) in contract_params.iter().enumerate() {
            if let Some(final_param) = params.get(idx) {
                if final_param.0 == "..." {
                    // Only a `...` in the contract types it; any other slot there is a position, not the forwarded values
                    let typ = if param.0 == "..." {
                        param.1.clone()
                    } else {
                        Some(LuaType::Any)
                    };
                    if let Some(typ) = typ {
                        if variadic_type == LuaType::Unknown {
                            variadic_type = typ;
                        } else if variadic_type != typ {
                            variadic_type = LuaType::Any;
                        }
                    }

                    break;
                }
                let new_type = match (&final_param.1, &param.1) {
                    (Some(seen), Some(typ)) if seen != typ => Some(match merge {
                        ParamMerge::Union => TypeOps::Union.apply(db, seen, typ),
                        ParamMerge::Agreement => LuaType::Any,
                    }),
                    (seen, typ) => seen.clone().or_else(|| typ.clone()),
                };
                params[idx] = (final_param.0.clone(), new_type);
            } else {
                params.push((param.0.clone(), param.1.clone()));
            }
        }
    }

    if !variadic_type.is_unknown()
        && let Some(param) = params.last_mut()
        && param.0 == "..."
        && param.1.is_none()
    {
        param.1 = Some(variadic_type);
    }
    params
}

fn filter_signature_type(
    db: &DbIndex,
    typ: &LuaType,
    origin_signature_id: Option<&LuaSignatureId>,
    preserve_returns: bool,
    preserve_implicit_receiver: bool,
) -> Option<Vec<Arc<LuaFunctionType>>> {
    let mut result: Vec<Arc<LuaFunctionType>> = Vec::new();
    let mut stack = Vec::new();
    stack.push(typ.clone());
    let guard = InferGuard::new();
    while let Some(typ) = stack.pop() {
        match typ {
            LuaType::DocFunction(func) => {
                result.push(func.clone());
            }
            LuaType::Signature(sig_id) => {
                // Skip the current closure's own signature to avoid self-reference
                if origin_signature_id != Some(&sig_id) {
                    if let Some(sig) = db.get_signature_index().get(&sig_id) {
                        // Convert annotated signature to DocFunction for param propagation.
                        // When preserve_returns is false (monkey-patch path), emit a DocFunction
                        // for annotated params or an implicit colon receiver. We want parameter
                        // propagation but must NOT force a return constraint (could break
                        // os.exit-style patches).
                        // When preserve_returns is true (hook hover path), emit even for return-
                        // only hooks so that `@return`-annotated hooks with no params display
                        // correctly as `function() -> boolean` rather than silently degrading.
                        // A peer closure filling the same slot has params only
                        // because this pass copied them off its own receiver.
                        // Inheriting them would make this closure's receiver a
                        // question of which sibling the batch resolved first.
                        let annotated_params =
                            !sig.param_docs.is_empty() && !sig.params_filled_from_slot;
                        let has_useful_info = if preserve_returns {
                            annotated_params || !sig.get_return_type().is_nil()
                        } else {
                            annotated_params || (preserve_implicit_receiver && sig.is_colon_define)
                        };
                        if has_useful_info {
                            let ret = if preserve_returns {
                                sig.get_return_type()
                            } else {
                                LuaType::Unknown
                            };
                            result.push(Arc::new(contract_outside_signature(db, sig, ret)));
                        }
                    }
                }
            }
            LuaType::Union(union) => {
                let types = union.into_vec();
                for typ in types.into_iter().rev() {
                    stack.push(typ);
                }
            }
            LuaType::Ref(type_ref_id) => {
                // On cycle, skip this type but continue accumulating other results.
                if guard.check(&type_ref_id).is_err() {
                    continue;
                }
                let Some(type_decl) = db.get_type_index().get_type_decl(&type_ref_id) else {
                    continue;
                };
                if let Some(func) = type_decl.get_alias_origin(db, None) {
                    match func {
                        LuaType::DocFunction(f) => {
                            result.push(f.clone());
                        }
                        LuaType::Union(union) => {
                            let types = union.into_vec();
                            for typ in types.into_iter().rev() {
                                stack.push(typ);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn find_best_function_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    index_member_expr: LuaIndexMemberExpr,
    origin_signature: &LuaSignature,
    origin_sig_id: Option<&LuaSignatureId>,
) -> Option<LuaType> {
    // Find a signature that is not the one itself
    if let Ok(result) = find_decl_function_type(db, cache, prefix_type, index_member_expr) {
        let preserve_implicit_receiver = !origin_signature.is_colon_define
            && origin_signature
                .params
                .first()
                .is_some_and(|param| param != "...");
        if let Some(filtered_types) = filter_signature_type(
            db,
            &result.typ,
            origin_sig_id,
            false,
            preserve_implicit_receiver,
        ) {
            // Parent declarations may describe distinct callback and callable forms.
            if !result.is_current_owner
                && let Some(parent_type) = filtered_types.first()
            {
                return Some(LuaType::DocFunction(parent_type.clone()));
            }

            match filtered_types.len() {
                0 => {}
                1 => return Some(LuaType::DocFunction(filtered_types[0].clone())),
                _ => {
                    return Some(LuaType::from_vec(
                        filtered_types
                            .into_iter()
                            .map(|func| LuaType::DocFunction(func.clone()))
                            .collect(),
                    ));
                }
            }
        }

        if !result.is_current_owner {
            return Some(result.typ);
        }
    }

    match origin_signature.overloads.len() {
        0 => None,
        1 => origin_signature
            .overloads
            .clone()
            .into_iter()
            .next()
            .map(LuaType::DocFunction),
        _ => Some(LuaType::from_vec(
            origin_signature
                .overloads
                .clone()
                .into_iter()
                .map(LuaType::DocFunction)
                .collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaParser, ParserConfig};

    use super::resolve_call_param_doc_function;
    use crate::{
        AsyncState, DbIndex, FileId, LuaFunctionType, LuaSignatureId, LuaType, LuaUnionType,
    };

    fn parse_call_and_closure(code: &str) -> (LuaCallExpr, LuaClosureExpr) {
        let tree = LuaParser::parse(code, ParserConfig::default());
        let root = tree.get_chunk_node();
        let call_expr = root
            .descendants::<LuaCallExpr>()
            .next()
            .expect("expected call expression");
        let closure_expr = root
            .descendants::<LuaClosureExpr>()
            .next()
            .expect("expected closure expression");
        (call_expr, closure_expr)
    }

    fn make_doc_func(
        param_name: &str,
        param_type: LuaType,
        return_type: LuaType,
    ) -> Arc<LuaFunctionType> {
        Arc::new(LuaFunctionType::new(
            AsyncState::None,
            false,
            false,
            vec![(param_name.to_string(), Some(param_type))],
            return_type,
        ))
    }

    fn make_call_sig_with_callback_union(union: LuaUnionType) -> LuaFunctionType {
        LuaFunctionType::new(
            AsyncState::None,
            false,
            false,
            vec![(
                "callback".to_string(),
                Some(LuaType::Union(Arc::new(union))),
            )],
            LuaType::Nil,
        )
    }

    #[test]
    fn resolve_call_param_doc_function_union_doc_function_order_is_deterministic() {
        let db = DbIndex::new();
        let file_id = FileId::new(1);
        let (call_expr, closure_expr) = parse_call_and_closure("register(function() end)");
        let origin_signature_id = LuaSignatureId::from_closure(file_id, &closure_expr);

        let alpha_doc_func = make_doc_func("alpha", LuaType::String, LuaType::Boolean);
        let beta_doc_func = make_doc_func("beta", LuaType::Integer, LuaType::Number);

        let alpha_first_sig = make_call_sig_with_callback_union(LuaUnionType::from_vec(vec![
            LuaType::DocFunction(alpha_doc_func.clone()),
            LuaType::DocFunction(beta_doc_func.clone()),
        ]));
        let beta_first_sig = make_call_sig_with_callback_union(LuaUnionType::from_vec(vec![
            LuaType::DocFunction(beta_doc_func),
            LuaType::DocFunction(alpha_doc_func),
        ]));

        let selected_alpha_first = resolve_call_param_doc_function(
            &db,
            &call_expr,
            &alpha_first_sig,
            0,
            origin_signature_id,
            file_id,
        )
        .expect("expected callback doc function to be selected from alpha-first union");
        let selected_beta_first = resolve_call_param_doc_function(
            &db,
            &call_expr,
            &beta_first_sig,
            0,
            origin_signature_id,
            file_id,
        )
        .expect("expected callback doc function to be selected from beta-first union");

        assert_eq!(
            selected_alpha_first.as_ref(),
            selected_beta_first.as_ref(),
            "doc function selection should be independent of union member order"
        );
    }
}
