use crate::{
    DbIndex, FileId, InFiled, LuaArrayType, LuaMemberKey, LuaTypeCache, LuaTypeOwner, TypeOps,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{LuaDeclId, LuaMemberId, LuaMemberOwner, LuaType},
    semantic::member_key_matches_type,
};
use glua_parser::{
    BinaryOperator, LuaAstNode, LuaExpr, LuaIndexExpr, LuaIndexKey, LuaLiteralToken, LuaTableExpr,
    LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};

use super::super::{
    LuaAnalyzer,
    stats::{is_assignment_file_define_member, is_local_mutable},
};
use super::{
    DynamicKeyCollectionWideningKey, MemberAssignmentWideningCacheKey, WideningCacheLookup,
    lookup_widening_cache, member_assignment_state_mask, member_assignment_state_masks_compatible,
    record_widening_cache,
};

pub(in crate::compilation::analyzer::lua) fn get_widened_member_assignment_collection_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    incoming_type: &LuaType,
) -> Option<LuaType> {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return None;
    };
    let incoming_array = normalize_infer_collection_type(analyzer.db, incoming_type)?;
    let (owner, key) = {
        let member_index = analyzer.db.get_member_index();
        (
            member_index.get_member_owner(member_id)?.clone(),
            member_index.get_member(member_id)?.get_key().clone(),
        )
    };
    match get_cached_widened_member_collection_assignment_type(
        analyzer,
        &owner,
        &key,
        *member_id,
        incoming_array.get_base(),
    ) {
        Some(Some(widened_type)) => return Some(widened_type),
        Some(None) => return None,
        None => {}
    }
    let related_members = analyzer
        .db
        .get_member_index()
        .get_members_for_owner_key(&owner, &key);
    let mut widened_base = incoming_array.get_base().clone();
    let mut saw_related_collection = false;

    for related_member in related_members {
        let related_member_id = related_member.get_id();
        if related_member_id == *member_id {
            continue;
        }
        if !is_member_realm_compatible(analyzer.db, *member_id, related_member_id) {
            continue;
        }

        let Some(existing_cache) = analyzer
            .db
            .get_type_index()
            .get_type_cache(&related_member_id.into())
            .cloned()
        else {
            continue;
        };
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) =
            normalize_infer_collection_type(analyzer.db, existing_cache.as_type())
        else {
            continue;
        };
        saw_related_collection = true;
        widened_base = TypeOps::Union.apply(analyzer.db, existing_array.get_base(), &widened_base);
    }

    if !saw_related_collection {
        return None;
    }

    Some(LuaType::Array(
        LuaArrayType::from_base_type(widened_base).into(),
    ))
}

pub(in crate::compilation::analyzer::lua) fn get_cached_widened_member_collection_assignment_type(
    analyzer: &mut LuaAnalyzer,
    owner: &LuaMemberOwner,
    key: &LuaMemberKey,
    member_id: LuaMemberId,
    incoming_base: &LuaType,
) -> Option<Option<LuaType>> {
    let incoming_base = crate::widen_literal_type_for_assignment(incoming_base);
    let member_index = analyzer.db.get_member_index();
    let visible_count = member_index.visible_member_count_for_owner_key(owner, key);
    let cache_key = MemberAssignmentWideningCacheKey {
        owner: owner.clone(),
        key: key.clone(),
    };

    let cache = match lookup_widening_cache(
        &analyzer.member_collection_assignment_widening_cache,
        &cache_key,
        visible_count,
    ) {
        WideningCacheLookup::FirstSighting => return Some(None),
        WideningCacheLookup::Fallback => return None,
        WideningCacheLookup::Hit(cache) => cache,
    };

    let current_state_mask = member_assignment_state_mask(analyzer, member_id);
    let mut widened_base = incoming_base;
    let mut saw_related_collection = false;
    for (state_mask, base_type) in &cache.by_state_mask {
        if !member_assignment_state_masks_compatible(analyzer, current_state_mask, *state_mask) {
            continue;
        }
        saw_related_collection = true;
        widened_base = TypeOps::Union.apply(analyzer.db, &widened_base, base_type);
    }

    Some(
        saw_related_collection
            .then(|| LuaType::Array(LuaArrayType::from_base_type(widened_base).into())),
    )
}

pub(in crate::compilation::analyzer::lua) fn record_member_collection_assignment_widening_cache(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    assigned_type: &LuaType,
) {
    let Some(assigned_array) = normalize_infer_collection_type(analyzer.db, assigned_type) else {
        return;
    };

    let LuaTypeOwner::Member(member_id) = type_owner else {
        return;
    };
    if !is_assignment_file_define_member(analyzer.db, *member_id) {
        return;
    }

    let member_index = analyzer.db.get_member_index();
    let Some(owner) = member_index.get_member_owner(member_id).cloned() else {
        return;
    };
    let Some(key) = member_index
        .get_member(member_id)
        .map(|member| member.get_key().clone())
    else {
        return;
    };
    let visible_count = member_index.visible_member_count_for_owner_key(&owner, &key);
    let state_mask = member_assignment_state_mask(analyzer, *member_id);
    let cache_key = MemberAssignmentWideningCacheKey { owner, key };

    let assigned_base = crate::widen_literal_type_for_assignment(assigned_array.get_base());
    let db = &*analyzer.db;
    record_widening_cache(
        &mut analyzer.member_collection_assignment_widening_cache,
        cache_key,
        visible_count,
        state_mask,
        assigned_base,
        |base_type, assigned_base| {
            *base_type = TypeOps::Union.apply(db, base_type, &assigned_base);
        },
    );
}

pub(in crate::compilation::analyzer) fn is_member_realm_compatible(
    db: &DbIndex,
    current_member_id: LuaMemberId,
    related_member_id: LuaMemberId,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }

    let infer_index = db.get_gmod_infer_index();
    infer_index.are_offsets_compatible(
        &current_member_id.file_id,
        current_member_id.get_position(),
        &related_member_id.file_id,
        related_member_id.get_position(),
    )
}

pub(in crate::compilation::analyzer::lua) fn widen_existing_member_collection_type(
    analyzer: &mut LuaAnalyzer,
    var: &LuaVarExpr,
    value_type: &LuaType,
) -> Option<()> {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return Some(());
    };

    let is_collection_append = is_collection_append_write(index_expr).unwrap_or(false);
    let incoming_is_collection = normalize_infer_collection_type(analyzer.db, value_type).is_some();

    if !incoming_is_collection && !is_collection_append {
        return Some(());
    }

    let deferred_collection_widening = incoming_is_collection
        && try_defer_dynamic_key_collection_widening(analyzer, index_expr, value_type)
            .unwrap_or(false);
    if !deferred_collection_widening
        && let Some(member_ids) = find_related_member_ids(analyzer, index_expr.clone())
    {
        widen_member_collections_with_collection_type(analyzer, &member_ids, value_type);
    }

    if is_collection_append
        && let Some(prefix_expr) = index_expr.get_prefix_expr()
        && let Some(prefix_index_expr) = LuaIndexExpr::cast(prefix_expr.syntax().clone())
    {
        let deferred_element_widening =
            try_defer_dynamic_key_element_widening(analyzer, &prefix_index_expr, value_type)
                .unwrap_or(false);
        if !deferred_element_widening
            && let Some(member_ids) = find_related_member_ids(analyzer, prefix_index_expr)
        {
            widen_member_collections_with_element_type(analyzer, &member_ids, value_type);
        }
    }

    Some(())
}

pub(in crate::compilation::analyzer::lua) fn try_defer_dynamic_key_collection_widening(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
    value_type: &LuaType,
) -> Option<bool> {
    let incoming_array = normalize_infer_collection_type(analyzer.db, value_type)?;
    let (owner, key) = dynamic_key_owner_and_member_key(analyzer, index_expr)?;
    record_pending_dynamic_key_collection_widening(analyzer, owner, key, incoming_array.get_base());
    Some(true)
}

pub(in crate::compilation::analyzer::lua) fn try_defer_dynamic_key_element_widening(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
    element_type: &LuaType,
) -> Option<bool> {
    let (owner, key) = dynamic_key_owner_and_member_key(analyzer, index_expr)?;
    record_pending_dynamic_key_collection_widening(analyzer, owner, key, element_type);
    Some(true)
}

pub(in crate::compilation::analyzer::lua) fn dynamic_key_owner_and_member_key(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
) -> Option<(LuaMemberOwner, LuaMemberKey)> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let owner = direct_local_table_prefix_member_owner(analyzer, &prefix_expr).or_else(|| {
        let prefix_type = analyzer.infer_expr(&prefix_expr).ok()?;
        get_member_owner_for_prefix_type(prefix_type)
    })?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let member_key =
        LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key).ok()?;
    member_key.is_expr().then_some((owner, member_key))
}

pub(in crate::compilation::analyzer::lua) fn direct_local_table_prefix_member_owner(
    analyzer: &mut LuaAnalyzer,
    prefix_expr: &LuaExpr,
) -> Option<LuaMemberOwner> {
    let decl_id = direct_local_prefix_decl_id(analyzer, prefix_expr)?;
    if local_decl_has_declared_type(analyzer, decl_id) {
        return None;
    }

    if let Some(cached_owner) = analyzer
        .direct_local_table_member_owner_cache
        .get(&decl_id)
        .cloned()
    {
        return cached_owner;
    }

    let owner = resolve_direct_local_table_prefix_member_owner(analyzer, decl_id);
    analyzer
        .direct_local_table_member_owner_cache
        .insert(decl_id, owner.clone());
    owner
}

pub(in crate::compilation::analyzer::lua) fn direct_local_prefix_has_declared_type(
    analyzer: &LuaAnalyzer,
    prefix_expr: &LuaExpr,
) -> bool {
    direct_local_prefix_decl_id(analyzer, prefix_expr)
        .is_some_and(|decl_id| local_decl_has_declared_type(analyzer, decl_id))
}

fn direct_local_prefix_decl_id(analyzer: &LuaAnalyzer, prefix_expr: &LuaExpr) -> Option<LuaDeclId> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };
    analyzer
        .db
        .get_reference_index()
        .get_local_reference(&analyzer.file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
}

fn local_decl_has_declared_type(analyzer: &LuaAnalyzer, decl_id: LuaDeclId) -> bool {
    analyzer
        .db
        .get_type_index()
        .get_type_cache(&decl_id.into())
        .is_some_and(LuaTypeCache::is_doc)
}

fn resolve_direct_local_table_prefix_member_owner(
    analyzer: &LuaAnalyzer,
    decl_id: LuaDeclId,
) -> Option<LuaMemberOwner> {
    if is_local_mutable(analyzer, decl_id) {
        return None;
    }

    let decl = analyzer.db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    if initializer.get_ret_idx() != 0 {
        return None;
    }
    let root = analyzer
        .db
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let node = initializer.get_expr_syntax_id().to_node_from_root(&root)?;
    let table_expr = LuaTableExpr::cast(node)?;
    Some(LuaMemberOwner::Element(InFiled::new(
        decl_id.file_id,
        table_expr.get_range(),
    )))
}

pub(in crate::compilation::analyzer::lua) fn record_pending_dynamic_key_collection_widening(
    analyzer: &mut LuaAnalyzer,
    owner: LuaMemberOwner,
    key: LuaMemberKey,
    additional_base: &LuaType,
) {
    let cache_key = DynamicKeyCollectionWideningKey { owner, key };
    let additional_base = crate::widen_literal_type_for_assignment(additional_base);
    let widened_base = match analyzer
        .pending_dynamic_key_collection_widenings
        .remove(&cache_key)
    {
        Some(current) => TypeOps::Union.apply(analyzer.db, &current, &additional_base),
        None => additional_base,
    };
    analyzer
        .pending_dynamic_key_collection_widenings
        .insert(cache_key, widened_base);
}

pub(in crate::compilation::analyzer::lua) fn find_related_member_ids(
    analyzer: &mut LuaAnalyzer,
    index_expr: LuaIndexExpr,
) -> Option<Vec<LuaMemberId>> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let prefix_type = analyzer.infer_expr(&prefix_expr).ok()?;
    let owner = get_member_owner_for_prefix_type(prefix_type)?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let member_key =
        LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key).ok()?;
    let members = if member_key.is_expr() {
        let access_key_type = member_key_as_expr_type(&member_key)?;
        analyzer
            .db
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .into_iter()
            .filter(|member| {
                member_key_matches_type(analyzer.db, access_key_type, member.get_key())
            })
            .collect::<Vec<_>>()
    } else {
        analyzer
            .db
            .get_member_index()
            .get_members_for_owner_key(&owner, &member_key)
    };

    if members.is_empty() {
        return None;
    }

    Some(members.into_iter().map(|member| member.get_id()).collect())
}

pub(in crate::compilation::analyzer::lua) fn member_key_as_expr_type(
    member_key: &LuaMemberKey,
) -> Option<&LuaType> {
    match member_key {
        LuaMemberKey::ExprType(typ) => Some(typ),
        _ => None,
    }
}

pub(in crate::compilation::analyzer::lua) fn get_member_owner_for_prefix_type(
    prefix_type: LuaType,
) -> Option<LuaMemberOwner> {
    resolve_index_expr_member_owner_for_file(&prefix_type, None).map(|(owner, _)| owner)
}

pub(in crate::compilation::analyzer) fn resolve_index_expr_member_owner_for_file(
    prefix_type: &LuaType,
    preferred_file_id: Option<FileId>,
) -> Option<(LuaMemberOwner, bool)> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => {
            Some((LuaMemberOwner::Element(in_file_range.clone()), false))
        }
        LuaType::Def(def_id) => Some((LuaMemberOwner::Type(def_id.clone()), false)),
        LuaType::Ref(ref_id) => Some((LuaMemberOwner::Type(ref_id.clone()), true)),
        LuaType::Instance(instance) => {
            Some((LuaMemberOwner::Element(instance.get_range().clone()), false))
        }
        LuaType::TableOf(inner) => {
            resolve_index_expr_member_owner_for_file(inner, preferred_file_id)
        }
        LuaType::TypeGuard(inner) => {
            resolve_index_expr_member_owner_for_file(inner, preferred_file_id)
        }
        LuaType::Union(union) => {
            pick_preferred_index_expr_member_owner(union.types(), preferred_file_id)
        }
        LuaType::Intersection(intersection) => pick_preferred_index_expr_member_owner(
            intersection.get_types().iter(),
            preferred_file_id,
        ),
        LuaType::MergedTable(merged_table) => pick_preferred_index_expr_member_owner(
            merged_table.get_types().iter(),
            preferred_file_id,
        ),
        LuaType::MultiLineUnion(union) => pick_preferred_index_expr_member_owner(
            union.get_unions().iter().map(|(typ, _)| typ),
            preferred_file_id,
        ),
        _ => None,
    }
}

fn pick_preferred_index_expr_member_owner<'a>(
    types: impl Iterator<Item = &'a LuaType>,
    preferred_file_id: Option<FileId>,
) -> Option<(LuaMemberOwner, bool)> {
    let mut exact_type_owner = None;
    let mut fallback_owner = None;
    for typ in types {
        let Some(owner_info) = resolve_index_expr_member_owner_for_file(typ, preferred_file_id)
        else {
            continue;
        };

        if owner_matches_preferred_file(&owner_info.0, preferred_file_id) {
            return Some(owner_info);
        }

        if matches!(&owner_info.0, LuaMemberOwner::Type(_)) && !owner_info.1 {
            if exact_type_owner.is_none() {
                exact_type_owner = Some(owner_info);
            }
            continue;
        }

        if fallback_owner.is_none() {
            fallback_owner = Some(owner_info);
        }
    }

    exact_type_owner.or(fallback_owner)
}

pub(in crate::compilation::analyzer::lua) fn owner_matches_preferred_file(
    owner: &LuaMemberOwner,
    preferred_file_id: Option<FileId>,
) -> bool {
    let Some(preferred_file_id) = preferred_file_id else {
        return false;
    };

    matches!(owner, LuaMemberOwner::Element(range) if range.file_id == preferred_file_id)
}

pub(in crate::compilation::analyzer::lua) fn is_collection_append_write(
    index_expr: &LuaIndexExpr,
) -> Option<bool> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let LuaIndexKey::Expr(index_key_expr) = index_expr.get_index_key()? else {
        return Some(false);
    };
    let LuaExpr::BinaryExpr(binary_expr) = index_key_expr else {
        return Some(false);
    };
    if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpAdd {
        return Some(false);
    }

    let (left, right) = binary_expr.get_exprs()?;
    if !is_literal_integer_one(&right) {
        return Some(false);
    }

    let LuaExpr::UnaryExpr(unary_expr) = left else {
        return Some(false);
    };
    if unary_expr.get_op_token()?.get_op() != UnaryOperator::OpLen {
        return Some(false);
    }

    let len_expr = unary_expr.get_expr()?;
    Some(expr_access_path(&prefix_expr) == expr_access_path(&len_expr))
}

pub(in crate::compilation::analyzer::lua) fn expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

pub(in crate::compilation::analyzer::lua) fn is_literal_integer_one(expr: &LuaExpr) -> bool {
    let LuaExpr::LiteralExpr(literal_expr) = expr else {
        return false;
    };

    matches!(
        literal_expr.get_literal(),
        Some(LuaLiteralToken::Number(number))
            if matches!(number.get_number_value(), NumberResult::Int(1))
    )
}

pub(in crate::compilation::analyzer::lua) fn widen_member_collections_with_collection_type(
    analyzer: &mut LuaAnalyzer,
    member_ids: &[LuaMemberId],
    incoming_type: &LuaType,
) -> Option<()> {
    let incoming_array = normalize_infer_collection_type(analyzer.db, incoming_type)?;

    for member_id in member_ids {
        let existing_cache = analyzer
            .db
            .get_type_index()
            .get_type_cache(&(*member_id).into())
            .cloned()?;
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) =
            normalize_infer_collection_type(analyzer.db, existing_cache.as_type())
        else {
            continue;
        };

        let widened_base = TypeOps::Union.apply(
            analyzer.db,
            existing_array.get_base(),
            incoming_array.get_base(),
        );
        write_type_cache(
            analyzer.db,
            (*member_id).into(),
            LuaTypeCache::InferType(LuaType::Array(
                LuaArrayType::from_base_type(widened_base).into(),
            )),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }

    Some(())
}

pub(in crate::compilation::analyzer::lua) fn widen_member_collections_with_element_type(
    analyzer: &mut LuaAnalyzer,
    member_ids: &[LuaMemberId],
    element_type: &LuaType,
) -> Option<()> {
    for member_id in member_ids {
        let existing_cache = analyzer
            .db
            .get_type_index()
            .get_type_cache(&(*member_id).into())
            .cloned()?;
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) =
            normalize_infer_collection_type(analyzer.db, existing_cache.as_type())
        else {
            continue;
        };

        let widened_base =
            TypeOps::Union.apply(analyzer.db, existing_array.get_base(), element_type);
        write_type_cache(
            analyzer.db,
            (*member_id).into(),
            LuaTypeCache::InferType(LuaType::Array(
                LuaArrayType::from_base_type(widened_base).into(),
            )),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }

    Some(())
}

pub(in crate::compilation::analyzer::lua) fn normalize_infer_collection_type(
    db: &crate::DbIndex,
    typ: &LuaType,
) -> Option<LuaArrayType> {
    match typ {
        LuaType::Array(array) => Some(LuaArrayType::from_base_type(array.get_base().clone())),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => {
            Some(LuaArrayType::from_base_type(tuple.cast_down_array_base(db)))
        }
        LuaType::TypeGuard(inner) => normalize_infer_collection_type(db, inner),
        LuaType::Union(union) => normalize_infer_collection_types(db, union.types()),
        LuaType::Intersection(intersection) => {
            normalize_infer_collection_types(db, intersection.get_types().iter())
        }
        LuaType::MergedTable(merged_table) => {
            normalize_infer_collection_types(db, merged_table.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            normalize_infer_collection_types(db, union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn normalize_infer_collection_types<'a>(
    db: &crate::DbIndex,
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaArrayType> {
    let mut base_type = None;
    for typ in types {
        if typ.is_never() {
            continue;
        }

        let collection = normalize_infer_collection_type(db, typ)?;
        base_type = Some(match base_type {
            Some(current) => TypeOps::Union.apply(db, &current, collection.get_base()),
            None => collection.get_base().clone(),
        });
    }

    base_type.map(LuaArrayType::from_base_type)
}

pub(in crate::compilation::analyzer::lua) fn flush_pending_dynamic_key_collection_widening_for_members(
    analyzer: &mut LuaAnalyzer,
    owner: LuaMemberOwner,
    pending_items: Vec<(LuaMemberKey, LuaType)>,
) {
    let Some(members) = analyzer.db.get_member_index().get_members(&owner) else {
        return;
    };
    let mut member_ids_by_pending_key = vec![Vec::new(); pending_items.len()];

    for member in members {
        for (index, (member_key, _)) in pending_items.iter().enumerate() {
            let Some(access_key_type) = member_key_as_expr_type(member_key) else {
                continue;
            };
            if member_key_matches_type(analyzer.db, access_key_type, member.get_key()) {
                member_ids_by_pending_key[index].push(member.get_id());
            }
        }
    }

    for ((_, additional_base), member_ids) in
        pending_items.into_iter().zip(member_ids_by_pending_key)
    {
        widen_member_collections_with_element_type(analyzer, &member_ids, &additional_base);
    }
}
