use std::sync::Arc;

use glua_parser::{BinaryOperator, LuaAstNode, LuaCallExpr, LuaExpr, LuaNameExpr, LuaSyntaxKind};
use rowan::TextRange;

use super::infer_name::type_decl_is_vgui_panel;
use super::{
    super::{InferGuard, LuaInferCache, instantiate_type_generic, resolve_signature},
    InferFailReason, InferResult,
};
use crate::AsyncState;
use crate::compilation::analyzer::unresolve::get_wrapped_callable_target_expr;
use crate::{
    CacheEntry, DbIndex, InFiled, LuaArrayType, LuaFunctionType, LuaGenericType, LuaInstanceType,
    LuaIntersectionType, LuaOperatorMetaMethod, LuaOperatorOwner, LuaSignature, LuaSignatureId,
    LuaTupleType, LuaType, LuaTypeDeclId, LuaUnionType, ReturnTypeKind, VariadicType,
};
use crate::{GMOD_DOMAIN_CONVAR, GMOD_ROLE_REFERENCE};
use crate::{GmodConVarKind, GmodLoadEdgeKind, GmodStateMask};
use crate::{
    InferGuardRef,
    semantic::{
        generic::{
            TypeSubstitutor, check_vgui_panel_ref_role, get_tpl_ref_extend_type,
            instantiate_doc_function,
        },
        get_member_value_expr,
        infer::narrow::get_type_at_call_expr_inline_cast,
        infer_expr_semantic_decl,
    },
};
use crate::{
    SemanticDeclGuard, SemanticDeclLevel, build_self_type, infer_self_type,
    instantiate_func_generic, semantic::infer_expr,
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
    if let Some(cache_entry) = cache.call_cache.get(&key) {
        match cache_entry {
            CacheEntry::Cache(ty) => return Ok(ty.clone()),
            _ => return Err(InferFailReason::RecursiveInfer),
        }
    }

    cache.call_cache.insert(key.clone(), CacheEntry::Ready);
    let prefix_signature_id = matches!(call_expr_type, LuaType::DocFunction(_))
        .then(|| get_prefix_expr_signature_id(db, cache, &call_expr))
        .flatten();
    if let Some(signature_id) = prefix_signature_id
        && should_prefer_signature_for_call(db, signature_id, &call_expr_type)
    {
        let result =
            infer_signature_doc_function(db, cache, signature_id, call_expr.clone(), args_count);
        cache.call_cache.insert(
            key,
            result
                .as_ref()
                .map(|it| CacheEntry::Cache(it.clone()))
                .unwrap_or_else(|err| CacheEntry::Error(err.clone())),
        );
        return result;
    }

    let result = match &call_expr_type {
        LuaType::DocFunction(func) => infer_doc_function(
            db,
            cache,
            func,
            call_expr.clone(),
            args_count,
            prefix_signature_id,
        ),
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
            if union.types().all(|t| matches!(t, LuaType::DocFunction(_))) {
                infer_generic_doc_function_union(db, cache, union, call_expr.clone(), args_count)
            } else {
                infer_union(db, cache, union, call_expr.clone(), args_count)
            }
        }
        // Calling `any` yields `any`, the same answer every other reader of an
        // `any` gets. Failing instead makes the call's type depend on whether
        // some earlier write happened to reach the slot first: a member with
        // two realm-branched definitions settles to `any`, so the walk can
        // infer the call against a signature while a later unresolve retry
        // infers it against the settled `any` and comes back undetermined.
        // Which of the two lands is a property of how the workspace was
        // batched, not of the source.
        // The `...` param is what makes it accept any arity: the arity checker
        // looks for that name, so omitting it reports every argument as
        // redundant.
        LuaType::Any => Ok(Arc::new(LuaFunctionType::new(
            AsyncState::None,
            false,
            true,
            vec![("...".to_string(), Some(LuaType::Any))],
            LuaType::Any,
        ))),
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

        let func_ret =
            refine_known_vgui_panel_return(db, cache, func_ty.get_ret().clone(), &call_expr);
        let func_ret = refine_known_vgui_parent_return(db, cache, func_ret, &call_expr);
        match &func_ret {
            LuaType::TypeGuard(_) => Ok(func_ty),
            _ => unwrapp_return_type(db, cache, func_ret, call_expr).map(|new_ret| {
                LuaFunctionType::new(
                    func_ty.get_async_state(),
                    func_ty.is_colon_define(),
                    func_ty.is_variadic(),
                    func_ty.get_params().to_vec(),
                    new_ret,
                )
                .with_optional_params(func_ty.get_optional_params().to_vec())
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

fn refine_known_vgui_panel_return(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_type: LuaType,
    call_expr: &LuaCallExpr,
) -> LuaType {
    if !db.get_emmyrc().gmod.enabled {
        return return_type;
    }
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return return_type;
    };
    let Some(args) = call_expr.get_args_list() else {
        return return_type;
    };
    if !args.get_args().enumerate().any(|(arg_idx, _)| {
        check_vgui_panel_ref_role(db, cache, &prefix_expr, arg_idx, call_expr.is_colon_call())
    }) {
        return return_type;
    }

    let Some(type_id) = single_non_nil_instance_type_id(&return_type) else {
        return return_type;
    };
    if !type_decl_is_vgui_panel(db, &type_id, 0)
        && db
            .get_gmod_class_metadata_index()
            .get_vgui_panel_base(type_id.get_name())
            .is_none()
    {
        return return_type;
    }

    remove_nested_nil(return_type)
}

fn refine_known_vgui_parent_return(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_type: LuaType,
    call_expr: &LuaCallExpr,
) -> LuaType {
    if !db.get_emmyrc().gmod.enabled || !call_expr.is_colon_call() {
        return return_type;
    }
    let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr() else {
        return return_type;
    };
    if !index_expr.get_index_key().is_some_and(|key| {
        key.get_name()
            .is_some_and(|name| name.get_name_text() == "GetParent")
    }) {
        return return_type;
    }
    let Some(receiver) = index_expr.get_prefix_expr() else {
        return return_type;
    };
    let mut depth = 1;
    let mut root_receiver = receiver;
    while let LuaExpr::CallExpr(parent_call) = root_receiver.clone() {
        let Some(LuaExpr::IndexExpr(parent_index)) = parent_call.get_prefix_expr() else {
            break;
        };
        if !parent_call.is_colon_call()
            || !parent_index.get_index_key().is_some_and(|key| {
                key.get_name()
                    .is_some_and(|name| name.get_name_text() == "GetParent")
            })
            || parent_call
                .get_args_list()
                .is_some_and(|args| args.get_args().next().is_some())
        {
            break;
        }
        let Some(parent_receiver) = parent_index.get_prefix_expr() else {
            break;
        };
        depth += 1;
        root_receiver = parent_receiver;
    }
    let receiver_type = infer_expr(db, cache, root_receiver).unwrap_or(LuaType::Unknown);
    let mut child_ids = Vec::new();
    collect_non_nil_type_ids(&receiver_type, &mut child_ids);
    child_ids.sort_by(|left, right| left.get_name().cmp(right.get_name()));
    child_ids.dedup();

    let metadata = db.get_gmod_class_metadata_index();
    let is_vgui_receiver = if child_ids.is_empty() {
        matches!(
            receiver_type,
            LuaType::TableConst(_) | LuaType::Table | LuaType::Unknown
        )
    } else {
        child_ids.iter().any(|child_id| {
            type_decl_is_vgui_panel(db, child_id, 0)
                || metadata.get_vgui_panel_base(child_id.get_name()).is_some()
        })
    };
    if !is_vgui_receiver {
        return return_type;
    }

    let mut parent_id = None;
    for child_id in &child_ids {
        if !metadata.vgui_panel_parent_chain_is_complete(child_id) {
            if is_broad_panel_type(&return_type) {
                cache
                    .vgui_parent_fallback_calls
                    .insert(call_expr.get_syntax_id());
            }
            return return_type;
        }
        let Some(chain) = metadata.get_vgui_panel_parent_chain(child_id) else {
            if is_broad_panel_type(&return_type) {
                cache
                    .vgui_parent_fallback_calls
                    .insert(call_expr.get_syntax_id());
            }
            return return_type;
        };
        let Some(candidate) = chain.get(depth - 1) else {
            if is_broad_panel_type(&return_type) {
                cache
                    .vgui_parent_fallback_calls
                    .insert(call_expr.get_syntax_id());
            }
            return return_type;
        };
        match &parent_id {
            Some(existing) if existing != candidate => {
                if is_broad_panel_type(&return_type) {
                    cache
                        .vgui_parent_fallback_calls
                        .insert(call_expr.get_syntax_id());
                }
                return return_type;
            }
            Some(_) => {}
            None => parent_id = Some(candidate.clone()),
        }
    }
    match parent_id {
        Some(parent_id) => {
            // A chain answer taken mid-analysis can be one the final chain
            // state contradicts; the settled pass re-derives these reads.
            cache
                .vgui_parent_chain_calls
                .insert(call_expr.get_syntax_id());
            LuaType::Ref(parent_id)
        }
        None => {
            if is_broad_panel_type(&return_type) {
                cache
                    .vgui_parent_fallback_calls
                    .insert(call_expr.get_syntax_id());
            }
            return_type
        }
    }
}

fn is_broad_panel_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => id.get_name() == "Panel",
        LuaType::Union(union) => {
            let mut has_panel = false;
            for t in union.types() {
                if t.is_nil() {
                    continue;
                }
                if matches!(t, LuaType::Ref(id) | LuaType::Def(id) if id.get_name() == "Panel") {
                    has_panel = true;
                } else {
                    return false;
                }
            }
            has_panel
        }
        _ => false,
    }
}

fn single_non_nil_instance_type_id(typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Instance(instance) => single_non_nil_instance_type_id(instance.get_base()),
        LuaType::Union(union) => {
            let mut resolved = None;
            for typ in union.types().filter(|typ| !typ.is_nil()) {
                let type_id = single_non_nil_instance_type_id(typ)?;
                if resolved
                    .as_ref()
                    .is_some_and(|existing| existing != &type_id)
                {
                    return None;
                }
                resolved = Some(type_id);
            }
            resolved
        }
        LuaType::Def(type_id) | LuaType::Ref(type_id) => Some(type_id.clone()),
        _ => None,
    }
}

fn collect_non_nil_type_ids(typ: &LuaType, type_ids: &mut Vec<LuaTypeDeclId>) {
    match typ {
        LuaType::Instance(instance) => collect_non_nil_type_ids(instance.get_base(), type_ids),
        LuaType::Union(union) => {
            for typ in union.types().filter(|typ| !typ.is_nil()) {
                collect_non_nil_type_ids(typ, type_ids);
            }
        }
        LuaType::Def(type_id) | LuaType::Ref(type_id) => type_ids.push(type_id.clone()),
        _ => {}
    }
}

fn remove_nested_nil(typ: LuaType) -> LuaType {
    match typ {
        LuaType::Instance(instance) => LuaType::Instance(
            LuaInstanceType::new(
                remove_nested_nil(instance.get_base().clone()),
                instance.get_range().clone(),
            )
            .into(),
        ),
        LuaType::Union(union) => {
            LuaType::from_vec(union.types().filter(|typ| !typ.is_nil()).cloned().collect())
        }
        _ => typ,
    }
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
        LuaType::Table | LuaType::TableConst(_) | LuaType::Instance(_) => {}
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

fn should_prefer_signature_for_call(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    call_expr_type: &LuaType,
) -> bool {
    if matches!(call_expr_type, LuaType::Signature(current) if *current == signature_id) {
        return false;
    }

    db.get_signature_index()
        .get(&signature_id)
        .is_some_and(|signature| match call_expr_type {
            LuaType::DocFunction(func) => {
                let missing_optional_default = signature
                    .get_param_optional_flags()
                    .into_iter()
                    .enumerate()
                    .any(|(idx, is_optional)| is_optional && !func.is_param_optional(idx));
                let richer_return =
                    func.get_ret().is_unknown() && !signature.get_return_type().is_unknown();

                missing_optional_default
                    || richer_return
                    || !signature.overloads.is_empty()
                    || !signature.out_params.is_empty()
                    || !signature.nil_return_guard_params().is_empty()
                    || (signature.direct_param_return_alias().is_some()
                        || signature.class_name_param_return_alias().is_some())
                    || !signature.falsy_param_nil_free_return_slots().is_empty()
                    || !signature.falsy_param_return_aliases().is_empty()
            }
            _ => false,
        })
}

pub(crate) fn get_prefix_expr_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaSignatureId> {
    let mut prefix_expr = call_expr.get_prefix_expr()?;
    while let LuaExpr::ParenExpr(paren_expr) = prefix_expr {
        prefix_expr = paren_expr.get_expr()?;
    }
    if let LuaExpr::NameExpr(name_expr) = &prefix_expr
        && let Some(signature_id) = get_local_name_signature_id(db, cache, name_expr)
    {
        return Some(signature_id);
    }
    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;
    get_signature_id_from_semantic_decl_value_expr(db, semantic_decl)
}

fn get_local_name_signature_id(
    db: &DbIndex,
    cache: &LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaSignatureId> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
    let closure = LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)?;
    let LuaExpr::ClosureExpr(closure) = closure else {
        return None;
    };
    Some(LuaSignatureId::from_closure(decl.get_file_id(), &closure))
}

fn get_signature_id_from_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return Some(signature_id);
    }
    let file_id = match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
            decl_id.file_id
        }
        crate::LuaSemanticDeclId::Member(member_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
            member_id.file_id
        }
        crate::LuaSemanticDeclId::Signature(signature_id) => return Some(signature_id),
        crate::LuaSemanticDeclId::TypeDecl(_) => return None,
    };
    let LuaExpr::ClosureExpr(closure) = get_semantic_decl_value_expr(db, semantic_decl)? else {
        return None;
    };
    Some(LuaSignatureId::from_closure(file_id, &closure))
}

fn get_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<LuaExpr> {
    match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            let value_syntax_id = decl.get_value_syntax_id()?;
            let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
            LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)
        }
        crate::LuaSemanticDeclId::Member(member_id) => get_member_value_expr(db, member_id),
        crate::LuaSemanticDeclId::Signature(_) | crate::LuaSemanticDeclId::TypeDecl(_) => None,
    }
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
    prefix_signature_id: Option<LuaSignatureId>,
) -> InferCallFuncResult {
    if func.contain_tpl() {
        let result = instantiate_func_generic(db, cache, func, call_expr.clone())?;
        return Ok(Arc::new(result));
    }

    // Handle self-type substitution for functions with SelfInfer in return type
    // (e.g., tableof<self>). This covers cases like `local getTable = Entity.GetTable; getTable(self)`
    if func.contain_self() {
        let self_type = infer_self_type(db, cache, &call_expr);
        if let Some(self_type) = self_type {
            let mut substitutor = crate::semantic::generic::TypeSubstitutor::new();
            substitutor.add_self_type(self_type);
            if let LuaType::DocFunction(f) = instantiate_doc_function(db, func, &substitutor) {
                return Ok(f);
            }
        }
    }

    if let Some(signature_id) = prefix_signature_id
        && let Some(signature) = db.get_signature_index().get(&signature_id)
        && ((signature.direct_param_return_alias().is_some()
            || signature.class_name_param_return_alias().is_some())
            || !signature.falsy_param_nil_free_return_slots().is_empty()
            || !signature.falsy_param_return_aliases().is_empty())
    {
        let specialized =
            specialize_return_aliases_for_call(db, cache, signature, func, &call_expr);
        return Ok(specialize_falsy_param_returns_for_call(
            db,
            cache,
            signature,
            specialized.as_deref().unwrap_or(func),
            &call_expr,
        ));
    }

    if let Some(registered_convar_type) =
        get_registered_convar_type_at_call(db, cache, prefix_signature_id, &call_expr, func)
    {
        return Ok(Arc::new(
            LuaFunctionType::new(
                func.get_async_state(),
                func.is_colon_define(),
                func.is_variadic(),
                func.get_params().to_vec(),
                registered_convar_type,
            )
            .with_optional_params(func.get_optional_params().to_vec()),
        ));
    }

    Ok(func.clone().into())
}

fn get_registered_convar_type_at_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature_id: Option<LuaSignatureId>,
    call_expr: &LuaCallExpr,
    func: &LuaFunctionType,
) -> Option<LuaType> {
    if !is_getconvar_reference_call(db, signature_id, call_expr) {
        return None;
    }
    get_registered_convar_type_for_verified_call(db, cache, call_expr, func)
}

fn get_registered_convar_type_at_signature_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature_id: LuaSignatureId,
    signature: &LuaSignature,
    call_expr: &LuaCallExpr,
    func: &LuaFunctionType,
) -> Option<LuaType> {
    if !is_getconvar_reference_signature_call(db, signature_id, signature, call_expr) {
        return None;
    }
    get_registered_convar_type_for_verified_call(db, cache, call_expr, func)
}

fn get_registered_convar_type_for_verified_call(
    db: &DbIndex,
    cache: &LuaInferCache,
    call_expr: &LuaCallExpr,
    func: &LuaFunctionType,
) -> Option<LuaType> {
    if !db.get_emmyrc().gmod.enabled || !func.get_ret().is_nullable() {
        return None;
    }
    let convar_name = crate::ast_util::literal_string_arg_value(call_expr, 0)?;
    let convar_name = convar_name.trim();
    if convar_name.is_empty() {
        return None;
    }

    let call_file_id = cache.get_file_id();
    let call_offset = call_expr.get_range().start();
    let call_state = db
        .get_gmod_infer_index()
        .get_state_mask_at_offset(&call_file_id, call_offset);

    db.get_gmod_infer_index()
        .get_system_aggregate()
        .convar_registrations(convar_name)
        .iter()
        .any(|registration| {
            convar_registration_is_load_available(db, call_file_id, call_offset, registration)
                && convar_registration_state_is_compatible(
                    db,
                    call_state,
                    registration.file_id,
                    registration.name_range.start(),
                    registration.convar_kind,
                )
        })
        .then(|| remove_nil_from_type(func.get_ret().clone()))
}

fn is_getconvar_reference_signature_call(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    signature: &LuaSignature,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };
    if prefix_expr.syntax().text() != "GetConVar" {
        return false;
    }
    if signature.call_arg_roles_for_param(0).iter().any(|role| {
        role.is_direct_arg()
            && role.domain == GMOD_DOMAIN_CONVAR
            && role.role == GMOD_ROLE_REFERENCE
    }) {
        return true;
    }
    db.get_vfs()
        .get_file_path(&signature_id.get_file_id())
        .and_then(|path| path.to_str())
        .map(|path| path.replace('\\', "/"))
        .is_some_and(|path| path.ends_with("/lua/includes/util.lua"))
}

fn is_getconvar_reference_call(
    db: &DbIndex,
    signature_id: Option<LuaSignatureId>,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(signature_id) = signature_id else {
        return false;
    };
    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return false;
    };
    is_getconvar_reference_signature_call(db, signature_id, signature, call_expr)
}

fn convar_registration_is_load_available(
    db: &DbIndex,
    call_file_id: crate::FileId,
    call_offset: rowan::TextSize,
    registration: &crate::GmodSystemRegistration,
) -> bool {
    if db
        .get_gmod_load_index()
        .get_file_info(&registration.file_id)
        .is_some_and(|load_info| load_info.shadowed_by.is_some())
    {
        return false;
    }

    if registration.file_id == call_file_id {
        return registration.name_range.start() < call_offset;
    }

    let mut pending = vec![call_file_id];
    let mut visited = rustc_hash::FxHashSet::default();
    while let Some(file_id) = pending.pop() {
        if !visited.insert(file_id) {
            continue;
        }
        let Some(load_info) = db.get_gmod_load_index().get_file_info(&file_id) else {
            continue;
        };
        for edge in &load_info.incoming_edges {
            if edge.source_file_id == registration.file_id && load_edge_executes_target(edge.kind) {
                if edge
                    .range
                    .is_some_and(|range| registration.name_range.start() < range.start())
                {
                    return true;
                }
            }
            if load_edge_executes_target(edge.kind) {
                pending.push(edge.source_file_id);
            }
        }
    }
    false
}

fn convar_registration_state_is_compatible(
    db: &DbIndex,
    call_state: GmodStateMask,
    registration_file_id: crate::FileId,
    registration_offset: rowan::TextSize,
    convar_kind: Option<GmodConVarKind>,
) -> bool {
    let registration_state = db
        .get_gmod_infer_index()
        .get_state_mask_at_offset(&registration_file_id, registration_offset);
    if !crate::GmodInferIndex::are_state_masks_compatible(call_state, registration_state) {
        return false;
    }
    match convar_kind {
        Some(GmodConVarKind::Client) => {
            !call_state.intersects(GmodStateMask::SERVER)
                && call_state.is_compatible_with(GmodStateMask::CLIENT.union(GmodStateMask::MENU))
        }
        _ => registration_state.is_compatible_with(call_state),
    }
}

fn load_edge_executes_target(kind: GmodLoadEdgeKind) -> bool {
    matches!(
        kind,
        GmodLoadEdgeKind::Include
            | GmodLoadEdgeKind::IncludeCS
            | GmodLoadEdgeKind::Require
            | GmodLoadEdgeKind::WrapperInclude
            | GmodLoadEdgeKind::DynamicInclude
    )
}

fn infer_generic_doc_function_union(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    union: &LuaUnionType,
    call_expr: LuaCallExpr,
    args_count: Option<usize>,
) -> InferCallFuncResult {
    let overloads = union
        .types()
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
        )
        .with_optional_params(signature.get_param_optional_flags());
        if is_generic {
            fake_doc_function =
                instantiate_func_generic(db, cache, &fake_doc_function, call_expr.clone())?;
        }

        let fake_doc_function =
            apply_signature_return_kinds_to_function(signature, &fake_doc_function);
        let fake_doc_function = specialize_nil_guarded_return_for_call(
            db,
            cache,
            signature,
            fake_doc_function.as_ref(),
            &call_expr,
        );
        let fake_doc_function = specialize_falsy_param_returns_for_call(
            db,
            cache,
            signature,
            fake_doc_function.as_ref(),
            &call_expr,
        );
        let fake_doc_function = specialize_return_aliases_for_call(
            db,
            cache,
            signature,
            fake_doc_function.as_ref(),
            &call_expr,
        )
        .unwrap_or(fake_doc_function);
        Ok(specialize_registered_convar_return_for_call(
            db,
            cache,
            signature_id,
            signature,
            fake_doc_function.as_ref(),
            &call_expr,
        ))
    } else {
        let mut new_overloads = signature.overloads.clone();
        let fake_doc_function = LuaFunctionType::new(
            signature.async_state,
            signature.is_colon_define,
            signature.is_vararg,
            signature.get_type_params(),
            signature.get_return_type(),
        )
        .with_optional_params(signature.get_param_optional_flags());
        new_overloads.push(apply_signature_return_kinds_to_function(
            signature,
            &fake_doc_function,
        ));

        let resolved = resolve_signature(
            db,
            cache,
            new_overloads,
            call_expr.clone(),
            is_generic,
            args_count,
        )?;
        let resolved = specialize_nil_guarded_return_for_call(
            db,
            cache,
            signature,
            resolved.as_ref(),
            &call_expr,
        );
        let resolved = specialize_falsy_param_returns_for_call(
            db,
            cache,
            signature,
            resolved.as_ref(),
            &call_expr,
        );
        let resolved =
            specialize_return_aliases_for_call(db, cache, signature, resolved.as_ref(), &call_expr)
                .unwrap_or(resolved);
        Ok(specialize_registered_convar_return_for_call(
            db,
            cache,
            signature_id,
            signature,
            resolved.as_ref(),
            &call_expr,
        ))
    }
}

pub(crate) fn signature_call_selects_declared_overload(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature_id: LuaSignatureId,
    call_expr: LuaCallExpr,
) -> bool {
    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return false;
    };
    if signature.overloads.is_empty() || !signature.is_resolve_return() {
        return false;
    }
    let is_generic = signature_is_generic(db, cache, signature, &call_expr).unwrap_or(false);
    let declared_overload_count = signature.overloads.len();
    let mut candidates = signature.overloads.clone();
    candidates.push(Arc::new(
        LuaFunctionType::new(
            signature.async_state,
            signature.is_colon_define,
            signature.is_vararg,
            signature.get_type_params(),
            signature.get_return_type(),
        )
        .with_optional_params(signature.get_param_optional_flags()),
    ));
    crate::semantic::overload_resolve::resolve_signature_with_index(
        db, cache, candidates, call_expr, is_generic, None,
    )
    .is_ok_and(|(_, selected_idx)| selected_idx < declared_overload_count)
}

fn specialize_class_name_param_return_alias_for_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature: &LuaSignature,
    func_ty: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Option<Arc<LuaFunctionType>> {
    let param_idx = signature.class_name_param_return_alias()?;
    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let arg = call_arg_for_param(call_expr, func_ty, &args, param_idx)?;
    let class_name = resolve_class_name_string_from_arg(db, cache, &arg)?;
    let type_id = LuaTypeDeclId::global(&class_name);
    db.get_type_index().get_type_decl(&type_id)?;

    let range = InFiled {
        file_id: cache.get_file_id(),
        value: call_expr.get_range(),
    };
    let return_type = LuaType::Instance(LuaInstanceType::new(LuaType::Ref(type_id), range).into());

    Some(Arc::new(
        LuaFunctionType::new(
            func_ty.get_async_state(),
            func_ty.is_colon_define(),
            func_ty.is_variadic(),
            func_ty.get_params().to_vec(),
            return_type,
        )
        .with_optional_params(func_ty.get_optional_params().to_vec()),
    ))
}

/// Resolve a classname argument to a concrete string. Supports string literals
/// and already-inferred string constants (including flow-narrowed defaults).
fn resolve_class_name_string_from_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg: &LuaExpr,
) -> Option<String> {
    if let LuaExpr::LiteralExpr(lit) = arg
        && let Some(glua_parser::LuaLiteralToken::String(s)) = lit.get_literal()
    {
        return Some(s.get_value());
    }

    let ty = infer_expr(db, cache, arg.clone()).ok()?;
    match ty {
        LuaType::StringConst(s) | LuaType::DocStringConst(s) => Some(s.as_str().to_string()),
        _ => None,
    }
}

fn specialize_return_aliases_for_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature: &LuaSignature,
    func_ty: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Option<Arc<LuaFunctionType>> {
    specialize_direct_param_return_alias_for_call(db, cache, signature, func_ty, call_expr)
        .or_else(|| {
            specialize_class_name_param_return_alias_for_call(
                db, cache, signature, func_ty, call_expr,
            )
        })
        .or_else(|| restore_definition_through_return_alias(db, cache, func_ty, call_expr))
}

/// Gives back the definition a declared pass-through was handed.
///
/// Binding a template parameter turns `Def(X)` into `Ref(X)`, so that a generic
/// function cannot claim to define the class it was merely given. A function
/// annotated `@[return_alias(n)]` says it returns argument `n` itself, and
/// `assert(FindMetaTable("Panel"))` is the shape that needs it: without this the
/// methods written on the result extend nothing, because only a `Def` does.
///
/// Only the exact `Ref(X)` -> `Def(X)` step is restored, so a return the
/// annotation transformed — `std.NotNull<T>` dropping `nil`, say — keeps its
/// transformation.
fn restore_definition_through_return_alias(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func_ty: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Option<Arc<LuaFunctionType>> {
    // The alias may be the whole return or, where the function also passes the
    // rest of its arguments back, the first of several.
    let returned_id = match func_ty.get_ret() {
        LuaType::Ref(returned_id) => returned_id.clone(),
        LuaType::Variadic(variadic) => match variadic.get_type(0) {
            Some(LuaType::Ref(returned_id)) => returned_id.clone(),
            _ => return None,
        },
        _ => return None,
    };
    let signature_id = get_prefix_expr_signature_id(db, cache, call_expr)?;
    let attribute =
        crate::db_index::find_signature_attribute_use(db, signature_id, "return_alias")?;
    let param = attribute
        .get_param_by_name("param")
        .or_else(|| attribute.args.first().and_then(|(_, typ)| typ.as_ref()))?;
    let (LuaType::IntegerConst(param_idx) | LuaType::DocIntegerConst(param_idx)) = param else {
        return None;
    };
    let param_idx = usize::try_from(*param_idx).ok()?;
    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let arg = call_arg_for_param(call_expr, func_ty, &args, param_idx)?;
    let LuaType::Def(arg_id) = infer_expr(db, cache, arg).ok()? else {
        return None;
    };
    if arg_id != returned_id {
        return None;
    }

    let restored = match func_ty.get_ret() {
        LuaType::Variadic(variadic) => match std::ops::Deref::deref(variadic) {
            VariadicType::Multi(slots) => {
                let mut slots = slots.clone();
                *slots.first_mut()? = LuaType::Def(arg_id);
                LuaType::Variadic(VariadicType::Multi(slots).into())
            }
            VariadicType::Base(_) => return None,
        },
        _ => LuaType::Def(arg_id),
    };

    Some(Arc::new(
        LuaFunctionType::new(
            func_ty.get_async_state(),
            func_ty.is_colon_define(),
            func_ty.is_variadic(),
            func_ty.get_params().to_vec(),
            restored,
        )
        .with_optional_params(func_ty.get_optional_params().to_vec()),
    ))
}

fn specialize_direct_param_return_alias_for_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature: &LuaSignature,
    func_ty: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Option<Arc<LuaFunctionType>> {
    let param_idx = signature.direct_param_return_alias()?;
    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let arg = call_arg_for_param(call_expr, func_ty, &args, param_idx)?;
    let Ok(return_type) = infer_expr(db, cache, arg) else {
        return None;
    };
    if direct_param_alias_type_is_uninformative(&return_type) || &return_type == func_ty.get_ret() {
        return None;
    }

    Some(Arc::new(
        LuaFunctionType::new(
            func_ty.get_async_state(),
            func_ty.is_colon_define(),
            func_ty.is_variadic(),
            func_ty.get_params().to_vec(),
            return_type,
        )
        .with_optional_params(func_ty.get_optional_params().to_vec()),
    ))
}

fn direct_param_alias_type_is_uninformative(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown => true,
        LuaType::Union(union) => {
            let mut saw_opaque = false;
            for component in union.types() {
                if matches!(component, LuaType::Any | LuaType::Unknown) {
                    saw_opaque = true;
                } else if !component.is_nil() {
                    return false;
                }
            }
            saw_opaque
        }
        LuaType::MultiLineUnion(union) => {
            let mut saw_opaque = false;
            for (component, _) in union.get_unions() {
                if matches!(component, LuaType::Any | LuaType::Unknown) {
                    saw_opaque = true;
                } else if !component.is_nil() {
                    return false;
                }
            }
            saw_opaque
        }
        _ => false,
    }
}

fn specialize_registered_convar_return_for_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature_id: LuaSignatureId,
    signature: &LuaSignature,
    func: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Arc<LuaFunctionType> {
    if let Some(registered_convar_type) = get_registered_convar_type_at_signature_call(
        db,
        cache,
        signature_id,
        signature,
        call_expr,
        func,
    ) {
        return Arc::new(
            LuaFunctionType::new(
                func.get_async_state(),
                func.is_colon_define(),
                func.is_variadic(),
                func.get_params().to_vec(),
                registered_convar_type,
            )
            .with_optional_params(func.get_optional_params().to_vec()),
        );
    }
    Arc::new(func.clone())
}

fn specialize_nil_guarded_return_for_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature: &LuaSignature,
    func_ty: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Arc<LuaFunctionType> {
    if signature.nil_return_guard_params().is_empty() || !func_ty.get_ret().is_nullable() {
        return Arc::new(func_ty.clone());
    }

    let Some(args) = call_expr.get_args_list() else {
        return Arc::new(func_ty.clone());
    };
    let args = args.get_args().collect::<Vec<_>>();

    let guard_satisfied = signature.nil_return_guard_params().iter().all(|param_idx| {
        call_arg_for_param(call_expr, func_ty, &args, *param_idx)
            .and_then(|arg| infer_expr(db, cache, arg.clone()).ok())
            .is_some_and(|arg_type| arg_type.is_always_truthy())
    });

    if !guard_satisfied {
        return Arc::new(func_ty.clone());
    }

    Arc::new(
        LuaFunctionType::new(
            func_ty.get_async_state(),
            func_ty.is_colon_define(),
            func_ty.is_variadic(),
            func_ty.get_params().to_vec(),
            remove_nil_from_type(func_ty.get_ret().clone()),
        )
        .with_optional_params(func_ty.get_optional_params().to_vec()),
    )
}

fn specialize_falsy_param_returns_for_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature: &LuaSignature,
    func_ty: &LuaFunctionType,
    call_expr: &LuaCallExpr,
) -> Arc<LuaFunctionType> {
    if (signature.falsy_param_nil_free_return_slots().is_empty()
        && signature.falsy_param_return_aliases().is_empty())
        || !func_ty.get_ret().is_nullable()
    {
        return Arc::new(func_ty.clone());
    }

    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut return_type = func_ty.get_ret().clone();
    let mut changed = false;

    for fact in signature.falsy_param_nil_free_return_slots() {
        let arg = call_arg_for_param(call_expr, func_ty, &args, fact.param_idx);
        if arg_is_omitted_or_always_falsy(db, cache, arg.as_ref()) {
            let old_return_type = return_type.clone();
            let new_return_type = remove_nil_from_return_slot(return_type, fact.return_slot);
            changed |= new_return_type != old_return_type;
            return_type = new_return_type;
        }
    }

    for fact in signature.falsy_param_return_aliases() {
        let falsy_arg = call_arg_for_param(call_expr, func_ty, &args, fact.falsy_param_idx);
        if !arg_is_omitted_or_always_falsy(db, cache, falsy_arg.as_ref()) {
            continue;
        }
        let Some(alias_arg) = call_arg_for_param(call_expr, func_ty, &args, fact.aliased_param_idx)
        else {
            continue;
        };
        if alias_arg_is_not_nil_free_syntax(&alias_arg) {
            continue;
        }
        let Ok(alias_type) = infer_expr(db, cache, alias_arg) else {
            continue;
        };
        if alias_type.is_unknown() || alias_type.is_nullable() {
            continue;
        }
        let old_return_type = return_type.clone();
        let new_return_type = replace_return_slot_with_type(
            return_type,
            fact.return_slot,
            remove_nil_from_type(alias_type),
        );
        changed |= new_return_type != old_return_type;
        return_type = new_return_type;
    }

    if !changed {
        return Arc::new(func_ty.clone());
    }

    Arc::new(
        LuaFunctionType::new(
            func_ty.get_async_state(),
            func_ty.is_colon_define(),
            func_ty.is_variadic(),
            func_ty.get_params().to_vec(),
            return_type,
        )
        .with_optional_params(func_ty.get_optional_params().to_vec()),
    )
}

fn alias_arg_is_not_nil_free_syntax(arg: &LuaExpr) -> bool {
    matches!(
        arg,
        LuaExpr::BinaryExpr(binary)
            if binary.get_op_token().map(|op| op.get_op()) != Some(BinaryOperator::OpOr)
    )
}

fn replace_return_slot_with_type(t: LuaType, slot: usize, replacement: LuaType) -> LuaType {
    match t {
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Multi(types) => {
                let mut types = types.clone();
                if let Some(slot_type) = types.get_mut(slot) {
                    *slot_type = replacement;
                }
                LuaType::Variadic(VariadicType::Multi(types).into())
            }
            VariadicType::Base(_) if slot == 0 => {
                LuaType::Variadic(VariadicType::Base(replacement).into())
            }
            _ => LuaType::Variadic(variadic),
        },
        LuaType::Union(union) => LuaType::from_vec(
            union
                .types()
                .map(|typ| replace_return_slot_with_type(typ.clone(), slot, replacement.clone()))
                .collect(),
        ),
        _ if slot == 0 => replacement,
        other => other,
    }
}

fn arg_is_omitted_or_always_falsy(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg: Option<&LuaExpr>,
) -> bool {
    let Some(arg) = arg else {
        return true;
    };
    infer_expr(db, cache, arg.clone()).is_ok_and(|typ| typ.is_always_falsy())
}

fn remove_nil_from_return_slot(t: LuaType, slot: usize) -> LuaType {
    match t {
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Multi(types) => {
                let mut types = types.clone();
                if let Some(slot_type) = types.get_mut(slot) {
                    *slot_type = remove_nil_from_type(slot_type.clone());
                }
                LuaType::Variadic(VariadicType::Multi(types).into())
            }
            VariadicType::Base(base) if slot == 0 => {
                LuaType::Variadic(VariadicType::Base(remove_nil_from_type(base.clone())).into())
            }
            _ => LuaType::Variadic(variadic),
        },
        LuaType::Union(union) => LuaType::from_vec(
            union
                .types()
                .map(|typ| remove_nil_from_return_slot(typ.clone(), slot))
                .collect(),
        ),
        other if slot == 0 => remove_nil_from_type(other),
        other => other,
    }
}

fn call_arg_for_param(
    call_expr: &LuaCallExpr,
    func_ty: &LuaFunctionType,
    args: &[LuaExpr],
    param_idx: usize,
) -> Option<LuaExpr> {
    match (func_ty.is_colon_define(), call_expr.is_colon_call()) {
        (true, false) => args.get(param_idx.checked_add(1)?).cloned(),
        (false, true) if param_idx == 0 => call_expr.get_prefix_expr(),
        (false, true) => args.get(param_idx.checked_sub(1)?).cloned(),
        _ => args.get(param_idx).cloned(),
    }
}

fn remove_nil_from_type(t: LuaType) -> LuaType {
    match t {
        LuaType::Nil => LuaType::Unknown,
        LuaType::Union(u) => LuaType::from_vec(
            u.types()
                .filter(|it| !matches!(it, LuaType::Nil))
                .cloned()
                .collect(),
        ),
        LuaType::Instance(instance_type) => {
            let new_base = remove_nil_from_type(instance_type.get_base().clone());
            if new_base.is_unknown() {
                LuaType::Unknown
            } else {
                LuaType::Instance(
                    LuaInstanceType::new(new_base, instance_type.get_range().clone()).into(),
                )
            }
        }
        _ => t,
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
    let substitutor = TypeSubstitutor::from_type_array(generic_params.clone());

    let type_decl = db
        .get_type_index()
        .get_type_decl(&type_id)
        .ok_or_else(|| InferFailReason::UnResolveTypeDecl(type_id.clone()))?;
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
                return Ok(normalize_call_operator_doc_function(func.as_ref()));
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

fn normalize_call_operator_doc_function(func: &LuaFunctionType) -> Arc<LuaFunctionType> {
    let mut params = func.get_params().to_vec();
    let mut optional_params = func.get_optional_params().to_vec();
    if !params.is_empty() && !func.is_colon_define() {
        params.remove(0);
        optional_params.remove(0);
    }

    Arc::new(
        LuaFunctionType::new(
            func.get_async_state(),
            false,
            func.is_variadic(),
            params,
            func.get_ret().clone(),
        )
        .with_optional_params(optional_params),
    )
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
                    if !signature.is_resolve_return() {
                        return Err(InferFailReason::UnResolveSignatureReturn(signature_id));
                    }

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
                    )
                    .with_optional_params(signature.get_param_optional_flags());
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

                return Ok(materialize_instance_return(return_type.clone(), id));
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

fn materialize_instance_return(base: LuaType, range: InFiled<TextRange>) -> LuaType {
    let (non_nil_base, nullable) = separate_nested_instance_nil(base);
    let Some(non_nil_base) = non_nil_base else {
        return LuaType::Nil;
    };
    let materialized = LuaType::Instance(LuaInstanceType::new(non_nil_base, range).into());
    if nullable {
        LuaType::from_vec(vec![materialized, LuaType::Nil])
    } else {
        materialized
    }
}

fn separate_nested_instance_nil(base: LuaType) -> (Option<LuaType>, bool) {
    match base {
        LuaType::Nil => (None, true),
        LuaType::Union(union) => {
            let mut nullable = false;
            let mut non_nil_types = Vec::new();
            for typ in union.types() {
                let (non_nil_type, nested_nullable) = separate_nested_instance_nil(typ.clone());
                nullable |= nested_nullable;
                if let Some(non_nil_type) = non_nil_type {
                    non_nil_types.push(non_nil_type);
                }
            }
            let non_nil_type =
                (!non_nil_types.is_empty()).then(|| LuaType::from_vec(non_nil_types));
            (non_nil_type, nullable)
        }
        LuaType::Instance(instance) => {
            let (non_nil_base, nullable) =
                separate_nested_instance_nil(instance.get_base().clone());
            let non_nil_instance = non_nil_base.map(|non_nil_base| {
                LuaType::Instance(
                    LuaInstanceType::new(non_nil_base, instance.get_range().clone()).into(),
                )
            });
            (non_nil_instance, nullable)
        }
        _ => (Some(base), false),
    }
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

    Arc::new(
        LuaFunctionType::new(
            function.get_async_state(),
            function.is_colon_define(),
            function.is_variadic(),
            function.get_params().to_vec(),
            return_type,
        )
        .with_optional_params(function.get_optional_params().to_vec()),
    )
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
                .types()
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
        InferFailReason, InferGuard, LuaSignatureId, LuaType, LuaUnionType, SignatureReturnStatus,
        VirtualWorkspace, semantic::infer_call_expr_func,
    };
    use glua_parser::LuaAstNode;

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

    #[test]
    fn test_union_call_defers_when_an_overload_return_is_unresolved() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local function resolved() return "a" end
            local function pending() return "b" end
            resolved()
            "#,
        );

        let (signature_ids, call_expr) = {
            let db = ws.analysis.compilation.get_db();
            let tree = db
                .get_vfs()
                .get_syntax_tree(&file_id)
                .expect("Tree must exist");
            let chunk = tree.get_chunk_node();
            let signature_ids = chunk
                .descendants::<glua_parser::LuaClosureExpr>()
                .map(|closure| LuaSignatureId::from_closure(file_id, &closure))
                .collect::<Vec<_>>();
            let call_expr = chunk
                .descendants::<glua_parser::LuaCallExpr>()
                .next()
                .expect("Call expr must exist");
            (signature_ids, call_expr)
        };
        assert_eq!(signature_ids.len(), 2);

        ws.get_db_mut()
            .get_signature_index_mut()
            .get_mut(&signature_ids[1])
            .expect("Signature must exist")
            .resolve_return = SignatureReturnStatus::UnResolve;

        let union_type = LuaType::Union(
            LuaUnionType::from_vec(vec![
                LuaType::Signature(signature_ids[0]),
                LuaType::Signature(signature_ids[1]),
            ])
            .into(),
        );

        let semantic_model = ws.analysis.compilation.get_semantic_model(file_id).unwrap();
        let db = semantic_model.get_db();
        let mut cache = semantic_model.get_cache().borrow_mut();
        let result = infer_call_expr_func(
            db,
            &mut cache,
            call_expr,
            union_type,
            &InferGuard::new(),
            None,
        );

        assert!(
            matches!(
                result,
                Err(InferFailReason::UnResolveSignatureReturn(id)) if id == signature_ids[1]
            ),
            "expected the union call to defer on the unresolved overload, got: {result:?}"
        );
    }
}
