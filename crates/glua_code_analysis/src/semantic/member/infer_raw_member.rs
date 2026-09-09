use std::sync::Arc;

use rowan::TextSize;
use smol_str::SmolStr;

use crate::{
    DbIndex, FileId, GlobalId, InFiled, InferFailReason, InferGuard, InferGuardRef, LuaGenericType,
    LuaMemberIndexItem, LuaMemberKey, LuaMemberOwner, LuaMergedTableType, LuaObjectType,
    LuaTupleType, LuaType, LuaTypeDeclId, TypeOps, check_type_compact,
    semantic::{
        LuaInferCache,
        generic::{TypeSubstitutor, instantiate_type_generic},
    },
};

use super::{
    RawGetMemberTypeResult, cached_local_class_table_member_ids, get_buildin_type_map_type_id,
    local_class_table_member_ids, member_key_as_type, member_key_matches_type,
    merge_open_table_types,
};

pub fn infer_raw_member_type(
    db: &DbIndex,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    infer_raw_member_type_guard(db, None, prefix_type, member_key, &InferGuard::new())
}

pub fn infer_raw_member_type_with_cache(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    infer_raw_member_type_guard(db, Some(cache), prefix_type, member_key, &InferGuard::new())
}

fn infer_raw_member_type_guard(
    db: &DbIndex,
    mut cache: Option<&mut LuaInferCache>,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
    infer_guard: &InferGuardRef,
) -> RawGetMemberTypeResult {
    match prefix_type {
        LuaType::Table => Ok(LuaType::Unknown),
        LuaType::Any => Ok(LuaType::Any),
        LuaType::Unknown => Err(InferFailReason::FieldNotFound),
        LuaType::TableConst(id) => {
            let owner = LuaMemberOwner::Element(id.clone());
            infer_owner_raw_member_type(db, owner, member_key)
        }
        LuaType::String
        | LuaType::Io
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => {
            if db.get_emmyrc().gmod.enabled {
                let is_numeric = match member_key {
                    LuaMemberKey::Integer(_) => true,
                    LuaMemberKey::ExprType(t) => {
                        t.is_integer()
                            || t.is_number()
                            || matches!(t, LuaType::Number | LuaType::Integer)
                    }
                    _ => false,
                };
                if is_numeric {
                    return Ok(LuaType::String);
                }
            }
            let decl_id = get_buildin_type_map_type_id(prefix_type).ok_or(InferFailReason::None)?;
            let owner = LuaMemberOwner::Type(decl_id);
            infer_owner_raw_member_type(db, owner, member_key)
        }
        LuaType::Ref(type_id) => infer_custom_type_raw_member_type(
            db,
            cache.as_deref_mut(),
            type_id,
            member_key,
            infer_guard,
        ),
        LuaType::Def(type_id) => infer_custom_type_raw_member_type(
            db,
            cache.as_deref_mut(),
            type_id,
            member_key,
            infer_guard,
        ),
        LuaType::Tuple(tuple) => infer_tuple_raw_member_type(tuple, member_key),
        LuaType::Object(object) => infer_object_raw_member_type(db, object, member_key),
        LuaType::Array(array_type) => {
            infer_array_raw_member_type(db, array_type.get_base(), member_key)
        }
        LuaType::TableGeneric(table_generic) => {
            infer_table_generic_raw_member_type(db, table_generic, member_key)
        }
        LuaType::Generic(generic_type) => infer_generic_raw_member_type(
            db,
            cache.as_deref_mut(),
            generic_type,
            member_key,
            infer_guard,
        ),
        LuaType::MergedTable(merged_table) => infer_merged_table_raw_member_type(
            db,
            cache.as_deref_mut(),
            merged_table,
            member_key,
            infer_guard,
        ),
        LuaType::TableOf(inner) => {
            infer_raw_member_type_guard(db, cache, inner, member_key, infer_guard)
        }
        // other do not support now
        _ => Err(InferFailReason::None),
    }
}

fn infer_merged_table_raw_member_type(
    db: &DbIndex,
    mut cache: Option<&mut LuaInferCache>,
    merged_table: &LuaMergedTableType,
    member_key: &LuaMemberKey,
    infer_guard: &InferGuardRef,
) -> RawGetMemberTypeResult {
    let mut member_types = Vec::new();
    let mut last_resolve_reason = InferFailReason::FieldNotFound;

    for component in merged_table.get_types() {
        match infer_raw_member_type_guard(
            db,
            cache.as_deref_mut(),
            component,
            member_key,
            &infer_guard.fork(),
        ) {
            Ok(typ) if !typ.is_never() => member_types.push(typ),
            Ok(_) => {}
            Err(InferFailReason::FieldNotFound | InferFailReason::None) => {}
            Err(reason) if reason.is_need_resolve() => last_resolve_reason = reason,
            Err(reason) => return Err(reason),
        }
    }

    if member_types.is_empty() {
        return Err(last_resolve_reason);
    }

    Ok(merge_open_table_types(db, member_types))
}

fn infer_owner_raw_member_type(
    db: &DbIndex,
    member_owner: LuaMemberOwner,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    if let Some(member_item) = db
        .get_member_index()
        .get_member_item(&member_owner, member_key)
    {
        return member_item.resolve_type(db);
    }

    let Some(access_key_type) = member_key_as_type(member_key) else {
        return Err(InferFailReason::FieldNotFound);
    };

    // See `infer_owner_raw_member_type_with_realm`: a literal access that got
    // past the exact-key lookup can only be answered by an expression-keyed
    // member, so the rest of the owner's members are not candidates.
    let member_index = db.get_member_index();
    let owner_members = if matches!(member_key, LuaMemberKey::Name(_) | LuaMemberKey::Integer(_)) {
        member_index.get_expr_key_members(&member_owner)
    } else {
        member_index.get_members(&member_owner)
    };
    let Some(owner_members) = owner_members else {
        return Err(InferFailReason::FieldNotFound);
    };

    let mut result_type = LuaType::Never;
    for member in owner_members {
        if !member_key_matches_type(db, &access_key_type, member.get_key()) {
            continue;
        }

        let member_type = db
            .get_type_index()
            .get_type_cache(&member.get_id().into())
            .map(|it| it.as_type().clone())
            .unwrap_or(LuaType::Unknown);
        result_type = TypeOps::Union.apply(db, &result_type, &member_type);
    }

    if result_type.is_never() {
        return Err(InferFailReason::FieldNotFound);
    }

    if matches!(
        access_key_type,
        LuaType::String | LuaType::Number | LuaType::Integer
    ) {
        result_type = TypeOps::Union.apply(db, &result_type, &LuaType::Nil);
    }

    Ok(result_type)
}

pub(crate) fn infer_owner_raw_member_type_with_realm(
    db: &DbIndex,
    member_owner: LuaMemberOwner,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: Option<TextSize>,
    source_min_position: Option<InFiled<TextSize>>,
) -> RawGetMemberTypeResult {
    infer_owner_raw_member_type_with_realm_uncached(
        db,
        &member_owner,
        member_key,
        caller_file_id,
        caller_position,
        source_min_position.as_ref(),
    )
}

/// Memoised entry point for hot read paths. The union fold over writers is
/// identical for every read of the same member from the same place, so
/// repeated reads share one answer instead of re-resolving every writer.
pub(crate) fn infer_owner_raw_member_type_with_realm_cached(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_owner: LuaMemberOwner,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: Option<TextSize>,
    source_min_position: Option<InFiled<TextSize>>,
) -> RawGetMemberTypeResult {
    let cache_key = (
        member_owner.clone(),
        member_key.clone(),
        caller_file_id,
        caller_position,
        source_min_position.clone(),
    );
    if let Some(cached) = cache.owner_raw_member_type_cache.get(&cache_key) {
        return cached.clone();
    }
    let result = infer_owner_raw_member_type_with_realm_uncached(
        db,
        &member_owner,
        member_key,
        caller_file_id,
        caller_position,
        source_min_position.as_ref(),
    );
    cache
        .owner_raw_member_type_cache
        .insert(cache_key, result.clone());
    result
}

fn infer_owner_raw_member_type_with_realm_uncached(
    db: &DbIndex,
    member_owner: &LuaMemberOwner,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: Option<TextSize>,
    source_min_position: Option<&InFiled<TextSize>>,
) -> RawGetMemberTypeResult {
    let member_index = db.get_member_index();
    if let Some(member_item) = member_index.get_member_item(member_owner, member_key) {
        let exact_type = resolve_member_item_with_realm(
            db,
            member_item,
            caller_file_id,
            caller_position,
            source_min_position,
        )?;
        // A dynamic write answers a dynamic read, but it is not evidence that
        // the integer entries beside it are absent: `t[n]` on a table filled
        // by both `t[1] = "a"` and `t[k] = 5` holds either, since a literal
        // index and a computed one address the same entries. Named fields do
        // not join: beside a computed write they are the table's own schema,
        // and a key computed at runtime is the registry entry, not a field.
        let LuaMemberKey::ExprType(access_key_type) = member_key else {
            return Ok(exact_type);
        };
        let mut integer_keys = member_index
            .get_member_keys(member_owner)
            .filter(|key| {
                matches!(key, LuaMemberKey::Integer(_))
                    && member_key_matches_type(db, access_key_type, key)
            })
            .collect::<Vec<_>>();
        integer_keys.sort();
        let mut member_types = Vec::with_capacity(1 + integer_keys.len());
        member_types.push(exact_type);
        member_types.extend(
            integer_keys
                .into_iter()
                .filter_map(|key| member_index.get_member_item(member_owner, key))
                .filter_map(|item| {
                    resolve_member_item_with_realm(
                        db,
                        item,
                        caller_file_id,
                        caller_position,
                        source_min_position,
                    )
                    .ok()
                }),
        );
        let result_type = TypeOps::union_all(db, member_types);
        return Ok(result_type);
    }

    let Some(access_key_type) = member_key_as_type(member_key) else {
        return Err(InferFailReason::FieldNotFound);
    };

    // Two literal keys match only when they are equal, which the exact-key
    // lookup above already covered, so a literal access that reaches here can
    // only be answered by an expression-keyed member.
    let owner_members = if matches!(member_key, LuaMemberKey::Name(_) | LuaMemberKey::Integer(_)) {
        member_index.get_expr_key_members(member_owner)
    } else {
        member_index.get_members(member_owner)
    };
    let Some(owner_members) = owner_members else {
        return Err(InferFailReason::FieldNotFound);
    };

    let mut member_types = Vec::new();
    // A member that resolves to Unknown is still a resolved member, so "did
    // anything match" is tracked separately from the accumulated type.
    let mut saw_match = false;
    for member in owner_members {
        if !member_key_matches_type(db, &access_key_type, member.get_key()) {
            continue;
        }

        let member_item = LuaMemberIndexItem::One(member.get_id());
        if let Ok(member_type) = resolve_member_item_with_realm(
            db,
            &member_item,
            caller_file_id,
            caller_position,
            source_min_position,
        ) {
            saw_match = true;
            member_types.push(member_type);
        }
    }

    if !saw_match {
        return Err(InferFailReason::FieldNotFound);
    }

    let mut result_type = TypeOps::union_all(db, member_types);

    if matches!(
        access_key_type,
        LuaType::String | LuaType::Number | LuaType::Integer
    ) {
        result_type = TypeOps::Union.apply(db, &result_type, &LuaType::Nil);
    }

    Ok(result_type)
}

pub(crate) fn resolve_member_item_with_realm(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: FileId,
    caller_position: Option<TextSize>,
    source_min_position: Option<&InFiled<TextSize>>,
) -> RawGetMemberTypeResult {
    if let Some(source_min_position) = source_min_position {
        let mut member_ids = if let Some(pos) = caller_position {
            member_item.visible_member_ids_with_realm_at_offset(db, &caller_file_id, pos)
        } else {
            member_item.visible_member_ids_with_realm(db, &caller_file_id)
        };
        member_ids.retain(|member_id| {
            source_survives_table_generation_cutoff(
                db,
                member_id.file_id,
                member_id.get_position(),
                source_min_position,
            )
        });
        if member_ids.is_empty() {
            return Err(InferFailReason::FieldNotFound);
        }
        let item = match member_ids.as_slice() {
            [member_id] => LuaMemberIndexItem::One(*member_id),
            _ => LuaMemberIndexItem::Many(member_ids),
        };
        return item.resolve_type(db);
    }

    if let Some(pos) = caller_position {
        member_item.resolve_type_with_realm_at_offset(db, &caller_file_id, pos)
    } else {
        member_item.resolve_type_with_realm(db, &caller_file_id)
    }
}

pub(crate) fn source_survives_table_generation_cutoff(
    db: &DbIndex,
    source_file_id: FileId,
    source_position: TextSize,
    source_min_position: &InFiled<TextSize>,
) -> bool {
    if source_file_id != source_min_position.file_id || source_position >= source_min_position.value
    {
        return true;
    }

    let member_index = db.get_member_index();
    let source_function =
        member_index.enclosing_function_scope_range(source_file_id, source_position);
    let generation_function = member_index
        .enclosing_function_scope_range(source_min_position.file_id, source_min_position.value);
    source_function.is_some() && source_function != generation_function
}

fn infer_custom_type_raw_member_type(
    db: &DbIndex,
    mut cache: Option<&mut LuaInferCache>,
    type_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
    infer_guard: &InferGuardRef,
) -> RawGetMemberTypeResult {
    infer_guard.check(type_id)?;
    let type_index = db.get_type_index();
    let type_decl = type_index
        .get_type_decl(type_id)
        .ok_or(InferFailReason::None)?;
    if type_decl.is_alias() {
        if let Some(origin_type) = type_decl.get_alias_origin(db, None) {
            return infer_raw_member_type_guard(db, cache, &origin_type, member_key, infer_guard);
        } else {
            return Err(InferFailReason::None);
        }
    }

    let owner = LuaMemberOwner::Type(type_id.clone());
    if let Some(member_item) = db.get_member_index().get_member_item(&owner, member_key) {
        return member_item.resolve_type(db);
    }
    let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new(type_id.get_name()));
    if let Some(member_item) = db
        .get_member_index()
        .get_member_item(&global_owner, member_key)
    {
        return member_item.resolve_type(db);
    }
    let local_class_member_ids = match cache.as_deref_mut() {
        Some(cache) => cached_local_class_table_member_ids(db, cache, type_id, member_key),
        None => Arc::new(local_class_table_member_ids(db, type_id, member_key)),
    };
    if !local_class_member_ids.is_empty() {
        let member_item = match local_class_member_ids.as_slice() {
            [member_id] => LuaMemberIndexItem::One(*member_id),
            _ => LuaMemberIndexItem::Many(local_class_member_ids.as_ref().clone()),
        };
        return member_item.resolve_type(db);
    }

    if type_decl.is_class()
        && let Some(super_types) = type_index.get_super_types(type_id)
    {
        for super_type in super_types {
            let result = infer_raw_member_type_guard(
                db,
                cache.as_deref_mut(),
                &super_type,
                member_key,
                &infer_guard.fork(),
            );

            match result {
                Ok(member_type) => {
                    return Ok(member_type);
                }
                Err(InferFailReason::FieldNotFound) | Err(InferFailReason::None) => {}
                Err(err) => return Err(err),
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_tuple_raw_member_type(
    tuple: &LuaTupleType,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    if let LuaMemberKey::Integer(i) = &member_key {
        let i = *i;
        let index = if i > 0 { i - 1 } else { 0 };
        return match tuple.get_type(index as usize) {
            Some(typ) => Ok(typ.clone()),
            None => Err(InferFailReason::FieldNotFound),
        };
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_object_raw_member_type(
    db: &DbIndex,
    object: &LuaObjectType,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    if let Some(member_type) = object.get_field(member_key) {
        return Ok(member_type.clone());
    }

    let index_accesses = object.get_index_access();
    for (key, value) in index_accesses {
        let access_key_type = match &member_key {
            LuaMemberKey::Name(name) => LuaType::StringConst(name.clone().into()),
            LuaMemberKey::Integer(i) => LuaType::IntegerConst(*i),
            LuaMemberKey::ExprType(lua_type) => lua_type.clone(),
            LuaMemberKey::None => continue,
        };

        if check_type_compact(db, key, &access_key_type).is_ok() {
            return Ok(value.clone());
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_array_raw_member_type(
    db: &DbIndex,
    array_type: &LuaType,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    let typ = if db.get_emmyrc().strict.array_index {
        TypeOps::Union.apply(db, array_type, &LuaType::Nil)
    } else {
        array_type.clone()
    };
    match member_key {
        LuaMemberKey::Integer(_) => Ok(typ),
        LuaMemberKey::ExprType(member_type) => {
            if member_type.is_integer() {
                Ok(typ)
            } else {
                Err(InferFailReason::FieldNotFound)
            }
        }
        _ => Err(InferFailReason::FieldNotFound),
    }
}

fn infer_table_generic_raw_member_type(
    db: &DbIndex,
    table_params: &Arc<Vec<LuaType>>,
    member_key: &LuaMemberKey,
) -> RawGetMemberTypeResult {
    if table_params.len() != 2 {
        return Err(InferFailReason::None);
    }
    let key_type = &table_params[0];
    let value_type = &table_params[1];
    let access_key_type = match member_key {
        LuaMemberKey::Integer(i) => LuaType::IntegerConst(*i),
        LuaMemberKey::Name(name) => LuaType::StringConst(SmolStr::new(name.as_str()).into()),
        LuaMemberKey::ExprType(expr_type) => expr_type.clone(),
        LuaMemberKey::None => return Err(InferFailReason::FieldNotFound),
    };
    if check_type_compact(db, key_type, &access_key_type).is_ok() {
        return Ok(value_type.clone());
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_generic_raw_member_type(
    db: &DbIndex,
    cache: Option<&mut LuaInferCache>,
    generic_type: &LuaGenericType,
    member_key: &LuaMemberKey,
    infer_guard: &InferGuardRef,
) -> RawGetMemberTypeResult {
    let base_ref_id = generic_type.get_base_type_id_ref();
    let generic_params = generic_type.get_params();
    let substitutor = TypeSubstitutor::from_type_array(generic_params.clone());
    let type_decl = db
        .get_type_index()
        .get_type_decl(&base_ref_id)
        .ok_or(InferFailReason::None)?;

    if let Some(origin) = type_decl.get_alias_origin(db, Some(&substitutor)) {
        return infer_raw_member_type_guard(db, cache, &origin, member_key, infer_guard);
    }

    let base_ref_type = LuaType::Ref(base_ref_id.clone());
    let result = infer_raw_member_type_guard(db, cache, &base_ref_type, member_key, infer_guard)?;
    Ok(instantiate_type_generic(db, &result, &substitutor))
}
