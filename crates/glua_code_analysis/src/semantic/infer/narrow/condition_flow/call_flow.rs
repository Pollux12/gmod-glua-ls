use std::{ops::Deref, sync::Arc};

use glua_parser::{LuaAstNode, LuaCallExpr, LuaChunk, LuaExpr, LuaIndexMemberExpr, LuaNameExpr};

use crate::{
    DbIndex, FlowNode, FlowTree, InferFailReason, InferGuard, LuaAliasCallKind, LuaAliasCallType,
    LuaFunctionType, LuaInferCache, LuaSemanticDeclId, LuaSignatureCast, LuaSignatureId, LuaType,
    TypeOps, infer_call_expr_func, infer_expr,
    semantic::{
        SemanticDeclGuard, SemanticDeclLevel, get_member_value_expr,
        infer::{
            VarRefId,
            infer_index::infer_member_by_member_key,
            infer_param_with_cache,
            narrow::{
                ResultTypeOrContinue, condition_flow::InferConditionFlow, get_single_antecedent,
                get_type_at_cast_flow::cast_type, get_type_at_flow::get_type_at_flow,
                gmod_null_type, narrow_down_type, narrow_false_or_nil, remove_false_or_nil,
                var_ref_id::get_var_expr_var_ref_id,
            },
        },
        infer_expr_semantic_decl,
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
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    // Keep references for predicate fallbacks that need the original call shape.
    let call_expr_ref = call_expr.clone();
    let prefix_expr_ref = prefix_expr.clone();

    // If we can't infer the function type, skip type-based narrowing but still
    // fall through to supported fallback handling.
    let result = match infer_expr(db, cache, prefix_expr.clone()) {
        Err(_) => Ok(ResultTypeOrContinue::Continue),
        Ok(maybe_func) => match maybe_func {
            LuaType::DocFunction(f) => {
                let prefix_signature_id = get_call_prefix_signature_id(db, cache, &call_expr);
                let is_valid_guard = prefix_signature_id.is_some_and(|signature_id| {
                    call_prefix_signature_is_valid_guard(db, cache, &call_expr_ref, signature_id)
                });
                let signature_cast = prefix_signature_id.and_then(|signature_id| {
                    db.get_flow_index()
                        .get_signature_cast(&signature_id)
                        .map(|cast| (cast, signature_id))
                });
                let return_type = f.get_ret();
                match return_type {
                    LuaType::TypeGuard(_) => get_type_at_call_expr_by_type_guard(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        flow_node,
                        call_expr,
                        f,
                        signature_cast,
                        is_valid_guard,
                        condition_flow,
                    ),
                    _ => {
                        // If the return type is not a type guard, we cannot infer the type cast.
                        Ok(ResultTypeOrContinue::Continue)
                    }
                }
            }
            LuaType::Signature(signature_id) => {
                let Some(signature) = db.get_signature_index().get(&signature_id) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };

                let ret = signature.get_return_type();
                let signature_cast = db.get_flow_index().get_signature_cast(&signature_id);
                let is_valid_guard =
                    call_prefix_signature_is_valid_guard(db, cache, &call_expr_ref, signature_id);
                let mut type_guard_did_not_apply = false;
                match ret {
                    LuaType::TypeGuard(_) => {
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
                            prefix_expr,
                            signature_cast,
                            signature_id,
                            condition_flow,
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
        },
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
            )? {
                return Ok(ResultTypeOrContinue::Result(member_guard_type));
            }
        }
    }

    result
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
    use crate::{GMOD_DOMAIN_MEMBER_GUARD, find_best_call_arg_role_for_param};

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

    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return false;
    };

    find_best_call_arg_role_for_param(signature, 0, GMOD_DOMAIN_MEMBER_GUARD, &[]).is_some()
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

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    let Some(candidates) = collect_member_guard_narrow_candidates(db, &antecedent_type) else {
        return Ok(None);
    };

    let index_member = LuaIndexMemberExpr::IndexExpr(index_expr);
    let mut callable_candidates = Vec::new();
    for candidate in &candidates {
        let member_type = match infer_member_by_member_key(
            db,
            cache,
            candidate,
            index_member.clone(),
            &InferGuard::new(),
        ) {
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
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(target_expr) = type_guard_target_expr(&call_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, target_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

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
        _ => return Ok(ResultTypeOrContinue::Continue),
    };

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => narrow_type_guard_true_branch(
            db,
            cache,
            var_ref_id,
            antecedent_type,
            guard_type,
            is_valid_guard,
        ),
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
        result_type
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

fn type_guard_target_expr(call_expr: &LuaCallExpr) -> Option<LuaExpr> {
    if call_expr.is_colon_call()
        && let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr()
        && let Some(self_expr) = index_expr.get_prefix_expr()
    {
        return Some(self_expr);
    }

    call_expr
        .get_args_list()
        .and_then(|args| args.get_args().next())
}

fn narrow_type_guard_true_branch(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_ref_id: &VarRefId,
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

    if antecedent_type.is_unknown() || antecedent_type.is_any() {
        return guard_type;
    }

    // The inferred type cache for mutable unannotated parameters is flow-insensitive:
    // later writes can poison the origin type used by earlier guards (for example,
    // Starfall's `sfmeshdata = meshToStream(...)` made `elseif istable(sfmeshdata)`
    // start from `string`). In that case the runtime TypeGuard is the better authority.
    if is_inferred_mutable_param_without_declared_type(db, cache, var_ref_id) {
        return guard_type;
    }

    remove_false_or_nil(antecedent_type)
}

fn narrow_valid_guard_true_branch(
    db: &DbIndex,
    antecedent_type: LuaType,
    guard_type: LuaType,
) -> LuaType {
    if let Some(narrowed_type) =
        narrow_down_type(db, antecedent_type.clone(), guard_type.clone(), None)
    {
        return narrowed_type;
    }

    if antecedent_type.is_unknown() || antecedent_type.is_any() {
        return LuaType::Any;
    }

    let truthy_type = remove_false_or_nil(antecedent_type);
    TypeOps::Remove.apply(db, &truthy_type, &gmod_null_type())
}

fn is_inferred_mutable_param_without_declared_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_ref_id: &VarRefId,
) -> bool {
    let Some(decl_id) = var_ref_id.get_decl_id_ref() else {
        return false;
    };
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    if !decl.is_param() || infer_param_with_cache(db, cache, decl).is_ok() {
        return false;
    }
    if !db
        .get_type_index()
        .get_type_cache(&decl_id.into())
        .is_some_and(|type_cache| type_cache.is_infer())
    {
        return false;
    }

    db.get_reference_index()
        .get_decl_references(&decl_id.file_id, &decl_id)
        .is_some_and(|decl_refs| decl_refs.mutable)
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

    if let LuaExpr::NameExpr(name_expr) = &expr
        && let Some(signature_id) = get_local_name_signature_id(db, cache, name_expr, depth)
    {
        return Some(signature_id);
    }

    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;

    get_signature_id_from_semantic_decl_value_expr(db, cache, semantic_decl, depth)
}

fn get_local_name_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
    depth: usize,
) -> Option<LuaSignatureId> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
    let value_expr = LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)?;

    if let LuaExpr::ClosureExpr(closure) = &value_expr {
        return Some(LuaSignatureId::from_closure(decl.get_file_id(), closure));
    }

    get_callable_expr_signature_id(db, cache, value_expr, depth + 1)
}

fn get_signature_id_from_semantic_decl_value_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    semantic_decl: LuaSemanticDeclId,
    depth: usize,
) -> Option<LuaSignatureId> {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return Some(signature_id);
    }

    match &semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&(*decl_id).into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
        }
        LuaSemanticDeclId::Member(member_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&(*member_id).into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
        }
        LuaSemanticDeclId::Signature(signature_id) => return Some(*signature_id),
        LuaSemanticDeclId::TypeDecl(_) => return None,
    }

    let file_id = match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => decl_id.file_id,
        LuaSemanticDeclId::Member(member_id) => member_id.file_id,
        LuaSemanticDeclId::Signature(signature_id) => return Some(signature_id),
        LuaSemanticDeclId::TypeDecl(_) => return None,
    };

    let value_expr = get_semantic_decl_value_expr(db, semantic_decl)?;
    if let LuaExpr::ClosureExpr(closure) = &value_expr {
        return Some(LuaSignatureId::from_closure(file_id, closure));
    }

    get_callable_expr_signature_id(db, cache, value_expr, depth + 1)
}

fn get_semantic_decl_value_expr(db: &DbIndex, semantic_decl: LuaSemanticDeclId) -> Option<LuaExpr> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            let value_syntax_id = decl.get_value_syntax_id()?;
            let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
            LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)
        }
        LuaSemanticDeclId::Member(member_id) => get_member_value_expr(db, member_id),
        LuaSemanticDeclId::Signature(_) | LuaSemanticDeclId::TypeDecl(_) => None,
    }
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

    let Some(syntax_tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return Ok(result_type);
    };
    let signature_root = syntax_tree.get_chunk_node();

    match condition_flow {
        InferConditionFlow::TrueCondition => {
            let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                return Ok(result_type);
            };
            cast_type(
                db,
                signature_id.get_file_id(),
                cast_op_type,
                result_type,
                condition_flow,
            )
        }
        InferConditionFlow::FalseCondition => {
            if let Some(fallback_cast_ptr) = &signature_cast.fallback_cast {
                let Some(fallback_op_type) = fallback_cast_ptr.to_node(&signature_root) else {
                    return Ok(result_type);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    fallback_op_type,
                    result_type,
                    InferConditionFlow::TrueCondition,
                )
            } else {
                let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                    return Ok(result_type);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    cast_op_type,
                    result_type,
                    condition_flow,
                )
            }
        }
    }
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

#[allow(clippy::too_many_arguments)]
fn get_type_at_call_expr_by_signature_self(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    call_prefix: LuaExpr,
    signature_cast: &LuaSignatureCast,
    signature_id: LuaSignatureId,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let LuaExpr::IndexExpr(call_prefix_index) = call_prefix else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(self_expr) = call_prefix_index.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(name_var_ref_id) = get_var_expr_var_ref_id(db, cache, self_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let Some(syntax_tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let signature_root = syntax_tree.get_chunk_node();

    // Choose the appropriate cast based on condition_flow and whether fallback exists
    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                return Ok(ResultTypeOrContinue::Continue);
            };
            cast_type(
                db,
                signature_id.get_file_id(),
                cast_op_type,
                antecedent_type,
                condition_flow,
            )?
        }
        InferConditionFlow::FalseCondition => {
            // Use fallback_cast if available, otherwise use the default behavior
            if let Some(fallback_cast_ptr) = &signature_cast.fallback_cast {
                let Some(fallback_op_type) = fallback_cast_ptr.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    fallback_op_type,
                    antecedent_type.clone(),
                    InferConditionFlow::TrueCondition, // Apply fallback as force cast
                )?
            } else {
                // Original behavior: remove the true type from antecedent
                let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    cast_op_type,
                    antecedent_type,
                    condition_flow,
                )?
            }
        }
    };

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

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let Some(syntax_tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let signature_root = syntax_tree.get_chunk_node();

    // Choose the appropriate cast based on condition_flow and whether fallback exists
    let result_type = match condition_flow {
        InferConditionFlow::TrueCondition => {
            let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                return Ok(ResultTypeOrContinue::Continue);
            };
            cast_type(
                db,
                signature_id.get_file_id(),
                cast_op_type,
                antecedent_type,
                condition_flow,
            )?
        }
        InferConditionFlow::FalseCondition => {
            // Use fallback_cast if available, otherwise use the default behavior
            if let Some(fallback_cast_ptr) = &signature_cast.fallback_cast {
                let Some(fallback_op_type) = fallback_cast_ptr.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    fallback_op_type,
                    antecedent_type.clone(),
                    InferConditionFlow::TrueCondition, // Apply fallback as force cast
                )?
            } else {
                // Original behavior: remove the true type from antecedent
                let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                cast_type(
                    db,
                    signature_id.get_file_id(),
                    cast_op_type,
                    antecedent_type,
                    condition_flow,
                )?
            }
        }
    };

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
