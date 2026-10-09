use crate::compilation::analyzer::lua::StatementWrite;
use crate::{
    DbIndex, FileId, GlobalId, InFiled, LuaArrayType, LuaMemberKey, LuaTypeCache, TypeOps,
    compilation::analyzer::common::{TypeCacheWriteMode, write_type_cache},
    db_index::{LuaDeclId, LuaMemberId, LuaMemberOwner, LuaType},
    semantic::member_key_matches_type,
};
use glua_parser::{
    BinaryOperator, LuaAstNode, LuaExpr, LuaIndexExpr, LuaIndexKey, LuaLiteralToken, LuaTableExpr,
    LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};

use super::super::{LuaAnalyzer, stats::is_local_mutable};

/// The collection type `member_id` holds once `incoming` joins the collections
/// the other members of its slot hold, when any of them holds one.
pub(in crate::compilation::analyzer) fn widened_member_assignment_collection_type(
    db: &DbIndex,
    member_id: LuaMemberId,
    incoming: &LuaType,
) -> Option<LuaType> {
    let incoming_array = normalize_infer_collection_type(db, incoming)?;
    let member_index = db.get_member_index();
    let owner = member_index.get_member_owner(&member_id)?.clone();
    let key = member_index.get_member(&member_id)?.get_key().clone();
    let mut widened_base = incoming_array.get_base().clone();
    let mut saw_related_collection = false;
    for related_member in member_index.get_members_for_owner_key(&owner, &key) {
        let related_member_id = related_member.get_id();
        if related_member_id == member_id
            || !is_member_realm_compatible(db, member_id, related_member_id)
        {
            continue;
        }
        let Some(existing_cache) = db
            .get_type_index()
            .get_type_cache(&related_member_id.into())
            .cloned()
        else {
            continue;
        };
        if !existing_cache.is_infer() {
            continue;
        }
        let Some(existing_array) = normalize_infer_collection_type(db, existing_cache.as_type())
        else {
            continue;
        };
        saw_related_collection = true;
        widened_base = TypeOps::Union.apply(db, existing_array.get_base(), &widened_base);
    }

    saw_related_collection
        .then(|| LuaType::Array(LuaArrayType::from_base_type(widened_base).into()))
}

fn is_member_realm_compatible(
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

/// A slot whose dynamic-key element writes widen its collection once, when
/// the analysis ends.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::compilation::analyzer::lua) struct DynamicKeyCollectionWideningKey {
    pub(in crate::compilation::analyzer::lua) owner: LuaMemberOwner,
    pub(in crate::compilation::analyzer::lua) key: LuaMemberKey,
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
    let incoming_is_collection =
        normalize_infer_collection_type(&analyzer.db, value_type).is_some();

    if !incoming_is_collection && !is_collection_append {
        return Some(());
    }

    // A value stored under a key that varies at runtime is one of the table's
    // elements, not a collection its other writers reset: another row is no
    // evidence this one is a list. Only an append into it is.
    let dynamic_key =
        incoming_is_collection && dynamic_key_owner_and_member_key(analyzer, index_expr).is_some();
    if incoming_is_collection
        && !dynamic_key
        && let Some(mut member_ids) = find_related_member_ids(analyzer, index_expr.clone())
    {
        // The written member holds this value, not a collection it joins.
        let written = LuaMemberId::new(index_expr.get_syntax_id(), analyzer.file_id);
        member_ids.retain(|member_id| *member_id != written);
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
        get_member_owner_for_prefix_type(&analyzer.db, prefix_type)
    })?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let member_key =
        LuaMemberKey::from_index_key_or_unknown(&analyzer.db, cache, &index_key).ok()?;
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
    let node = analyzer
        .db
        .get_vfs()
        .syntax_node_around(&decl_id.file_id, initializer.get_expr_syntax_id())?;
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
        Some(current) => TypeOps::Union.apply(&analyzer.db, &current, &additional_base),
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
    let owner = get_member_owner_for_prefix_type(&analyzer.db, prefix_type)?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let member_key =
        LuaMemberKey::from_index_key_or_unknown(&analyzer.db, cache, &index_key).ok()?;
    let members = if member_key.is_expr() {
        let access_key_type = member_key_as_expr_type(&member_key)?;
        analyzer
            .db
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .into_iter()
            .filter(|member| {
                member_key_matches_type(&analyzer.db, access_key_type, member.get_key())
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
    db: &DbIndex,
    prefix_type: LuaType,
) -> Option<LuaMemberOwner> {
    resolve_index_expr_member_owner_for_file(db, &prefix_type, None).map(|(owner, _)| owner)
}

pub(in crate::compilation::analyzer) fn resolve_index_expr_member_owner_for_file(
    db: &DbIndex,
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
            resolve_index_expr_member_owner_for_file(db, inner, preferred_file_id)
        }
        LuaType::TypeGuard(inner) => {
            resolve_index_expr_member_owner_for_file(db, inner, preferred_file_id)
        }
        LuaType::Union(union) => {
            pick_preferred_index_expr_member_owner(db, union.types(), preferred_file_id)
        }
        LuaType::Intersection(intersection) => pick_preferred_index_expr_member_owner(
            db,
            intersection.get_types().iter(),
            preferred_file_id,
        ),
        LuaType::MergedTable(merged_table) => pick_preferred_index_expr_member_owner(
            db,
            merged_table.get_types().iter(),
            preferred_file_id,
        ),
        LuaType::MultiLineUnion(union) => pick_preferred_index_expr_member_owner(
            db,
            union.get_unions().iter().map(|(typ, _)| typ),
            preferred_file_id,
        ),
        _ => None,
    }
}

fn pick_preferred_index_expr_member_owner<'a>(
    db: &DbIndex,
    types: impl Iterator<Item = &'a LuaType>,
    preferred_file_id: Option<FileId>,
) -> Option<(LuaMemberOwner, bool)> {
    // Guarded bootstraps of one property (`x.y = x.y or {}` spelled in several
    // files) all initialise the same table: the `or {}` branch only fires when
    // the property is still nil, so whichever file runs first creates it and
    // the rest reuse it. Homing a member on whichever literal the writing file
    // prefers makes the owner — and, through `is_guarded_table_bootstrap_branch`
    // pruning the memberless literals, the slot's surviving `TableConst` arm —
    // a function of file order. When every owner-bearing arm of the union is
    // such a bootstrap, elect the lowest-sorting literal instead — the same
    // writer `canonical_guarded_table_bootstrap_type` settles the slot's type
    // on — so the attachment and the surviving arm agree and neither moves
    // with the batch. Every other shape keeps the legacy preference below.
    let candidates: Vec<_> = types
        .into_iter()
        .filter_map(|typ| {
            let owner_info = resolve_index_expr_member_owner_for_file(db, typ, preferred_file_id)?;
            Some((typ, owner_info))
        })
        .collect();

    if candidates.len() >= 2 {
        let all_guarded_bootstraps = candidates.iter().all(|(typ, _)| match typ {
            LuaType::TableConst(range) => {
                crate::compilation::analyzer::is_guarded_table_definition_site(db, range)
            }
            _ => false,
        });
        if all_guarded_bootstraps {
            let canonical = candidates
                .iter()
                .filter_map(|(typ, _)| match typ {
                    LuaType::TableConst(range) => Some(range),
                    _ => None,
                })
                .min_by_key(|range| {
                    (
                        range.file_id.id,
                        u32::from(range.value.start()),
                        u32::from(range.value.end()),
                    )
                })
                .map(|range| (*range).clone());
            return canonical.map(|range| (LuaMemberOwner::Element(range), false));
        }
    }

    let mut exact_type_owner = None;
    let mut fallback_owner = None;
    for (_, owner_info) in candidates {
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

pub(in crate::compilation::analyzer::lua) fn expr_access_path(
    expr: &LuaExpr,
) -> Option<smol_str::SmolStr> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

/// The largest number of alias hops [`alias_target_global_path`] follows.
const ALIAS_TARGET_WALK_DEPTH: u8 = 4;

/// The registered global path a member write's prefix names through local
/// aliases, read purely syntactically.
///
/// `local Repair = Glide.Repair` names the same table as `Glide.Repair`, so a
/// write through `Repair` is a path write for provenance purposes. A local
/// declaration qualifies only when its initializer is itself a plain name or
/// index expression — a call, a table literal, or a function may produce a
/// table the path does not name. The terminal path must already be a
/// registered global path, and no inference runs: this answers while the
/// write is being filed, before any type is settled, and without gating on
/// which class currently wins the path.
pub(in crate::compilation::analyzer) fn alias_target_global_path(
    db: &DbIndex,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> Option<GlobalId> {
    alias_target_global_path_depth(db, file_id, prefix_expr, ALIAS_TARGET_WALK_DEPTH)
}

fn alias_target_global_path_depth(
    db: &DbIndex,
    file_id: FileId,
    expr: &LuaExpr,
    depth: u8,
) -> Option<GlobalId> {
    if depth == 0 {
        return None;
    }
    match expr {
        LuaExpr::IndexExpr(index_expr) => {
            registered_global_path(db, &index_expr.get_access_path()?)
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|file_refs| file_refs.get_decl_id(&name_expr.get_range()))
                .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id));
            match decl {
                Some(decl) if decl.is_local() => {
                    let initializer = decl.get_initializer()?.get_expr_syntax_id();
                    let initializer = LuaExpr::cast(
                        db.get_vfs()
                            .syntax_node_around(&decl.get_file_id(), initializer)?,
                    )?;
                    alias_target_global_path_depth(db, decl.get_file_id(), &initializer, depth - 1)
                }
                _ => registered_global_path(db, &name_expr.get_access_path()?),
            }
        }
        _ => None,
    }
}

fn registered_global_path(db: &DbIndex, path: &str) -> Option<GlobalId> {
    let path = GlobalId::new(path);
    db.get_member_index()
        .is_known_global_path(&path)
        .then_some(path)
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
    if member_ids.is_empty() {
        return Some(());
    }
    analyzer.write(StatementWrite::WidenCollections {
        member_ids: member_ids.to_vec(),
        incoming: CollectionWidening::Collection(incoming_type.clone()),
    });
    Some(())
}

pub(in crate::compilation::analyzer::lua) fn widen_member_collections_with_element_type(
    analyzer: &mut LuaAnalyzer,
    member_ids: &[LuaMemberId],
    element_type: &LuaType,
) -> Option<()> {
    if member_ids.is_empty() {
        return Some(());
    }
    analyzer.write(StatementWrite::WidenCollections {
        member_ids: member_ids.to_vec(),
        incoming: CollectionWidening::Element(element_type.clone()),
    });
    Some(())
}

/// What widens the members' collection types.
#[derive(Debug, Clone)]
pub(in crate::compilation::analyzer) enum CollectionWidening {
    /// Another collection, whose element type joins theirs.
    Collection(LuaType),
    /// One more element.
    Element(LuaType),
}

/// Widens each member's inferred collection type with `incoming`, against the
/// types the members hold now; stops at the first member holding none.
pub(in crate::compilation::analyzer) fn widen_member_collections(
    db: &mut DbIndex,
    member_ids: &[LuaMemberId],
    incoming: &CollectionWidening,
) -> Option<()> {
    let incoming_base = match incoming {
        CollectionWidening::Collection(incoming_type) => {
            normalize_infer_collection_type(db, incoming_type)?
                .get_base()
                .clone()
        }
        CollectionWidening::Element(element_type) => element_type.clone(),
    };
    for member_id in member_ids {
        let existing_cache = db
            .get_type_index()
            .get_type_cache(&(*member_id).into())
            .cloned()?;
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) = normalize_infer_collection_type(db, existing_cache.as_type())
        else {
            continue;
        };

        let widened_base = TypeOps::Union.apply(db, existing_array.get_base(), &incoming_base);
        write_type_cache(
            db,
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
            if member_key_matches_type(&analyzer.db, access_key_type, member.get_key()) {
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
