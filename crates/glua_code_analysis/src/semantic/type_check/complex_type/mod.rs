mod array_type_check;
mod call_type_check;
mod intersection_type_check;
mod object_type_check;
mod table_generic_check;
mod tuple_type_check;

use array_type_check::check_array_type_compact;
use call_type_check::check_call_type_compact;
use intersection_type_check::check_intersection_type_compact;
use object_type_check::check_object_type_compact;
use table_generic_check::check_table_generic_type_compact;
use tuple_type_check::check_tuple_type_compact;

use crate::{
    LuaMemberKey, LuaMemberOwner, LuaType, LuaUnionType, TypeSubstitutor,
    semantic::{member::find_members, type_check::type_check_context::TypeCheckContext},
};

use super::{
    TypeCheckResult, check_general_type_compact, type_check_fail_reason::TypeCheckFailReason,
    type_check_guard::TypeCheckGuard,
};

// all is duck typing
pub fn check_complex_type_compact(
    context: &mut TypeCheckContext,
    source: &LuaType,
    compact_type: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    // TODO: 缓存以提高性能
    // 如果是泛型+不包含模板参数+alias, 那么尝试实例化再检查
    if let LuaType::Generic(generic) = compact_type {
        if !generic.contain_tpl() {
            let base_id = generic.get_base_type_id();
            if let Some(decl) = context.db.get_type_index().get_type_decl(&base_id)
                && decl.is_alias()
            {
                let substitutor = TypeSubstitutor::from_alias_for_type(
                    context.db,
                    generic.get_params().clone(),
                    base_id.clone(),
                );
                if let Some(alias_origin) = decl.get_alias_origin(context.db, Some(&substitutor)) {
                    return check_general_type_compact(
                        context,
                        source,
                        &alias_origin,
                        check_guard.next_level()?,
                    );
                }
            }
        }
    }

    match source {
        LuaType::Array(source_array_type) => {
            match check_array_type_compact(
                context,
                source_array_type.get_base(),
                compact_type,
                check_guard,
            ) {
                Err(TypeCheckFailReason::DonotCheck) => {}
                result => return result,
            }
        }
        LuaType::Tuple(tuple) => {
            match check_tuple_type_compact(context, tuple, compact_type, check_guard) {
                Err(TypeCheckFailReason::DonotCheck) => {}
                result => return result,
            }
        }
        LuaType::Object(source_object) => {
            match check_object_type_compact(context, source_object, compact_type, check_guard) {
                Err(TypeCheckFailReason::DonotCheck) => {}
                result => return result,
            }
        }
        LuaType::TableGeneric(source_generic_param) => {
            match check_table_generic_type_compact(
                context,
                source_generic_param,
                compact_type,
                check_guard,
            ) {
                Err(TypeCheckFailReason::DonotCheck) => {}
                result => return result,
            }
        }
        LuaType::Intersection(source_intersection) => {
            match check_intersection_type_compact(
                context,
                source_intersection,
                compact_type,
                check_guard,
            ) {
                Err(TypeCheckFailReason::DonotCheck) => {}
                result => return result,
            }
        }
        LuaType::Union(union_type) => {
            if matches!(compact_type, LuaType::TableConst(_)) {
                return check_union_type_compact_table_const(
                    context,
                    union_type,
                    compact_type,
                    check_guard.next_level()?,
                );
            }
            if let LuaType::Union(compact_union) = compact_type {
                return check_union_type_compact_union(
                    context,
                    source,
                    compact_union,
                    check_guard.next_level()?,
                );
            }
            for sub_type in union_type.into_vec() {
                match check_general_type_compact(
                    context,
                    &sub_type,
                    compact_type,
                    check_guard.next_level()?,
                ) {
                    Ok(_) => return Ok(()),
                    Err(e) if e.is_type_not_match() => {}
                    Err(e) => return Err(e),
                }
            }

            return Err(TypeCheckFailReason::TypeNotMatch);
        }
        LuaType::Generic(_) => {
            return Ok(());
        }
        LuaType::Call(alias_call) => {
            return check_call_type_compact(context, alias_call, compact_type, check_guard);
        }
        LuaType::MultiLineUnion(multi_union) => {
            let union = multi_union.to_union();
            return check_complex_type_compact(
                context,
                &union,
                compact_type,
                check_guard.next_level()?,
            );
        }
        _ => {}
    }
    // Do I need to check union types?
    if let LuaType::Union(union) = compact_type {
        for sub_compact in union.into_vec() {
            match check_complex_type_compact(
                context,
                source,
                &sub_compact,
                check_guard.next_level()?,
            ) {
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }

        return Ok(());
    }

    Err(TypeCheckFailReason::TypeNotMatch)
}

// too complex
fn check_union_type_compact_union(
    context: &mut TypeCheckContext,
    source: &LuaType,
    compact_union: &LuaUnionType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    let compact_types = compact_union.into_vec();
    for compact_sub_type in compact_types {
        check_general_type_compact(
            context,
            source,
            &compact_sub_type,
            check_guard.next_level()?,
        )?;
    }

    Ok(())
}

fn check_union_type_compact_table_const(
    context: &mut TypeCheckContext,
    source_union: &LuaUnionType,
    compact_type: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    let union_targets = source_union.into_vec();
    let table_members = table_const_members(context, compact_type);
    let narrowed_targets = narrow_union_targets_by_table_literals(
        context,
        &union_targets,
        &table_members,
        check_guard,
    )?;

    check_table_const_excess_against_targets(
        context,
        &narrowed_targets,
        &table_members,
        check_guard,
    )?;

    for sub_type in narrowed_targets {
        let mut branch_context = context.clone();
        branch_context.skip_excess_property_checks = true;
        match check_general_type_compact(
            &mut branch_context,
            &sub_type,
            compact_type,
            check_guard.next_level()?,
        ) {
            Ok(_) => return Ok(()),
            Err(e) if e.is_type_not_match() => {}
            Err(e) => return Err(e),
        }
    }

    Err(TypeCheckFailReason::TypeNotMatch)
}

fn table_const_members(
    context: &TypeCheckContext,
    compact_type: &LuaType,
) -> Vec<(LuaMemberKey, LuaType)> {
    let LuaType::TableConst(table_range) = compact_type else {
        return Vec::new();
    };

    let member_owner = LuaMemberOwner::Element(table_range.clone());
    let member_index = context.db.get_member_index();
    let Some(members) = member_index.get_members(&member_owner) else {
        return Vec::new();
    };

    let type_index = context.db.get_type_index();
    members
        .iter()
        .filter_map(|member| {
            type_index
                .get_type_cache(&member.get_id().into())
                .map(|cache| (member.get_key().clone(), cache.as_type().clone()))
        })
        .collect()
}

fn narrow_union_targets_by_table_literals(
    context: &mut TypeCheckContext,
    union_targets: &[LuaType],
    table_members: &[(LuaMemberKey, LuaType)],
    check_guard: TypeCheckGuard,
) -> Result<Vec<LuaType>, TypeCheckFailReason> {
    let mut narrowed_targets = Vec::new();
    let mut rejected = false;

    for target in union_targets {
        let Some(target_members) = find_members(context.db, target) else {
            narrowed_targets.push(target.clone());
            continue;
        };

        let mut compatible = true;
        for (table_key, table_type) in table_members {
            if !is_discriminant_type(table_type) {
                continue;
            }

            let Some(target_member) = target_members
                .iter()
                .find(|target_member| &target_member.key == table_key)
            else {
                continue;
            };

            if !is_discriminant_type(&target_member.typ) {
                continue;
            }

            let mut check_context = context.clone();
            match check_general_type_compact(
                &mut check_context,
                &target_member.typ,
                table_type,
                check_guard.next_level()?,
            ) {
                Ok(_) => {}
                Err(err) if err.is_type_not_match() => {
                    compatible = false;
                    rejected = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }

        if compatible {
            narrowed_targets.push(target.clone());
        }
    }

    if rejected && !narrowed_targets.is_empty() {
        Ok(narrowed_targets)
    } else {
        Ok(union_targets.to_vec())
    }
}

fn check_table_const_excess_against_targets(
    context: &mut TypeCheckContext,
    targets: &[LuaType],
    table_members: &[(LuaMemberKey, LuaType)],
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    if targets
        .iter()
        .any(|target| target_allows_unknown_properties(context, target))
    {
        return Ok(());
    }

    let target_members = targets
        .iter()
        .filter_map(|target| find_members(context.db, target))
        .flatten()
        .collect::<Vec<_>>();

    if target_members.is_empty() {
        return Ok(());
    }

    for (table_key, _) in table_members {
        if target_members
            .iter()
            .any(|target_member| &target_member.key == table_key)
        {
            continue;
        }

        let Some(table_key_type) = member_key_type_for_index(table_key) else {
            continue;
        };

        let mut accepted_by_index = false;
        for target_member in &target_members {
            let LuaMemberKey::ExprType(index_key_type) = &target_member.key else {
                continue;
            };
            match check_general_type_compact(
                context,
                index_key_type,
                &table_key_type,
                check_guard.next_level()?,
            ) {
                Ok(_) => {
                    accepted_by_index = true;
                    break;
                }
                Err(err) if err.is_type_not_match() => {}
                Err(err) => return Err(err),
            }
        }

        if accepted_by_index {
            continue;
        }

        return Err(TypeCheckFailReason::TypeNotMatch);
    }

    Ok(())
}

fn target_allows_unknown_properties(context: &TypeCheckContext, target: &LuaType) -> bool {
    match target {
        LuaType::Any | LuaType::Unknown | LuaType::Table | LuaType::TableGeneric(_) => true,
        LuaType::Ref(type_id) | LuaType::Def(type_id) => context
            .db
            .get_type_index()
            .get_type_decl(type_id)
            .map(|decl| !decl.is_exact())
            .unwrap_or(true),
        LuaType::TableOf(_) | LuaType::Generic(_) | LuaType::Instance(_) => true,
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .any(|typ| target_allows_unknown_properties(context, typ)),
        _ => false,
    }
}

fn member_key_type_for_index(member_key: &LuaMemberKey) -> Option<LuaType> {
    match member_key {
        LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
        LuaMemberKey::Name(name) => Some(LuaType::StringConst(name.clone().into())),
        LuaMemberKey::ExprType(typ) => Some(typ.clone()),
        LuaMemberKey::None => None,
    }
}

fn is_discriminant_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::IntegerConst(_)
        | LuaType::DocIntegerConst(_)
        | LuaType::BooleanConst(_)
        | LuaType::DocBooleanConst(_) => true,
        LuaType::Union(union) => union.into_vec().iter().all(is_discriminant_type),
        _ => false,
    }
}
