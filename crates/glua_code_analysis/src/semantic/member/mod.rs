mod find_index;
mod find_members;
mod get_member_map;
mod infer_raw_member;

use rustc_hash::FxHashSet;

use crate::{
    DbIndex, FileId, GmodStateMask, InFiled, LuaDecl, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaMemberOwner, LuaSemanticDeclId, TypeOps,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::type_check::check_type_compact,
};
pub use find_index::find_index_operations;
pub(crate) use find_members::visible_super_types_in_workspace_for_file_at_offset;
pub use find_members::{
    find_members, find_members_in_workspace_for_file, find_members_in_workspace_for_file_at_offset,
    find_members_with_key, find_members_with_key_in_workspace_for_file,
    find_members_with_key_in_workspace_for_file_at_offset,
};
pub use get_member_map::{
    get_member_map, get_member_map_in_workspace_for_file,
    get_member_map_in_workspace_for_file_at_offset,
};
use glua_parser::{
    LuaAssignStat, LuaExpr, LuaFuncStat, LuaSyntaxKind, LuaSyntaxNode, LuaTableExpr, LuaTableField,
};
use glua_parser::{LuaAstNode, LuaIndexExpr};
pub(crate) use infer_raw_member::{
    infer_owner_raw_member_type_with_realm, resolve_member_item_with_realm,
    source_survives_table_generation_cutoff,
};
pub use infer_raw_member::{infer_raw_member_type, infer_raw_member_type_with_cache};
use rowan::{TextRange, TextSize};

use super::{
    DynamicFieldDefinitionSyntax, InferFailReason, LuaInferCache, SemanticDeclLevel, infer_expr,
    infer_expr_list_value_type_at, infer_node_semantic_decl, infer_table_should_be,
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
    let mut seen = FxHashSet::default();

    for typ in types {
        if typ.is_never() {
            continue;
        }

        if let LuaType::Union(union) = &typ {
            let components = union.into_vec();
            if components.iter().all(is_open_table_merge_component) {
                for component in components {
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

pub(crate) fn local_class_table_member_ids(
    db: &DbIndex,
    type_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
) -> Vec<LuaMemberId> {
    let Some(type_decl) = db.get_type_index().get_type_decl(type_id) else {
        return Vec::new();
    };
    if !type_decl.is_class() {
        return Vec::new();
    }

    // Only declarations bound to this class can back it with a table
    // literal. The type index tracks those bindings on every type-cache
    // write, so resolving them is a lookup of the class's own declarations
    // instead of a scan over every declaration in the class's files.
    let Some(bound_decls) = db.get_type_index().decls_bound_to_type(type_id) else {
        return Vec::new();
    };
    if bound_decls.is_empty() {
        return Vec::new();
    }

    let member_index = db.get_member_index();
    let mut member_ids = Vec::new();
    // A class declared several times in one file names that file once per
    // declaration, and each of them would otherwise resolve the same bindings.
    let mut scanned_files = FxHashSet::default();
    for location in type_decl.get_locations() {
        if !scanned_files.insert(location.file_id) {
            continue;
        }
        let Some(decl_tree) = db.get_decl_index().get_decl_tree(&location.file_id) else {
            continue;
        };
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&location.file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        for decl_id in bound_decls {
            if decl_id.file_id != location.file_id {
                continue;
            }
            let Some(decl) = decl_tree.get_decl(decl_id) else {
                continue;
            };
            // `local_table_decl_member_owner` opens with these two conditions
            // and they are field reads, so they run before the index lookups.
            // Keep them in step with it.
            if decl
                .get_initializer()
                .is_none_or(|initializer| initializer.get_ret_idx() != 0)
            {
                continue;
            }
            let Some(owner) = local_table_decl_member_owner(decl, &root) else {
                continue;
            };
            let Some(member_item) = member_index.get_member_item(&owner, member_key) else {
                continue;
            };
            member_ids.extend(
                member_item
                    .get_member_ids()
                    .into_iter()
                    .filter(|member_id| {
                        member_index
                            .get_member(member_id)
                            .is_some_and(|member| member.get_key() == member_key)
                    }),
            );
        }
    }

    member_ids.sort_by_key(|member_id| {
        let syntax_id = member_id.get_syntax_id();
        (
            member_id.file_id.id,
            u32::from(member_id.get_position()),
            u32::from(syntax_id.get_range().end()),
            syntax_id.get_kind() as u16,
        )
    });
    member_ids.dedup();
    member_ids
}

fn local_table_decl_member_owner(decl: &LuaDecl, root: &LuaSyntaxNode) -> Option<LuaMemberOwner> {
    let initializer = decl.get_initializer()?;
    if initializer.get_ret_idx() != 0 {
        return None;
    }

    let expr_id = initializer.get_expr_syntax_id();
    if !LuaTableExpr::can_cast(expr_id.get_kind()) {
        return None;
    }
    let table_expr = LuaTableExpr::cast(expr_id.to_node_from_root(root)?)?;
    Some(LuaMemberOwner::Element(InFiled::new(
        decl.get_id().file_id,
        table_expr.get_range(),
    )))
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
        LuaType::Table
            | LuaType::TableConst(_)
            | LuaType::Object(_)
            | LuaType::MergedTable(_)
            | LuaType::TableOf(_)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{DbIndex, LuaMemberKey, LuaObjectType, LuaType, LuaTypeDeclId};

    use super::merge_open_table_types;

    #[test]
    fn merge_open_table_types_keeps_tableof_as_table_component() {
        let db = DbIndex::new();
        let table_of = LuaType::TableOf(Box::new(LuaType::Ref(LuaTypeDeclId::global("Entity"))));

        let mut fields = BTreeMap::new();
        fields.insert(LuaMemberKey::Name("flags".into()), LuaType::Table);
        let object = LuaType::Object(LuaObjectType::new_with_fields(fields, Vec::new()).into());

        let merged = merge_open_table_types(&db, vec![table_of.clone(), object.clone()]);
        let LuaType::MergedTable(merged_table) = merged else {
            panic!("expected tableof and object fragments to merge as an open table");
        };

        assert_eq!(merged_table.get_types(), &[table_of, object]);
    }

    #[test]
    fn merge_open_table_types_keeps_mixed_union_alternatives_distinct() {
        let db = DbIndex::new();
        let mut left_fields = BTreeMap::new();
        left_fields.insert(LuaMemberKey::Name("left".into()), LuaType::Integer);
        let left = LuaType::Object(LuaObjectType::new_with_fields(left_fields, Vec::new()).into());
        let mut right_fields = BTreeMap::new();
        right_fields.insert(LuaMemberKey::Name("right".into()), LuaType::String);
        let right =
            LuaType::Object(LuaObjectType::new_with_fields(right_fields, Vec::new()).into());
        let mixed = LuaType::from_vec_structural(vec![left, right, LuaType::BooleanConst(false)]);

        assert_eq!(merge_open_table_types(&db, vec![mixed.clone()]), mixed);
    }

    /// The class→table-owner lookup answers the live decl bindings: editing
    /// the annotation that binds a local table to a class moves the table's
    /// members to the newly bound class without any cache to invalidate.
    #[test]
    fn local_class_table_lookup_follows_decl_rebinds_across_edits() {
        use crate::VirtualWorkspace;

        use super::local_class_table_member_ids;

        const INITIAL: &str = r#"
            ---@class BoundClass
            ---@class OtherClass

            ---@type BoundClass
            local instance = { speed = 10 }

            ---@type OtherClass
            local other = { power = 5 }
        "#;
        const REBOUND: &str = r#"
            ---@class BoundClass
            ---@class OtherClass

            ---@type OtherClass
            local instance = { speed = 10 }

            ---@type OtherClass
            local other = { power = 5 }
        "#;

        let mut ws = VirtualWorkspace::new();
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/bound.lua");
        ws.analysis
            .update_file_by_uri(&uri, Some(INITIAL.to_string()))
            .map(|(id, _)| id)
            .expect("initial file indexed");
        let bound = LuaTypeDeclId::global("BoundClass");
        let other = LuaTypeDeclId::global("OtherClass");
        let speed = LuaMemberKey::Name("speed".into());
        let db = ws.analysis.compilation.get_db();
        assert_eq!(
            local_class_table_member_ids(db, &bound, &speed).len(),
            1,
            "the table bound to BoundClass backs it",
        );
        assert!(
            local_class_table_member_ids(db, &other, &speed).is_empty(),
            "no table bound to OtherClass carries speed yet",
        );

        ws.analysis
            .update_file_by_uri(&uri, Some(REBOUND.to_string()))
            .map(|(id, _)| id)
            .expect("rebound file indexed");
        let db = ws.analysis.compilation.get_db();
        assert!(
            local_class_table_member_ids(db, &bound, &speed).is_empty(),
            "rebinding the only table away leaves BoundClass with no backer",
        );
        assert_eq!(
            local_class_table_member_ids(db, &other, &speed).len(),
            1,
            "the rebound table now backs OtherClass",
        );
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DynamicFieldResolution {
    pub typ: LuaType,
    pub semantic_decl: Option<LuaSemanticDeclId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicFieldDefinitionVisibility {
    Runtime,
    ShapeOnly,
}

struct VisibleDynamicFieldDefinition {
    member_id: Option<LuaMemberId>,
    visibility: DynamicFieldDefinitionVisibility,
}

pub(crate) fn resolve_dynamic_field_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
    access_position: Option<TextSize>,
) -> Result<Option<DynamicFieldResolution>, InferFailReason> {
    if !db.get_emmyrc().gmod.enabled
        || !db.get_emmyrc().gmod.infer_dynamic_fields
        || !cache.get_config().dynamic_fields_visible
    {
        return Ok(None);
    }

    let cache_key = (prefix_type.clone(), member_key.clone(), access_position);
    if let Some(cached) = cache.dynamic_field_resolution_cache.get(&cache_key) {
        return Ok(cached
            .clone()
            .map(|(typ, semantic_decl)| DynamicFieldResolution { typ, semantic_decl }));
    }

    let Some(field_name) = member_key.get_name() else {
        return Ok(None);
    };
    let caller_file_id = cache.get_file_id();
    let definitions = dynamic_field_definitions(
        db,
        cache,
        caller_file_id,
        prefix_type,
        field_name,
        access_position,
    );
    if definitions.is_empty() {
        // Absence is only knowable once the index is complete: until then
        // the walk may simply not have reached the defining file yet. Never
        // memoise this — the answer is about build state, not about the
        // field.
        if !db.get_dynamic_field_index().is_sealed()
            && !cache.get_config().building_dynamic_field_index
        {
            return Err(InferFailReason::UnSealedDynamicFields);
        }
        cache.dynamic_field_resolution_cache.insert(cache_key, None);
        return Ok(None);
    }

    let mut member_types = Vec::new();
    let mut semantic_decl = None;
    let mut has_runtime_type = false;
    for definition in definitions {
        let Some(member_id) = definition.member_id else {
            continue;
        };
        if semantic_decl.is_none() {
            semantic_decl = Some(LuaSemanticDeclId::Member(member_id));
        }
        if let Some(typ) = dynamic_field_member_type(db, cache, &member_id) {
            member_types.push(typ);
            if definition.visibility == DynamicFieldDefinitionVisibility::Runtime {
                has_runtime_type = true;
            }
        }
    }

    if !member_types.is_empty() && !has_runtime_type {
        member_types.push(LuaType::Nil);
    }

    let typ = match member_types.as_slice() {
        // Finding no contributing member says nothing about what the field
        // holds, so the honest answer is unresolved rather than unconstrained.
        [] => LuaType::Unknown,
        [only] => only.clone(),
        _ => LuaType::from_vec(member_types),
    };
    cache
        .dynamic_field_resolution_cache
        .insert(cache_key, Some((typ.clone(), semantic_decl.clone())));
    Ok(Some(DynamicFieldResolution { typ, semantic_decl }))
}

pub(crate) fn resolve_dynamic_field_member_for_file(
    db: &DbIndex,
    caller_file_id: FileId,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
) -> Option<DynamicFieldResolution> {
    let mut cache = LuaInferCache::new(caller_file_id, Default::default());
    resolve_dynamic_field_member(db, &mut cache, prefix_type, member_key, None).unwrap_or_default()
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
        let value_idx = vars
            .iter()
            .position(|var| var.syntax().text_range() == node.text_range())?;
        let mut definition_cache = dynamic_field_definition_cache(cache, member_id.file_id);
        return infer_expr_list_value_type_at(db, &mut definition_cache, &exprs, value_idx)
            .ok()
            .map(|typ| typ.unwrap_or(LuaType::Nil));
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
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    prefix_type: &LuaType,
    field_name: &str,
    access_position: Option<TextSize>,
) -> Vec<VisibleDynamicFieldDefinition> {
    match prefix_type {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => dynamic_field_definitions_for_owner(
            db,
            cache,
            caller_file_id,
            &crate::DynamicFieldOwner::Type(type_id.clone()),
            field_name,
            access_position,
            None,
        ),
        LuaType::TableConst(table_range) => {
            let table_owner = crate::DynamicFieldOwner::Table(table_range.clone());
            let owner = crate::canonical_dynamic_field_owner(db, table_owner.clone());
            let source_min_position = (owner != table_owner)
                .then(|| InFiled::new(table_range.file_id, table_range.value.start()));
            dynamic_field_definitions_for_owner(
                db,
                cache,
                caller_file_id,
                &owner,
                field_name,
                access_position,
                source_min_position.as_ref(),
            )
        }
        LuaType::Instance(instance) => dynamic_field_definitions(
            db,
            cache,
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
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    owner: &crate::DynamicFieldOwner,
    field_name: &str,
    access_position: Option<TextSize>,
    source_min_position: Option<&InFiled<TextSize>>,
) -> Vec<VisibleDynamicFieldDefinition> {
    let dynamic_fields_global = db.get_emmyrc().gmod.dynamic_fields_global;
    let caller_mask = effective_dynamic_field_state_mask(db, caller_file_id, access_position);
    let access_function = access_position.and_then(|position| {
        db.get_member_index()
            .enclosing_function_scope_range(caller_file_id, position)
    });
    let mut visible = Vec::new();
    // Consulted in the index's canonical order, unfiltered and unsorted: the
    // elected arm and the elected semantic decl both follow it.
    for definition in db
        .get_dynamic_field_index()
        .field_definitions(owner, field_name)
    {
        if source_min_position.is_some_and(|source_min_position| {
            !source_survives_table_generation_cutoff(
                db,
                definition.file_id,
                definition.value.start(),
                source_min_position,
            )
        }) {
            continue;
        }
        if !(dynamic_fields_global || definition.file_id == caller_file_id) {
            continue;
        }
        if !is_dynamic_field_realm_compatible(db, caller_mask, definition) {
            continue;
        }
        let syntax = dynamic_field_definition_syntax(db, cache, definition);
        if let Some(visibility) = dynamic_field_definition_visibility_at(
            db,
            caller_file_id,
            definition,
            syntax.enclosing_assign_range,
            access_position,
            access_function,
        ) {
            visible.push(VisibleDynamicFieldDefinition {
                member_id: syntax.member_id,
                visibility,
            });
        }
    }
    visible
}

fn dynamic_field_definition_visibility_at(
    db: &DbIndex,
    caller_file_id: FileId,
    definition: &crate::InFiled<TextRange>,
    enclosing_assign_range: Option<TextRange>,
    access_position: Option<TextSize>,
    access_function: Option<TextRange>,
) -> Option<DynamicFieldDefinitionVisibility> {
    let Some(access_position) = access_position else {
        return Some(DynamicFieldDefinitionVisibility::Runtime);
    };
    if definition.file_id != caller_file_id {
        return Some(DynamicFieldDefinitionVisibility::Runtime);
    }
    if enclosing_assign_range
        .is_some_and(|range| range.contains(access_position) && range != definition.value)
    {
        return None;
    }

    let member_index = db.get_member_index();
    let definition_function =
        member_index.enclosing_function_scope_range(definition.file_id, definition.value.start());

    if definition_function != access_function
        && (definition_function.is_some() || access_function.is_some())
    {
        // A write in a function that encloses the reading one runs before the
        // inner function can be called, so it is at least as good evidence as
        // the cross-file write trusted unconditionally above.
        if let (Some(definition_function), Some(access_function)) =
            (definition_function, access_function)
            && definition_function.contains_range(access_function)
            && definition.value.start() <= access_position
        {
            return Some(DynamicFieldDefinitionVisibility::Runtime);
        }
        return Some(DynamicFieldDefinitionVisibility::ShapeOnly);
    }

    (definition.value.start() <= access_position)
        .then_some(DynamicFieldDefinitionVisibility::Runtime)
}

/// The definition site's member id and enclosing assignment range.
///
/// Both are read off the definition file's tree alone, so they are memoised
/// per definition rather than re-walked from the root for every access
/// position that consults the same definition.
fn dynamic_field_definition_syntax(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    definition: &crate::InFiled<TextRange>,
) -> DynamicFieldDefinitionSyntax {
    if let Some(cached) = cache.dynamic_field_definition_syntax_cache.get(definition) {
        return *cached;
    }
    let syntax = dynamic_field_definition_syntax_uncached(db, definition);
    cache
        .dynamic_field_definition_syntax_cache
        .insert(definition.clone(), syntax);
    syntax
}

fn dynamic_field_definition_syntax_uncached(
    db: &DbIndex,
    definition: &crate::InFiled<TextRange>,
) -> DynamicFieldDefinitionSyntax {
    let mut syntax = DynamicFieldDefinitionSyntax::default();
    let file_id = definition.file_id;
    let range = definition.value;
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return syntax;
    };
    let root = tree.get_red_root();
    let Some(token) = root.token_at_offset(range.start()).right_biased() else {
        return syntax;
    };

    let mut current = token.parent();
    while let Some(node) = current {
        if syntax.member_id.is_none() {
            if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
                // Legacy: range matches the full index expression. New: range
                // matches the index key (field name) within it.
                if index_expr.get_range() == range
                    || index_expr.get_index_key().and_then(|key| key.get_range()) == Some(range)
                {
                    syntax.member_id = Some(LuaMemberId::new(index_expr.get_syntax_id(), file_id));
                }
            }
            if syntax.member_id.is_none()
                && let Some(table_field) = LuaTableField::cast(node.clone())
                && (table_field.get_range() == range
                    || table_field.get_field_key().and_then(|key| key.get_range()) == Some(range))
            {
                syntax.member_id = Some(LuaMemberId::new(table_field.get_syntax_id(), file_id));
            }
        }
        if syntax.enclosing_assign_range.is_none()
            && let Some(assign_stat) = LuaAssignStat::cast(node.clone())
        {
            syntax.enclosing_assign_range = Some(assign_stat.get_range());
        }
        if syntax.member_id.is_some() && syntax.enclosing_assign_range.is_some() {
            break;
        }
        current = node.parent();
    }

    syntax
}

fn effective_dynamic_field_state_mask(
    db: &DbIndex,
    file_id: FileId,
    position: Option<TextSize>,
) -> GmodStateMask {
    let infer_index = db.get_gmod_infer_index();
    let Some(position) = position else {
        return infer_index
            .get_realm_file_metadata(&file_id)
            .map_or(GmodStateMask::empty(), |metadata| {
                metadata.inferred_realm.state_mask()
            });
    };

    infer_index
        .get_member_annotation_realm_at_offset(&file_id, position)
        .map(crate::GmodRealm::state_mask)
        .unwrap_or_else(|| infer_index.get_state_mask_at_offset(&file_id, position))
}

fn is_dynamic_field_realm_compatible(
    db: &DbIndex,
    caller_mask: GmodStateMask,
    definition: &crate::InFiled<TextRange>,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }

    caller_mask.is_compatible_with(effective_dynamic_field_state_mask(
        db,
        definition.file_id,
        Some(definition.value.start()),
    ))
}

pub(crate) fn member_key_as_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::None => None,
        LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
        LuaMemberKey::Name(name) => Some(LuaType::StringConst(name.clone().into())),
        LuaMemberKey::ExprType(typ) => Some(typ.clone()),
    }
}

pub fn get_member_value_expr(db: &DbIndex, member_id: LuaMemberId) -> Option<LuaExpr> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(field) = LuaTableField::cast(node.clone()) {
        return field.get_value_expr();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node) {
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
        LuaType::Union(union) => union.types().any(unknown_index_key_matches_access),
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
    let mut visited_members = FxHashSet::default();

    let mut current_owner = resolve_member_owner(db, infer_config, &member_id, caller_position);
    let mut final_owner = current_owner.clone();

    while let Some(LuaSemanticDeclId::Member(current_member_id)) = &current_owner {
        if visited_members.contains(current_member_id) {
            break;
        }

        visited_members.insert(*current_member_id);

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
