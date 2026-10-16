use std::{ops::Deref, sync::Arc};

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaChunk, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIndexExpr,
    LuaIndexKey, LuaIndexMemberExpr, LuaStat, LuaTableExpr, LuaTableField, LuaVarExpr,
};

use crate::{
    DbIndex, FileId, FlowNode, FlowTree, GMOD_ATTR_SELF_GUARD, InFiled, InferFailReason,
    InferGuard, LuaAliasCallKind, LuaAliasCallType, LuaFunctionType, LuaInferCache, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaSignatureCast, LuaSignatureId, LuaType,
    TypeOps, check_type_compact, find_signature_attribute_use, infer_call_expr_func, infer_expr,
    semantic::{
        SemanticDeclGuard, SemanticDeclLevel,
        infer::{
            VarRefId,
            infer_index::infer_member_by_member_key,
            narrow::{
                ResultTypeOrContinue, call_var_ref_root,
                condition_flow::{
                    InferConditionFlow, binary_flow::can_narrow_type_guard_through_decl_alias,
                    get_condition_antecedent_type,
                },
                get_type_at_cast_flow::apply_cast,
                get_type_at_flow::FlowWalkPolicy,
                gmod_null_type, narrow_down_type, narrow_false_or_nil, remove_false_or_nil,
                var_ref_id::get_var_expr_var_ref_id,
            },
        },
        infer_expr_semantic_decl, known_signature_id, local_name_value, semantic_decl_value,
    },
    signature_is_valid_guard_or_base_runtime_isvalid_in_realm,
};

#[allow(clippy::too_many_arguments)]
pub fn get_type_at_call_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };
    if !call_names_var(db, cache, root, &call_expr, &prefix_expr, var_ref_id) {
        return Ok(ResultTypeOrContinue::Continue);
    }

    // Keep references for predicate fallbacks that need the original call shape.
    let call_expr_ref = call_expr.clone();
    let prefix_expr_ref = prefix_expr.clone();

    // If we can't infer the function type, skip type-based narrowing but still
    // fall through to supported fallback handling.
    let result = match infer_expr(db, cache, prefix_expr.clone()) {
        Err(_) => Ok(ResultTypeOrContinue::Continue),
        Ok(function_type) => get_type_at_call_expr_by_function_type(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            call_expr,
            function_type,
            condition_flow,
            policy,
        ),
    };

    // Fallback: check metadata-driven member-guard predicate patterns when
    // normal type-based narrowing didn't produce a result. The callee must
    // resolve to a signature carrying `call_arg("gmod.member_guard", ...)`
    // metadata on its first parameter; unannotated spellings are ignored.
    if let Ok(ResultTypeOrContinue::Continue) = result {
        if let Some(member_guard_type) = try_narrow_member_guard(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            &call_expr_ref,
            &prefix_expr_ref,
            condition_flow,
            policy,
        )? {
            return Ok(ResultTypeOrContinue::Result(member_guard_type));
        }

        if let Some(fallback_target_expr) =
            resolve_member_guard_alias_target(db, cache, root, &prefix_expr_ref)
        {
            if let Some(member_guard_type) = try_narrow_member_guard(
                db,
                tree,
                cache,
                root,
                var_ref_id,
                flow_node,
                &call_expr_ref,
                &fallback_target_expr,
                condition_flow,
                policy,
            )? {
                return Ok(ResultTypeOrContinue::Result(member_guard_type));
            }
        }
    }

    result
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_function_type(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    function_type: LuaType,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let call_expr_ref = call_expr.clone();
    match function_type {
        LuaType::Union(union) => {
            let mut merged = LuaType::Never;
            for candidate in union.types() {
                let ResultTypeOrContinue::Result(narrowed) =
                    get_type_at_call_expr_by_function_type(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr.clone(),
                        candidate.clone(),
                        condition_flow,
                        policy,
                    )?
                else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                merged = TypeOps::Union.apply(db, &merged, &narrowed);
            }
            Ok(ResultTypeOrContinue::Result(merged))
        }

        LuaType::DocFunction(f) => {
            let prefix_signature_id = get_call_prefix_signature_id(db, cache, &call_expr);
            let is_valid_guard = prefix_signature_id.is_some_and(|signature_id| {
                call_prefix_signature_is_valid_guard(db, cache, &call_expr_ref, signature_id)
            });
            let signature_cast = prefix_signature_id.and_then(|signature_id| {
                db.get_signature_index()
                    .get(&signature_id)
                    .and_then(|signature| signature.return_cast.as_ref())
                    .map(|cast| (cast, signature_id))
            });
            let return_type = f.get_ret();
            let type_guard_result = match return_type {
                _ if is_valid_guard || matches!(return_type, LuaType::TypeGuard(_)) => {
                    get_type_at_call_expr_by_type_guard(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr.clone(),
                        f,
                        signature_cast,
                        is_valid_guard,
                        condition_flow,
                        policy,
                    )
                }
                _ => {
                    // If the return type is not a type guard, we cannot infer the type cast.
                    Ok(ResultTypeOrContinue::Continue)
                }
            };
            if matches!(type_guard_result, Ok(ResultTypeOrContinue::Continue))
                && let Some(signature_id) = prefix_signature_id
                && let Some(narrowed_type) = try_inferred_positive_guard(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    &call_expr_ref,
                    signature_id,
                    policy,
                )?
            {
                return Ok(ResultTypeOrContinue::Result(narrowed_type));
            }
            type_guard_result
        }
        LuaType::Signature(signature_id) => {
            let Some(signature) = db.get_signature_index().get(&signature_id) else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            let ret = signature.get_return_type();
            let signature_cast = signature.return_cast.as_ref();
            let is_valid_guard =
                call_prefix_signature_is_valid_guard(db, cache, &call_expr_ref, signature_id);
            let mut type_guard_did_not_apply = false;
            match ret {
                _ if is_valid_guard || matches!(ret, LuaType::TypeGuard(_)) => {
                    // Try TypeGuard narrowing. If it doesn't apply (e.g., when the
                    // target is a member access, not a simple variable), fall through
                    // to the member guard fallback below.
                    let type_guard_result = get_type_at_call_expr_by_type_guard(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr.clone(),
                        signature.to_doc_func_type(),
                        signature_cast.map(|cast| (cast, signature_id)),
                        is_valid_guard,
                        condition_flow,
                        policy,
                    );
                    if !matches!(type_guard_result, Ok(ResultTypeOrContinue::Continue)) {
                        return type_guard_result;
                    }
                    // TypeGuard narrowing didn't apply; fall through to member guard fallback.
                    type_guard_did_not_apply = true;
                }
                LuaType::Call(call) => {
                    return get_type_at_call_expr_by_call(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        &call,
                        condition_flow,
                    );
                }
                _ => {}
            }

            if let Some(narrowed_type) = try_inferred_positive_guard(
                db,
                tree,
                cache,
                root,
                var_ref_id,
                flow_node,
                &call_expr_ref,
                signature_id,
                condition_flow,
                policy.clone(),
            )? {
                return Ok(ResultTypeOrContinue::Result(narrowed_type));
            }

            // If TypeGuard narrowing didn't apply, skip the signature_cast path
            // and go directly to the member guard fallback.
            if type_guard_did_not_apply {
                // Fall through to the fallback at the end of the function.
            } else if let Some(signature_cast) = signature_cast {
                return match signature_cast.name.as_str() {
                    "self" => get_type_at_call_expr_by_signature_self(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        signature_cast,
                        signature_id,
                        condition_flow,
                        policy,
                    ),
                    name => get_type_at_call_expr_by_signature_param_name(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        signature_cast,
                        signature_id,
                        name,
                        condition_flow,
                        policy,
                    ),
                };
            }

            // No @cast annotation found — fall through so supported predicate
            // fallback narrowing can still run.
            Ok(ResultTypeOrContinue::Continue)
        }
        _ => {
            // If the prefix expression is not a function, we cannot infer the type cast.
            Ok(ResultTypeOrContinue::Continue)
        }
    }
}

/// Whether `call_expr`, called through `prefix_expr`, names the variable
/// `var_ref_id`: every narrowing a callee declares applies to an argument,
/// the object it is called on, the object whose member the first argument
/// reads, or the call itself, so a call naming none of them leaves the
/// variable as it was whatever its callee is.
fn call_names_var(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    var_ref_id: &VarRefId,
) -> bool {
    // A call names at most an entry of its first argument, and finding which
    // takes inferring its callee, so only an entry is looked for in one.
    let names_entry = matches!(var_ref_id, VarRefId::IndexRef(..));
    let var_ref_of = |cache: &mut LuaInferCache, mut expr: LuaExpr| {
        while let LuaExpr::ParenExpr(paren) = expr {
            expr = paren.get_expr()?;
        }
        if !names_entry && matches!(expr, LuaExpr::CallExpr(_)) {
            return None;
        }
        get_var_expr_var_ref_id(db, cache, expr)
    };
    let names_var = |cache: &mut LuaInferCache, expr: LuaExpr| {
        var_ref_of(cache, expr.clone()).is_some_and(|id| {
            id == *var_ref_id
                || can_narrow_type_guard_through_decl_alias(db, cache, root, var_ref_id, &id, &expr)
        })
    };
    let object_of = |expr: &LuaExpr| match expr {
        LuaExpr::IndexExpr(index_expr) => index_expr.get_prefix_expr(),
        _ => None,
    };
    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let first_arg_ref = args.first().and_then(|arg| var_ref_of(cache, arg.clone()));
    if args
        .first()
        .is_some_and(|arg| names_var(cache, arg.clone()))
    {
        return true;
    }
    if let VarRefId::IndexRef(root, _) = var_ref_id
        && first_arg_ref
            .and_then(call_var_ref_root)
            .is_some_and(|first_arg_root| first_arg_root == *root)
        && names_var(cache, LuaExpr::CallExpr(call_expr.clone()))
    {
        return true;
    }
    let first_arg_object = args.first().and_then(object_of);
    object_of(prefix_expr)
        .into_iter()
        .chain(first_arg_object)
        .chain(args.into_iter().skip(1))
        .any(|target| names_var(cache, target))
}

#[allow(clippy::too_many_arguments)]
fn try_inferred_positive_guard(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<Option<LuaType>, InferFailReason> {
    if call_expr.is_colon_call() {
        return Ok(None);
    }
    let Some(guard) = db
        .get_signature_index()
        .inferred_positive_guard(&signature_id)
        .cloned()
    else {
        return Ok(None);
    };
    if let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr()
        && typed_literal_field_has_writes(db, cache, &index_expr)
    {
        return Ok(None);
    }
    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return Ok(None);
    };
    let Some(argument) = inferred_guard_target_expr(call_expr, signature, guard.param_idx) else {
        return Ok(None);
    };
    let Some(argument_ref_id) = get_var_expr_var_ref_id(db, cache, argument.clone()) else {
        return Ok(None);
    };
    if argument_ref_id != *var_ref_id
        && !can_narrow_type_guard_through_decl_alias(
            db,
            cache,
            root,
            var_ref_id,
            &argument_ref_id,
            &argument,
        )
    {
        return Ok(None);
    }

    let antecedent_type = get_condition_antecedent_type(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        policy.clone(),
    )?;
    Ok(match condition_flow {
        InferConditionFlow::TrueCondition => Some(narrow_type_guard_true_branch(
            db,
            antecedent_type,
            guard.narrowed_type,
            false,
        )),
        InferConditionFlow::FalseCondition => guard
            .false_removed_type
            .map(|removed| TypeOps::Remove.apply(db, &antecedent_type, &removed)),
    })
}

fn inferred_guard_target_expr(
    call_expr: &LuaCallExpr,
    signature: &crate::LuaSignature,
    param_idx: usize,
) -> Option<LuaExpr> {
    let argument_idx = match (call_expr.is_colon_call(), signature.is_colon_define) {
        (false, true) => Some(param_idx + 1),
        (true, false) => param_idx.checked_sub(1),
        _ => Some(param_idx),
    }?;
    call_expr.get_args_list()?.get_args().nth(argument_idx)
}

/// Resolves an alias chain for member-guard predicate callees. When the
/// `prefix_expr` is a local name bound to an immutable alias of another
/// global that carries member-guard metadata, returns the aliased name
/// expression so narrowing can proceed on the original call shape.
fn resolve_member_guard_alias_target(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    prefix_expr: &LuaExpr,
) -> Option<LuaExpr> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };

    let references_index = db.get_reference_index();
    let local_ref = references_index.get_local_reference(&cache.get_file_id());
    let Some(decl_id) = local_ref.and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
    else {
        return is_member_guard_callee(db, cache, name_expr)
            .then(|| LuaExpr::NameExpr(name_expr.clone()));
    };

    let decl = db.get_decl_index().get_decl(&decl_id)?;

    if db
        .get_reference_index()
        .get_decl_references(&cache.get_file_id(), &decl_id)
        .is_some_and(|decl_refs| decl_refs.mutable)
    {
        return None;
    }

    let value_syntax_id = decl.get_value_syntax_id()?;

    let node = value_syntax_id.to_node_from_root(root.syntax())?;

    let alias_expr = LuaExpr::cast(node)?;

    let LuaExpr::NameExpr(alias_name_expr) = alias_expr else {
        return None;
    };

    if name_expr_has_local_binding(db, cache, &alias_name_expr) {
        return None;
    }

    is_member_guard_callee(db, cache, &alias_name_expr).then(|| LuaExpr::NameExpr(alias_name_expr))
}

fn name_expr_has_local_binding(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &glua_parser::LuaNameExpr,
) -> bool {
    let Some(name) = name_expr.get_name_text() else {
        return false;
    };

    let file_id = cache.get_file_id();
    let by_reference = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .is_some();
    let by_scope = db
        .get_decl_index()
        .get_decl_tree(&file_id)
        .and_then(|decl_tree| decl_tree.find_local_decl(&name, name_expr.get_position()))
        .is_some();
    by_reference || by_scope
}

/// Returns `true` when `name_expr` refers to an unshadowed global whose
/// resolved signature carries `call_arg("gmod.member_guard", ...)` metadata
/// on its first parameter. Replaces the previous hardcoded member-guard
/// name check.
fn is_member_guard_callee(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &glua_parser::LuaNameExpr,
) -> bool {
    use crate::{GMOD_DOMAIN_MEMBER_GUARD, find_best_direct_call_arg_role_for_param};

    if name_expr_has_local_binding(db, cache, name_expr) {
        return false;
    }

    // Resolve the callee's type and extract its signature.
    let Ok(callee_type) = infer_expr(db, cache, LuaExpr::NameExpr(name_expr.clone())) else {
        return false;
    };

    let signature_id = match callee_type {
        LuaType::Signature(sig_id) => sig_id,
        LuaType::DocFunction(_) => {
            // DocFunction types don't directly carry signature IDs.
            // Try to resolve through the semantic declaration path.
            let Some(sig_id) =
                get_callable_expr_signature_id(db, cache, LuaExpr::NameExpr(name_expr.clone()), 0)
            else {
                return false;
            };
            sig_id
        }
        _ => return false,
    };

    let Some(signature) = db.get_signature_index().get_declared(&signature_id) else {
        return false;
    };

    find_best_direct_call_arg_role_for_param(signature, 0, GMOD_DOMAIN_MEMBER_GUARD, &[]).is_some()
}

/// Metadata-driven member-guard narrowing. When `call_expr` is a call to a
/// callee whose resolved signature carries `call_arg("gmod.member_guard", ...)`
/// metadata on its first parameter, and that first argument is a member access
/// on the variable tracked by `var_ref_id`, narrow the variable's type to the
/// subtypes where the accessed member is callable (true branch) or not callable
/// (false branch). Replaces the previous hardcoded member-guard name check.
#[allow(clippy::too_many_arguments)]
fn try_narrow_member_guard(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<Option<LuaType>, InferFailReason> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return Ok(None);
    };

    if !is_member_guard_callee(db, cache, name_expr) {
        return Ok(None);
    }

    let Some(arg_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().next())
    else {
        return Ok(None);
    };

    let LuaExpr::IndexExpr(index_expr) = arg_expr else {
        return Ok(None);
    };
    let Some(prefix_obj_expr) = index_expr.get_prefix_expr() else {
        return Ok(None);
    };

    let Some(prefix_ref_id) = get_var_expr_var_ref_id(db, cache, prefix_obj_expr) else {
        return Ok(None);
    };
    if prefix_ref_id != *var_ref_id {
        return Ok(None);
    }

    let antecedent_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;
    let Some(candidates) = collect_member_guard_narrow_candidates(db, &antecedent_type) else {
        return Ok(None);
    };

    let index_member = LuaIndexMemberExpr::IndexExpr(index_expr);
    let mut callable_candidates = Vec::new();
    for candidate in &candidates {
        let member_type =
            match infer_member_by_member_key(db, cache, candidate, index_member.clone()) {
                Ok(member_type) => member_type,
                Err(_) => continue,
            };

        if contains_callable_member_type(&member_type) {
            callable_candidates.push(candidate.clone());
        }
    }

    if callable_candidates.is_empty() {
        return Ok(None);
    }

    let narrowed_type = match condition_flow {
        InferConditionFlow::TrueCondition => LuaType::from_vec(callable_candidates),
        InferConditionFlow::FalseCondition => {
            let remaining_candidates = candidates
                .into_iter()
                .filter(|candidate| !callable_candidates.contains(candidate))
                .collect::<Vec<_>>();
            if remaining_candidates.is_empty() {
                return Ok(None);
            }
            LuaType::from_vec(remaining_candidates)
        }
    };

    Ok(Some(narrowed_type))
}

fn collect_member_guard_narrow_candidates(
    db: &DbIndex,
    antecedent_type: &LuaType,
) -> Option<Vec<LuaType>> {
    match antecedent_type {
        LuaType::Union(union_type) => Some(union_type.types().cloned().collect()),
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            let mut candidates = vec![LuaType::Ref(type_decl_id.clone())];
            let all_sub_types = db.get_type_index().get_all_sub_types(type_decl_id);
            for sub_type in all_sub_types {
                candidates.push(LuaType::Ref(sub_type.get_id()));
            }
            Some(candidates)
        }
        LuaType::Instance(instance_type) => {
            collect_member_guard_narrow_candidates(db, instance_type.get_base())
        }
        _ => None,
    }
}

fn contains_callable_member_type(member_type: &LuaType) -> bool {
    match member_type {
        LuaType::Function | LuaType::Signature(_) | LuaType::DocFunction(_) => true,
        LuaType::Union(union_type) => union_type.types().any(contains_callable_member_type),
        LuaType::Intersection(intersection_type) => intersection_type
            .get_types()
            .iter()
            .any(contains_callable_member_type),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_type_guard(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    func_type: Arc<LuaFunctionType>,
    signature_cast: Option<(&LuaSignatureCast, LuaSignatureId)>,
    is_valid_guard: bool,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(target_expr) = type_guard_target_expr(&call_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, target_expr.clone()) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_ref_id != *var_ref_id
        && !can_narrow_type_guard_through_decl_alias(
            db,
            cache,
            root,
            var_ref_id,
            &maybe_ref_id,
            &target_expr,
        )
    {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let Some(guard_type) = call_type_guard_type(db, cache, &call_expr, func_type, is_valid_guard)?
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let antecedent_type = get_condition_antecedent_type(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        policy.clone(),
    )?;

    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            narrow_type_guard_true_branch(db, antecedent_type, guard_type, is_valid_guard)
        }
        InferConditionFlow::FalseCondition => {
            if is_exhaustive_type_guard && !is_valid_guard {
                remove_exhaustively_guarded_classes(db, &antecedent_type, &guard_type)
            } else {
                TypeOps::Remove.apply(db, &antecedent_type, &guard_type)
            }
        }
    };

    let result_type = if let Some((signature_cast, signature_id)) = signature_cast {
        apply_signature_cast_to_type_guard_result(
            db,
            cache,
            &call_expr,
            var_ref_id,
            result_type,
            signature_cast,
            signature_id,
            condition_flow,
        )?
    } else {
        result_type
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

fn remove_exhaustively_guarded_classes(
    db: &DbIndex,
    antecedent: &LuaType,
    guard: &LuaType,
) -> LuaType {
    fn is_guarded_class(db: &DbIndex, candidate: &LuaType, guard: &LuaType) -> bool {
        match (candidate, guard) {
            (LuaType::Instance(instance), _) => is_guarded_class(db, instance.get_base(), guard),
            (_, LuaType::Instance(instance)) => {
                is_guarded_class(db, candidate, instance.get_base())
            }
            (_, LuaType::Union(union)) => union
                .types()
                .any(|arm| is_guarded_class(db, candidate, arm)),
            (
                LuaType::Ref(child) | LuaType::Def(child),
                LuaType::Ref(parent) | LuaType::Def(parent),
            ) => crate::semantic::type_check::is_sub_type_of(db, child, parent),
            _ => false,
        }
    }

    match antecedent {
        LuaType::Union(union) => {
            let remaining = union
                .types()
                .map(|arm| remove_exhaustively_guarded_classes(db, arm, guard))
                .filter(|arm| !arm.is_never())
                .collect::<Vec<_>>();
            if remaining.is_empty() {
                LuaType::Never
            } else {
                LuaType::from_vec(remaining)
            }
        }
        LuaType::Instance(instance) => {
            let base = remove_exhaustively_guarded_classes(db, instance.get_base(), guard);
            if base.is_never() {
                LuaType::Never
            } else {
                LuaType::Instance(
                    crate::LuaInstanceType::new(base, instance.get_range().clone()).into(),
                )
            }
        }
        _ if is_guarded_class(db, antecedent, guard) => LuaType::Never,
        _ => TypeOps::Remove.apply(db, antecedent, guard),
    }
}

fn call_type_guard_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    func_type: Arc<LuaFunctionType>,
    is_valid_guard: bool,
) -> Result<Option<LuaType>, InferFailReason> {
    let mut return_type = func_type.get_ret().clone();
    if return_type.contain_tpl() {
        let call_expr_type = LuaType::DocFunction(func_type);
        let inst_func = infer_call_expr_func(
            db,
            cache,
            call_expr.clone(),
            call_expr_type,
            &InferGuard::new(),
            None,
        )?;

        return_type = inst_func.get_ret().clone();
    }

    let guard_type = match return_type {
        LuaType::TypeGuard(guard) => guard.deref().clone(),
        // A valid guard answers true only for a valid object, whatever its
        // declared return, as the runtime `IsValid` in `includes/util.lua` does.
        _ if is_valid_guard => LuaType::Any,
        _ => return Ok(ResultTypeOrContinue::Continue),
    };

    let antecedent_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;

    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            narrow_type_guard_true_branch(db, antecedent_type, guard_type, is_valid_guard)
        }
        InferConditionFlow::FalseCondition => {
            TypeOps::Remove.apply(db, &antecedent_type, &guard_type)
        }
    };

    let result_type = if let Some((signature_cast, signature_id)) = signature_cast {
        apply_signature_cast_to_type_guard_result(
            db,
            cache,
            &call_expr,
            var_ref_id,
            result_type,
            signature_cast,
            signature_id,
            condition_flow,
        )?
    } else {
        signature_cast_target_expr(db, call_expr, id, &cast.name)
    };
    if !targets_var(cache, target) || cast.cast.action == crate::LuaCastAction::Remove {
        return None;
    }
    match &cast.cast.target {
        Some(crate::LuaCastTarget::Type(typ)) if !typ.is_any() && !typ.is_unknown() => {
            Some(typ.clone())
        }
        _ => None,
    }
}

fn type_guard_target_expr(call_expr: &LuaCallExpr) -> Option<LuaExpr> {
    let mut target = if call_expr.is_colon_call()
        && let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr()
        && let Some(self_expr) = index_expr.get_prefix_expr()
    {
        self_expr
    } else {
        call_expr.get_args_list()?.get_args().next()?
    };
    while let LuaExpr::ParenExpr(paren) = target {
        target = paren.get_expr()?;
    }
    Some(target)
}

fn narrow_type_guard_true_branch(
    db: &DbIndex,
    antecedent_type: LuaType,
    guard_type: LuaType,
    is_valid_guard: bool,
) -> LuaType {
    if is_valid_guard {
        return narrow_valid_guard_true_branch(db, antecedent_type, guard_type);
    }

    if guard_type.is_any() {
        return if antecedent_type.is_unknown() {
            LuaType::Any
        } else {
            remove_false_or_nil(antecedent_type)
        };
    }

    if guard_type.is_nullable() {
        return guard_type;
    }

    if let Some(narrowed_type) =
        narrow_down_type(db, antecedent_type.clone(), guard_type.clone(), None)
    {
        return narrowed_type;
    }

    // Inside the guard the value is what the guard proves: the arms it accepts, or
    // the guarded type itself when nothing the value is known to hold passes.
    guard_accepted_arms(db, &antecedent_type, &guard_type).unwrap_or(guard_type)
}

fn guard_accepted_arms(db: &DbIndex, value: &LuaType, guard_type: &LuaType) -> Option<LuaType> {
    let arms = match value {
        LuaType::Union(union) => union.types().cloned().collect::<Vec<_>>(),
        _ => vec![value.clone()],
    };
    let accepted = arms
        .into_iter()
        .filter(|arm| {
            !arm.is_nil()
                && !arm.is_unknown()
                && !arm.is_any()
                && check_type_compact(db, guard_type, arm).is_ok()
        })
        .collect::<Vec<_>>();
    (!accepted.is_empty()).then(|| LuaType::from_vec(accepted))
}

fn narrow_valid_guard_true_branch(
    db: &DbIndex,
    antecedent_type: LuaType,
    guard_type: LuaType,
) -> LuaType {
    if !guard_type.is_any()
        && let Some(narrowed_type) = narrow_down_type(db, antecedent_type.clone(), guard_type, None)
    {
        return narrowed_type;
    }

    if antecedent_type.is_unknown() {
        return LuaType::Unknown;
    }
    if antecedent_type.is_any() {
        return LuaType::Any;
    }

    let truthy_type = remove_false_or_nil(antecedent_type);
    let non_null = TypeOps::Remove.apply(db, &truthy_type, &gmod_null_type());
    // `IsValid` answers true only for a live engine handle, never for a
    // boolean/number/string. Plain truthiness keeps a `boolean` as `true`, but
    // that surviving `true` cannot be the thing `IsValid` accepted, so any
    // primitive left in a union has to drop out -- otherwise a value typed
    // `Entity|boolean` narrows to `Entity|true` and every later field read on it
    // reports a phantom nil.
    remove_non_validatable_primitives(non_null)
}

/// Strips the primitive value types `IsValid` can never accept from a union.
/// A lone primitive is left untouched: `IsValid` being true over a purely
/// primitive type is unreachable rather than informative, and answering `never`
/// there would only trade a phantom nil for a phantom empty type.
fn remove_non_validatable_primitives(typ: LuaType) -> LuaType {
    let LuaType::Union(union) = &typ else {
        return typ;
    };
    let kept: Vec<LuaType> = union
        .types()
        .filter(|component| {
            !(component.is_boolean() || component.is_number() || component.is_string())
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        typ
    } else {
        LuaType::from_vec(kept)
    }
}

/// Narrows `source_type` by a `---@return_cast` on the branch `condition_flow`
/// takes: the cast on a truthy return, otherwise the fallback cast when the
/// signature has one, else the cast negated.
fn apply_signature_cast(
    db: &DbIndex,
    signature_cast: &LuaSignatureCast,
    source_type: LuaType,
    condition_flow: InferConditionFlow,
) -> LuaType {
    match condition_flow {
        InferConditionFlow::TrueCondition => {
            let cast_result = apply_cast(
                db,
                &signature_cast.cast,
                source_type.clone(),
                InferConditionFlow::TrueCondition,
            );
            // A successful predicate adds a constraint; it must not widen an
            // existing subtype.
            narrow_down_type(db, source_type, cast_result.clone(), None).unwrap_or(cast_result)
        }
        InferConditionFlow::FalseCondition => match &signature_cast.fallback_cast {
            Some(fallback) => {
                apply_cast(db, fallback, source_type, InferConditionFlow::TrueCondition)
            }
            None => apply_cast(db, &signature_cast.cast, source_type, condition_flow),
        },
    }
}

/// Whether `typ`, with its aliases resolved, admits `nil`.
fn may_be_nil(db: &DbIndex, typ: &LuaType, visited: &mut rustc_hash::FxHashSet<LuaType>) -> bool {
    if !visited.insert(typ.clone()) {
        return false;
    }
    let typ = crate::get_real_type(db, typ).unwrap_or(typ);
    typ.is_nil()
        || typ.is_nullable()
        || match typ {
            LuaType::Union(union) => union.types().any(|arm| may_be_nil(db, arm, visited)),
            LuaType::MultiLineUnion(union) => union
                .get_unions()
                .iter()
                .any(|(arm, _)| may_be_nil(db, arm, visited)),
            _ => false,
        }
}

/// The value `call_expr` answers true for only when it holds one: what a valid guard
/// such as `IsValid` tests, or a `TypeGuard` predicate whose type excludes nil.
pub(crate) fn presence_guard_target(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaExpr> {
    let signature_id = get_call_prefix_signature_id(db, cache, call_expr)?;
    let guards_presence = call_prefix_signature_is_valid_guard(db, cache, call_expr, signature_id)
        || db
            .get_signature_index()
            .get(&signature_id)
            .is_some_and(|signature| {
                matches!(signature.get_return_type(), LuaType::TypeGuard(guard)
                    if !guard.contain_tpl()
                        && !may_be_nil(db, &guard, &mut rustc_hash::FxHashSet::default()))
            });
    guards_presence
        .then(|| type_guard_target_expr(call_expr))
        .flatten()
}

fn get_call_prefix_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaSignatureId> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    get_callable_expr_signature_id(db, cache, prefix_expr, 0)
}

fn call_prefix_signature_is_valid_guard(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> bool {
    let call_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&cache.get_file_id(), call_expr.get_position());
    signature_is_valid_guard_or_base_runtime_isvalid_in_realm(db, signature_id, call_realm)
}

fn get_callable_expr_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    depth: usize,
) -> Option<LuaSignatureId> {
    if depth > 8 {
        return None;
    }

    if let LuaExpr::IndexExpr(index_expr) = &expr
        && typed_literal_field_has_writes(db, cache, index_expr)
    {
        return None;
    }

    if let LuaExpr::NameExpr(name_expr) = &expr {
        if !declaration_alone_reaches(db, cache.get_file_id(), name_expr) {
            return None;
        }
        if let Some(decl_id) = db
            .get_reference_index()
            .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
        {
            let semantic_decl = LuaSemanticDeclId::LuaDecl(decl_id);
            let owns_contract = db
                .get_property_index()
                .get_signature_owner(&semantic_decl)
                .is_some()
                || db
                    .get_type_index()
                    .get_type_cache(&decl_id.into())
                    .is_some_and(|type_cache| type_cache.is_doc());
            if owns_contract && let Some(signature_id) = known_signature_id(db, &semantic_decl) {
                return Some(signature_id);
            }
        }
        if let Some((decl_file_id, value_expr)) =
            local_name_value(db, cache.get_file_id(), name_expr)
        {
            return get_value_expr_signature_id(db, cache, decl_file_id, value_expr, depth);
        }
    }

    if let LuaExpr::IndexExpr(index_expr) = &expr
        && let Some(signature_id) = table_literal_field_signature_id(db, cache, index_expr, depth)
    {
        return Some(signature_id);
    }

    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    );
    semantic_decl.and_then(|semantic_decl| declared_signature_id(db, cache, semantic_decl, depth))
}

fn declared_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    semantic_decl: LuaSemanticDeclId,
    depth: usize,
) -> Option<LuaSignatureId> {
    if let Some(signature_id) = known_signature_id(db, &semantic_decl) {
        return Some(signature_id);
    }
    let (file_id, value_expr) = semantic_decl_value(db, semantic_decl)?;
    get_value_expr_signature_id(db, cache, file_id, value_expr, depth)
}

fn typed_literal_field_has_writes(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(LuaExpr::NameExpr(receiver)) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Some((table_file, table, Some(contract))) = receiver_table_literal(db, cache, &receiver)
    else {
        return false;
    };
    let type_id = match contract {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => type_id,
        _ => return false,
    };
    let member_key = match index_expr.get_index_key() {
        Some(LuaIndexKey::Name(name)) => LuaMemberKey::Name(name.get_name_text().into()),
        Some(LuaIndexKey::String(string)) => LuaMemberKey::Name(string.get_value().into()),
        _ => return false,
    };
    let mut owners = vec![LuaMemberOwner::Element(InFiled::new(
        table_file,
        table.get_range(),
    ))];
    owners.extend(
        type_id
            .collect_super_types_with_self(db, LuaType::Ref(type_id.clone()))
            .into_iter()
            .filter_map(|typ| match typ {
                LuaType::Ref(id) | LuaType::Def(id) => Some(LuaMemberOwner::Type(id)),
                _ => None,
            }),
    );
    owners.iter().any(|owner| {
        db.get_member_index()
            .get_members_for_owner_key(owner, &member_key)
            .iter()
            .any(|member| {
                member.get_syntax_id().get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
            })
            || crate::dynamic_field_owner_of(db, owner).is_some_and(|owner| match &member_key {
                LuaMemberKey::Name(name) => db
                    .get_dynamic_field_index()
                    .field_definitions(&owner, name.as_str())
                    .iter()
                    .any(|definition| {
                        definition.file_id != table_file
                            || !table.get_fields().any(|field| {
                                field.get_field_key().and_then(|key| key.get_range())
                                    == Some(definition.value)
                            })
                    }),
                _ => false,
            })
    })
}

fn receiver_table_literal(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    receiver: &glua_parser::LuaNameExpr,
) -> Option<(FileId, LuaTableExpr, Option<LuaType>)> {
    let file_id = cache.get_file_id();
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, receiver.get_range())?;
    if let Some((table_file, LuaExpr::TableExpr(table))) = local_name_value(db, file_id, receiver) {
        let contract = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .filter(|cache| cache.is_doc())
            .map(|cache| cache.as_type().clone());
        return Some((table_file, table, contract));
    }
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let crate::LuaDeclExtra::Param {
        idx: 0,
        signature_id,
        owner_member_id: Some(member_id),
    } = decl.extra
    else {
        return None;
    };
    let signature = db.get_signature_index().get(&signature_id)?;
    if decl.get_name() != "self"
        || signature.is_colon_define
        || db
            .get_call_site_param_index()
            .has_param_evidence(&signature_id, 0)
    {
        return None;
    }
    let field = db
        .get_vfs()
        .syntax_node_around(&member_id.file_id, *member_id.get_syntax_id())
        .and_then(LuaTableField::cast)?;
    let LuaExpr::ClosureExpr(closure) = field.get_value_expr()? else {
        return None;
    };
    if LuaSignatureId::from_closure(member_id.file_id, &closure) != signature_id {
        return None;
    }
    let field_type =
        crate::semantic::infer_table_field_value_should_be(db, cache, field.clone()).ok()?;
    let contract = match remove_false_or_nil(field_type) {
        LuaType::DocFunction(function) if !function.is_colon_define() => {
            function.get_params().first()?.1.clone()?
        }
        _ => return None,
    };
    if !matches!(contract, LuaType::Ref(_) | LuaType::Def(_))
        || signature
            .get_param_info_by_id(0)
            .is_some_and(|param| param.type_ref != contract)
    {
        return None;
    }
    let table = field.get_parent::<LuaTableExpr>()?;
    let table_type = crate::semantic::infer_table_should_be(db, cache, table.clone()).ok()?;
    check_type_compact(db, &table_type, &contract).ok()?;
    Some((member_id.file_id, table, Some(contract)))
}

fn table_literal_field_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
    depth: usize,
) -> Option<LuaSignatureId> {
    let file_id = cache.get_file_id();
    let LuaExpr::NameExpr(receiver) = index_expr.get_prefix_expr()? else {
        return None;
    };
    if !declaration_alone_reaches(db, file_id, &receiver) {
        return None;
    }
    let (table_file_id, table, _) = receiver_table_literal(db, cache, &receiver)?;
    let member_key = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => LuaMemberKey::Name(name.get_name_text().into()),
        LuaIndexKey::String(string) => LuaMemberKey::Name(string.get_value().into()),
        _ => return None,
    };
    let owner = LuaMemberOwner::Element(InFiled::new(table_file_id, table.get_range()));
    let member_item = db.get_member_index().get_member_item(&owner, &member_key)?;
    if !member_item.is_one() {
        return None;
    }
    let semantic_decl = member_item.resolve_semantic_decl_with_realm_at_offset(
        db,
        &file_id,
        index_expr.get_position(),
    )?;
    declared_signature_id(db, cache, semantic_decl, depth)
}

/// Whether the declaration `name_expr` refers to is the only definition reaching it, so the
/// declaration's value is what the name holds there.
fn declaration_alone_reaches(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &glua_parser::LuaNameExpr,
) -> bool {
    let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())
    else {
        return true;
    };
    let Some(tree) = db.get_flow_index().get_flow_tree(&file_id) else {
        return true;
    };
    let Some(flow_id) = tree.get_flow_id(name_expr.get_syntax_id()) else {
        return true;
    };
    matches!(
        tree.reaching_definitions(decl_id, flow_id).as_ref(),
        [crate::LuaDefinitionId::Declaration(_)]
    )
}

fn get_value_expr_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    value_expr: LuaExpr,
    depth: usize,
) -> Option<LuaSignatureId> {
    if let LuaExpr::ClosureExpr(closure) = &value_expr {
        return Some(LuaSignatureId::from_closure(file_id, closure));
    }

    get_callable_expr_signature_id(db, cache, value_expr, depth + 1)
}

#[allow(clippy::too_many_arguments)]
fn apply_signature_cast_to_type_guard_result(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    var_ref_id: &VarRefId,
    result_type: LuaType,
    signature_cast: &LuaSignatureCast,
    signature_id: LuaSignatureId,
    condition_flow: InferConditionFlow,
) -> Result<LuaType, InferFailReason> {
    let Some(cast_target_expr) =
        signature_cast_target_expr(db, call_expr, signature_id, signature_cast.name.as_str())
    else {
        return Ok(result_type);
    };

    let Some(cast_target_ref_id) = get_var_expr_var_ref_id(db, cache, cast_target_expr) else {
        return Ok(result_type);
    };

    if cast_target_ref_id != *var_ref_id {
        return Ok(result_type);
    }

    Ok(apply_signature_cast(
        db,
        signature_cast,
        result_type,
        condition_flow,
    ))
}

fn signature_cast_target_expr(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
    name: &str,
) -> Option<LuaExpr> {
    if name == "self" {
        let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
            return None;
        };
        return index_expr.get_prefix_expr();
    }

    let arg_list = call_expr.get_args_list()?;
    let signature = db.get_signature_index().get(&signature_id)?;
    let mut param_idx = signature.find_param_idx(name)?;

    match (call_expr.is_colon_call(), signature.is_colon_define) {
        (true, false) => {
            if param_idx == 0 {
                return None;
            }

            param_idx -= 1;
        }
        (false, true) => {
            param_idx += 1;
        }
        _ => {}
    }

    arg_list.get_args().nth(param_idx)
}

fn signature_self_target_expr(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> Option<LuaExpr> {
    if call_expr.is_colon_call() {
        let LuaExpr::IndexExpr(prefix) = call_expr.get_prefix_expr()? else {
            return None;
        };
        prefix.get_prefix_expr()
    } else if db.get_signature_index().get(&signature_id)?.is_colon_define {
        call_expr.get_args_list()?.get_args().next()
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_signature_self(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    signature_cast: &LuaSignatureCast,
    signature_id: LuaSignatureId,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(self_expr) = signature_self_target_expr(db, &call_expr, signature_id) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(name_var_ref_id) = get_var_expr_var_ref_id(db, cache, self_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let mut antecedent_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;

    if matches!(condition_flow, InferConditionFlow::TrueCondition)
        && find_signature_attribute_use(db, signature_id, GMOD_ATTR_SELF_GUARD).is_some()
    {
        antecedent_type = narrow_valid_guard_true_branch(db, antecedent_type, LuaType::Any);
    }

    let result_type = apply_signature_cast(db, signature_cast, antecedent_type, condition_flow);

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_signature_param_name(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    signature_cast: &LuaSignatureCast,
    signature_id: LuaSignatureId,
    name: &str,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let colon_call = call_expr.is_colon_call();
    let Some(arg_list) = call_expr.get_args_list() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(mut param_idx) = signature.find_param_idx(name) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let colon_define = signature.is_colon_define;
    match (colon_call, colon_define) {
        (true, false) => {
            if param_idx == 0 {
                return Ok(ResultTypeOrContinue::Continue);
            }

            param_idx -= 1;
        }
        (false, true) => {
            param_idx += 1;
        }
        _ => {}
    }

    let Some(expr) = arg_list.get_args().nth(param_idx) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(name_var_ref_id) = get_var_expr_var_ref_id(db, cache, expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;

    let result_type = apply_signature_cast(db, signature_cast, antecedent_type, condition_flow);

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(unused, clippy::too_many_arguments)]
fn get_type_at_call_expr_by_call(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_expr: LuaCallExpr,
    alias_call_type: &Arc<LuaAliasCallType>,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(maybe_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::CallExpr(call_expr.clone()))
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    if alias_call_type.get_call_kind() == LuaAliasCallKind::RawGet {
        let antecedent_type = infer_expr(db, cache, LuaExpr::CallExpr(call_expr))?;
        let result_type = match condition_flow {
            InferConditionFlow::FalseCondition => narrow_false_or_nil(db, antecedent_type),
            InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
        };
        return Ok(ResultTypeOrContinue::Result(result_type));
    };

    Ok(ResultTypeOrContinue::Continue)
}
