use std::{ops::Deref, sync::Arc};

use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr, LuaIndexExpr};

use crate::{
    DbIndex, DocTypeInferContext, GenericTplId, LuaFunctionType, LuaSemanticDeclId, LuaType,
    LuaUnionType, SemanticDeclLevel, SemanticModel, TypeOps, TypeSubstitutor, VariadicType,
    infer_doc_type,
};

// Generic constraint context
pub struct CallConstraintContext {
    pub params: Vec<(String, Option<LuaType>)>,
    pub arg_infos: Vec<LuaType>,
    pub substitutor: TypeSubstitutor,
}

pub fn build_call_constraint_context(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<(CallConstraintContext, Arc<LuaFunctionType>)> {
    let doc_func = infer_call_doc_function(semantic_model, call_expr)?;
    let mut params = doc_func.get_params().to_vec();
    let mut arg_infos = get_arg_infos(semantic_model, call_expr)?;

    let mut substitutor = TypeSubstitutor::new();

    // Read the explicitly passed generic arguments
    if let Some(type_list) = call_expr.get_call_generic_type_list() {
        let doc_ctx =
            DocTypeInferContext::new(semantic_model.get_db(), semantic_model.get_file_id());
        for (idx, doc_type) in type_list.get_types().enumerate() {
            let ty = infer_doc_type(doc_ctx, &doc_type);
            substitutor.insert_type(GenericTplId::Func(idx as u32), ty, true);
        }
    }

    // Handle the difference between colon calls and function definitions on the self parameter
    match (call_expr.is_colon_call(), doc_func.is_colon_define()) {
        (true, true) | (false, false) => {}
        (false, true) => {
            params.insert(0, ("self".into(), Some(LuaType::SelfInfer)));
        }
        (true, false) => {
            arg_infos.insert(0, infer_call_source_type(semantic_model, call_expr)?);
        }
    }

    collect_generic_assignments(&mut substitutor, &params, &arg_infos);

    Some((
        CallConstraintContext {
            params,
            arg_infos,
            substitutor,
        },
        doc_func,
    ))
}

// Convert the inference result into a form that is easier to compare
pub fn normalize_constraint_type(db: &DbIndex, ty: LuaType) -> LuaType {
    match ty {
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => tuple.cast_down_array_base(db),
        // Shaped sequential literals infer as TableConst; normalize to their
        // array element base for generic argument matching, matching the prior
        // tuple-based behavior.
        LuaType::TableConst(ref range) => crate::table_const_array_base(db, range).unwrap_or(ty),
        _ => ty,
    }
}

// Collect the generic inference for each parameter
fn collect_generic_assignments(
    substitutor: &mut TypeSubstitutor,
    params: &[(String, Option<LuaType>)],
    arg_infos: &[LuaType],
) {
    for (idx, (_, param_type)) in params.iter().enumerate() {
        let Some(param_type) = param_type else {
            continue;
        };
        let Some(arg_type) = arg_infos.get(idx) else {
            continue;
        };
        record_generic_assignment(param_type, arg_type, substitutor);
    }
}

// Actually write into the generic substitution table
fn record_generic_assignment(
    param_type: &LuaType,
    arg_type: &LuaType,
    substitutor: &mut TypeSubstitutor,
) {
    match param_type {
        LuaType::TplRef(tpl_ref) => {
            substitutor.insert_type(tpl_ref.get_tpl_id(), arg_type.clone(), true);
        }
        LuaType::ConstTplRef(tpl_ref) => {
            substitutor.insert_type(tpl_ref.get_tpl_id(), arg_type.clone(), false);
        }
        LuaType::StrTplRef(str_tpl_ref) => {
            substitutor.insert_type(str_tpl_ref.get_tpl_id(), arg_type.clone(), true);
        }
        LuaType::Variadic(variadic) => {
            if let Some(inner) = variadic.get_type(0) {
                record_generic_assignment(inner, arg_type, substitutor);
            }
        }
        _ => {}
    }
}

// Resolve the concrete type of the caller for colon calls
fn infer_call_source_type(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<LuaType> {
    match call_expr.get_prefix_expr()? {
        LuaExpr::IndexExpr(index_expr) => {
            let decl = semantic_model.find_decl(
                index_expr.syntax().clone().into(),
                SemanticDeclLevel::default(),
            )?;

            if let LuaSemanticDeclId::Member(member_id) = decl
                && let Some(LuaSemanticDeclId::Member(member_id)) =
                    semantic_model.get_member_origin_owner(member_id)
            {
                let cur_node = semantic_model
                    .get_db()
                    .get_vfs()
                    .syntax_node_around(&member_id.file_id, *member_id.get_syntax_id())?;
                let index_expr = LuaIndexExpr::cast(cur_node)?;

                return index_expr.get_prefix_expr().map(|prefix_expr| {
                    semantic_model
                        .infer_expr(prefix_expr.clone())
                        .unwrap_or(LuaType::SelfInfer)
                });
            }

            return if let Some(prefix_expr) = index_expr.get_prefix_expr() {
                let expr_type = semantic_model
                    .infer_expr(prefix_expr.clone())
                    .unwrap_or(LuaType::SelfInfer);
                Some(expr_type)
            } else {
                None
            };
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl = semantic_model.find_decl(
                name_expr.syntax().clone().into(),
                SemanticDeclLevel::default(),
            )?;
            if let LuaSemanticDeclId::Member(member_id) = decl {
                let cur_node = semantic_model
                    .get_db()
                    .get_vfs()
                    .syntax_node_around(&member_id.file_id, *member_id.get_syntax_id())?;
                let index_expr = LuaIndexExpr::cast(cur_node)?;

                return index_expr.get_prefix_expr().map(|prefix_expr| {
                    semantic_model
                        .infer_expr(prefix_expr.clone())
                        .unwrap_or(LuaType::SelfInfer)
                });
            }

            return None;
        }
        _ => {}
    }

    None
}

// Infer the type of each argument
fn get_arg_infos(semantic_model: &SemanticModel, call_expr: &LuaCallExpr) -> Option<Vec<LuaType>> {
    let arg_exprs = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    let mut arg_infos = infer_expr_list_types(semantic_model, &arg_exprs);
    for (arg_type, _) in arg_infos.iter_mut() {
        let extend_type = get_constraint_type(semantic_model, arg_type, 0);
        if let Some(extend_type) = extend_type {
            *arg_type = extend_type;
        }
    }

    let arg_infos = arg_infos
        .into_iter()
        .map(|(arg_type, _)| arg_type)
        .collect();

    Some(arg_infos)
}

fn infer_call_doc_function(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<Arc<LuaFunctionType>> {
    let prefix_expr = call_expr.get_prefix_expr()?.clone();
    let function = semantic_model.infer_expr(prefix_expr).ok()?;
    match function {
        LuaType::Signature(signature_id) => {
            let signature = semantic_model
                .get_db()
                .get_signature_index()
                .get(&signature_id)?;
            for overload in &signature.overloads {
                if overload_matches_call(semantic_model, call_expr, overload) {
                    return Some(overload.clone());
                }
            }
            Some(signature.to_doc_func_type())
        }
        LuaType::DocFunction(func) => Some(func),
        _ => None,
    }
}

fn overload_matches_call(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    overload: &LuaFunctionType,
) -> bool {
    let mut params = overload.get_params().to_vec();
    let Some(mut arg_infos) = get_arg_infos(semantic_model, call_expr) else {
        return false;
    };

    match (call_expr.is_colon_call(), overload.is_colon_define()) {
        (true, true) | (false, false) => {}
        (false, true) => {
            params.insert(0, ("self".into(), Some(LuaType::SelfInfer)));
        }
        (true, false) => {
            let Some(source_type) = infer_call_source_type(semantic_model, call_expr) else {
                return false;
            };
            arg_infos.insert(0, source_type);
        }
    }

    let variadic_index = params.iter().position(|(name, _)| name == "...");
    let fixed_param_count = variadic_index.unwrap_or(params.len());
    let required_param_count = params
        .iter()
        .take(fixed_param_count)
        .enumerate()
        .filter(|(idx, _)| !overload.is_param_optional(*idx))
        .count();
    if arg_infos.len() < required_param_count {
        return false;
    }
    if variadic_index.is_none() && arg_infos.len() > fixed_param_count {
        return false;
    }

    for (idx, (_, param_type)) in params.iter().take(fixed_param_count).enumerate() {
        let Some(arg_type) = arg_infos.get(idx) else {
            break;
        };
        let Some(param_type) = param_type else {
            continue;
        };
        if matches!(
            (param_type, arg_type),
            (
                LuaType::Any | LuaType::Unknown | LuaType::Never | LuaType::SelfInfer,
                _
            ) | (
                _,
                LuaType::Any | LuaType::Unknown | LuaType::Never | LuaType::SelfInfer
            )
        ) {
            continue;
        }
        if semantic_model
            .type_check_detail(param_type, arg_type)
            .is_err()
        {
            return false;
        }
    }

    true
}

// Get the constraint type
fn get_constraint_type(
    semantic_model: &SemanticModel,
    arg_type: &LuaType,
    depth: usize,
) -> Option<LuaType> {
    match arg_type {
        LuaType::TplRef(tpl_ref) | LuaType::ConstTplRef(tpl_ref) => {
            tpl_ref.get_constraint().cloned()
        }
        LuaType::StrTplRef(_) => None,
        LuaType::Union(union_type) => {
            if depth > 1 {
                return None;
            }
            get_union_constraint_type(semantic_model, union_type, depth)
        }
        _ => None,
    }
}

fn get_union_constraint_type(
    semantic_model: &SemanticModel,
    union_type: &LuaUnionType,
    depth: usize,
) -> Option<LuaType> {
    match union_type.nullable_inner() {
        Some(typ) => {
            let constraint_type = get_constraint_type(semantic_model, typ, depth + 1)?;
            Some(TypeOps::Union.apply(semantic_model.get_db(), &constraint_type, &LuaType::Nil))
        }
        None => {
            let mut constraint_types = None;
            for (idx, typ) in union_type.types().enumerate() {
                if let Some(constraint_type) = get_constraint_type(semantic_model, typ, depth + 1) {
                    let constraint_types = constraint_types.get_or_insert_with(|| {
                        let mut mapped = Vec::with_capacity(union_type.len());
                        mapped.extend(union_type.types().take(idx).cloned());
                        mapped
                    });
                    constraint_types.push(constraint_type);
                } else if let Some(constraint_types) = &mut constraint_types {
                    constraint_types.push(typ.clone());
                }
            }

            rebuild_constraint_union(semantic_model, constraint_types?)
        }
    }
}

fn rebuild_constraint_union(
    semantic_model: &SemanticModel,
    constraint_types: Vec<LuaType>,
) -> Option<LuaType> {
    Some(
        constraint_types
            .into_iter()
            .reduce(|left, right| TypeOps::Union.apply(semantic_model.get_db(), &left, &right))
            // Nothing to union: pin the escape value instead of the seed.
            .unwrap_or(LuaType::Unknown),
    )
}

// Infer multiple expressions into a list of concrete types
fn infer_expr_list_types(
    semantic_model: &SemanticModel,
    exprs: &[LuaExpr],
) -> Vec<(LuaType, LuaExpr)> {
    let mut value_types = Vec::new();
    for (idx, expr) in exprs.iter().enumerate() {
        let expr_type = semantic_model
            .infer_expr(expr.clone())
            .unwrap_or(LuaType::Unknown);
        match expr_type {
            LuaType::Variadic(variadic) => match variadic.deref() {
                // The last expression forwards its whole pack; earlier
                // positions keep only the first value.
                VariadicType::Base(base) if idx + 1 != exprs.len() => {
                    value_types.push((base.clone(), expr.clone()));
                }
                VariadicType::Base(_) => {
                    value_types.push((LuaType::Variadic(variadic.clone()), expr.clone()));
                }
                VariadicType::Multi(vecs) => {
                    for typ in vecs {
                        value_types.push((typ.clone(), expr.clone()));
                    }
                }
            },
            _ => value_types.push((expr_type.clone(), expr.clone())),
        }
    }
    value_types
}
