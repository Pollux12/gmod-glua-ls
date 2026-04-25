mod generic_tpl_pattern;
mod lambda_tpl_pattern;

use std::{ops::Deref, sync::Arc};

use glua_parser::LuaAstNode;
use itertools::Itertools;
use rowan::NodeOrToken;
use smol_str::SmolStr;

use crate::{
    GenericTplId, InferFailReason, InferencePriority, InferenceVariance, LuaAliasCallKind,
    LuaAliasCallType, LuaArrayType, LuaFunctionType, LuaMappedType, LuaMemberInfo, LuaMemberKey,
    LuaMemberOwner, LuaObjectType, LuaSemanticDeclId, LuaTupleStatus, LuaTupleType, LuaTypeDeclId,
    LuaUnionType, SemanticDeclLevel, TypeOps, VariadicType, check_type_compact,
    db_index::{DbIndex, LuaGenericType, LuaType},
    infer_node_semantic_decl,
    semantic::{
        generic::{
            tpl_context::TplContext, tpl_pattern::generic_tpl_pattern::generic_tpl_pattern_match,
            type_substitutor::SubstitutorValue,
        },
        member::{find_index_operations, get_member_map},
    },
};

use super::type_substitutor::TypeSubstitutor;
use std::collections::HashMap;

type TplPatternMatchResult = Result<(), InferFailReason>;

pub fn tpl_pattern_match_args(
    context: &mut TplContext,
    func_param_types: &[LuaType],
    call_arg_types: &[LuaType],
) -> TplPatternMatchResult {
    for i in 0..func_param_types.len() {
        if i >= call_arg_types.len() {
            break;
        }

        let func_param_type = &func_param_types[i];
        let call_arg_type = &call_arg_types[i];

        match (func_param_type, call_arg_type) {
            (LuaType::Variadic(variadic), _) => {
                variadic_tpl_pattern_match(context, variadic, &call_arg_types[i..])?;
                break;
            }
            (_, LuaType::Variadic(variadic)) => {
                multi_param_tpl_pattern_match_multi_return(
                    context,
                    &func_param_types[i..],
                    variadic,
                )?;
                break;
            }
            _ => {
                tpl_pattern_match(context, func_param_type, call_arg_type)?;
            }
        }
    }

    Ok(())
}

pub fn multi_param_tpl_pattern_match_multi_return(
    context: &mut TplContext,
    func_param_types: &[LuaType],
    multi_return: &VariadicType,
) -> TplPatternMatchResult {
    match &multi_return {
        VariadicType::Base(base) => {
            let mut call_arg_types = Vec::new();
            for param in func_param_types {
                if param.is_variadic() {
                    call_arg_types.push(LuaType::Variadic(multi_return.clone().into()));
                    break;
                } else {
                    call_arg_types.push(base.clone());
                }
            }

            tpl_pattern_match_args(context, func_param_types, &call_arg_types)?;
        }
        VariadicType::Multi(_) => {
            let mut call_arg_types = Vec::new();
            for (i, param) in func_param_types.iter().enumerate() {
                let Some(return_type) = multi_return.get_type(i) else {
                    break;
                };

                if param.is_variadic() {
                    call_arg_types.push(LuaType::Variadic(
                        multi_return.get_new_variadic_from(i).into(),
                    ));
                    break;
                } else {
                    call_arg_types.push(return_type.clone());
                }
            }

            tpl_pattern_match_args(context, func_param_types, &call_arg_types)?;
        }
    }

    Ok(())
}

fn get_str_tpl_infer_type(
    context: &mut TplContext,
    name: &str,
    extend_type: Option<&LuaType>,
) -> LuaType {
    match name {
        "unknown" => LuaType::Unknown,
        "never" => LuaType::Never,
        "nil" | "void" => LuaType::Nil,
        "any" => LuaType::Any,
        "userdata" => LuaType::Userdata,
        "thread" => LuaType::Thread,
        "boolean" | "bool" => LuaType::Boolean,
        "string" => LuaType::String,
        "integer" | "int" => LuaType::Integer,
        "number" => LuaType::Number,
        "io" => LuaType::Io,
        "self" => LuaType::SelfInfer,
        "global" => LuaType::Global,
        "function" => LuaType::Function,
        _ => {
            let type_decl_id = LuaTypeDeclId::global(&name);
            let ref_type = LuaType::Ref(type_decl_id.clone());
            let type_decl_exists = context
                .db
                .get_type_index()
                .get_type_decl(&type_decl_id)
                .is_some();

            if type_decl_exists {
                ref_type
            } else if let Some(extend_type) = extend_type {
                if let LuaType::Ref(extend_type_decl_id) = extend_type
                    && let Some(extend_type_decl) = context
                        .db
                        .get_type_index()
                        .get_type_decl(extend_type_decl_id)
                    && extend_type_decl.is_class()
                {
                    context
                        .cache
                        .add_pending_str_tpl_type_decl(type_decl_id, extend_type.clone());
                    ref_type
                } else {
                    extend_type.clone()
                }
            } else {
                ref_type
            }
        }
    }
}

pub fn tpl_pattern_match(
    context: &mut TplContext,
    pattern: &LuaType,
    target: &LuaType,
) -> TplPatternMatchResult {
    let target = escape_alias(context.db, target);
    if !pattern.contain_tpl() {
        return Ok(());
    }

    match pattern {
        LuaType::TplRef(tpl) => {
            if tpl.get_tpl_id().is_func() {
                context
                    .substitutor
                    .insert_type(tpl.get_tpl_id(), target.clone(), true);
            }
        }
        LuaType::ConstTplRef(tpl) => {
            if tpl.get_tpl_id().is_func() {
                context
                    .substitutor
                    .insert_type(tpl.get_tpl_id(), target, false);
            }
        }
        LuaType::StrTplRef(str_tpl) => {
            if let LuaType::StringConst(s) = target {
                let prefix = str_tpl.get_prefix();
                let suffix = str_tpl.get_suffix();
                let type_name = SmolStr::new(format!("{}{}{}", prefix, s, suffix));
                let constraint = str_tpl.get_constraint().cloned();
                let inferred_type =
                    get_str_tpl_infer_type(context, &type_name, constraint.as_ref());
                context
                    .substitutor
                    .insert_type(str_tpl.get_tpl_id(), inferred_type, true);
            }
        }
        LuaType::Array(array_type) => {
            array_tpl_pattern_match(context, array_type.get_base(), &target)?;
        }
        LuaType::TableGeneric(table_generic_params) => {
            table_generic_tpl_pattern_match(context, table_generic_params, &target)?;
        }
        LuaType::Generic(generic) => {
            generic_tpl_pattern_match(context, generic, &target)?;
        }
        LuaType::Union(union) => {
            union_tpl_pattern_match(context, union, &target)?;
        }
        LuaType::DocFunction(doc_func) => {
            func_tpl_pattern_match(context, doc_func, &target)?;
        }
        LuaType::Tuple(tuple) => {
            tuple_tpl_pattern_match(context, tuple, &target)?;
        }
        LuaType::Object(obj) => {
            object_tpl_pattern_match(context, obj, &target)?;
        }
        LuaType::Mapped(mapped) => {
            mapped_tpl_pattern_match(context, mapped, &target)?;
        }
        _ => {}
    }

    Ok(())
}

pub(super) fn try_expand_generic_alias_for_pattern(
    db: &DbIndex,
    generic: &LuaGenericType,
) -> Option<LuaType> {
    let base = generic.get_base_type_id_ref();
    let type_decl = db.get_type_index().get_type_decl(base)?;
    if !type_decl.is_alias() {
        return None;
    }

    let origin = type_decl.get_alias_ref()?;
    let substitutor =
        TypeSubstitutor::from_alias_for_type(db, generic.get_params().clone(), base.clone());
    Some(instantiate_type_for_pattern(db, origin, &substitutor))
}

fn instantiate_type_for_pattern(
    db: &DbIndex,
    ty: &LuaType,
    substitutor: &TypeSubstitutor,
) -> LuaType {
    match ty {
        LuaType::Array(array_type) => LuaType::Array(
            LuaArrayType::from_base_type(instantiate_type_for_pattern(
                db,
                array_type.get_base(),
                substitutor,
            ))
            .into(),
        ),
        LuaType::Tuple(tuple) => LuaType::Tuple(
            LuaTupleType::new(
                tuple
                    .get_types()
                    .iter()
                    .map(|ty| instantiate_type_for_pattern(db, ty, substitutor))
                    .collect(),
                tuple.status,
            )
            .into(),
        ),
        LuaType::DocFunction(doc_func) => {
            let params = doc_func
                .get_params()
                .iter()
                .map(|(name, ty)| {
                    (
                        name.clone(),
                        ty.as_ref()
                            .map(|ty| instantiate_type_for_pattern(db, ty, substitutor)),
                    )
                })
                .collect();
            let ret = instantiate_type_for_pattern(db, doc_func.get_ret(), substitutor);
            LuaType::DocFunction(
                LuaFunctionType::new(
                    doc_func.get_async_state(),
                    doc_func.is_colon_define(),
                    doc_func.is_variadic(),
                    params,
                    ret,
                )
                .into(),
            )
        }
        LuaType::Object(object) => {
            let fields = object
                .get_fields()
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        instantiate_type_for_pattern(db, value, substitutor),
                    )
                })
                .collect();
            let index_access = object
                .get_index_access()
                .iter()
                .map(|(key, value)| {
                    (
                        instantiate_type_for_pattern(db, key, substitutor),
                        instantiate_type_for_pattern(db, value, substitutor),
                    )
                })
                .collect();
            LuaType::Object(LuaObjectType::new_with_fields(fields, index_access).into())
        }
        LuaType::Union(union) => LuaType::from_vec(
            union
                .into_vec()
                .into_iter()
                .map(|ty| instantiate_type_for_pattern(db, &ty, substitutor))
                .collect(),
        ),
        LuaType::Generic(generic) => LuaType::Generic(
            LuaGenericType::new(
                generic.get_base_type_id(),
                generic
                    .get_params()
                    .iter()
                    .map(|ty| instantiate_type_for_pattern(db, ty, substitutor))
                    .collect(),
            )
            .into(),
        ),
        LuaType::TableGeneric(params) => LuaType::TableGeneric(
            params
                .iter()
                .map(|ty| instantiate_type_for_pattern(db, ty, substitutor))
                .collect::<Vec<_>>()
                .into(),
        ),
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => {
            match substitutor.get(tpl.get_tpl_id()) {
                Some(SubstitutorValue::Type(ty)) => ty.default().clone(),
                Some(SubstitutorValue::MultiTypes(types)) => {
                    LuaType::Variadic(VariadicType::Multi(types.clone()).into())
                }
                Some(SubstitutorValue::MultiBase(base)) => base.clone(),
                Some(SubstitutorValue::Params(params)) => params
                    .first()
                    .and_then(|(_, ty)| ty.clone())
                    .unwrap_or(LuaType::Unknown),
                _ => ty.clone(),
            }
        }
        LuaType::Variadic(variadic) => LuaType::Variadic(
            match variadic.deref() {
                VariadicType::Base(base) => {
                    VariadicType::Base(instantiate_type_for_pattern(db, base, substitutor))
                }
                VariadicType::Multi(types) => VariadicType::Multi(
                    types
                        .iter()
                        .map(|ty| instantiate_type_for_pattern(db, ty, substitutor))
                        .collect(),
                ),
            }
            .into(),
        ),
        LuaType::Call(alias_call) => LuaType::Call(
            LuaAliasCallType::new(
                alias_call.get_call_kind(),
                alias_call
                    .get_operands()
                    .iter()
                    .map(|ty| instantiate_type_for_pattern(db, ty, substitutor))
                    .collect(),
            )
            .into(),
        ),
        LuaType::Mapped(mapped) => {
            let mut param = mapped.param.1.clone();
            param.type_constraint = param
                .type_constraint
                .as_ref()
                .map(|ty| instantiate_type_for_pattern(db, ty, substitutor));
            LuaType::Mapped(
                LuaMappedType::new(
                    (mapped.param.0, param),
                    instantiate_type_for_pattern(db, &mapped.value, substitutor),
                    mapped.is_readonly,
                    mapped.is_optional,
                )
                .into(),
            )
        }
        LuaType::TypeGuard(inner) => {
            LuaType::TypeGuard(instantiate_type_for_pattern(db, inner, substitutor).into())
        }
        LuaType::TableOf(inner) => {
            LuaType::TableOf(instantiate_type_for_pattern(db, inner, substitutor).into())
        }
        _ => ty.clone(),
    }
}

pub fn constant_decay(typ: LuaType) -> LuaType {
    match &typ {
        LuaType::FloatConst(_) => LuaType::Number,
        LuaType::DocIntegerConst(_) | LuaType::IntegerConst(_) => LuaType::Integer,
        LuaType::DocStringConst(_) | LuaType::StringConst(_) => LuaType::String,
        LuaType::DocBooleanConst(_) | LuaType::BooleanConst(_) => LuaType::Boolean,
        _ => typ,
    }
}

fn object_tpl_pattern_match(
    context: &mut TplContext,
    origin_obj: &LuaObjectType,
    target: &LuaType,
) -> TplPatternMatchResult {
    match target {
        LuaType::Object(target_object) => {
            // 先匹配 fields
            for (k, v) in origin_obj.get_fields().iter().sorted_by_key(|(k, _)| *k) {
                let target_value = target_object.get_fields().get(k);
                if let Some(target_value) = target_value {
                    tpl_pattern_match(context, v, target_value)?;
                }
            }
            // 再匹配索引访问
            let target_index_access = target_object.get_index_access();
            for (origin_key, v) in origin_obj.get_index_access() {
                // 先匹配 key 类型进行转换
                let target_access = target_index_access.iter().find(|(target_key, _)| {
                    check_type_compact(context.db, origin_key, target_key).is_ok()
                });
                if let Some(target_access) = target_access {
                    tpl_pattern_match(context, origin_key, &target_access.0)?;
                    tpl_pattern_match(context, v, &target_access.1)?;
                }
            }
        }
        LuaType::TableConst(inst) => {
            let owner = LuaMemberOwner::Element(inst.clone());
            object_tpl_pattern_match_member_owner_match(context, origin_obj, owner)?;
        }
        _ => {}
    }

    Ok(())
}

fn object_tpl_pattern_match_member_owner_match(
    context: &mut TplContext,
    object: &LuaObjectType,
    owner: LuaMemberOwner,
) -> TplPatternMatchResult {
    let owner_type = match &owner {
        LuaMemberOwner::Element(inst) => LuaType::TableConst(inst.clone()),
        LuaMemberOwner::Type(type_id) => LuaType::Ref(type_id.clone()),
        _ => {
            return Err(InferFailReason::None);
        }
    };

    let members = get_member_map(context.db, &owner_type).ok_or(InferFailReason::None)?;
    for (k, v) in members {
        let resolve_key = match &k {
            LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
            LuaMemberKey::Name(s) => Some(LuaType::StringConst(s.clone().into())),
            _ => None,
        };
        let resolve_type = match v.len() {
            0 => LuaType::Any,
            1 => v[0].typ.clone(),
            _ => {
                let mut types = Vec::new();
                for m in &v {
                    types.push(m.typ.clone());
                }
                LuaType::from_vec(types)
            }
        };

        // this is a workaround, I need refactor infer member map
        if resolve_type.is_unknown()
            && !v.is_empty()
            && let Some(LuaSemanticDeclId::Member(member_id)) = &v[0].property_owner_id
        {
            return Err(InferFailReason::UnResolveMemberType(*member_id));
        }

        if let Some(_) = resolve_key
            && let Some(field_value) = object.get_field(&k)
        {
            tpl_pattern_match(context, field_value, &resolve_type)?;
        }
    }

    Ok(())
}

fn mapped_tpl_pattern_match(
    context: &mut TplContext,
    mapped: &LuaMappedType,
    target: &LuaType,
) -> TplPatternMatchResult {
    let source_info = mapped_source_info(mapped);
    let key_constraint_tpl_id = mapped_key_constraint_tpl_id(mapped);
    if source_info.is_none() && key_constraint_tpl_id.is_none() {
        return Ok(());
    }

    let target_fields = mapped_target_fields(context, target)?;
    if target_fields.is_empty() {
        return Ok(());
    }

    if let Some(source_info) = source_info {
        homomorphic_mapped_tpl_pattern_match(context, mapped, target_fields, source_info)?;
    } else if let Some(key_constraint_tpl_id) = key_constraint_tpl_id {
        constrained_mapped_tpl_pattern_match(
            context,
            mapped,
            target_fields,
            key_constraint_tpl_id,
        )?;
    }

    Ok(())
}

fn homomorphic_mapped_tpl_pattern_match(
    context: &mut TplContext,
    mapped: &LuaMappedType,
    target_fields: Vec<(LuaMemberKey, LuaType)>,
    source_info: MappedSourceInfo,
) -> TplPatternMatchResult {
    let mut fields = HashMap::new();
    let mut key_types = Vec::new();
    let mut saw_uninferred_field = false;
    for (member_key, target_type) in target_fields {
        let Some(key_type) = member_key_to_key_type(&member_key) else {
            saw_uninferred_field = true;
            continue;
        };

        key_types.push(key_type.clone());
        let target_type = reverse_mapped_source_field_type(context, mapped, target_type);
        if target_type == LuaType::Never {
            saw_uninferred_field = true;
            continue;
        }

        let mut inferred = Vec::new();
        collect_reverse_mapped_field_inferences(
            context,
            &mapped.value,
            &target_type,
            source_info.source_tpl_id,
            mapped.param.0,
            &key_type,
            &mut inferred,
        );

        if inferred.is_empty() {
            saw_uninferred_field = true;
            continue;
        }

        fields.insert(member_key, LuaType::from_vec(inferred));
    }

    if let Some(key_constraint_tpl_id) = source_info.key_constraint_tpl_id
        && !key_types.is_empty()
    {
        let key_type = LuaType::from_vec(key_types);
        context.with_inference_priority(InferencePriority::MappedTypeConstraint, true, |context| {
            context
                .substitutor
                .insert_type(key_constraint_tpl_id, key_type, false);
        });
    }

    if !fields.is_empty() {
        let source_type = reverse_mapped_source_type(fields);
        let priority = if saw_uninferred_field {
            InferencePriority::PartialHomomorphicMappedType
        } else {
            InferencePriority::HomomorphicMappedType
        };
        context.with_inference_priority(priority, true, |context| {
            context
                .substitutor
                .insert_type(source_info.source_tpl_id, source_type, true);
        });
    }

    Ok(())
}

fn constrained_mapped_tpl_pattern_match(
    context: &mut TplContext,
    mapped: &LuaMappedType,
    target_fields: Vec<(LuaMemberKey, LuaType)>,
    key_constraint_tpl_id: GenericTplId,
) -> TplPatternMatchResult {
    let mut key_types = Vec::new();
    let mut prop_types = Vec::new();
    for (member_key, target_type) in target_fields {
        if let Some(key_type) = member_key_to_key_type(&member_key) {
            key_types.push(key_type);
        }

        let target_type = reverse_mapped_source_field_type(context, mapped, target_type);
        if target_type != LuaType::Never {
            prop_types.push(mapped_inference_value_type(target_type));
        }
    }

    if !key_types.is_empty() {
        let key_type = LuaType::from_vec(key_types);
        context.with_inference_priority(InferencePriority::MappedTypeConstraint, true, |context| {
            context
                .substitutor
                .insert_type(key_constraint_tpl_id, key_type, false);
        });
    }

    if !prop_types.is_empty() {
        let prop_type = LuaType::from_vec(prop_types);
        context.with_inference_priority(InferencePriority::Direct, true, |context| {
            tpl_pattern_match(context, &mapped.value, &prop_type)
        })?;
    }

    Ok(())
}

fn mapped_inference_value_type(ty: LuaType) -> LuaType {
    match ty {
        LuaType::Union(union) => {
            LuaType::from_vec(union.into_vec().into_iter().map(constant_decay).collect())
        }
        _ => constant_decay(ty),
    }
}

fn reverse_mapped_source_field_type(
    context: &TplContext,
    mapped: &LuaMappedType,
    target_type: LuaType,
) -> LuaType {
    if mapped.is_optional {
        return TypeOps::Remove.apply(context.db, &target_type, &LuaType::Nil);
    }

    target_type
}

fn reverse_mapped_source_type(fields: HashMap<LuaMemberKey, LuaType>) -> LuaType {
    if let Some(array_type) = reverse_mapped_array_source_type(&fields) {
        return array_type;
    }

    if let Some(tuple_type) = reverse_mapped_tuple_source_type(&fields) {
        return tuple_type;
    }

    LuaType::Object(LuaObjectType::new_with_fields(fields, Vec::new()).into())
}

fn reverse_mapped_array_source_type(fields: &HashMap<LuaMemberKey, LuaType>) -> Option<LuaType> {
    if fields.len() != 1 {
        return None;
    }

    let (key, value) = fields.iter().next()?;
    match key {
        LuaMemberKey::ExprType(LuaType::Integer | LuaType::Number) => Some(LuaType::Array(
            LuaArrayType::from_base_type(value.clone()).into(),
        )),
        _ => None,
    }
}

fn reverse_mapped_tuple_source_type(fields: &HashMap<LuaMemberKey, LuaType>) -> Option<LuaType> {
    let mut members = Vec::with_capacity(fields.len());
    for (key, value) in fields {
        let LuaMemberKey::Integer(index) = key else {
            return None;
        };
        if *index <= 0 {
            return None;
        }

        members.push((*index as usize, value.clone()));
    }

    members.sort_by_key(|(index, _)| *index);
    for (offset, (index, _)) in members.iter().enumerate() {
        if *index != offset + 1 {
            return None;
        }
    }

    Some(LuaType::Tuple(
        LuaTupleType::new(
            members.into_iter().map(|(_, value)| value).collect(),
            LuaTupleStatus::InferResolve,
        )
        .into(),
    ))
}

#[derive(Debug, Clone, Copy)]
struct MappedSourceInfo {
    source_tpl_id: GenericTplId,
    key_constraint_tpl_id: Option<GenericTplId>,
}

fn mapped_source_info(mapped: &LuaMappedType) -> Option<MappedSourceInfo> {
    let constraint = mapped.param.1.type_constraint.as_ref()?;
    mapped_source_info_from_constraint(constraint, 0)
}

fn mapped_key_constraint_tpl_id(mapped: &LuaMappedType) -> Option<GenericTplId> {
    let constraint = mapped.param.1.type_constraint.as_ref()?;
    tpl_id_from_type(constraint)
}

fn mapped_source_info_from_constraint(
    constraint: &LuaType,
    depth: usize,
) -> Option<MappedSourceInfo> {
    if depth > 8 {
        return None;
    }

    match constraint {
        LuaType::Call(alias_call) if alias_call.get_call_kind() == LuaAliasCallKind::KeyOf => {
            let operands = alias_call.get_operands();
            if operands.len() != 1 {
                return None;
            }

            Some(MappedSourceInfo {
                source_tpl_id: tpl_id_from_type(&operands[0])?,
                key_constraint_tpl_id: None,
            })
        }
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => {
            let mut info = tpl
                .get_constraint()
                .and_then(|constraint| mapped_source_info_from_constraint(constraint, depth + 1))?;
            info.key_constraint_tpl_id = Some(tpl.get_tpl_id());
            Some(info)
        }
        _ => None,
    }
}

fn mapped_target_fields(
    context: &TplContext,
    target: &LuaType,
) -> Result<Vec<(LuaMemberKey, LuaType)>, InferFailReason> {
    match target {
        LuaType::Object(target_object) => Ok(target_object
            .get_fields()
            .iter()
            .map(|(key, ty)| (key.clone(), ty.clone()))
            .collect()),
        LuaType::Array(target_array) => Ok(vec![(
            LuaMemberKey::ExprType(LuaType::Integer),
            target_array.get_base().clone(),
        )]),
        LuaType::Tuple(target_tuple) => Ok(target_tuple
            .get_types()
            .iter()
            .enumerate()
            .map(|(index, ty)| (LuaMemberKey::Integer((index + 1) as i64), ty.clone()))
            .collect()),
        LuaType::TableConst(_) | LuaType::Ref(_) | LuaType::Def(_) | LuaType::Generic(_) => {
            let members = get_member_map(context.db, target).ok_or(InferFailReason::None)?;
            let mut fields = Vec::new();
            for (key, infos) in members {
                let resolve_type = member_infos_to_type(&infos)?;
                fields.push((key, resolve_type));
            }
            Ok(fields)
        }
        _ => Ok(Vec::new()),
    }
}

fn member_infos_to_type(infos: &[LuaMemberInfo]) -> Result<LuaType, InferFailReason> {
    let resolve_type = match infos.len() {
        0 => LuaType::Any,
        1 => infos[0].typ.clone(),
        _ => LuaType::from_vec(infos.iter().map(|info| info.typ.clone()).collect()),
    };

    if resolve_type.is_unknown()
        && !infos.is_empty()
        && let Some(LuaSemanticDeclId::Member(member_id)) = &infos[0].property_owner_id
    {
        return Err(InferFailReason::UnResolveMemberType(*member_id));
    }

    Ok(resolve_type)
}

fn member_key_to_key_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
        LuaMemberKey::Name(s) => Some(LuaType::StringConst(s.clone().into())),
        LuaMemberKey::ExprType(ty) => Some(ty.clone()),
        _ => None,
    }
}

fn tpl_id_from_type(ty: &LuaType) -> Option<GenericTplId> {
    match ty {
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => Some(tpl.get_tpl_id()),
        _ => None,
    }
}

fn collect_reverse_mapped_field_inferences(
    context: &TplContext,
    pattern: &LuaType,
    target: &LuaType,
    source_tpl_id: GenericTplId,
    mapped_key_tpl_id: GenericTplId,
    key_type: &LuaType,
    inferred: &mut Vec<LuaType>,
) -> bool {
    let target = escape_alias(context.db, target);
    match pattern {
        LuaType::Call(alias_call)
            if mapped_index_call_matches(
                alias_call,
                source_tpl_id,
                mapped_key_tpl_id,
                key_type,
            ) =>
        {
            inferred.push(target);
            true
        }
        LuaType::Call(pattern_call) => {
            let LuaType::Call(target_call) = &target else {
                return false;
            };
            if pattern_call.get_call_kind() != target_call.get_call_kind()
                || pattern_call.get_operands().len() != target_call.get_operands().len()
            {
                return false;
            }

            let mut matched = false;
            for (pattern_operand, target_operand) in pattern_call
                .get_operands()
                .iter()
                .zip(target_call.get_operands().iter())
            {
                matched |= collect_reverse_mapped_field_inferences(
                    context,
                    pattern_operand,
                    target_operand,
                    source_tpl_id,
                    mapped_key_tpl_id,
                    key_type,
                    inferred,
                );
            }
            matched
        }
        LuaType::Generic(pattern_generic) => {
            if let Some(expanded) =
                try_expand_generic_alias_for_pattern(context.db, pattern_generic)
            {
                return collect_reverse_mapped_field_inferences(
                    context,
                    &expanded,
                    &target,
                    source_tpl_id,
                    mapped_key_tpl_id,
                    key_type,
                    inferred,
                );
            }

            let LuaType::Generic(target_generic) = &target else {
                return false;
            };
            if pattern_generic.get_base_type_id_ref() != target_generic.get_base_type_id_ref() {
                return false;
            }

            let mut matched = false;
            for (pattern_param, target_param) in pattern_generic
                .get_params()
                .iter()
                .zip(target_generic.get_params().iter())
            {
                matched |= collect_reverse_mapped_field_inferences(
                    context,
                    pattern_param,
                    target_param,
                    source_tpl_id,
                    mapped_key_tpl_id,
                    key_type,
                    inferred,
                );
            }
            matched
        }
        LuaType::DocFunction(pattern_func) => collect_reverse_mapped_function_inferences(
            context,
            pattern_func,
            &target,
            source_tpl_id,
            mapped_key_tpl_id,
            key_type,
            inferred,
        ),
        LuaType::Array(pattern_array) => {
            let LuaType::Array(target_array) = &target else {
                return false;
            };
            collect_reverse_mapped_field_inferences(
                context,
                pattern_array.get_base(),
                target_array.get_base(),
                source_tpl_id,
                mapped_key_tpl_id,
                key_type,
                inferred,
            )
        }
        LuaType::Tuple(pattern_tuple) => {
            let LuaType::Tuple(target_tuple) = &target else {
                return false;
            };
            let mut matched = false;
            for (pattern_ty, target_ty) in pattern_tuple
                .get_types()
                .iter()
                .zip(target_tuple.get_types().iter())
            {
                matched |= collect_reverse_mapped_field_inferences(
                    context,
                    pattern_ty,
                    target_ty,
                    source_tpl_id,
                    mapped_key_tpl_id,
                    key_type,
                    inferred,
                );
            }
            matched
        }
        LuaType::Object(pattern_object) => {
            let LuaType::Object(target_object) = &target else {
                return false;
            };
            let mut matched = false;
            for (field_key, pattern_ty) in pattern_object.get_fields() {
                if let Some(target_ty) = target_object.get_fields().get(field_key) {
                    matched |= collect_reverse_mapped_field_inferences(
                        context,
                        pattern_ty,
                        target_ty,
                        source_tpl_id,
                        mapped_key_tpl_id,
                        key_type,
                        inferred,
                    );
                }
            }
            matched
        }
        LuaType::TableGeneric(pattern_params) => {
            let LuaType::TableGeneric(target_params) = &target else {
                return false;
            };
            let mut matched = false;
            for (pattern_param, target_param) in pattern_params.iter().zip(target_params.iter()) {
                matched |= collect_reverse_mapped_field_inferences(
                    context,
                    pattern_param,
                    target_param,
                    source_tpl_id,
                    mapped_key_tpl_id,
                    key_type,
                    inferred,
                );
            }
            matched
        }
        LuaType::Union(pattern_union) => {
            let mut matched = false;
            for pattern_ty in pattern_union.into_vec() {
                matched |= collect_reverse_mapped_field_inferences(
                    context,
                    &pattern_ty,
                    &target,
                    source_tpl_id,
                    mapped_key_tpl_id,
                    key_type,
                    inferred,
                );
            }
            matched
        }
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            let Some(type_decl) = context.db.get_type_index().get_type_decl(type_id) else {
                return false;
            };
            let Some(origin) = type_decl.get_alias_ref() else {
                return false;
            };
            collect_reverse_mapped_field_inferences(
                context,
                origin,
                &target,
                source_tpl_id,
                mapped_key_tpl_id,
                key_type,
                inferred,
            )
        }
        _ => false,
    }
}

fn collect_reverse_mapped_function_inferences(
    context: &TplContext,
    pattern_func: &LuaFunctionType,
    target: &LuaType,
    source_tpl_id: GenericTplId,
    mapped_key_tpl_id: GenericTplId,
    key_type: &LuaType,
    inferred: &mut Vec<LuaType>,
) -> bool {
    let target_func = match target {
        LuaType::DocFunction(func) => Some(func.clone()),
        LuaType::Signature(signature_id) => context
            .db
            .get_signature_index()
            .get(signature_id)
            .map(|signature| signature.to_doc_func_type()),
        _ => None,
    };
    let Some(target_func) = target_func else {
        return false;
    };

    let mut pattern_params = pattern_func.get_params().to_vec();
    if pattern_func.is_colon_define() {
        pattern_params.insert(0, ("self".to_string(), Some(LuaType::Any)));
    }

    let mut target_params = target_func.get_params().to_vec();
    if target_func.is_colon_define() {
        target_params.insert(0, ("self".to_string(), Some(LuaType::Any)));
    }

    let mut matched = false;
    for ((_, pattern_param), (_, target_param)) in pattern_params.iter().zip(target_params.iter()) {
        let Some(pattern_param) = pattern_param else {
            continue;
        };
        let target_param = target_param.as_ref().unwrap_or(&LuaType::Any);
        matched |= collect_reverse_mapped_field_inferences(
            context,
            pattern_param,
            target_param,
            source_tpl_id,
            mapped_key_tpl_id,
            key_type,
            inferred,
        );
    }

    matched |= collect_reverse_mapped_field_inferences(
        context,
        pattern_func.get_ret(),
        target_func.get_ret(),
        source_tpl_id,
        mapped_key_tpl_id,
        key_type,
        inferred,
    );

    matched
}

fn mapped_index_call_matches(
    alias_call: &LuaAliasCallType,
    source_tpl_id: GenericTplId,
    mapped_key_tpl_id: GenericTplId,
    key_type: &LuaType,
) -> bool {
    if !matches!(
        alias_call.get_call_kind(),
        LuaAliasCallKind::Index | LuaAliasCallKind::RawGet
    ) {
        return false;
    }

    let operands = alias_call.get_operands();
    if operands.len() != 2 || tpl_id_from_type(&operands[0]) != Some(source_tpl_id) {
        return false;
    }

    if tpl_id_from_type(&operands[1]) == Some(mapped_key_tpl_id) {
        return true;
    }

    &operands[1] == key_type
}

fn array_tpl_pattern_match(
    context: &mut TplContext,
    base: &LuaType,
    target: &LuaType,
) -> TplPatternMatchResult {
    match target {
        LuaType::Array(target_array_type) => {
            tpl_pattern_match(context, base, target_array_type.get_base())?;
        }
        LuaType::Tuple(target_tuple) => {
            let target_base = target_tuple.cast_down_array_base(context.db);
            tpl_pattern_match(context, base, &target_base)?;
        }
        LuaType::Object(target_object) => {
            let target_base = target_object
                .cast_down_array_base(context.db)
                .ok_or(InferFailReason::None)?;
            tpl_pattern_match(context, base, &target_base)?;
        }
        _ => {}
    }

    Ok(())
}

fn table_generic_tpl_pattern_match(
    context: &mut TplContext,
    table_generic_params: &[LuaType],
    target: &LuaType,
) -> TplPatternMatchResult {
    if table_generic_params.len() != 2 {
        return Err(InferFailReason::None);
    }

    match target {
        LuaType::TableGeneric(target_table_generic_params) => {
            let min_len = table_generic_params
                .len()
                .min(target_table_generic_params.len());
            for i in 0..min_len {
                tpl_pattern_match(
                    context,
                    &table_generic_params[i],
                    &target_table_generic_params[i],
                )?;
            }
        }
        LuaType::Array(target_array_base) => {
            tpl_pattern_match(context, &table_generic_params[0], &LuaType::Integer)?;
            tpl_pattern_match(
                context,
                &table_generic_params[1],
                target_array_base.get_base(),
            )?;
        }
        LuaType::Tuple(target_tuple) => {
            let len = target_tuple.get_types().len();
            let mut keys = Vec::new();
            for i in 0..len {
                keys.push(LuaType::IntegerConst((i as i64) + 1));
            }

            let key_type = LuaType::Union(LuaUnionType::from_vec(keys).into());
            let target_base = target_tuple.cast_down_array_base(context.db);
            tpl_pattern_match(context, &table_generic_params[0], &key_type)?;
            tpl_pattern_match(context, &table_generic_params[1], &target_base)?;
        }
        LuaType::TableConst(inst) => {
            let owner = LuaMemberOwner::Element(inst.clone());
            table_generic_tpl_pattern_member_owner_match(
                context,
                table_generic_params,
                owner,
                &[],
            )?;
        }
        LuaType::Ref(type_id) => {
            let owner = LuaMemberOwner::Type(type_id.clone());
            table_generic_tpl_pattern_member_owner_match(
                context,
                table_generic_params,
                owner,
                &[],
            )?;
        }
        LuaType::Def(type_id) => {
            let owner = LuaMemberOwner::Type(type_id.clone());
            table_generic_tpl_pattern_member_owner_match(
                context,
                table_generic_params,
                owner,
                &[],
            )?;
        }
        LuaType::Generic(generic) => {
            let owner = LuaMemberOwner::Type(generic.get_base_type_id());
            let target_params = generic.get_params();
            table_generic_tpl_pattern_member_owner_match(
                context,
                table_generic_params,
                owner,
                target_params,
            )?;
        }
        LuaType::Object(obj) => {
            let mut keys = Vec::new();
            let mut values = Vec::new();
            for (k, v) in obj.get_fields() {
                match k {
                    LuaMemberKey::Integer(i) => {
                        keys.push(LuaType::IntegerConst(*i));
                    }
                    LuaMemberKey::Name(s) => {
                        keys.push(LuaType::StringConst(s.clone().into()));
                    }
                    _ => {}
                };
                values.push(mapped_inference_value_type(v.clone()));
            }
            for (k, v) in obj.get_index_access() {
                keys.push(k.clone());
                values.push(mapped_inference_value_type(v.clone()));
            }

            let key_type = LuaType::Union(LuaUnionType::from_vec(keys).into());
            let value_type = LuaType::Union(LuaUnionType::from_vec(values).into());
            tpl_pattern_match(context, &table_generic_params[0], &key_type)?;
            tpl_pattern_match(context, &table_generic_params[1], &value_type)?;
        }

        LuaType::Global | LuaType::Any | LuaType::Table | LuaType::Userdata => {
            // too many
            tpl_pattern_match(context, &table_generic_params[0], &LuaType::Any)?;
            tpl_pattern_match(context, &table_generic_params[1], &LuaType::Any)?;
        }
        _ => {}
    }

    Ok(())
}

// KV 表匹配 ref/def/tableconst
fn table_generic_tpl_pattern_member_owner_match(
    context: &mut TplContext,
    table_generic_params: &[LuaType],
    owner: LuaMemberOwner,
    target_params: &[LuaType],
) -> TplPatternMatchResult {
    if table_generic_params.len() != 2 {
        return Err(InferFailReason::None);
    }

    let owner_type = match &owner {
        LuaMemberOwner::Element(inst) => LuaType::TableConst(inst.clone()),
        LuaMemberOwner::Type(type_id) => match target_params.len() {
            0 => LuaType::Ref(type_id.clone()),
            _ => LuaType::Generic(Arc::new(LuaGenericType::new(
                type_id.clone(),
                target_params.to_vec(),
            ))),
        },
        _ => {
            return Err(InferFailReason::None);
        }
    };

    let members = get_member_map(context.db, &owner_type).ok_or(InferFailReason::None)?;
    // 如果是 pairs 调用, 我们需要尝试寻找元方法, 但目前`__pairs` 被放进成员表中
    if is_pairs_call(context).unwrap_or(false)
        && try_handle_pairs_metamethod(context, table_generic_params, &members).is_ok()
    {
        return Ok(());
    }

    let target_key_type = table_generic_params[0].clone();
    let mut keys = Vec::new();
    let mut values = Vec::new();
    for (k, v) in members {
        let key_type = match k {
            LuaMemberKey::Integer(i) => LuaType::IntegerConst(i),
            LuaMemberKey::Name(s) => LuaType::StringConst(s.clone().into()),
            LuaMemberKey::ExprType(typ) => typ,
            _ => continue,
        };

        if !target_key_type.is_generic()
            && !is_table_generic_key_match(context.db, &target_key_type, &key_type)
        {
            continue;
        }

        keys.push(key_type);

        let resolve_type = match v.len() {
            0 => LuaType::Any,
            1 => v[0].typ.clone(),
            _ => {
                let mut types = Vec::new();
                for m in v {
                    types.push(m.typ.clone());
                }
                LuaType::from_vec(types)
            }
        };

        values.push(mapped_inference_value_type(resolve_type));
    }

    if keys.is_empty() {
        find_index_operations(context.db, &owner_type)
            .ok_or(InferFailReason::None)?
            .iter()
            .for_each(|m| {
                if target_key_type.is_generic() {
                    return;
                }
                let key_type = match &m.key {
                    LuaMemberKey::ExprType(typ) => typ.clone(),
                    _ => return,
                };
                if is_table_generic_key_match(context.db, &target_key_type, &key_type) {
                    keys.push(key_type);
                    values.push(mapped_inference_value_type(m.typ.clone()));
                }
            });
    }

    let key_type = match &keys[..] {
        [] => return Err(InferFailReason::None),
        [first] => first.clone(),
        _ => LuaType::Union(LuaUnionType::from_vec(keys).into()),
    };
    let value_type = match &values[..] {
        [first] => first.clone(),
        _ => LuaType::Union(LuaUnionType::from_vec(values).into()),
    };

    tpl_pattern_match(context, &table_generic_params[0], &key_type)?;
    tpl_pattern_match(context, &table_generic_params[1], &value_type)?;

    Ok(())
}

fn is_table_generic_key_match(db: &DbIndex, target_key_type: &LuaType, key_type: &LuaType) -> bool {
    check_type_compact(db, key_type, target_key_type).is_ok()
}

fn union_tpl_pattern_match(
    context: &mut TplContext,
    union: &LuaUnionType,
    target: &LuaType,
) -> TplPatternMatchResult {
    let mut error_count = 0;
    let mut last_error = InferFailReason::None;
    for u in union.into_vec() {
        match tpl_pattern_match(context, &u, target) {
            // 返回 ok 时并不一定匹配成功, 仅表示没有发生错误
            Ok(_) => {}
            Err(e) => {
                error_count += 1;
                last_error = e;
            }
        }
    }

    if error_count == union.into_vec().len() {
        Err(last_error)
    } else {
        Ok(())
    }
}

fn func_tpl_pattern_match(
    context: &mut TplContext,
    tpl_func: &LuaFunctionType,
    target: &LuaType,
) -> TplPatternMatchResult {
    match target {
        LuaType::DocFunction(target_doc_func) => {
            func_tpl_pattern_match_doc_func(context, tpl_func, target_doc_func)?;
        }
        LuaType::Signature(signature_id) => {
            let signature = context
                .db
                .get_signature_index()
                .get(signature_id)
                .ok_or(InferFailReason::None)?;
            if !signature.is_resolve_return() {
                return lambda_tpl_pattern::check_lambda_tpl_pattern(
                    context,
                    tpl_func,
                    *signature_id,
                );
            }
            let fake_doc_func = signature.to_doc_func_type();
            func_tpl_pattern_match_doc_func(context, tpl_func, &fake_doc_func)?;
        }
        _ => {}
    }

    Ok(())
}

fn func_tpl_pattern_match_doc_func(
    context: &mut TplContext,
    tpl_func: &LuaFunctionType,
    target_func: &LuaFunctionType,
) -> TplPatternMatchResult {
    let mut tpl_func_params = tpl_func.get_params().to_vec();
    if tpl_func.is_colon_define() {
        tpl_func_params.insert(0, ("self".to_string(), Some(LuaType::Any)));
    }

    let mut target_func_params = target_func.get_params().to_vec();

    if target_func.is_colon_define() {
        target_func_params.insert(0, ("self".to_string(), Some(LuaType::Any)));
    }

    param_type_list_pattern_match_type_list(context, &tpl_func_params, &target_func_params)?;

    let tpl_return = tpl_func.get_ret();
    let target_return = target_func.get_ret();
    let priority = active_inference_priority(context);
    context.with_inference_priority(priority, true, |context| {
        return_type_pattern_match_target_type(context, tpl_return, target_return)
    })?;

    Ok(())
}

fn active_inference_priority(context: &TplContext) -> InferencePriority {
    match context.substitutor.priority() {
        InferencePriority::None => InferencePriority::Direct,
        priority => priority,
    }
}

fn param_type_list_pattern_match_type_list(
    context: &mut TplContext,
    sources: &[(String, Option<LuaType>)],
    targets: &[(String, Option<LuaType>)],
) -> TplPatternMatchResult {
    let type_len = sources.len();
    let mut target_offset = 0;
    for i in 0..type_len {
        let source = match sources.get(i) {
            Some(t) => t.1.clone().unwrap_or(LuaType::Any),
            None => break,
        };

        match &source {
            LuaType::Variadic(inner) => {
                let i = i + target_offset;
                if i >= targets.len() {
                    if let VariadicType::Base(LuaType::TplRef(tpl_ref)) = inner.deref() {
                        let tpl_id = tpl_ref.get_tpl_id();
                        context.substitutor.insert_type(tpl_id, LuaType::Nil, true);
                    }
                    break;
                }

                if let VariadicType::Base(LuaType::TplRef(generic_tpl)) = inner.deref() {
                    let tpl_id = generic_tpl.get_tpl_id();
                    if let Some(inferred_type_value) = context.substitutor.get(tpl_id) {
                        match inferred_type_value {
                            SubstitutorValue::Type(_) => {
                                continue;
                            }
                            SubstitutorValue::MultiTypes(types) => {
                                if types.len() > 1 {
                                    target_offset += types.len() - 1;
                                }
                                continue;
                            }
                            SubstitutorValue::Params(params) => {
                                if params.len() > 1 {
                                    target_offset += params.len() - 1;
                                }
                                continue;
                            }
                            _ => {}
                        }
                    }
                }

                let mut target_rest_params = &targets[i..];
                // If the variadic parameter is not the last one, then target_rest_params should exclude the parameters that come after it.
                if i + 1 < type_len {
                    let source_rest_len = type_len - i - 1;
                    if source_rest_len >= target_rest_params.len() {
                        continue;
                    }
                    let target_rest_len = target_rest_params.len() - source_rest_len;
                    target_rest_params = &target_rest_params[..target_rest_len];
                    if target_rest_len > 1 {
                        target_offset += target_rest_len - 1;
                    }
                }

                func_varargs_tpl_pattern_match(inner, target_rest_params, context.substitutor)?;
            }
            _ => {
                let target = match targets.get(i + target_offset) {
                    Some(t) => t.1.clone().unwrap_or(LuaType::Any),
                    None => break,
                };
                context.with_inference_priority_and_variance(
                    active_inference_priority(context),
                    true,
                    InferenceVariance::Contravariant,
                    |context| tpl_pattern_match(context, &source, &target),
                )?;
            }
        }
    }

    Ok(())
}

fn return_type_pattern_match_target_type(
    context: &mut TplContext,
    source: &LuaType,
    target: &LuaType,
) -> TplPatternMatchResult {
    match (source, target) {
        // toooooo complex
        (LuaType::Variadic(variadic_source), LuaType::Variadic(variadic_target)) => {
            match variadic_target.deref() {
                VariadicType::Base(target_base) => match variadic_source.deref() {
                    VariadicType::Base(source_base) => {
                        if let LuaType::TplRef(type_ref) = source_base {
                            let tpl_id = type_ref.get_tpl_id();
                            context
                                .substitutor
                                .insert_type(tpl_id, target_base.clone(), true);
                        }
                    }
                    VariadicType::Multi(source_multi) => {
                        for ret_type in source_multi {
                            match ret_type {
                                LuaType::Variadic(inner) => {
                                    if let VariadicType::Base(base) = inner.deref()
                                        && let LuaType::TplRef(type_ref) = base
                                    {
                                        let tpl_id = type_ref.get_tpl_id();
                                        context.substitutor.insert_type(
                                            tpl_id,
                                            target_base.clone(),
                                            true,
                                        );
                                    }

                                    break;
                                }
                                LuaType::TplRef(tpl_ref) => {
                                    let tpl_id = tpl_ref.get_tpl_id();
                                    context.substitutor.insert_type(
                                        tpl_id,
                                        target_base.clone(),
                                        true,
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                },
                VariadicType::Multi(target_types) => {
                    variadic_tpl_pattern_match(context, variadic_source, target_types)?;
                }
            }
        }
        (LuaType::Variadic(variadic), _) => {
            variadic_tpl_pattern_match(context, variadic, std::slice::from_ref(target))?;
        }
        (_, LuaType::Variadic(variadic)) => {
            multi_param_tpl_pattern_match_multi_return(
                context,
                std::slice::from_ref(source),
                variadic,
            )?;
        }
        _ => {
            tpl_pattern_match(context, source, target)?;
        }
    }

    Ok(())
}

fn func_varargs_tpl_pattern_match(
    variadic: &VariadicType,
    target_rest_params: &[(String, Option<LuaType>)],
    substitutor: &mut TypeSubstitutor,
) -> TplPatternMatchResult {
    match variadic {
        VariadicType::Base(base) => {
            if let LuaType::TplRef(tpl_ref) = base {
                let tpl_id = tpl_ref.get_tpl_id();
                substitutor.insert_params(
                    tpl_id,
                    target_rest_params
                        .iter()
                        .map(|(n, t)| (n.clone(), t.clone()))
                        .collect(),
                );
            }
        }
        VariadicType::Multi(_) => {}
    }

    Ok(())
}

pub fn variadic_tpl_pattern_match(
    context: &mut TplContext,
    tpl: &VariadicType,
    target_rest_types: &[LuaType],
) -> TplPatternMatchResult {
    match tpl {
        VariadicType::Base(base) => match base {
            LuaType::TplRef(tpl_ref) => {
                let tpl_id = tpl_ref.get_tpl_id();
                match target_rest_types.len() {
                    0 => {
                        context.substitutor.insert_type(tpl_id, LuaType::Nil, true);
                    }
                    1 => {
                        // If the single argument is itself a multi-return (e.g. a function call
                        // returning multiple values), expand it so that `T...` receives all the
                        // return values rather than a single Variadic wrapper.
                        match &target_rest_types[0] {
                            LuaType::Variadic(variadic) => match variadic.deref() {
                                VariadicType::Multi(types) => match types.len() {
                                    0 => {
                                        context.substitutor.insert_type(tpl_id, LuaType::Nil, true);
                                    }
                                    1 => {
                                        context.substitutor.insert_type(
                                            tpl_id,
                                            types[0].clone(),
                                            true,
                                        );
                                    }
                                    _ => {
                                        context.substitutor.insert_multi_types(
                                            tpl_id,
                                            types
                                                .iter()
                                                .map(|t| constant_decay(t.clone()))
                                                .collect(),
                                        );
                                    }
                                },
                                VariadicType::Base(base) => {
                                    context.substitutor.insert_multi_base(tpl_id, base.clone());
                                }
                            },
                            arg => {
                                context.substitutor.insert_type(tpl_id, arg.clone(), true);
                            }
                        }
                    }
                    _ => {
                        context.substitutor.insert_multi_types(
                            tpl_id,
                            target_rest_types
                                .iter()
                                .map(|t| constant_decay(t.clone()))
                                .collect(),
                        );
                    }
                }
            }
            LuaType::ConstTplRef(tpl_ref) => {
                let tpl_id = tpl_ref.get_tpl_id();
                match target_rest_types.len() {
                    0 => {
                        context.substitutor.insert_type(tpl_id, LuaType::Nil, false);
                    }
                    1 => {
                        context.substitutor.insert_type(
                            tpl_id,
                            target_rest_types[0].clone(),
                            false,
                        );
                    }
                    _ => {
                        context
                            .substitutor
                            .insert_multi_types(tpl_id, target_rest_types.to_vec());
                    }
                }
            }
            _ => {}
        },
        VariadicType::Multi(multi) => {
            for (i, ret_type) in multi.iter().enumerate() {
                match ret_type {
                    LuaType::Variadic(inner) => {
                        if i < target_rest_types.len() {
                            variadic_tpl_pattern_match(context, inner, &target_rest_types[i..])?;
                        }

                        break;
                    }
                    LuaType::TplRef(tpl_ref) => {
                        let tpl_id = tpl_ref.get_tpl_id();
                        match target_rest_types.get(i) {
                            Some(t) => {
                                context.substitutor.insert_type(tpl_id, t.clone(), true);
                            }
                            None => {
                                break;
                            }
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn tuple_tpl_pattern_match(
    context: &mut TplContext,
    tpl_tuple: &LuaTupleType,
    target: &LuaType,
) -> TplPatternMatchResult {
    match target {
        LuaType::Tuple(target_tuple) => {
            let tpl_tuple_types = tpl_tuple.get_types();
            let target_tuple_types = target_tuple.get_types();
            let tpl_tuple_len = tpl_tuple_types.len();
            for i in 0..tpl_tuple_len {
                let tpl_type = &tpl_tuple_types[i];

                if let LuaType::Variadic(inner) = tpl_type {
                    let target_rest_types = &target_tuple_types[i..];
                    variadic_tpl_pattern_match(context, inner, target_rest_types)?;
                    break;
                }

                let target_type = match target_tuple_types.get(i) {
                    Some(t) => t,
                    None => break,
                };

                tpl_pattern_match(context, tpl_type, target_type)?;
            }
        }
        LuaType::Array(target_array_base) => {
            let tupl_tuple_types = tpl_tuple.get_types();
            let last_type = tupl_tuple_types.last().ok_or(InferFailReason::None)?;
            if let LuaType::Variadic(inner) = last_type {
                match inner.deref() {
                    VariadicType::Base(base) => {
                        if let LuaType::TplRef(tpl_ref) = base {
                            let tpl_id = tpl_ref.get_tpl_id();
                            context
                                .substitutor
                                .insert_multi_base(tpl_id, target_array_base.get_base().clone());
                        }
                    }
                    VariadicType::Multi(_) => {}
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn escape_alias(db: &DbIndex, may_alias: &LuaType) -> LuaType {
    if let LuaType::Ref(type_id) = may_alias
        && let Some(type_decl) = db.get_type_index().get_type_decl(type_id)
        && type_decl.is_alias()
        && let Some(origin_type) = type_decl.get_alias_origin(db, None)
    {
        return origin_type.clone();
    }

    may_alias.clone()
}

fn is_pairs_call(context: &mut TplContext) -> Option<bool> {
    let call_expr = context.call_expr.as_ref()?;
    let prefix_expr = call_expr.get_prefix_expr()?;
    let semantic_decl = match prefix_expr.syntax().clone().into() {
        NodeOrToken::Node(node) => infer_node_semantic_decl(
            context.db,
            context.cache,
            node,
            SemanticDeclLevel::default(),
        ),
        _ => None,
    }?;

    let LuaSemanticDeclId::LuaDecl(decl_id) = semantic_decl else {
        return None;
    };
    let decl = context.db.get_decl_index().get_decl(&decl_id)?;
    if !context.db.get_module_index().is_std(&decl.get_file_id()) {
        return None;
    }
    let name = decl.get_name();
    if name != "pairs" {
        return None;
    }
    Some(true)
}

fn try_handle_pairs_metamethod(
    context: &mut TplContext,
    table_generic_params: &[LuaType],
    members: &HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
) -> TplPatternMatchResult {
    let pairs_member = members
        .get(&LuaMemberKey::Name("__pairs".into()))
        .ok_or(InferFailReason::None)?
        .first()
        .ok_or(InferFailReason::None)?;
    // 获取迭代函数返回类型
    let meta_return = match &pairs_member.typ {
        LuaType::Signature(signature_id) => context
            .db
            .get_signature_index()
            .get(signature_id)
            .map(|s| s.get_return_type()),
        LuaType::DocFunction(doc_func) => Some(doc_func.get_ret().clone()),
        _ => None,
    }
    .ok_or(InferFailReason::None)?;

    // 解析出迭代函数返回类型
    let final_return_type = match meta_return {
        LuaType::DocFunction(doc_func) => Some(doc_func.get_ret().clone()),
        LuaType::Signature(signature_id) => context
            .db
            .get_signature_index()
            .get(&signature_id)
            .map(|s| s.get_return_type()),
        _ => None,
    };

    if let Some(LuaType::Variadic(variadic)) = &final_return_type {
        let key_type = variadic.get_type(0).ok_or(InferFailReason::None)?;
        let value_type = variadic.get_type(1).ok_or(InferFailReason::None)?;
        tpl_pattern_match(context, &table_generic_params[0], key_type)?;
        tpl_pattern_match(context, &table_generic_params[1], value_type)?;
        return Ok(());
    }
    Err(InferFailReason::None)
}
