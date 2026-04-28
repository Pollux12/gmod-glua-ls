use glua_parser::{LuaAstNode, LuaAstToken, LuaExpr, LuaForRangeStat};

use crate::{
    DbIndex, InferFailReason, LuaDeclId, LuaInferCache, LuaMemberKey, LuaOperatorMetaMethod,
    LuaType, LuaTypeCache, TplContext, TypeOps, TypeSubstitutor, VariadicType,
    compilation::analyzer::unresolve::UnResolveIterVar, get_member_map, infer_expr,
    instantiate_doc_function, tpl_pattern_match_args,
};

use super::LuaAnalyzer;

pub fn analyze_for_range_stat(
    analyzer: &mut LuaAnalyzer,
    for_range_stat: LuaForRangeStat,
) -> Option<()> {
    let var_name_list = for_range_stat.get_var_name_list();
    let iter_exprs = for_range_stat.get_expr_list().collect::<Vec<_>>();
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let iter_var_types = infer_for_range_iter_expr_func(analyzer.db, cache, &iter_exprs);

    match iter_var_types {
        Ok(iter_var_types) => {
            for (idx, var_name) in var_name_list.enumerate() {
                let position = var_name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                let ret_type = iter_var_types
                    .get_type(idx)
                    .cloned()
                    .unwrap_or(LuaType::Unknown);
                let ret_type = TypeOps::Remove.apply(analyzer.db, &ret_type, &LuaType::Nil);
                analyzer
                    .db
                    .get_type_index_mut()
                    .bind_type(decl_id.into(), LuaTypeCache::InferType(ret_type));
            }
        }
        Err(InferFailReason::None) => {
            for var_name in var_name_list {
                let position = var_name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                analyzer
                    .db
                    .get_type_index_mut()
                    .bind_type(decl_id.into(), LuaTypeCache::InferType(LuaType::Unknown));
            }
        }
        Err(reason) => {
            let unresolved = UnResolveIterVar {
                file_id: analyzer.file_id,
                iter_exprs: iter_exprs.clone(),
                iter_vars: var_name_list.collect::<Vec<_>>(),
            };

            analyzer
                .context
                .add_unresolve(unresolved.into(), reason.clone());
        }
    }

    Some(())
}

pub fn infer_for_range_iter_expr_func(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    iter_exprs: &[LuaExpr],
) -> Result<VariadicType, InferFailReason> {
    if iter_exprs.is_empty() {
        return Err(InferFailReason::None);
    }

    let mut status_param = None;
    if iter_exprs.len() > 1 {
        let status_param_expr = iter_exprs[1].clone();
        status_param = Some(infer_expr(db, cache, status_param_expr)?);
    }

    let iter_func_expr = iter_exprs[0].clone();
    let first_expr_type = infer_expr(db, cache, iter_func_expr)?;
    if let Some(iter_types) =
        try_infer_pairs_iter_types_from_table_members(db, cache, &iter_exprs[0], &first_expr_type)?
    {
        return Ok(iter_types);
    }

    let doc_function = match first_expr_type {
        LuaType::DocFunction(func) => func,
        LuaType::Signature(sig_id) => {
            let sig = db
                .get_signature_index()
                .get(&sig_id)
                .ok_or(InferFailReason::None)?;
            if !sig.is_resolve_return() {
                return Err(InferFailReason::UnResolveSignatureReturn(sig_id));
            }
            sig.to_doc_func_type()
        }
        LuaType::Ref(type_decl_id) => {
            let type_decl = db
                .get_type_index()
                .get_type_decl(&type_decl_id)
                .ok_or(InferFailReason::None)?;
            if type_decl.is_alias() {
                let alias_origin = type_decl
                    .get_alias_origin(db, None)
                    .ok_or(InferFailReason::None)?;
                match alias_origin {
                    LuaType::DocFunction(doc_func) => doc_func,
                    _ => return Err(InferFailReason::None),
                }
            } else if type_decl.is_class() {
                let operator_index = db.get_operator_index();
                let operator_ids = operator_index
                    .get_operators(&type_decl_id.into(), LuaOperatorMetaMethod::Call)
                    .ok_or(InferFailReason::None)?;
                operator_ids
                    .iter()
                    .filter_map(|overload_id| {
                        let operator = operator_index.get_operator(overload_id)?;
                        let func = operator.get_operator_func(db);
                        match func {
                            LuaType::DocFunction(f) => Some(f.clone()),
                            _ => None,
                        }
                    })
                    .next()
                    .ok_or(InferFailReason::None)?
            } else {
                return Err(InferFailReason::None);
            }
        }
        LuaType::Variadic(multi) => {
            let first_type = multi.get_type(0).cloned().unwrap_or(LuaType::Unknown);
            let second_type = multi.get_type(1).cloned().unwrap_or(LuaType::Unknown);
            if !second_type.is_unknown() {
                status_param = Some(second_type);
            }

            match first_type {
                LuaType::DocFunction(func) => func,
                _ => return Err(InferFailReason::None),
            }
        }
        _ => return Err(InferFailReason::None),
    };

    let Some(status_param) = status_param else {
        return Ok(doc_function.get_variadic_ret());
    };
    let mut substitutor = TypeSubstitutor::new();
    let mut context = TplContext {
        db,
        cache,
        substitutor: &mut substitutor,
        call_expr: None,
    };
    let params = doc_function
        .get_params()
        .iter()
        .map(|(_, opt_ty)| opt_ty.clone().unwrap_or(LuaType::Any))
        .collect::<Vec<_>>();

    tpl_pattern_match_args(&mut context, &params, &[status_param])?;

    let instantiate_func = if let LuaType::DocFunction(f) =
        instantiate_doc_function(db, &doc_function, &substitutor)
    {
        f
    } else {
        doc_function
    };

    Ok(instantiate_func.get_variadic_ret())
}

fn try_infer_pairs_iter_types_from_table_members(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    iter_expr: &LuaExpr,
    first_expr_type: &LuaType,
) -> Result<Option<VariadicType>, InferFailReason> {
    if !matches!(first_expr_type, LuaType::Variadic(_)) {
        return Ok(None);
    }

    let LuaExpr::CallExpr(call_expr) = iter_expr else {
        return Ok(None);
    };
    if !is_global_pairs_call(db, cache, call_expr) {
        return Ok(None);
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return Ok(None);
    };
    let mut args = args_list.get_args();
    let Some(table_arg) = args.next() else {
        return Ok(None);
    };
    if args.next().is_some() {
        return Ok(None);
    }

    let table_type = infer_expr(db, cache, table_arg)?;
    let Some(members) = get_member_map(db, &table_type) else {
        return Ok(None);
    };
    if members.keys().any(is_pairs_metamethod_key) {
        return Ok(None);
    }

    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut member_entries = members.into_iter().collect::<Vec<_>>();
    member_entries.sort_by_key(|(key, _)| member_key_stable_key(key));
    for (key, member_infos) in member_entries {
        let key_type = match key {
            LuaMemberKey::Integer(i) => LuaType::IntegerConst(i),
            LuaMemberKey::Name(name) => LuaType::StringConst(name.into()),
            LuaMemberKey::ExprType(typ) => typ,
            LuaMemberKey::None => continue,
        };
        keys.push(key_type);

        let value_type = match member_infos.as_slice() {
            [] => LuaType::Any,
            [member] => member.typ.clone(),
            _ => LuaType::from_vec(
                member_infos
                    .into_iter()
                    .map(|member| member.typ)
                    .collect::<Vec<_>>(),
            ),
        };
        values.push(value_type);
    }

    if keys.is_empty() || values.is_empty() {
        return Ok(None);
    }

    Ok(Some(VariadicType::Multi(vec![
        LuaType::from_vec(keys),
        LuaType::from_vec(values),
    ])))
}

fn is_pairs_metamethod_key(key: &LuaMemberKey) -> bool {
    matches!(key, LuaMemberKey::Name(name) if name.as_str() == "__pairs")
}

fn member_key_stable_key(key: &LuaMemberKey) -> (u8, String) {
    match key {
        LuaMemberKey::None => (0, String::new()),
        LuaMemberKey::Integer(i) => (1, i.to_string()),
        LuaMemberKey::Name(name) => (2, name.to_string()),
        LuaMemberKey::ExprType(typ) => (3, format!("{typ:?}")),
    }
}

fn is_global_pairs_call(
    db: &DbIndex,
    cache: &LuaInferCache,
    call_expr: &glua_parser::LuaCallExpr,
) -> bool {
    let Some(LuaExpr::NameExpr(name_expr)) = call_expr.get_prefix_expr() else {
        return false;
    };
    if name_expr.get_name_text().as_deref() != Some("pairs") {
        return false;
    }

    db.get_reference_index()
        .get_local_reference(&cache.get_file_id())
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .is_none()
}
