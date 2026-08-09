use glua_parser::{LuaAstNode, LuaAstToken, LuaExpr, LuaForRangeStat};
use std::collections::{HashMap, HashSet};

use crate::{
    DbIndex, InferFailReason, LuaAliasCallKind, LuaAliasCallType, LuaDeclId, LuaInferCache,
    LuaMemberKey, LuaMemberOwner, LuaObjectType, LuaOperatorMetaMethod, LuaType, LuaTypeCache,
    TplContext, TypeOps, TypeSubstitutor, VariadicType,
    compilation::analyzer::{
        common::{TypeCacheWriteMode, write_type_cache},
        unresolve::UnResolveIterVar,
    },
    get_member_map, infer_expr, instantiate_doc_function, tpl_pattern_match_args,
};

use super::LuaAnalyzer;

pub fn analyze_for_range_stat(
    analyzer: &mut LuaAnalyzer,
    for_range_stat: LuaForRangeStat,
) -> Option<()> {
    let var_name_list = for_range_stat.get_var_name_list().collect::<Vec<_>>();
    let iter_exprs = for_range_stat.get_expr_list().collect::<Vec<_>>();
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let iter_var_types = infer_for_range_iter_expr_func(analyzer.db, cache, &iter_exprs);

    match iter_var_types {
        Ok(iter_var_types) => {
            for (idx, var_name) in var_name_list.iter().enumerate() {
                let position = var_name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                let ret_type = iter_var_types
                    .get_type(idx)
                    .cloned()
                    .unwrap_or(LuaType::Unknown);
                let ret_type = TypeOps::Remove.apply(analyzer.db, &ret_type, &LuaType::Nil);
                write_type_cache(
                    analyzer.db,
                    decl_id.into(),
                    LuaTypeCache::InferType(ret_type),
                    TypeCacheWriteMode::InsertOnly,
                );
            }

            if iter_var_types.contain_tpl() {
                // Nothing bound the generic, so the vars hold raw template refs: the
                // table's members were not indexed when this ran, and which of them
                // were is order-dependent. Keep the placeholder so dependants still
                // see a type at the usual time, and queue a retry that replaces it
                // once the member map is populated.
                let unresolved = UnResolveIterVar {
                    file_id: analyzer.file_id,
                    iter_exprs: iter_exprs.clone(),
                    iter_vars: var_name_list,
                };
                analyzer
                    .context
                    .add_unresolve(unresolved.into(), InferFailReason::UnResolveIterTemplate);
            }
        }
        Err(InferFailReason::None) => {
            for var_name in var_name_list {
                let position = var_name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                write_type_cache(
                    analyzer.db,
                    decl_id.into(),
                    LuaTypeCache::InferType(LuaType::Unknown),
                    TypeCacheWriteMode::InsertOnly,
                );
            }
        }
        Err(reason) => {
            let unresolved = UnResolveIterVar {
                file_id: analyzer.file_id,
                iter_exprs: iter_exprs.clone(),
                iter_vars: var_name_list,
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
        source_range: iter_exprs[0].get_range(),
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
    if let LuaType::TableOf(inner) = &table_type {
        // Keep the value as T[K] instead of materializing every member type. Large
        // scripted-class hierarchies can contain hundreds of callable members.
        let key_type = infer_table_projection_key_type(db, inner);
        let value_type = LuaType::Call(
            LuaAliasCallType::new(
                LuaAliasCallKind::Index,
                vec![inner.as_ref().clone(), key_type.clone()],
            )
            .into(),
        );
        return Ok(Some(VariadicType::Multi(vec![key_type, value_type])));
    }
    let Some(members) = get_member_map(db, &table_type) else {
        return Ok(None);
    };
    if members.keys().any(is_pairs_metamethod_key) {
        return Ok(None);
    }
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut member_entries = members
        .iter()
        .map(|(key, member_infos)| (key.clone(), member_infos.clone()))
        .collect::<Vec<_>>();
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
            _ => LuaType::from_inferred_vec(
                member_infos.into_iter().map(|member| member.typ).collect(),
            ),
        };
        values.push(value_type);
    }

    if keys.is_empty() || values.is_empty() {
        return Ok(None);
    }

    let iter_types = VariadicType::Multi(vec![
        compact_pairs_key_type(&keys),
        compact_pairs_value_type(db, values),
    ]);

    Ok(Some(iter_types))
}

fn infer_table_projection_key_type(db: &DbIndex, inner: &LuaType) -> LuaType {
    let mut pending = vec![inner.clone()];
    let mut visited_types = HashSet::new();
    let mut key_type = None;

    while let Some(typ) = pending.pop() {
        match typ {
            LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                if !visited_types.insert(type_id.clone()) {
                    continue;
                }

                let owner = LuaMemberOwner::Type(type_id.clone());
                // Iterate direct indexed keys and visit each supertype once;
                // flattened member discovery duplicates diamond inheritance paths.
                for key in db.get_member_index().get_member_keys(&owner) {
                    let Some(typ) = table_projection_member_key_type(key) else {
                        continue;
                    };
                    union_optional_type(db, &mut key_type, typ);
                }

                if let Some(super_types) = db.get_type_index().get_super_types_iter(&type_id) {
                    pending.extend(super_types.cloned());
                }
            }
            LuaType::Instance(_) => {
                let symbolic_key =
                    LuaType::Call(LuaAliasCallType::new(LuaAliasCallKind::KeyOf, vec![typ]).into());
                union_optional_type(db, &mut key_type, symbolic_key);
            }
            LuaType::Union(union) => pending.extend(union.types().cloned()),
            LuaType::MultiLineUnion(union) => pending.push(union.to_union()),
            LuaType::TableOf(inner) => pending.push(*inner),
            LuaType::TypeGuard(inner) => pending.push(inner.as_ref().clone()),
            typ => {
                let symbolic_key =
                    LuaType::Call(LuaAliasCallType::new(LuaAliasCallKind::KeyOf, vec![typ]).into());
                union_optional_type(db, &mut key_type, symbolic_key);
            }
        }
    }

    key_type.unwrap_or_else(|| {
        LuaType::Call(LuaAliasCallType::new(LuaAliasCallKind::KeyOf, vec![inner.clone()]).into())
    })
}

fn table_projection_member_key_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::Name(_) => Some(LuaType::String),
        LuaMemberKey::Integer(_) => Some(LuaType::Integer),
        LuaMemberKey::ExprType(typ) => Some(match typ {
            LuaType::StringConst(_) | LuaType::DocStringConst(_) => LuaType::String,
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => LuaType::Integer,
            LuaType::FloatConst(_) => LuaType::Number,
            typ => typ.clone(),
        }),
        LuaMemberKey::None => None,
    }
}

fn union_optional_type(db: &DbIndex, target: &mut Option<LuaType>, typ: LuaType) {
    *target = Some(match target.take() {
        Some(current) => TypeOps::Union.apply(db, &current, &typ),
        None => typ,
    });
}

const LARGE_EXACT_PAIRS_KEY_UNION_THRESHOLD: usize = 128;

fn compact_pairs_key_type(keys: &[LuaType]) -> LuaType {
    if keys.len() > LARGE_EXACT_PAIRS_KEY_UNION_THRESHOLD
        && keys
            .iter()
            .all(|key| matches!(key, LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)))
    {
        LuaType::Integer
    } else {
        LuaType::from_inferred_vec(keys.to_vec())
    }
}

fn compact_pairs_value_type(db: &DbIndex, values: Vec<LuaType>) -> LuaType {
    let values = values
        .into_iter()
        .map(|value| remove_pairs_yield_nil(db, &value))
        .filter(|value| !value.is_unknown() && !value.is_never())
        .collect::<Vec<_>>();
    if values.is_empty() {
        // All observed values were nil-only or otherwise uninformative; avoid collapsing to Nil.
        return LuaType::Unknown;
    }

    try_compact_record_values(db, &values).unwrap_or_else(|| LuaType::from_inferred_vec(values))
}

fn remove_pairs_yield_nil(db: &DbIndex, value_type: &LuaType) -> LuaType {
    let non_nil = TypeOps::Remove.apply(db, value_type, &LuaType::Nil);
    if non_nil.is_never() {
        LuaType::Unknown
    } else {
        non_nil
    }
}

fn try_compact_record_values(db: &DbIndex, values: &[LuaType]) -> Option<LuaType> {
    if values.len() < 2 || !values.iter().all(is_record_like_value_type) {
        return None;
    }

    let mut fields: HashMap<LuaMemberKey, (LuaType, usize)> = HashMap::new();
    for value in values {
        let Some(member_map) = get_member_map(db, value) else {
            continue;
        };
        let mut present_keys = HashSet::new();
        for (key, member_infos) in member_map.iter() {
            if matches!(key, LuaMemberKey::None) || member_infos.is_empty() {
                continue;
            }

            present_keys.insert(key.clone());
            for member in member_infos {
                fields
                    .entry(key.clone())
                    .and_modify(|(existing, _)| {
                        *existing = TypeOps::Union.apply(db, existing, &member.typ);
                    })
                    .or_insert((member.typ.clone(), 0));
            }
        }

        for key in present_keys {
            if let Some((_, present_count)) = fields.get_mut(&key) {
                *present_count += 1;
            }
        }
    }

    if fields.is_empty() {
        return None;
    }

    let total_values = values.len();
    let object_fields = fields
        .into_iter()
        .map(|(key, (typ, present_count))| {
            let typ = if present_count < total_values {
                TypeOps::Union.apply(db, &typ, &LuaType::Nil)
            } else {
                typ
            };
            (key, typ)
        })
        .collect();

    Some(LuaType::Object(
        LuaObjectType::new_with_fields(object_fields, Vec::new()).into(),
    ))
}

fn is_record_like_value_type(value: &LuaType) -> bool {
    matches!(
        value,
        LuaType::TableConst(_) | LuaType::Instance(_) | LuaType::Object(_)
    )
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
