mod find_index;
mod find_members;
mod get_member_map;
mod infer_raw_member;

use std::collections::HashSet;

use crate::{
    DbIndex, FileId, GmodRealm, LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaSemanticDeclId,
    TypeOps,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::type_check::check_type_compact,
};
pub use find_index::find_index_operations;
pub use find_members::{
    find_members, find_members_in_workspace_for_file, find_members_in_workspace_for_file_at_offset,
    find_members_with_key, find_members_with_key_in_workspace_for_file,
    find_members_with_key_in_workspace_for_file_at_offset,
};
pub use get_member_map::{
    get_member_map, get_member_map_in_workspace_for_file,
    get_member_map_in_workspace_for_file_at_offset,
};
use glua_parser::{LuaAssignStat, LuaSyntaxKind, LuaTableExpr, LuaTableField};
use glua_parser::{LuaAstNode, LuaIndexExpr};
pub(crate) use infer_raw_member::infer_owner_raw_member_type_with_realm;
pub use infer_raw_member::infer_raw_member_type;
use rowan::{TextRange, TextSize};

use super::{
    InferFailReason, LuaInferCache, SemanticDeclLevel, infer_expr, infer_node_semantic_decl,
    infer_table_should_be,
};

pub fn get_buildin_type_map_type_id(type_: &LuaType) -> Option<LuaTypeDeclId> {
    match type_ {
        LuaType::String
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => Some(LuaTypeDeclId::global("string")),
        LuaType::Io => Some(LuaTypeDeclId::global("io")),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaMemberInfo {
    pub property_owner_id: Option<LuaSemanticDeclId>,
    pub key: LuaMemberKey,
    pub typ: LuaType,
    pub feature: Option<LuaMemberFeature>,
    pub overload_index: Option<usize>,
}

type FindMembersResult = Option<Vec<LuaMemberInfo>>;
type RawGetMemberTypeResult = Result<LuaType, InferFailReason>;

pub(crate) fn intersect_member_types(db: &DbIndex, left: LuaType, right: LuaType) -> LuaType {
    if left == right {
        left
    } else {
        TypeOps::Intersect.apply(db, &left, &right)
    }
}

pub(crate) fn merge_open_table_types(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
    let mut table_components = Vec::new();
    let mut other_types = Vec::new();
    let mut seen = HashSet::new();

    for typ in types {
        if typ.is_never() {
            continue;
        }

        if let LuaType::Union(union) = &typ {
            let mut nested_components = Vec::new();
            let mut all_components_are_tables = true;
            for component in union.into_vec() {
                if is_open_table_merge_component(&component) {
                    nested_components.push(component);
                } else {
                    all_components_are_tables = false;
                    break;
                }
            }

            if all_components_are_tables {
                for component in nested_components {
                    if seen.insert(component.clone()) {
                        table_components.push(component);
                    }
                }
            } else if seen.insert(typ.clone()) {
                other_types.push(typ);
            }
            continue;
        }

        if let LuaType::MergedTable(merged) = &typ {
            for component in merged.get_types() {
                if seen.insert(component.clone()) {
                    table_components.push(component.clone());
                }
            }
            continue;
        }

        if is_open_table_merge_component(&typ) {
            if seen.insert(typ.clone()) {
                table_components.push(typ);
            }
        } else if seen.insert(typ.clone()) {
            other_types.push(typ);
        }
    }

    let table_type = merge_open_table_components(table_components);
    let mut result = table_type;
    for typ in other_types {
        result = match result {
            Some(existing) => Some(TypeOps::Union.apply(db, &existing, &typ)),
            None => Some(typ),
        };
    }

    result.unwrap_or(LuaType::Never)
}

fn merge_open_table_components(mut components: Vec<LuaType>) -> Option<LuaType> {
    if components
        .iter()
        .any(|component| !matches!(component, LuaType::Table))
    {
        components.retain(|component| !matches!(component, LuaType::Table));
    }

    match components.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        _ => Some(crate::LuaMergedTableType::new(components).into()),
    }
}

fn is_open_table_merge_component(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Table | LuaType::TableConst(_) | LuaType::Object(_) | LuaType::MergedTable(_)
    )
}

#[derive(Debug, Clone)]
pub(crate) struct DynamicFieldResolution {
    pub typ: LuaType,
    pub semantic_decl: Option<LuaSemanticDeclId>,
}

pub(crate) fn resolve_dynamic_field_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
    access_position: Option<TextSize>,
) -> Option<DynamicFieldResolution> {
    if !db.get_emmyrc().gmod.enabled || !db.get_emmyrc().gmod.infer_dynamic_fields {
        return None;
    }

    let cache_key = (prefix_type.clone(), member_key.clone(), access_position);
    if let Some(cached) = cache.dynamic_field_resolution_cache.get(&cache_key) {
        return cached
            .clone()
            .map(|(typ, semantic_decl)| DynamicFieldResolution { typ, semantic_decl });
    }

    let field_name = member_key.get_name()?;
    let definitions = dynamic_field_definitions(
        db,
        cache.get_file_id(),
        prefix_type,
        field_name,
        access_position,
    );
    if definitions.is_empty() {
        cache.dynamic_field_resolution_cache.insert(cache_key, None);
        return None;
    }

    let mut member_types = Vec::new();
    let mut semantic_decl = None;
    for definition in definitions {
        let Some(member_id) = dynamic_field_member_id(db, definition.file_id, definition.value)
        else {
            continue;
        };
        if semantic_decl.is_none() {
            semantic_decl = Some(LuaSemanticDeclId::Member(member_id));
        }
        if let Some(typ) = dynamic_field_member_type(db, cache, &member_id) {
            member_types.push(typ);
        }
    }

    let typ = match member_types.as_slice() {
        [] => LuaType::Any,
        [only] => only.clone(),
        _ => LuaType::from_vec(member_types),
    };
    cache
        .dynamic_field_resolution_cache
        .insert(cache_key, Some((typ.clone(), semantic_decl.clone())));
    Some(DynamicFieldResolution { typ, semantic_decl })
}

pub(crate) fn resolve_dynamic_field_member_for_file(
    db: &DbIndex,
    caller_file_id: FileId,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
) -> Option<DynamicFieldResolution> {
    let mut cache = LuaInferCache::new(caller_file_id, Default::default());
    resolve_dynamic_field_member(db, &mut cache, prefix_type, member_key, None)
}

fn dynamic_field_member_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_id: &LuaMemberId,
) -> Option<LuaType> {
    if let Some(cached) = cache.dynamic_field_type_cache.get(member_id) {
        return cached.clone();
    }

    if !cache.dynamic_field_resolving.insert(*member_id) {
        return None;
    }

    let result = dynamic_field_member_type_inner(db, cache, member_id);
    cache.dynamic_field_resolving.remove(member_id);
    cache
        .dynamic_field_type_cache
        .insert(*member_id, result.clone());
    result
}

fn dynamic_field_member_type_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_id: &LuaMemberId,
) -> Option<LuaType> {
    if let Some(type_cache) = db.get_type_index().get_type_cache(&(*member_id).into()) {
        let typ = type_cache.as_type().clone();
        if !matches!(
            typ,
            LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Table
        ) {
            return Some(typ);
        }
    }

    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(table_field) = LuaTableField::cast(node.clone()) {
        let value_expr = table_field.get_value_expr()?;
        let mut definition_cache = dynamic_field_definition_cache(cache, member_id.file_id);
        return infer_expr(db, &mut definition_cache, value_expr).ok();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        let assign_node = index_expr.syntax().parent()?;
        let assign_stat = LuaAssignStat::cast(assign_node)?;
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (var, expr) in vars.iter().zip(exprs.iter()) {
            if var.syntax().text_range() == node.text_range() {
                let mut definition_cache = dynamic_field_definition_cache(cache, member_id.file_id);
                return infer_expr(db, &mut definition_cache, expr.clone()).ok();
            }
        }
    }

    None
}

fn dynamic_field_definition_cache(cache: &LuaInferCache, file_id: FileId) -> LuaInferCache {
    let mut definition_cache = LuaInferCache::new(file_id, cache.get_config().clone());
    definition_cache.dynamic_field_type_cache = cache.dynamic_field_type_cache.clone();
    definition_cache.dynamic_field_resolving = cache.dynamic_field_resolving.clone();
    definition_cache
}

fn dynamic_field_definitions(
    db: &DbIndex,
    caller_file_id: FileId,
    prefix_type: &LuaType,
    field_name: &str,
    access_position: Option<TextSize>,
) -> Vec<crate::InFiled<TextRange>> {
    match prefix_type {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => dynamic_field_definitions_for_owner(
            db,
            caller_file_id,
            &crate::DynamicFieldOwner::Type(type_id.clone()),
            field_name,
            access_position,
        ),
        LuaType::TableConst(table_range) => dynamic_field_definitions_for_owner(
            db,
            caller_file_id,
            &crate::DynamicFieldOwner::Table(table_range.clone()),
            field_name,
            access_position,
        ),
        LuaType::Instance(instance) => dynamic_field_definitions(
            db,
            caller_file_id,
            instance.get_base(),
            field_name,
            access_position,
        ),
        _ => Vec::new(),
    }
}

fn dynamic_field_definitions_for_owner(
    db: &DbIndex,
    caller_file_id: FileId,
    owner: &crate::DynamicFieldOwner,
    field_name: &str,
    access_position: Option<TextSize>,
) -> Vec<crate::InFiled<TextRange>> {
    let dynamic_fields_global = db.get_emmyrc().gmod.dynamic_fields_global;
    let caller_realm = infer_dynamic_field_caller_realm(db, &caller_file_id);
    db.get_dynamic_field_index()
        .get_field_definitions(owner, field_name)
        .into_iter()
        .filter(|definition| dynamic_fields_global || definition.file_id == caller_file_id)
        .filter(|definition| is_dynamic_field_realm_compatible(db, caller_realm, definition))
        .filter(|definition| {
            dynamic_field_definition_visible_at(db, caller_file_id, definition, access_position)
        })
        .collect()
}

fn dynamic_field_definition_visible_at(
    db: &DbIndex,
    caller_file_id: FileId,
    definition: &crate::InFiled<TextRange>,
    access_position: Option<TextSize>,
) -> bool {
    let Some(access_position) = access_position else {
        return true;
    };
    if definition.file_id != caller_file_id {
        return true;
    }
    if definition.value.start() > access_position {
        return false;
    }
    !definition_enclosing_assignment_contains(db, definition, access_position)
}

fn definition_enclosing_assignment_contains(
    db: &DbIndex,
    definition: &crate::InFiled<TextRange>,
    access_position: TextSize,
) -> bool {
    let Some(root) = db.get_vfs().get_syntax_tree(&definition.file_id) else {
        return false;
    };
    let root = root.get_red_root();
    let Some(token) = root
        .token_at_offset(definition.value.start())
        .right_biased()
    else {
        return false;
    };
    token
        .parent_ancestors()
        .find_map(LuaAssignStat::cast)
        .is_some_and(|assign_stat| {
            let range = assign_stat.get_range();
            range.contains(access_position) && range != definition.value
        })
}

fn infer_dynamic_field_caller_realm(db: &DbIndex, caller_file_id: &FileId) -> GmodRealm {
    db.get_gmod_infer_index()
        .get_realm_file_metadata(caller_file_id)
        .map(|metadata| metadata.inferred_realm)
        .unwrap_or(GmodRealm::Unknown)
}

fn is_dynamic_field_realm_compatible(
    db: &DbIndex,
    caller_realm: GmodRealm,
    definition: &crate::InFiled<TextRange>,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }

    let definition_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&definition.file_id, definition.value.start());
    !matches!(
        (caller_realm, definition_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

fn dynamic_field_member_id(db: &DbIndex, file_id: FileId, range: TextRange) -> Option<LuaMemberId> {
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
    let token = root.token_at_offset(range.start()).right_biased()?;
    let mut current = token.parent();
    while let Some(node) = current {
        if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
            // Legacy: range matches the full index expression
            if index_expr.get_range() == range {
                return Some(LuaMemberId::new(index_expr.get_syntax_id(), file_id));
            }
            // New: range matches the index key (field name) within this expression
            if let Some(key) = index_expr.get_index_key() {
                if key.get_range() == Some(range) {
                    return Some(LuaMemberId::new(index_expr.get_syntax_id(), file_id));
                }
            }
        }
        if let Some(table_field) = LuaTableField::cast(node.clone()) {
            if table_field.get_range() == range {
                return Some(LuaMemberId::new(table_field.get_syntax_id(), file_id));
            }
            if let Some(key) = table_field.get_field_key()
                && key.get_range() == Some(range)
            {
                return Some(LuaMemberId::new(table_field.get_syntax_id(), file_id));
            }
        }
        current = node.parent();
    }

    None
}

pub(crate) fn member_key_as_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::None => None,
        LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
        LuaMemberKey::Name(name) => Some(LuaType::StringConst(name.clone().into())),
        LuaMemberKey::ExprType(typ) => Some(typ.clone()),
    }
}

pub(crate) fn member_key_matches_type(
    db: &DbIndex,
    access_key_type: &LuaType,
    member_key: &LuaMemberKey,
) -> bool {
    if let LuaMemberKey::ExprType(member_key_type) = member_key
        && member_key_type.is_unknown()
    {
        return unknown_index_key_matches_access(access_key_type);
    }

    let Some(member_key_type) = member_key_as_type(member_key) else {
        return false;
    };
    if let Some(is_match) = exact_literal_member_key_match(access_key_type, &member_key_type) {
        return is_match;
    }

    check_type_compact(db, access_key_type, &member_key_type).is_ok()
        || check_type_compact(db, &member_key_type, access_key_type).is_ok()
}

fn exact_literal_member_key_match(
    access_key_type: &LuaType,
    member_key_type: &LuaType,
) -> Option<bool> {
    match (access_key_type, member_key_type) {
        (LuaType::StringConst(left), LuaType::StringConst(right))
        | (LuaType::StringConst(left), LuaType::DocStringConst(right))
        | (LuaType::DocStringConst(left), LuaType::StringConst(right))
        | (LuaType::DocStringConst(left), LuaType::DocStringConst(right)) => Some(left == right),
        (LuaType::IntegerConst(left), LuaType::IntegerConst(right))
        | (LuaType::IntegerConst(left), LuaType::DocIntegerConst(right))
        | (LuaType::DocIntegerConst(left), LuaType::IntegerConst(right))
        | (LuaType::DocIntegerConst(left), LuaType::DocIntegerConst(right)) => Some(left == right),
        (LuaType::BooleanConst(left), LuaType::BooleanConst(right))
        | (LuaType::BooleanConst(left), LuaType::DocBooleanConst(right))
        | (LuaType::DocBooleanConst(left), LuaType::BooleanConst(right))
        | (LuaType::DocBooleanConst(left), LuaType::DocBooleanConst(right)) => Some(left == right),
        _ => None,
    }
}

fn unknown_index_key_matches_access(access_key_type: &LuaType) -> bool {
    match access_key_type {
        LuaType::Any | LuaType::Unknown | LuaType::String | LuaType::Number | LuaType::Integer => {
            true
        }
        LuaType::TypeGuard(inner) => unknown_index_key_matches_access(inner),
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .any(unknown_index_key_matches_access),
        _ => false,
    }
}

pub fn find_member_origin_owner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
) -> Option<LuaSemanticDeclId> {
    find_member_origin_owner_inner(db, infer_config, member_id, None)
}

pub fn find_member_origin_owner_at_offset(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
    caller_position: rowan::TextSize,
) -> Option<LuaSemanticDeclId> {
    find_member_origin_owner_inner(db, infer_config, member_id, Some(caller_position))
}

fn find_member_origin_owner_inner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    const MAX_ITERATIONS: usize = 50;
    let mut visited_members = HashSet::new();

    let mut current_owner = resolve_member_owner(db, infer_config, &member_id, caller_position);
    let mut final_owner = current_owner.clone();
    let mut iteration_count = 0;

    while let Some(LuaSemanticDeclId::Member(current_member_id)) = &current_owner {
        if visited_members.contains(current_member_id) || iteration_count >= MAX_ITERATIONS {
            break;
        }

        visited_members.insert(*current_member_id);
        iteration_count += 1;

        match resolve_member_owner(db, infer_config, current_member_id, caller_position) {
            Some(next_owner) => {
                final_owner = Some(next_owner.clone());
                current_owner = Some(next_owner);
            }
            None => break,
        }
    }

    final_owner
}

fn resolve_member_owner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: &LuaMemberId,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let current_node = member_id.get_syntax_id().to_node_from_root(&root)?;
    match member_id.get_syntax_id().get_kind() {
        LuaSyntaxKind::TableFieldAssign => {
            if LuaTableField::can_cast(current_node.kind().into()) {
                let table_field = LuaTableField::cast(current_node.clone())?;
                // 如果表是类, 那么通过类型推断获取 owner
                if let Some(owner_id) = resolve_table_field_through_type_inference(
                    db,
                    infer_config,
                    &table_field,
                    caller_position,
                ) {
                    return Some(owner_id);
                }
                // 非类, 那么通过右值推断
                let value_expr = table_field.get_value_expr()?;
                let value_node = value_expr.get_syntax_id().to_node_from_root(&root)?;
                infer_node_semantic_decl(db, infer_config, value_node, SemanticDeclLevel::default())
            } else {
                None
            }
        }
        LuaSyntaxKind::IndexExpr => {
            let assign_node = current_node.parent()?;
            let assign_stat = LuaAssignStat::cast(assign_node)?;
            let (vars, exprs) = assign_stat.get_var_and_expr_list();

            for (var, expr) in vars.iter().zip(exprs.iter()) {
                if var.syntax().text_range() == current_node.text_range() {
                    let expr_node = expr.get_syntax_id().to_node_from_root(&root)?;
                    return infer_node_semantic_decl(
                        db,
                        infer_config,
                        expr_node,
                        SemanticDeclLevel::default(),
                    );
                }
            }
            None
        }
        _ => None,
    }
}

fn resolve_table_field_through_type_inference(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    table_field: &LuaTableField,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let parent = table_field.syntax().parent()?;
    let table_expr = LuaTableExpr::cast(parent)?;
    let table_type = infer_table_should_be(db, infer_config, table_expr).ok()?;

    if !matches!(table_type, LuaType::Ref(_) | LuaType::Def(_)) {
        return None;
    }

    let field_key = table_field.get_field_key()?;
    let key = LuaMemberKey::from_index_key(db, infer_config, &field_key).ok()?;
    let caller_file_id = infer_config.get_file_id();
    let workspace_id = db.get_module_index().get_workspace_id(caller_file_id);
    let member_infos = match (workspace_id, caller_position) {
        (Some(workspace_id), Some(caller_position)) => {
            find_members_with_key_in_workspace_for_file_at_offset(
                db,
                &table_type,
                key,
                false,
                workspace_id,
                caller_file_id,
                caller_position,
            )?
        }
        (Some(workspace_id), None) => find_members_with_key_in_workspace_for_file(
            db,
            &table_type,
            key,
            false,
            workspace_id,
            caller_file_id,
        )?,
        (None, _) => find_members_with_key(db, &table_type, key, false)?,
    };

    member_infos
        .first()
        .cloned()
        .and_then(|m| m.property_owner_id)
}
