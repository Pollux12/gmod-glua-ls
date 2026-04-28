use std::{
    collections::{BTreeMap, HashSet},
    ops::Deref,
    sync::Arc,
};

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaFuncStat, LuaIndexExpr,
    LuaLocalStat, LuaTableExpr, LuaTableField,
};

use crate::{
    DbIndex, FileId, GmodRealm, InFiled, InferFailReason, LuaDeclId, LuaDeclTypeKind,
    LuaDocReturnInfo, LuaMember, LuaMemberId, LuaMemberInfo, LuaMemberKey, LuaOperator,
    LuaOperatorMetaMethod, LuaOperatorOwner, LuaSemanticDeclId, LuaType, LuaTypeCache, LuaTypeDecl,
    LuaTypeDeclId, LuaTypeFlag, OperatorFunction, RenderLevel, SemanticDeclLevel,
    SignatureReturnStatus, TypeOps, VariadicType,
    compilation::analyzer::{
        common::{add_member, bind_type},
        lua::{analyze_return_point, compute_module_semantic_id, infer_for_range_iter_expr_func},
        unresolve::UnResolveSpecialCall,
    },
    db_index::{LuaFunctionType, LuaMemberOwner, LuaSignature, LuaSignatureId},
    find_members_with_key, humanize_type,
    semantic::{
        InferGuard, LuaInferCache, SemanticDeclGuard, infer_call_expr_func, infer_expr,
        infer_expr_semantic_decl,
    },
};

use super::{
    ResolveResult, UnResolveDecl, UnResolveIterVar, UnResolveMember, UnResolveModule,
    UnResolveModuleRef, UnResolveReturn, UnResolveTableField,
};

pub fn try_resolve_decl(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    decl: &mut UnResolveDecl,
) -> ResolveResult {
    let expr = decl.expr.clone();
    let expr_type = infer_expr(db, cache, expr.clone())?;
    let decl_id = decl.decl_id;
    let expr_type = match &expr_type {
        LuaType::Variadic(multi) => multi
            .get_type(decl.ret_idx)
            .cloned()
            .unwrap_or(LuaType::Unknown),
        _ => expr_type,
    };

    bind_type(db, decl_id.into(), LuaTypeCache::InferType(expr_type));
    Ok(())
}

pub fn try_resolve_member(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    unresolve_member: &mut UnResolveMember,
) -> ResolveResult {
    if let Some(prefix_expr) = &unresolve_member.prefix {
        let prefix_type = infer_expr(db, cache, prefix_expr.clone())?;
        let member_owner = match prefix_type {
            LuaType::TableConst(in_file_range) => LuaMemberOwner::Element(in_file_range),
            LuaType::Def(def_id) => {
                let type_decl = db
                    .get_type_index()
                    .get_type_decl(&def_id)
                    .ok_or(InferFailReason::None)?;
                // if is exact type, no need to extend field
                if type_decl.is_exact() {
                    return Ok(());
                }
                LuaMemberOwner::Type(def_id)
            }
            LuaType::Instance(instance) => LuaMemberOwner::Element(instance.get_range().clone()),
            _ => {
                // Some annotation bundles define methods as `function TypeName:Method()`
                // without binding a typed declaration for `TypeName` in scope.
                // If a global type exists for that name, attach unresolved members there.
                let LuaExpr::NameExpr(name_expr) = prefix_expr else {
                    return Ok(());
                };
                let Some(name_token) = name_expr.get_name_token() else {
                    return Ok(());
                };
                let type_decl_id = LuaTypeDeclId::global(name_token.get_name_text());
                if db.get_type_index().get_type_decl(&type_decl_id).is_none() {
                    return Ok(());
                }
                LuaMemberOwner::Type(type_decl_id)
            }
        };
        let member_id = unresolve_member.member_id;
        add_member(db, member_owner, member_id);
        unresolve_member.prefix = None;
    }

    if let Some(expr) = unresolve_member.expr.clone() {
        let expr_type = infer_expr(db, cache, expr)?;
        let expr_type = match &expr_type {
            LuaType::Variadic(multi) => multi
                .get_type(unresolve_member.ret_idx)
                .cloned()
                .unwrap_or(LuaType::Unknown),
            _ => expr_type,
        };

        let member_id = unresolve_member.member_id;
        bind_type(db, member_id.into(), LuaTypeCache::InferType(expr_type));
    }

    Ok(())
}

pub fn try_resolve_table_field(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    unresolve_table_field: &mut UnResolveTableField,
) -> ResolveResult {
    let field = unresolve_table_field.field.clone();
    let field_key = field.get_field_key().ok_or(InferFailReason::None)?;
    let field_expr = field_key.get_expr().ok_or(InferFailReason::None)?;
    let field_type = infer_expr(db, cache, field_expr.clone())?;
    let member_key: LuaMemberKey = match field_type {
        LuaType::StringConst(s) => LuaMemberKey::Name((*s).clone()),
        LuaType::IntegerConst(i) => LuaMemberKey::Integer(i),
        _ => {
            if field_type.is_table() {
                LuaMemberKey::ExprType(field_type)
            } else {
                return Err(InferFailReason::None);
            }
        }
    };
    let file_id = unresolve_table_field.file_id;
    let table_expr = unresolve_table_field.table_expr.clone();
    let owner_id = LuaMemberOwner::Element(InFiled {
        file_id,
        value: table_expr.get_range(),
    });

    db.get_reference_index_mut().add_index_reference(
        member_key.clone(),
        file_id,
        field.get_syntax_id(),
    );

    let decl_type = match field.get_value_expr() {
        Some(expr) => infer_expr(db, cache, expr)?,
        None => return Err(InferFailReason::None),
    };

    let member_id = LuaMemberId::new(field.get_syntax_id(), file_id);
    let member = LuaMember::new(
        member_id,
        member_key,
        unresolve_table_field.decl_feature,
        None,
    );
    db.get_member_index_mut().add_member(owner_id, member);
    db.get_type_index_mut()
        .bind_type(member_id.into(), LuaTypeCache::InferType(decl_type.clone()));

    merge_table_field_to_def(db, cache, table_expr, member_id);
    Ok(())
}

fn merge_table_field_to_def(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    table_expr: LuaTableExpr,
    member_id: LuaMemberId,
) -> Option<()> {
    let file_id = cache.get_file_id();
    let local_name = table_expr
        .get_parent::<LuaLocalStat>()?
        .get_local_name_by_value(LuaExpr::TableExpr(table_expr.clone()))?;
    let decl_id = LuaDeclId::new(file_id, local_name.get_position());
    let type_cache = db.get_type_index().get_type_cache(&decl_id.into())?;
    if let LuaType::Def(id) = type_cache.deref() {
        let owner = LuaMemberOwner::Type(id.clone());
        db.get_member_index_mut()
            .set_member_owner(owner.clone(), member_id.file_id, member_id);
        db.get_member_index_mut()
            .add_member_to_owner(owner.clone(), member_id);
    }

    Some(())
}

pub fn try_resolve_module(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    module: &mut UnResolveModule,
) -> ResolveResult {
    let expr = module.expr.clone();
    let expr_type = infer_expr(db, cache, expr.clone())?;
    let expr_type = match &expr_type {
        LuaType::Variadic(multi) => multi.get_type(0).cloned().unwrap_or(LuaType::Unknown),
        _ => expr_type,
    };

    // Compute semantic_id for the exported expression using the shared helper
    let semantic_id = compute_module_semantic_id(db, module.file_id, &module.expr);

    let module_info = db
        .get_module_index_mut()
        .get_module_mut(module.file_id)
        .ok_or(InferFailReason::None)?;
    module_info.export_type = Some(expr_type);
    module_info.semantic_id = semantic_id;
    Ok(())
}

pub fn try_resolve_return_point(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    return_: &mut UnResolveReturn,
) -> ResolveResult {
    let return_docs = analyze_return_point(db, cache, &return_.return_points)?;

    let signature = db
        .get_signature_index_mut()
        .get_mut(&return_.signature_id)
        .ok_or(InferFailReason::None)?;

    if should_apply_resolved_return_docs(signature, &return_docs) {
        signature.resolve_return = SignatureReturnStatus::InferResolve;
        signature.return_docs = return_docs;
    }

    Ok(())
}

fn should_apply_resolved_return_docs(
    signature: &LuaSignature,
    return_docs: &[LuaDocReturnInfo],
) -> bool {
    let current_return = signature.get_return_type();
    let new_return = return_docs_to_type(return_docs);

    if signature.resolve_return == SignatureReturnStatus::UnResolve {
        return true;
    }

    if signature.resolve_return != SignatureReturnStatus::InferResolve {
        return false;
    }

    if current_return.is_unknown() && new_return.is_any() {
        return true; // Allow upgrading Unknown to Any
    }

    (current_return.is_unknown() || current_return.is_any())
        && !(new_return.is_unknown() || new_return.is_any())
}

fn return_docs_to_type(return_docs: &[LuaDocReturnInfo]) -> LuaType {
    match return_docs.len() {
        0 => LuaType::Nil,
        1 => return_docs[0].type_ref.clone(),
        _ => LuaType::Variadic(
            VariadicType::Multi(
                return_docs
                    .iter()
                    .map(|info| info.type_ref.clone())
                    .collect(),
            )
            .into(),
        ),
    }
}

pub fn try_resolve_iter_var(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    unresolve_iter_var: &mut UnResolveIterVar,
) -> ResolveResult {
    let iter_var_types = infer_for_range_iter_expr_func(db, cache, &unresolve_iter_var.iter_exprs)?;
    for (idx, var_name) in unresolve_iter_var.iter_vars.iter().enumerate() {
        let position = var_name.get_position();
        let decl_id = LuaDeclId::new(unresolve_iter_var.file_id, position);
        let ret_type = iter_var_types
            .get_type(idx)
            .cloned()
            .unwrap_or(LuaType::Unknown);
        let ret_type = TypeOps::Remove.apply(db, &ret_type, &LuaType::Nil);

        db.get_type_index_mut()
            .bind_type(decl_id.into(), LuaTypeCache::InferType(ret_type));
    }
    Ok(())
}

pub fn try_resolve_module_ref(
    db: &mut DbIndex,
    _: &mut LuaInferCache,
    module_ref: &UnResolveModuleRef,
) -> ResolveResult {
    let module_index = db.get_module_index();
    let module = module_index
        .get_module(module_ref.module_file_id)
        .ok_or(InferFailReason::None)?;
    let export_type = module.export_type.clone().ok_or(InferFailReason::None)?;
    match &module_ref.owner_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            db.get_type_index_mut()
                .bind_type((*decl_id).into(), LuaTypeCache::InferType(export_type));
        }
        LuaSemanticDeclId::Member(member_id) => {
            db.get_type_index_mut()
                .bind_type((*member_id).into(), LuaTypeCache::InferType(export_type));
        }
        _ => {}
    };

    Ok(())
}

pub fn try_resolve_special_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    unresolve_special_call: &mut UnResolveSpecialCall,
) -> ResolveResult {
    let call_expr = unresolve_special_call.call_expr.clone();
    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let callable_param_infos = collect_special_call_param_infos_for_prefix(
        db,
        cache,
        unresolve_special_call.file_id,
        call_expr.get_position(),
        &call_expr,
        &prefix_expr,
    )?;
    if callable_param_infos.is_empty() {
        return Ok(());
    }

    let is_colon_call = unresolve_special_call.call_expr.is_colon_call();
    for param_info in callable_param_infos {
        materialize_str_tpl_class_from_call(
            db,
            cache,
            unresolve_special_call.file_id,
            &unresolve_special_call.call_expr,
            param_info.param_idx,
            &param_info.param_type,
            param_info.is_colon_define,
            is_colon_call,
        )?;

        if param_info.is_constructor {
            try_resolve_constructor_param(
                db,
                cache,
                unresolve_special_call.file_id,
                &unresolve_special_call.call_expr,
                &param_info,
            )?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SpecialCallParamInfo {
    param_idx: usize,
    param_type: LuaType,
    is_constructor: bool,
    is_colon_define: bool,
    signature_id: Option<LuaSignatureId>,
}

fn collect_special_call_param_infos_for_prefix(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
) -> Result<Vec<SpecialCallParamInfo>, InferFailReason> {
    let mut visited_wrapped_decls = HashSet::new();
    collect_special_call_param_infos_for_prefix_inner(
        db,
        cache,
        caller_file_id,
        caller_position,
        call_expr,
        prefix_expr,
        &mut visited_wrapped_decls,
    )
}

fn collect_special_call_param_infos_for_prefix_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    visited_wrapped_decls: &mut HashSet<LuaSemanticDeclId>,
) -> Result<Vec<SpecialCallParamInfo>, InferFailReason> {
    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr.clone(),
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    );

    if let Some(semantic_decl) = semantic_decl {
        let param_infos =
            collect_special_call_param_infos_from_semantic_decl(db, semantic_decl.clone())?;
        if !param_infos.is_empty() {
            return Ok(param_infos);
        }

        if visited_wrapped_decls.insert(semantic_decl.clone()) {
            if let Some(target_expr) = get_wrapped_callable_target_expr(db, semantic_decl) {
                let param_infos = collect_special_call_param_infos_for_prefix_inner(
                    db,
                    cache,
                    caller_file_id,
                    caller_position,
                    call_expr,
                    &target_expr,
                    visited_wrapped_decls,
                )?;
                if !param_infos.is_empty() {
                    return Ok(param_infos);
                }
            }
        }
    }

    let callable_type = infer_expr(db, cache, prefix_expr.clone())?;
    let param_infos = collect_special_call_param_infos(db, &callable_type);
    if !param_infos.is_empty() {
        return Ok(param_infos);
    }

    let operator_collection = collect_special_call_param_infos_from_callable_operators(
        db,
        caller_file_id,
        caller_position,
        &callable_type,
    );
    if operator_collection.had_operators {
        return Ok(operator_collection.param_infos);
    }

    let call_func = infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        callable_type,
        &InferGuard::new(),
        None,
    )?;
    Ok(collect_doc_function_special_call_params(call_func.as_ref()))
}

pub(crate) fn get_wrapped_callable_target_expr(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaExpr> {
    let LuaExpr::CallExpr(call_expr) = get_semantic_decl_value_expr(db, semantic_decl)? else {
        return None;
    };
    get_setmetatable_call_target_expr(&call_expr)
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

fn get_member_value_expr(db: &DbIndex, member_id: LuaMemberId) -> Option<LuaExpr> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(field) = LuaTableField::cast(node.clone()) {
        return field.get_value_expr();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        if let Some(assign_stat) = index_expr.get_parent::<LuaAssignStat>() {
            let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
            let value_idx = vars
                .iter()
                .position(|var| var.get_syntax_id() == index_expr.get_syntax_id())?;
            return value_exprs.get(value_idx).cloned();
        }

        if let Some(func_stat) = index_expr.get_parent::<LuaFuncStat>() {
            return func_stat.get_closure().map(LuaExpr::ClosureExpr);
        }
    }

    None
}

pub(crate) fn get_setmetatable_call_target_expr(call_expr: &LuaCallExpr) -> Option<LuaExpr> {
    let LuaExpr::NameExpr(name_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    if name_expr.get_name_text()? != "setmetatable" {
        return None;
    }

    let args = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    let LuaExpr::TableExpr(metatable) = args.get(1)?.clone() else {
        return None;
    };

    metatable.get_fields().find_map(|field| {
        let field_name = match field.get_field_key()? {
            glua_parser::LuaIndexKey::Name(name) => name.get_name_text().to_string(),
            glua_parser::LuaIndexKey::String(string) => string.get_value(),
            _ => return None,
        };
        if field_name != "__call" {
            return None;
        }

        match field.get_value_expr()? {
            LuaExpr::NameExpr(name_expr) => Some(LuaExpr::NameExpr(name_expr)),
            LuaExpr::IndexExpr(index_expr) => Some(LuaExpr::IndexExpr(index_expr)),
            _ => None,
        }
    })
}

fn signature_has_overload_special_call_params(signature: &LuaSignature) -> bool {
    signature
        .overloads
        .iter()
        .any(|overload| overload_has_special_call_params(overload))
}

fn overload_has_special_call_params(func: &LuaFunctionType) -> bool {
    func.get_params().iter().any(|(_, param_type)| {
        param_type
            .as_ref()
            .map(type_contains_str_tpl_ref)
            .unwrap_or(false)
    })
}

fn collect_signature_overload_special_call_params(
    signature: &LuaSignature,
) -> Vec<SpecialCallParamInfo> {
    signature
        .overloads
        .iter()
        .flat_map(|overload| collect_doc_function_special_call_params(overload))
        .collect()
}

fn signature_has_any_special_call_params(signature: &LuaSignature) -> bool {
    signature.has_special_call_params() || signature_has_overload_special_call_params(signature)
}

fn collect_special_call_param_infos_from_callable_operators(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    callable_type: &LuaType,
) -> SpecialCallOperatorCollection {
    match callable_type {
        LuaType::TableConst(in_file_range) => db
            .get_metatable_index()
            .get(in_file_range)
            .map(|meta_table| {
                collect_special_call_param_infos_from_operator_owner(
                    db,
                    caller_file_id,
                    caller_position,
                    &LuaOperatorOwner::Table(meta_table.clone()),
                )
            })
            .unwrap_or_default(),
        LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => {
            collect_special_call_param_infos_from_operator_owner(
                db,
                caller_file_id,
                caller_position,
                &LuaOperatorOwner::Type(type_decl_id.clone()),
            )
        }
        LuaType::Instance(instance) => collect_special_call_param_infos_from_callable_operators(
            db,
            caller_file_id,
            caller_position,
            instance.get_base(),
        ),
        LuaType::TypeGuard(inner) => collect_special_call_param_infos_from_callable_operators(
            db,
            caller_file_id,
            caller_position,
            inner,
        ),
        LuaType::Union(union) => union.into_vec().iter().fold(
            SpecialCallOperatorCollection::default(),
            |mut collection, union_type| {
                collection.extend(collect_special_call_param_infos_from_callable_operators(
                    db,
                    caller_file_id,
                    caller_position,
                    union_type,
                ));
                collection
            },
        ),
        LuaType::Intersection(intersection) => intersection.get_types().iter().fold(
            SpecialCallOperatorCollection::default(),
            |mut collection, intersection_type| {
                collection.extend(collect_special_call_param_infos_from_callable_operators(
                    db,
                    caller_file_id,
                    caller_position,
                    intersection_type,
                ));
                collection
            },
        ),
        LuaType::MultiLineUnion(union) => union.get_unions().iter().fold(
            SpecialCallOperatorCollection::default(),
            |mut collection, (union_type, _)| {
                collection.extend(collect_special_call_param_infos_from_callable_operators(
                    db,
                    caller_file_id,
                    caller_position,
                    union_type,
                ));
                collection
            },
        ),
        _ => SpecialCallOperatorCollection::default(),
    }
}

fn collect_special_call_param_infos_from_operator_owner(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    owner: &LuaOperatorOwner,
) -> SpecialCallOperatorCollection {
    let Some(operator_ids) = db
        .get_operator_index()
        .get_operators(owner, LuaOperatorMetaMethod::Call)
    else {
        return SpecialCallOperatorCollection::default();
    };

    let priority_tiers = get_operator_id_priority_tiers(db, caller_file_id, operator_ids);
    let visible_operator_ids = select_operator_ids_by_workspace_and_realm(
        db,
        caller_file_id,
        caller_position,
        priority_tiers,
    );

    let param_infos = visible_operator_ids
        .iter()
        .flat_map(|operator_id| {
            let Some(operator) = db.get_operator_index().get_operator(operator_id) else {
                return Vec::new();
            };

            match operator.get_operator_func(db) {
                LuaType::Signature(signature_id) => db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(|signature| {
                        adjust_operator_special_call_param_infos(
                            collect_signature_special_call_params(signature, signature_id),
                            should_strip_first_operator_param(signature.is_colon_define, owner),
                        )
                    })
                    .unwrap_or_default(),
                LuaType::DocFunction(func) => adjust_operator_special_call_param_infos(
                    collect_doc_function_special_call_params(func.as_ref()),
                    should_strip_first_operator_param(func.is_colon_define(), owner),
                ),
                _ => Vec::new(),
            }
        })
        .collect();

    SpecialCallOperatorCollection {
        param_infos,
        had_operators: true,
    }
}

#[derive(Debug, Default)]
struct SpecialCallOperatorCollection {
    param_infos: Vec<SpecialCallParamInfo>,
    had_operators: bool,
}

impl SpecialCallOperatorCollection {
    fn extend(&mut self, other: SpecialCallOperatorCollection) {
        self.had_operators |= other.had_operators;
        self.param_infos.extend(other.param_infos);
    }
}

fn get_operator_id_priority_tiers(
    db: &DbIndex,
    caller_file_id: FileId,
    operator_ids: &[crate::LuaOperatorId],
) -> Vec<(u8, Vec<crate::LuaOperatorId>)> {
    let module_index = db.get_module_index();
    let Some(caller_workspace_id) = module_index.get_workspace_id(caller_file_id) else {
        return vec![(0, operator_ids.to_vec())];
    };

    let mut priority_tiers = BTreeMap::new();
    for operator_id in operator_ids {
        let candidate_workspace_id = module_index
            .get_workspace_id(operator_id.file_id)
            .unwrap_or(crate::WorkspaceId::MAIN);
        let Some(priority) =
            module_index.workspace_resolution_priority(caller_workspace_id, candidate_workspace_id)
        else {
            continue;
        };

        priority_tiers
            .entry(priority)
            .or_insert_with(Vec::new)
            .push(*operator_id);
    }

    priority_tiers.into_iter().collect()
}

fn select_operator_ids_by_workspace_and_realm(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    priority_tiers: Vec<(u8, Vec<crate::LuaOperatorId>)>,
) -> Vec<crate::LuaOperatorId> {
    let fallback_operator_ids = priority_tiers
        .first()
        .map(|(_, operator_ids)| operator_ids.clone())
        .unwrap_or_default();

    if !db.get_emmyrc().gmod.enabled {
        return fallback_operator_ids;
    }

    let infer_index = db.get_gmod_infer_index();
    let caller_realm = infer_index.get_realm_at_offset(&caller_file_id, caller_position);
    for (_, tier_operator_ids) in priority_tiers {
        let compatible_operator_ids = tier_operator_ids
            .into_iter()
            .filter(|operator_id| {
                let operator_realm =
                    infer_index.get_realm_at_offset(&operator_id.file_id, operator_id.position);
                is_realm_compatible(caller_realm, operator_realm)
            })
            .collect::<Vec<_>>();
        if !compatible_operator_ids.is_empty() {
            return compatible_operator_ids;
        }
    }

    fallback_operator_ids
}

fn should_strip_first_operator_param(is_colon_define: bool, owner: &LuaOperatorOwner) -> bool {
    matches!(owner, LuaOperatorOwner::Type(_)) && !is_colon_define
}

fn is_realm_compatible(caller_realm: GmodRealm, candidate_realm: GmodRealm) -> bool {
    !matches!(
        (caller_realm, candidate_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

fn adjust_operator_special_call_param_infos(
    param_infos: Vec<SpecialCallParamInfo>,
    strip_first_param: bool,
) -> Vec<SpecialCallParamInfo> {
    if !strip_first_param {
        return param_infos;
    }

    param_infos
        .into_iter()
        .filter_map(|mut param_info| {
            param_info.param_idx = param_info.param_idx.checked_sub(1)?;
            param_info.is_colon_define = false;
            Some(param_info)
        })
        .collect()
}

fn collect_special_call_param_infos_from_semantic_decl(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Result<Vec<SpecialCallParamInfo>, InferFailReason> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let type_cache = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .ok_or(InferFailReason::UnResolveDeclType(decl_id))?;
            Ok(collect_special_call_param_infos(db, type_cache.as_type()))
        }
        LuaSemanticDeclId::Member(member_id) => {
            let type_cache = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .ok_or(InferFailReason::UnResolveMemberType(member_id))?;
            Ok(collect_special_call_param_infos(db, type_cache.as_type()))
        }
        LuaSemanticDeclId::Signature(signature_id) => Ok(db
            .get_signature_index()
            .get(&signature_id)
            .filter(|signature| signature_has_any_special_call_params(signature))
            .map(|signature| collect_signature_special_call_params(signature, signature_id))
            .unwrap_or_default()),
        LuaSemanticDeclId::TypeDecl(_) => Ok(Vec::new()),
    }
}

fn collect_special_call_param_infos(
    db: &DbIndex,
    callable_type: &LuaType,
) -> Vec<SpecialCallParamInfo> {
    match callable_type {
        LuaType::Signature(signature_id) => db
            .get_signature_index()
            .get(signature_id)
            .filter(|signature| signature_has_any_special_call_params(signature))
            .map(|signature| collect_signature_special_call_params(signature, *signature_id))
            .unwrap_or_default(),
        LuaType::DocFunction(func) => collect_doc_function_special_call_params(func),
        LuaType::TypeGuard(inner) => collect_special_call_param_infos(db, inner),
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .flat_map(|union_type| collect_special_call_param_infos(db, union_type))
            .collect(),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .flat_map(|intersection_type| collect_special_call_param_infos(db, intersection_type))
            .collect(),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .flat_map(|(union_type, _)| collect_special_call_param_infos(db, union_type))
            .collect(),
        _ => Vec::new(),
    }
}

fn collect_signature_special_call_params(
    signature: &LuaSignature,
    signature_id: LuaSignatureId,
) -> Vec<SpecialCallParamInfo> {
    let mut param_infos = Vec::new();
    for (idx, param_info) in &signature.param_docs {
        let is_constructor = param_info.get_attribute_by_name("constructor").is_some();
        let has_str_tpl = type_contains_str_tpl_ref(&param_info.type_ref);
        if is_constructor || has_str_tpl {
            param_infos.push(SpecialCallParamInfo {
                param_idx: *idx,
                param_type: param_info.type_ref.clone(),
                is_constructor,
                is_colon_define: signature.is_colon_define,
                signature_id: Some(signature_id),
            });
        }
    }

    param_infos.extend(collect_signature_overload_special_call_params(signature));

    param_infos.sort_by_key(|param_info| param_info.param_idx);
    param_infos
}

fn collect_doc_function_special_call_params(func: &LuaFunctionType) -> Vec<SpecialCallParamInfo> {
    func.get_params()
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, param_type))| {
            let param_type = param_type.as_ref()?;
            if !type_contains_str_tpl_ref(param_type) {
                return None;
            }

            Some(SpecialCallParamInfo {
                param_idx: idx,
                param_type: param_type.clone(),
                is_constructor: false,
                is_colon_define: func.is_colon_define(),
                signature_id: None,
            })
        })
        .collect()
}

fn type_contains_str_tpl_ref(typ: &LuaType) -> bool {
    match typ {
        LuaType::StrTplRef(_) => true,
        LuaType::TypeGuard(inner) => type_contains_str_tpl_ref(inner),
        LuaType::Union(union) => union.into_vec().iter().any(type_contains_str_tpl_ref),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(type_contains_str_tpl_ref),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(union_type, _)| type_contains_str_tpl_ref(union_type)),
        _ => false,
    }
}

fn materialize_str_tpl_class_from_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
    param_idx: usize,
    param_type: &LuaType,
    is_colon_define: bool,
    is_colon_call: bool,
) -> ResolveResult {
    let Some(str_tpl) = find_str_tpl_ref(db, param_type) else {
        return Ok(());
    };

    let constraint = match str_tpl.get_constraint() {
        Some(LuaType::Ref(type_decl_id)) => type_decl_id.clone(),
        _ => return Ok(()),
    };
    let is_class_constraint = db
        .get_type_index()
        .get_type_decl(&constraint)
        .map(|decl| decl.is_class())
        .unwrap_or(false);
    if !is_class_constraint {
        return Ok(());
    }

    let Some(arg_expr) = get_call_arg_expr(call_expr, param_idx, is_colon_define, is_colon_call)
    else {
        return Ok(());
    };
    let Some(arg_name) = infer_string_const_arg(db, cache, &arg_expr) else {
        return Ok(());
    };

    let class_name = format!(
        "{}{}{}",
        str_tpl.get_prefix(),
        arg_name,
        str_tpl.get_suffix()
    );
    let class_decl_id = LuaTypeDeclId::global(&class_name);
    let should_attach_super = match db.get_type_index().get_type_decl(&class_decl_id) {
        Some(existing_decl) => existing_decl.is_auto_generated(),
        None => true,
    };
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                arg_expr.get_range(),
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::AutoGenerated.into(),
                class_decl_id.clone(),
            ),
        );
    }

    if !should_attach_super {
        return Ok(());
    }

    let super_type = LuaType::Ref(constraint);
    let has_super = db
        .get_type_index()
        .get_super_types_iter(&class_decl_id)
        .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
        .unwrap_or(false);
    if !has_super {
        db.get_type_index_mut()
            .add_super_type(class_decl_id, file_id, super_type);
    }

    Ok(())
}

fn try_resolve_constructor_param(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
    param_info: &SpecialCallParamInfo,
) -> ResolveResult {
    let signature_id = param_info.signature_id.ok_or(InferFailReason::None)?;
    let (_, target_signature_name, root_class, strip_self, return_self) = {
        let signature = db
            .get_signature_index()
            .get(&signature_id)
            .ok_or(InferFailReason::None)?;
        let param_doc = signature
            .get_param_info_by_id(param_info.param_idx)
            .ok_or(InferFailReason::None)?;
        let constructor_use = param_doc
            .get_attribute_by_name("constructor")
            .ok_or(InferFailReason::None)?;

        let target_signature_name = constructor_use
            .get_param_by_name("name")
            .and_then(|typ| match typ {
                LuaType::DocStringConst(value) => Some(value.deref().clone()),
                _ => None,
            })
            .ok_or(InferFailReason::None)?;
        let root_class =
            constructor_use
                .get_param_by_name("root_class")
                .and_then(|typ| match typ {
                    LuaType::DocStringConst(value) => Some(value.deref().clone()),
                    _ => None,
                });
        let strip_self = constructor_use
            .get_param_by_name("strip_self")
            .and_then(|typ| match typ {
                LuaType::DocBooleanConst(value) => Some(*value),
                _ => None,
            })
            .unwrap_or(true);
        let return_self = constructor_use
            .get_param_by_name("return_self")
            .and_then(|typ| match typ {
                LuaType::DocBooleanConst(value) => Some(*value),
                _ => None,
            })
            .unwrap_or(true);

        Ok::<_, InferFailReason>((
            param_doc.type_ref.clone(),
            target_signature_name,
            root_class,
            strip_self,
            return_self,
        ))
    }?;

    let target_id = get_constructor_target_type(
        db,
        cache,
        &param_info.param_type,
        call_expr.clone(),
        param_info.param_idx,
        param_info.is_colon_define,
        call_expr.is_colon_call(),
    )
    .ok_or(InferFailReason::None)?;

    if let Some(root_class) = root_class {
        let root_type_id = LuaTypeDeclId::global(&root_class);
        if let Some(type_decl) = db.get_type_index().get_type_decl(&root_type_id)
            && type_decl.is_class()
        {
            let root_type = LuaType::Ref(root_type_id.clone());
            let has_super = db
                .get_type_index()
                .get_super_types_iter(&target_id)
                .map(|mut supers| supers.any(|existing_super| existing_super == &root_type))
                .unwrap_or(false);
            if !has_super {
                db.get_type_index_mut()
                    .add_super_type(target_id.clone(), file_id, root_type);
            }
        }
    }

    let target_type = LuaType::Ref(target_id);
    let member_key = LuaMemberKey::Name(target_signature_name);
    let members = db
        .get_module_index()
        .get_workspace_id(file_id)
        .and_then(|workspace_id| {
            crate::semantic::find_members_with_key_in_workspace_for_file_at_offset(
                db,
                &target_type,
                member_key.clone(),
                true,
                workspace_id,
                file_id,
                call_expr.get_position(),
            )
        })
        .or_else(|| {
            db.get_module_index()
                .get_workspace_id(file_id)
                .is_none()
                .then(|| find_members_with_key(db, &target_type, member_key, true))?
        })
        .ok_or(InferFailReason::FieldNotFound)?;
    let ctor_signature_member = members.first().ok_or(InferFailReason::FieldNotFound)?;

    set_signature_to_default_call(db, cache, ctor_signature_member, strip_self, return_self)
        .ok_or(InferFailReason::FieldNotFound)?;

    Ok(())
}

fn set_signature_to_default_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    member_info: &LuaMemberInfo,
    strip_self: bool,
    return_self: bool,
) -> Option<()> {
    let LuaType::Signature(signature_id) = member_info.typ else {
        return None;
    };
    let Some(LuaSemanticDeclId::Member(member_id)) = member_info.property_owner_id else {
        return None;
    };
    // 我们仍然需要再做一次判断确定是否来源于`Def`类型
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let index_expr = LuaIndexExpr::cast(member_id.get_syntax_id().to_node_from_root(&root)?)?;
    let prefix_expr = index_expr.get_prefix_expr()?;
    let prefix_type = infer_expr(db, cache, prefix_expr.clone()).ok()?;
    let LuaType::Def(decl_id) = prefix_type else {
        return None;
    };
    // 如果已经存在显式的`__call`定义, 则不添加
    let call = db.get_operator_index().get_operators(
        &LuaOperatorOwner::Type(decl_id.clone()),
        LuaOperatorMetaMethod::Call,
    );
    if call.is_some() {
        return None;
    }

    let operator = LuaOperator::new(
        decl_id.into(),
        LuaOperatorMetaMethod::Call,
        member_id.file_id,
        // 必须指向名称, 使用 index_expr 的完整范围不会跳转到函数上
        index_expr.get_name_token()?.syntax().text_range(),
        OperatorFunction::DefaultClassCtor {
            id: signature_id,
            strip_self,
            return_self,
        },
    );
    db.get_operator_index_mut().add_operator(operator);
    Some(())
}

fn get_constructor_target_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    param_type: &LuaType,
    call_expr: LuaCallExpr,
    call_index: usize,
    is_colon_define: bool,
    is_colon_call: bool,
) -> Option<LuaTypeDeclId> {
    if let Some(str_tpl) = find_str_tpl_ref(db, param_type) {
        let arg_expr = get_call_arg_expr(&call_expr, call_index, is_colon_define, is_colon_call)?;
        let name = infer_string_const_arg(db, cache, &arg_expr)?;
        let type_decl_id: LuaTypeDeclId = LuaTypeDeclId::global(
            format!("{}{}{}", str_tpl.get_prefix(), name, str_tpl.get_suffix()).as_str(),
        );
        let type_decl = db.get_type_index().get_type_decl(&type_decl_id)?;
        if type_decl.is_class() {
            return Some(type_decl_id);
        }
    }

    None
}

fn find_str_tpl_ref(db: &DbIndex, typ: &LuaType) -> Option<Arc<crate::LuaStringTplType>> {
    match typ {
        LuaType::StrTplRef(str_tpl) => Some(str_tpl.clone()),
        LuaType::TypeGuard(inner) => find_str_tpl_ref(db, inner),
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .filter_map(|union_type| find_str_tpl_ref(db, union_type))
            .min_by_key(|str_tpl| str_tpl_selection_key(db, str_tpl)),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .filter_map(|intersection_type| find_str_tpl_ref(db, intersection_type))
            .min_by_key(|str_tpl| str_tpl_selection_key(db, str_tpl)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .filter_map(|(union_type, _)| find_str_tpl_ref(db, union_type))
            .min_by_key(|str_tpl| str_tpl_selection_key(db, str_tpl)),
        _ => None,
    }
}

fn str_tpl_selection_key(db: &DbIndex, str_tpl: &crate::LuaStringTplType) -> String {
    let constraint_key = str_tpl
        .get_constraint()
        .map(|constraint| humanize_type(db, constraint, RenderLevel::Detailed))
        .unwrap_or_default();
    format!(
        "{}|{}|{}|{}",
        str_tpl.get_prefix(),
        str_tpl.get_name(),
        str_tpl.get_suffix(),
        constraint_key
    )
}

fn get_call_arg_expr(
    call_expr: &LuaCallExpr,
    param_idx: usize,
    is_colon_define: bool,
    is_colon_call: bool,
) -> Option<LuaExpr> {
    let arg_idx = match (is_colon_define, is_colon_call) {
        (true, false) => param_idx.checked_add(1)?,
        (false, true) => param_idx.checked_sub(1)?,
        _ => param_idx,
    };
    call_expr.get_args_list()?.get_args().nth(arg_idx)
}

fn infer_string_const_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: &LuaExpr,
) -> Option<String> {
    match infer_expr(db, cache, arg_expr.clone()).ok()? {
        LuaType::StringConst(s) => Some(s.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use rowan::{TextRange, TextSize};

    use super::{
        find_str_tpl_ref, get_operator_id_priority_tiers,
        select_operator_ids_by_workspace_and_realm,
    };
    use crate::{
        DbIndex, FileId, GenericTplId, GmodRealm, GmodRealmFileMetadata, InFiled, LuaOperator,
        LuaOperatorMetaMethod, LuaOperatorOwner, LuaType, LuaTypeDeclId, WorkspaceId,
        db_index::{
            AsyncState, LuaFunctionType, LuaStringTplType, LuaUnionType, OperatorFunction,
            WorkspaceKind,
        },
    };

    fn make_db() -> DbIndex {
        let mut db = DbIndex::new();
        db.get_module_index_mut()
            .set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        db
    }

    fn add_call_operator(
        db: &mut DbIndex,
        owner: &LuaOperatorOwner,
        file_id: FileId,
        start: u32,
    ) -> crate::LuaOperatorId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        let operator = LuaOperator::new(
            owner.clone(),
            LuaOperatorMetaMethod::Call,
            file_id,
            range,
            OperatorFunction::Func(std::sync::Arc::new(LuaFunctionType::new(
                AsyncState::None,
                false,
                false,
                vec![("arg".to_string(), Some(LuaType::String))],
                LuaType::Boolean,
            ))),
        );
        let id = operator.get_id();
        db.get_operator_index_mut().add_operator(operator);
        id
    }

    fn set_file_realms(db: &mut DbIndex, file_realms: &[(FileId, GmodRealm)]) {
        db.get_gmod_infer_index_mut().set_all_realm_file_metadata(
            file_realms
                .iter()
                .map(|(file_id, realm)| {
                    (
                        *file_id,
                        GmodRealmFileMetadata {
                            inferred_realm: *realm,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        );
    }

    #[test]
    fn operator_id_priority_tiers_keep_workspace_priority_order() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/.lua/std").into(),
            WorkspaceId::STD,
            WorkspaceKind::Std,
        );

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let library_file = FileId::new(2);
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );

        let std_file = FileId::new(3);
        module_index.add_module_by_path(std_file, "C:/Users/username/.lua/std/math.lua");

        let other_main_file = FileId::new(4);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let owner = LuaOperatorOwner::Type(LuaTypeDeclId::global("Callable"));
        let library_operator = add_call_operator(&mut db, &owner, library_file, 1);
        let std_operator = add_call_operator(&mut db, &owner, std_file, 2);
        let _other_main_operator = add_call_operator(&mut db, &owner, other_main_file, 3);

        let tiers = get_operator_id_priority_tiers(
            &db,
            caller_file,
            &[library_operator, std_operator, _other_main_operator],
        );

        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0], (1, vec![library_operator]));
        assert_eq!(tiers[1], (2, vec![std_operator]));
    }

    #[test]
    fn select_operator_ids_by_workspace_and_realm_uses_first_compatible_tier() {
        let mut db = make_db();
        let caller_file = FileId::new(1);
        let owner = LuaOperatorOwner::Table(InFiled::new(
            FileId::new(99),
            TextRange::new(TextSize::new(0), TextSize::new(1)),
        ));
        let tier_one_operator = add_call_operator(&mut db, &owner, FileId::new(10), 1);
        let tier_two_operator = add_call_operator(&mut db, &owner, FileId::new(11), 2);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (tier_one_operator.file_id, GmodRealm::Shared),
                (tier_two_operator.file_id, GmodRealm::Server),
            ],
        );

        let selected = select_operator_ids_by_workspace_and_realm(
            &db,
            caller_file,
            TextSize::new(0),
            vec![(0, vec![tier_one_operator]), (1, vec![tier_two_operator])],
        );

        assert_eq!(selected, vec![tier_one_operator]);
    }

    #[test]
    fn select_operator_ids_by_workspace_and_realm_falls_back_to_best_tier_when_needed() {
        let mut db = make_db();
        let caller_file = FileId::new(1);
        let owner = LuaOperatorOwner::Table(InFiled::new(
            FileId::new(99),
            TextRange::new(TextSize::new(0), TextSize::new(1)),
        ));
        let best_tier_operator = add_call_operator(&mut db, &owner, FileId::new(20), 1);
        let lower_tier_operator = add_call_operator(&mut db, &owner, FileId::new(21), 2);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (best_tier_operator.file_id, GmodRealm::Server),
                (lower_tier_operator.file_id, GmodRealm::Server),
            ],
        );

        let selected = select_operator_ids_by_workspace_and_realm(
            &db,
            caller_file,
            TextSize::new(0),
            vec![
                (0, vec![best_tier_operator]),
                (1, vec![lower_tier_operator]),
            ],
        );

        assert_eq!(selected, vec![best_tier_operator]);
    }

    #[test]
    fn find_str_tpl_ref_union_order_is_deterministic() {
        let alpha_tpl = LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
            "alpha.",
            "T",
            GenericTplId::Func(0),
            "",
            Some(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        )));
        let beta_tpl = LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
            "beta.",
            "T",
            GenericTplId::Func(0),
            "",
            Some(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        )));

        let alpha_first = LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
            alpha_tpl.clone(),
            beta_tpl.clone(),
        ])));
        let beta_first =
            LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![beta_tpl, alpha_tpl])));

        let db = make_db();
        let alpha_first_selected = find_str_tpl_ref(&db, &alpha_first)
            .expect("expected string template in alpha-first union");
        let beta_first_selected = find_str_tpl_ref(&db, &beta_first)
            .expect("expected string template in beta-first union");

        assert_eq!(
            alpha_first_selected.get_prefix(),
            beta_first_selected.get_prefix(),
            "string template selection should be independent of union member order"
        );
    }
}
