use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use rowan::TextSize;
use smol_str::SmolStr;

use crate::db_index::derivation;
use crate::{
    DbIndex, FileId, GlobalId, InferGuardRef, LuaGenericType, LuaInstanceType, LuaIntersectionType,
    LuaMemberKey, LuaMemberOwner, LuaMergedTableType, LuaObjectType, LuaSemanticDeclId,
    LuaTupleType, LuaType, LuaTypeDeclId, LuaUnionType, WorkspaceId,
    semantic::{
        InferGuard,
        generic::{TypeSubstitutor, instantiate_type_generic},
    },
};

use super::{
    FindMembersResult, LuaMemberInfo, get_buildin_type_map_type_id, intersect_member_types,
    local_class_table_member_ids, merge_open_table_types, resolve_dynamic_field_member_for_file,
};

#[derive(Debug, Clone)]
pub enum FindMemberFilter {
    /// Finds all members
    All,
    /// Members that are not methods: what a structural check compares.
    Fields,
    /// Finds members by the given key
    ByKey {
        /// The member key to search for
        member_key: LuaMemberKey,
        /// Whether to find all matching members; when `false`, stops after the first match
        find_all: bool,
    },
}

pub fn find_members(db: &DbIndex, prefix_type: &LuaType) -> FindMembersResult {
    let ctx = FindMembersContext::new(InferGuard::new());
    find_members_guard(db, prefix_type, &ctx, &FindMemberFilter::All)
}

/// The members a structural check against `prefix_type` compares: no methods,
/// and no dynamic fields, which outside a file are listed as `any` and never
/// required.
pub(crate) fn find_structural_members(db: &DbIndex, prefix_type: &LuaType) -> FindMembersResult {
    let ctx = FindMembersContext {
        dynamic_fields: false,
        ..FindMembersContext::new(InferGuard::new())
    };
    find_members_guard(db, prefix_type, &ctx, &FindMemberFilter::Fields)
}

pub fn find_members_in_workspace_for_file(
    db: &DbIndex,
    prefix_type: &LuaType,
    workspace_id: WorkspaceId,
    file_id: FileId,
) -> FindMembersResult {
    let ctx =
        FindMembersContext::new_with_workspace_and_file(InferGuard::new(), workspace_id, file_id);
    find_members_guard(db, prefix_type, &ctx, &FindMemberFilter::All)
}

pub fn find_members_in_workspace_for_file_at_offset(
    db: &DbIndex,
    prefix_type: &LuaType,
    workspace_id: WorkspaceId,
    file_id: FileId,
    caller_position: TextSize,
) -> FindMembersResult {
    let ctx = FindMembersContext::new_with_workspace_file_and_position(
        InferGuard::new(),
        workspace_id,
        file_id,
        caller_position,
    );
    find_members_guard(db, prefix_type, &ctx, &FindMemberFilter::All)
}

pub fn find_members_with_key(
    db: &DbIndex,
    prefix_type: &LuaType,
    member_key: LuaMemberKey,
    find_all: bool,
) -> FindMembersResult {
    let ctx = FindMembersContext::new(InferGuard::new());
    find_members_guard(
        db,
        prefix_type,
        &ctx,
        &FindMemberFilter::ByKey {
            member_key,
            find_all,
        },
    )
}

pub fn find_members_with_key_in_workspace_for_file(
    db: &DbIndex,
    prefix_type: &LuaType,
    member_key: LuaMemberKey,
    find_all: bool,
    workspace_id: WorkspaceId,
    file_id: FileId,
) -> FindMembersResult {
    let ctx =
        FindMembersContext::new_with_workspace_and_file(InferGuard::new(), workspace_id, file_id);
    find_members_guard(
        db,
        prefix_type,
        &ctx,
        &FindMemberFilter::ByKey {
            member_key,
            find_all,
        },
    )
}

pub fn find_members_with_key_in_workspace_for_file_at_offset(
    db: &DbIndex,
    prefix_type: &LuaType,
    member_key: LuaMemberKey,
    find_all: bool,
    workspace_id: WorkspaceId,
    file_id: FileId,
    caller_position: TextSize,
) -> FindMembersResult {
    let ctx = FindMembersContext::new_with_workspace_file_and_position(
        InferGuard::new(),
        workspace_id,
        file_id,
        caller_position,
    );
    find_members_guard(
        db,
        prefix_type,
        &ctx,
        &FindMemberFilter::ByKey {
            member_key,
            find_all,
        },
    )
}

pub(crate) fn visible_super_types_in_workspace_for_file_at_offset(
    db: &DbIndex,
    type_decl_id: &LuaTypeDeclId,
    workspace_id: WorkspaceId,
    file_id: FileId,
    caller_position: TextSize,
) -> Option<Vec<LuaType>> {
    let ctx = FindMembersContext::new_with_workspace_file_and_position(
        InferGuard::new(),
        workspace_id,
        file_id,
        caller_position,
    );
    super_types_for_context(db, type_decl_id, &ctx)
        .map(|super_types| super_types.into_iter().cloned().collect())
}

#[derive(Clone)]
struct FindMembersContext {
    infer_guard: InferGuardRef,
    substitutor: Option<TypeSubstitutor>,
    workspace_id: Option<WorkspaceId>,
    file_id: Option<FileId>,
    caller_position: Option<TextSize>,
    dynamic_fields: bool,
    /// Whether the lookup reached the current class from one of its subclasses.
    inherited: bool,
}

impl FindMembersContext {
    fn new(infer_guard: InferGuardRef) -> Self {
        Self {
            infer_guard,
            substitutor: None,
            workspace_id: None,
            file_id: None,
            caller_position: None,
            dynamic_fields: true,
            inherited: false,
        }
    }

    fn new_with_workspace_and_file(
        infer_guard: InferGuardRef,
        workspace_id: WorkspaceId,
        file_id: FileId,
    ) -> Self {
        Self {
            infer_guard,
            substitutor: None,
            workspace_id: Some(workspace_id),
            file_id: Some(file_id),
            caller_position: None,
            dynamic_fields: true,
            inherited: false,
        }
    }

    fn new_with_workspace_file_and_position(
        infer_guard: InferGuardRef,
        workspace_id: WorkspaceId,
        file_id: FileId,
        caller_position: TextSize,
    ) -> Self {
        Self {
            infer_guard,
            substitutor: None,
            workspace_id: Some(workspace_id),
            file_id: Some(file_id),
            caller_position: Some(caller_position),
            dynamic_fields: true,
            inherited: false,
        }
    }

    fn with_substitutor(&self, substitutor: TypeSubstitutor) -> Self {
        Self {
            infer_guard: self.infer_guard.clone(),
            substitutor: Some(substitutor),
            workspace_id: self.workspace_id,
            file_id: self.file_id,
            caller_position: self.caller_position,
            dynamic_fields: self.dynamic_fields,
            inherited: self.inherited,
        }
    }

    fn fork_infer(&self) -> Self {
        Self {
            infer_guard: self.infer_guard.fork(),
            substitutor: self.substitutor.clone(),
            workspace_id: self.workspace_id,
            file_id: self.file_id,
            caller_position: self.caller_position,
            dynamic_fields: self.dynamic_fields,
            inherited: self.inherited,
        }
    }

    /// This lookup as it continues into a class's supers.
    fn inherited(&self) -> Self {
        Self {
            inherited: true,
            ..self.clone()
        }
    }

    fn instantiate_type(&self, db: &DbIndex, ty: &LuaType) -> LuaType {
        if let Some(substitutor) = &self.substitutor {
            instantiate_type_generic(db, ty, substitutor)
        } else {
            ty.clone()
        }
    }

    fn infer_guard(&self) -> &InferGuardRef {
        &self.infer_guard
    }

    fn file_id(&self) -> Option<FileId> {
        self.file_id
    }

    fn workspace_id(&self) -> Option<WorkspaceId> {
        self.workspace_id
    }

    fn has_workspace_scope(&self) -> bool {
        self.workspace_id.is_some() && self.file_id.is_some()
    }

    fn caller_position(&self) -> Option<TextSize> {
        self.caller_position
    }

    /// The position dynamic-field visibility is judged from. `None` when the
    /// request carries no position, where every definition is visible.
    fn dynamic_field_access_site(&self, db: &DbIndex) -> Option<DynamicFieldAccessSite> {
        let file_id = self.file_id?;
        let position = self.caller_position?;
        Some(DynamicFieldAccessSite {
            file_id,
            position,
            // A listing asks about many fields from one position, and this is a
            // binary search plus a backward scan over every function range in
            // the file, so it is resolved per listing rather than per field.
            in_function: db
                .get_member_index()
                .enclosing_function_scope_range(file_id, position)
                .is_some(),
        })
    }
}

/// The position a dynamic-field listing is taken from.
struct DynamicFieldAccessSite {
    file_id: FileId,
    position: TextSize,
    in_function: bool,
}

fn find_members_guard(
    db: &DbIndex,
    prefix_type: &LuaType,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    match &prefix_type {
        LuaType::TableConst(id) => {
            let member_owner = LuaMemberOwner::Element(id.clone());
            let mut members =
                find_owner_members(db, ctx, &member_owner, filter).unwrap_or_default();
            append_dynamic_fields_for_table(db, ctx, id, &mut members, filter);
            Some(members)
        }
        LuaType::TableGeneric(table_type) => {
            find_table_generic_members(db, ctx, table_type, filter)
        }
        LuaType::String
        | LuaType::Io
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => {
            let type_decl_id = get_buildin_type_map_type_id(prefix_type)?;
            find_custom_type_members(db, ctx, &type_decl_id, filter)
        }
        LuaType::Ref(type_decl_id) => find_custom_type_members(db, ctx, type_decl_id, filter),
        LuaType::Def(type_decl_id) => find_custom_type_members(db, ctx, type_decl_id, filter),
        LuaType::Tuple(tuple_type) => find_tuple_members(db, ctx, tuple_type, filter),
        LuaType::Object(object_type) => find_object_members(db, ctx, object_type, filter),
        LuaType::Union(union_type) => find_union_members(db, union_type, ctx, filter),
        LuaType::MultiLineUnion(multi_union) => {
            let union_type = multi_union.to_union();
            if let LuaType::Union(union_type) = union_type {
                find_union_members(db, &union_type, ctx, filter)
            } else {
                None
            }
        }
        LuaType::Intersection(intersection_type) => {
            find_intersection_members(db, intersection_type, ctx, filter)
        }
        LuaType::MergedTable(merged_table) => {
            find_merged_table_members(db, merged_table, ctx, filter)
        }
        LuaType::Generic(generic_type) => find_generic_members(db, generic_type, ctx, filter),
        LuaType::Global => find_global_members(db, ctx, filter),
        LuaType::Instance(inst) => find_instance_members(db, inst, ctx, filter),
        LuaType::Namespace(ns) => find_namespace_members(db, ctx, ns, filter),
        LuaType::TableOf(inner) => find_members_guard(db, inner, ctx, filter),
        LuaType::ModuleRef(file_id) => {
            let module_info = db.get_module_index().get_module(*file_id);
            if let Some(module_info) = module_info
                && let Some(export_type) = &module_info.export_type
            {
                return find_members_guard(db, export_type, ctx, filter);
            }

            None
        }
        _ => None,
    }
}

/// Checks whether a member should be included
fn should_include_member(key: &LuaMemberKey, filter: &FindMemberFilter) -> bool {
    match filter {
        FindMemberFilter::All | FindMemberFilter::Fields => true,
        FindMemberFilter::ByKey { member_key, .. } => member_key == key,
    }
}

/// Checks whether collecting more members should stop
fn should_stop_collecting(current_count: usize, filter: &FindMemberFilter) -> bool {
    match filter {
        FindMemberFilter::ByKey { find_all, .. } => !find_all && current_count > 0,
        _ => false,
    }
}

fn find_table_generic_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    table_type: &[LuaType],
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    if table_type.len() != 2 {
        return None;
    }

    let key_type = ctx.instantiate_type(db, &table_type[0]);
    let value_type = ctx.instantiate_type(db, &table_type[1]);
    let member_key = LuaMemberKey::ExprType(key_type);

    if should_include_member(&member_key, filter) {
        members.push(LuaMemberInfo {
            property_owner_id: None,
            key: member_key,
            typ: value_type,
            feature: None,
            overload_index: None,
        });
    }
    Some(members)
}

fn find_custom_type_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    type_decl_id: &LuaTypeDeclId,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    ctx.infer_guard().check(type_decl_id).ok()?;
    let type_index = db.get_type_index();
    let type_decl = type_index.get_type_decl(type_decl_id);
    if let Some(type_decl) = type_decl.filter(|decl| decl.is_alias()) {
        if let Some(origin) = type_decl.get_alias_origin(db, None) {
            return find_members_guard(db, &origin, ctx, filter);
        } else {
            return find_members_guard(db, &LuaType::String, ctx, filter);
        }
    }
    let mut members = Vec::new();
    let type_member_owner = LuaMemberOwner::Type(type_decl_id.clone());
    if let Some(type_members) = find_owner_members(db, ctx, &type_member_owner, filter) {
        members.extend(type_members);

        if should_stop_collecting(members.len(), filter) {
            return Some(members);
        }
    }

    // The class's global holds the members written through it outside the
    // class's own declaration; a key the class declares itself wins.
    if let Some(global_owner) = type_decl_id.global_member_owner(db)
        && let Some(global_members) = find_owner_members(db, ctx, &global_owner, filter)
    {
        let declared_keys = members
            .iter()
            .map(|member| member.key.clone())
            .collect::<HashSet<_>>();
        members.extend(
            global_members
                .into_iter()
                .filter(|member| !declared_keys.contains(&member.key)),
        );

        if should_stop_collecting(members.len(), filter) {
            return Some(members);
        }
    }

    if members.is_empty()
        && let FindMemberFilter::ByKey {
            member_key,
            find_all,
        } = filter
    {
        let local_member_ids = local_class_table_member_ids(db, type_decl_id, member_key);
        if !local_member_ids.is_empty() {
            let member_index = db.get_member_index();
            for member_id in local_member_ids {
                let raw_type = db
                    .get_type_index()
                    .get_type_cache(&member_id.into())
                    .map(|t| t.as_type().clone())
                    .unwrap_or(LuaType::Unknown);
                let feature = member_index
                    .get_member(&member_id)
                    .map(|member| member.get_feature());

                members.push(LuaMemberInfo {
                    property_owner_id: Some(LuaSemanticDeclId::Member(member_id)),
                    key: member_key.clone(),
                    typ: ctx.instantiate_type(db, &raw_type),
                    feature,
                    overload_index: None,
                });

                if !find_all {
                    return Some(members);
                }
            }

            if should_stop_collecting(members.len(), filter) {
                return Some(members);
            }
        }
    }

    if type_decl.is_some_and(|decl| decl.is_class())
        && let Some(super_members) = find_super_type_members(db, type_decl_id, ctx, filter)
    {
        members.extend(super_members);
        if should_stop_collecting(members.len(), filter) {
            return Some(members);
        }
    }

    if append_dynamic_fields_for_type(db, ctx, type_decl_id, &mut members, filter) {
        return Some(members);
    }

    Some(members)
}

fn find_super_type_members(
    db: &DbIndex,
    type_decl_id: &LuaTypeDeclId,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    let super_ctx = ctx.inherited();
    for super_type in super_types_for_context(db, type_decl_id, ctx)? {
        let instantiated_super = ctx.instantiate_type(db, super_type);
        if let Some(super_members) = find_members_guard(db, &instantiated_super, &super_ctx, filter)
        {
            members.extend(super_members);
            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    Some(members)
}

fn super_types_for_context<'a>(
    db: &'a DbIndex,
    type_decl_id: &LuaTypeDeclId,
    ctx: &FindMembersContext,
) -> Option<Vec<&'a LuaType>> {
    let entries = db.get_type_index().get_super_type_entries(type_decl_id)?;
    let Some(caller_workspace_id) = ctx.workspace_id() else {
        return Some(entries.iter().map(|entry| &entry.value.typ).collect());
    };
    let caller_file_id = ctx.file_id()?;
    let infer_index = db.get_gmod_infer_index();
    let caller_realm = ctx.caller_position().map_or_else(
        || {
            infer_index
                .get_realm_file_metadata(&caller_file_id)
                .map_or(crate::GmodRealm::Unknown, |metadata| {
                    metadata.inferred_realm
                })
        },
        |position| infer_index.get_realm_at_offset(&caller_file_id, position),
    );
    let module_index = db.get_module_index();
    let gmod_enabled = db.get_emmyrc().gmod.enabled;
    let mut visible = entries
        .iter()
        .filter_map(|entry| {
            if gmod_enabled {
                derivation::note_type_super_realms_read(type_decl_id, entry.file_id);
                let entry_realm = derivation::untracked(|| {
                    infer_index
                        .get_realm_at_offset(&entry.file_id, entry.value.source_range.start())
                });
                if !caller_realm.is_compatible_with(entry_realm) {
                    return None;
                }
            }

            let candidate_workspace_id = module_index
                .get_workspace_id(entry.file_id)
                .unwrap_or(crate::WorkspaceId::MAIN);
            let workspace_key = module_index
                .workspace_resolution_key(caller_workspace_id, candidate_workspace_id)?;
            Some((
                workspace_key,
                candidate_workspace_id != caller_workspace_id,
                entry.file_id,
                &entry.value.typ,
            ))
        })
        .collect::<Vec<_>>();
    // The sort is stable, so a file's edges stay in position order.
    visible.sort_by_key(|(workspace_key, is_other_workspace, file_id, _)| {
        (*workspace_key, *is_other_workspace, file_id.id)
    });
    let mut seen_super_types = HashSet::default();
    visible.retain(|(_, _, _, super_type)| seen_super_types.insert(*super_type));

    Some(
        visible
            .into_iter()
            .map(|(_, _, _, super_type)| super_type)
            .collect(),
    )
}

fn find_owner_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    owner: &LuaMemberOwner,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    if ctx.has_workspace_scope() {
        find_workspace_scoped_owner_members(db, ctx, owner, filter)
    } else {
        find_unscoped_owner_members(db, ctx, owner, filter)
    }
}

fn find_unscoped_owner_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    owner: &LuaMemberOwner,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    let member_index = db.get_member_index();
    // A by-key search reads the index by key rather than walking the owner's
    // whole member list.
    let owner_members = match filter {
        FindMemberFilter::ByKey { member_key, .. } => {
            member_index.get_members_with_key(owner, member_key)?
        }
        FindMemberFilter::All => member_index.get_members(owner)?,
        FindMemberFilter::Fields => member_index.get_field_members(owner)?,
    };

    for member in owner_members {
        let member_key = member.get_key().clone();

        if should_include_member(&member_key, filter) {
            let raw_type = db
                .get_type_index()
                .get_type_cache(&member.get_id().into())
                .map(|t| t.as_type().clone())
                .unwrap_or(LuaType::Unknown);
            members.push(LuaMemberInfo {
                property_owner_id: Some(LuaSemanticDeclId::Member(member.get_id())),
                key: member_key,
                typ: ctx.instantiate_type(db, &raw_type),
                feature: Some(member.get_feature()),
                overload_index: None,
            });

            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    Some(members)
}

fn find_workspace_scoped_owner_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    owner: &LuaMemberOwner,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let caller_file_id = ctx.file_id()?;
    let member_index = db.get_member_index();

    match filter {
        FindMemberFilter::All | FindMemberFilter::Fields => {
            let owner_members = if matches!(filter, FindMemberFilter::Fields) {
                member_index.get_field_members(owner)?
            } else {
                member_index.get_members(owner)?
            };
            let mut seen = HashSet::default();
            let mut members = Vec::new();

            for member in owner_members {
                let member_key = member.get_key().clone();
                if !seen.insert(member_key.clone()) {
                    continue;
                }

                if let Some(member_infos) =
                    build_workspace_scoped_member_infos(db, ctx, owner, member_key, caller_file_id)
                {
                    members.extend(member_infos);
                }
            }

            Some(members)
        }
        FindMemberFilter::ByKey {
            member_key,
            find_all,
        } => {
            build_workspace_scoped_member_infos(db, ctx, owner, member_key.clone(), caller_file_id)
                .map(|mut members| {
                    if !find_all {
                        members.truncate(1);
                    }
                    members
                })
        }
    }
}

fn build_workspace_scoped_member_infos(
    db: &DbIndex,
    ctx: &FindMembersContext,
    owner: &LuaMemberOwner,
    member_key: LuaMemberKey,
    caller_file_id: FileId,
) -> Option<Vec<LuaMemberInfo>> {
    let member_index = db.get_member_index();
    let member_item = member_index.get_member_item(owner, &member_key)?;
    let visible_member_ids = ctx.caller_position().map_or_else(
        || member_item.visible_member_ids_with_realm(db, &caller_file_id),
        |caller_position| {
            member_item.visible_member_ids_with_realm_at_offset(
                db,
                &caller_file_id,
                caller_position,
            )
        },
    );
    if visible_member_ids.is_empty() {
        return None;
    }

    Some(
        visible_member_ids
            .into_iter()
            .map(|member_id| {
                let raw_type = db
                    .get_type_index()
                    .get_type_cache(&member_id.into())
                    .map(|t| t.as_type().clone())
                    .unwrap_or(LuaType::Unknown);
                let feature = member_index
                    .get_member(&member_id)
                    .map(|member| member.get_feature());

                LuaMemberInfo {
                    property_owner_id: Some(LuaSemanticDeclId::Member(member_id)),
                    key: member_key.clone(),
                    typ: ctx.instantiate_type(db, &raw_type),
                    feature,
                    overload_index: None,
                }
            })
            .collect(),
    )
}

fn find_tuple_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    tuple_type: &LuaTupleType,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    for (idx, typ) in tuple_type.get_types().iter().enumerate() {
        let member_key = LuaMemberKey::Integer((idx + 1) as i64);

        if should_include_member(&member_key, filter) {
            members.push(LuaMemberInfo {
                property_owner_id: None,
                key: member_key,
                typ: ctx.instantiate_type(db, typ),
                feature: None,
                overload_index: None,
            });

            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    Some(members)
}

fn find_object_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    object_type: &LuaObjectType,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    for (key, typ) in object_type.get_fields().iter() {
        if should_include_member(key, filter) {
            members.push(LuaMemberInfo {
                property_owner_id: None,
                key: key.clone(),
                typ: ctx.instantiate_type(db, typ),
                feature: None,
                overload_index: None,
            });

            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    Some(members)
}

fn find_union_members(
    db: &DbIndex,
    union_type: &LuaUnionType,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    let mut meet_string = false;
    for typ in union_type.types() {
        let instantiated_type = ctx.instantiate_type(db, typ);
        if instantiated_type.is_string() {
            if meet_string {
                continue;
            }
            meet_string = true;
        }

        let fork_ctx = ctx.fork_infer();
        let sub_members = find_members_guard(db, &instantiated_type, &fork_ctx, filter);
        if let Some(sub_members) = sub_members {
            members.extend(sub_members);

            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    Some(members)
}

fn find_intersection_members(
    db: &DbIndex,
    intersection_type: &LuaIntersectionType,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut order: Vec<LuaMemberKey> = Vec::new();
    let mut members: HashMap<LuaMemberKey, LuaMemberInfo> = HashMap::default();

    for typ in intersection_type.get_types().iter() {
        let instantiated_type = ctx.instantiate_type(db, typ);
        let fork_ctx = ctx.fork_infer();
        let sub_members = find_members_guard(db, &instantiated_type, &fork_ctx, filter);
        let Some(sub_members) = sub_members else {
            continue;
        };

        // Within a single component type, treat duplicate keys as overrides (first wins).
        let mut component_seen: HashSet<LuaMemberKey> = HashSet::default();
        for member in sub_members {
            if !component_seen.insert(member.key.clone()) {
                continue;
            }

            match members.entry(member.key.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    order.push(member.key.clone());
                    entry.insert(LuaMemberInfo {
                        property_owner_id: member.property_owner_id.clone(),
                        key: member.key,
                        typ: member.typ,
                        feature: None,
                        overload_index: None,
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().typ =
                        intersect_member_types(db, entry.get().typ.clone(), member.typ.clone());
                }
            }
        }
    }

    if members.is_empty() {
        None
    } else {
        let mut result = Vec::new();
        for key in order {
            let Some(member) = members.get(&key) else {
                continue;
            };
            let key = &member.key;
            let typ = &member.typ;

            result.push(LuaMemberInfo {
                property_owner_id: member.property_owner_id.clone(),
                key: key.clone(),
                typ: typ.clone(),
                feature: None,
                overload_index: None,
            });

            if should_stop_collecting(result.len(), filter) {
                break;
            }
        }

        Some(result)
    }
}

fn find_merged_table_members(
    db: &DbIndex,
    merged_table: &LuaMergedTableType,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut order: Vec<LuaMemberKey> = Vec::new();
    let mut members: HashMap<LuaMemberKey, LuaMemberInfo> = HashMap::default();

    for typ in merged_table.get_types().iter() {
        let instantiated_type = ctx.instantiate_type(db, typ);
        let fork_ctx = ctx.fork_infer();
        let Some(sub_members) = find_members_guard(db, &instantiated_type, &fork_ctx, filter)
        else {
            continue;
        };

        // One component can hold several writers of the same key. Keeping only
        // the first discards what the rest assign, and which one comes first is
        // the member sort order rather than anything the source says - the
        // table then disagrees with the union every other reader of that slot
        // gets from `resolve_member_item_type`. Union within the component,
        // then merge components as table fragments below.
        let mut component_members: HashMap<LuaMemberKey, LuaMemberInfo> = HashMap::default();
        let mut component_order: Vec<LuaMemberKey> = Vec::new();
        for member in sub_members {
            match component_members.entry(member.key.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    component_order.push(member.key.clone());
                    entry.insert(member);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let unioned = crate::TypeOps::Union.apply(db, &entry.get().typ, &member.typ);
                    entry.get_mut().typ = unioned;
                }
            }
        }

        for key in component_order {
            let Some(member) = component_members.remove(&key) else {
                continue;
            };

            match members.entry(member.key.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    order.push(member.key.clone());
                    entry.insert(member);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    // Components that are one slot answer identically; merging
                    // that answer with itself would fold a union of rival
                    // tables into one table.
                    if entry.get().typ != member.typ {
                        let merged_type =
                            merge_open_table_types(db, vec![entry.get().typ.clone(), member.typ]);
                        entry.get_mut().typ = merged_type;
                    }
                }
            }
        }
    }

    if members.is_empty() {
        None
    } else {
        let mut result = Vec::new();
        for key in order {
            let Some(member) = members.get(&key) else {
                continue;
            };
            result.push(member.clone());

            if should_stop_collecting(result.len(), filter) {
                break;
            }
        }

        Some(result)
    }
}

fn find_generic_members(
    db: &DbIndex,
    generic_type: &LuaGenericType,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let base_ref_id = generic_type.get_base_type_id_ref();
    let instantiated_params: Vec<LuaType> = generic_type
        .get_params()
        .iter()
        .map(|param| ctx.instantiate_type(db, param))
        .collect();
    let substitutor = TypeSubstitutor::from_type_array(instantiated_params);
    let type_decl = db.get_type_index().get_type_decl(&base_ref_id)?;
    let ctx_with_substitutor = ctx.with_substitutor(substitutor.clone());
    if let Some(origin) = type_decl.get_alias_origin(db, Some(&substitutor)) {
        return find_members_guard(db, &origin, &ctx_with_substitutor, filter);
    }

    find_members_guard(
        db,
        &LuaType::Ref(base_ref_id.clone()),
        &ctx_with_substitutor,
        filter,
    )
}

fn find_global_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    let global_index = db.get_global_index();
    // A keyed read names one global: its declarations are the scan's matches,
    // kept in the same canonical order.
    let global_decls = match filter {
        FindMemberFilter::ByKey {
            member_key: LuaMemberKey::Name(name),
            ..
        } => global_index
            .get_global_decl_ids(name)
            .cloned()
            .unwrap_or_default(),
        _ => global_index.get_all_global_decl_ids(),
    };
    for decl_id in global_decls {
        if let Some(current_workspace_id) = ctx.workspace_id {
            let candidate_workspace_id = db
                .get_module_index()
                .get_workspace_id(decl_id.file_id)
                .unwrap_or(WorkspaceId::MAIN);
            if db
                .get_module_index()
                .workspace_resolution_priority(current_workspace_id, candidate_workspace_id)
                .is_none()
            {
                continue;
            }
        }

        if let Some(decl) = db.get_decl_index().get_decl(&decl_id) {
            let member_key = LuaMemberKey::Name(decl.get_name().to_string().into());

            if should_include_member(&member_key, filter) {
                let raw_type = db
                    .get_type_index()
                    .get_type_cache(&decl_id.into())
                    .map(|t| t.as_type().clone())
                    .unwrap_or(LuaType::Unknown);
                members.push(LuaMemberInfo {
                    property_owner_id: Some(LuaSemanticDeclId::LuaDecl(decl_id)),
                    key: member_key,
                    typ: ctx.instantiate_type(db, &raw_type),
                    feature: None,
                    overload_index: None,
                });

                if should_stop_collecting(members.len(), filter) {
                    break;
                }
            }
        }
    }

    Some(members)
}

fn find_instance_members(
    db: &DbIndex,
    inst: &LuaInstanceType,
    ctx: &FindMembersContext,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();
    let range = inst.get_range();
    let member_owner = LuaMemberOwner::Element(range.clone());
    if let Some(normal_members) = find_owner_members(db, ctx, &member_owner, filter) {
        members.extend(normal_members);

        if should_stop_collecting(members.len(), filter) {
            return Some(members);
        }
    }

    let origin_type = ctx.instantiate_type(db, inst.get_base());
    if let Some(origin_members) = find_members_guard(db, &origin_type, ctx, filter) {
        members.extend(origin_members);
    }

    Some(members)
}

fn find_namespace_members(
    db: &DbIndex,
    ctx: &FindMembersContext,
    ns: &str,
    filter: &FindMemberFilter,
) -> FindMembersResult {
    let mut members = Vec::new();

    let prefix = format!("{}.", ns);
    let type_index = db.get_type_index();
    let type_decl_id_map = type_index.find_type_decls(FileId::VIRTUAL, &prefix);
    for (name, type_decl_id) in type_decl_id_map {
        let member_key = LuaMemberKey::Name(name.clone().into());

        if should_include_member(&member_key, filter) {
            if let Some(type_decl_id) = type_decl_id {
                let def_type = LuaType::Def(type_decl_id.clone());
                let typ = ctx.instantiate_type(db, &def_type);
                let property_owner_id = LuaSemanticDeclId::TypeDecl(type_decl_id);
                members.push(LuaMemberInfo {
                    property_owner_id: Some(property_owner_id),
                    key: member_key,
                    typ,
                    feature: None,
                    overload_index: None,
                });
            } else {
                let ns_type = LuaType::Namespace(SmolStr::new(format!("{}.{}", ns, name)).into());
                members.push(LuaMemberInfo {
                    property_owner_id: None,
                    key: member_key,
                    typ: ctx.instantiate_type(db, &ns_type),
                    feature: None,
                    overload_index: None,
                });
            }

            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    if let Some(global_path_members) = find_owner_members(
        db,
        ctx,
        &LuaMemberOwner::GlobalPath(GlobalId::new(ns)),
        filter,
    ) {
        let mut existing = members
            .iter()
            .map(|member| member.key.clone())
            .collect::<HashSet<_>>();
        for member in global_path_members {
            if !existing.insert(member.key.clone()) {
                continue;
            }

            members.push(member);

            if should_stop_collecting(members.len(), filter) {
                break;
            }
        }
    }

    Some(members)
}

fn append_dynamic_fields_for_type(
    db: &DbIndex,
    ctx: &FindMembersContext,
    type_decl_id: &LuaTypeDeclId,
    members: &mut Vec<LuaMemberInfo>,
    filter: &FindMemberFilter,
) -> bool {
    if !ctx.dynamic_fields || !crate::db_index::derivation::dynamic_fields_visible() {
        return false;
    }
    let emmyrc = db.get_optional_file_emmyrc(ctx.file_id);
    if !emmyrc.gmod.enabled || !emmyrc.gmod.infer_dynamic_fields {
        return false;
    }

    let owner = crate::DynamicFieldOwner::Type(type_decl_id.clone());
    let prefix_type = LuaType::Ref(type_decl_id.clone());
    if let Some(should_stop) = append_keyed_dynamic_field(
        db,
        ctx,
        &owner,
        &prefix_type,
        members,
        filter,
        emmyrc.gmod.dynamic_fields_global,
    ) {
        return should_stop;
    }

    append_dynamic_field_names(
        db,
        ctx,
        &owner,
        &prefix_type,
        members,
        filter,
        emmyrc.gmod.dynamic_fields_global,
    )
}

/// Appends every dynamic field of `owner` this lookup sees, in name order.
fn append_dynamic_field_names(
    db: &DbIndex,
    ctx: &FindMembersContext,
    owner: &crate::DynamicFieldOwner,
    prefix_type: &LuaType,
    members: &mut Vec<LuaMemberInfo>,
    filter: &FindMemberFilter,
    dynamic_fields_global: bool,
) -> bool {
    let own_writes_only = reads_own_writes_only(ctx, owner);
    let in_file = (!dynamic_fields_global).then(|| ctx.file_id()).flatten();
    let index = db.get_dynamic_field_index();
    let mut field_names = if own_writes_only {
        index
            .get_direct_fields(owner)
            .map(|fields| {
                fields
                    .iter()
                    .filter(|(_, definitions)| {
                        in_file.is_none_or(|file_id| {
                            definitions
                                .iter()
                                .any(|definition| definition.file_id == file_id)
                        })
                    })
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else if let Some(file_id) = in_file {
        index
            .get_fields_in_file(owner, file_id)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    } else {
        index
            .get_fields(owner)
            .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };

    field_names.sort_unstable();
    field_names.dedup();
    let access_site = ctx.dynamic_field_access_site(db);
    let existing = members
        .iter()
        .map(|member| member.key.clone())
        .collect::<HashSet<_>>();

    for field_name in field_names {
        let member_key = LuaMemberKey::Name(field_name.clone());
        if !should_include_member(&member_key, filter) {
            continue;
        }

        if existing.contains(&member_key) {
            continue;
        }

        if let Some(site) = &access_site
            && !dynamic_field_visible_at_offset(db, owner, &field_name, site, own_writes_only)
        {
            continue;
        }

        let resolved = ctx.file_id().and_then(|file_id| {
            resolve_dynamic_field_member_for_file(
                db,
                file_id,
                prefix_type,
                &member_key,
                own_writes_only,
            )
        });

        members.push(LuaMemberInfo {
            property_owner_id: None,
            key: member_key,
            typ: resolved
                .map(|resolution| resolution.typ)
                .unwrap_or(LuaType::Any),
            feature: None,
            overload_index: None,
        });

        if should_stop_collecting(members.len(), filter) {
            return true;
        }
    }

    false
}

/// Whether this lookup sees only the fields written on `owner` itself: a class
/// reached from a subclass passes down those, not the ones its other subclasses'
/// writes put on it.
fn reads_own_writes_only(ctx: &FindMembersContext, owner: &crate::DynamicFieldOwner) -> bool {
    ctx.inherited && matches!(owner, crate::DynamicFieldOwner::Type(_))
}

fn append_dynamic_fields_for_table(
    db: &DbIndex,
    ctx: &FindMembersContext,
    table_range: &crate::InFiled<rowan::TextRange>,
    members: &mut Vec<LuaMemberInfo>,
    filter: &FindMemberFilter,
) -> bool {
    if !ctx.dynamic_fields || !crate::db_index::derivation::dynamic_fields_visible() {
        return false;
    }
    let emmyrc = db.get_optional_file_emmyrc(ctx.file_id);
    if !emmyrc.gmod.enabled || !emmyrc.gmod.infer_dynamic_fields {
        return false;
    }

    let owner = crate::canonical_dynamic_field_owner(
        db,
        crate::DynamicFieldOwner::Table(table_range.clone()),
    );
    let prefix_type = LuaType::TableConst(table_range.clone());
    if let Some(should_stop) = append_keyed_dynamic_field(
        db,
        ctx,
        &owner,
        &prefix_type,
        members,
        filter,
        emmyrc.gmod.dynamic_fields_global,
    ) {
        return should_stop;
    }

    append_dynamic_field_names(
        db,
        ctx,
        &owner,
        &prefix_type,
        members,
        filter,
        emmyrc.gmod.dynamic_fields_global,
    )
}

/// A dynamic field is only as visible as its assignments. GLua load order only
/// constrains *top-level* statements: a same-file, top-level definition that
/// starts after the caller position has not executed yet, so the field must not
/// be offered there. A definition inside a function body runs when that
/// function is called, which load order does not pin down, and a caller inside
/// a function body runs after the whole file has loaded — both stay visible.
/// Definitions in other files are always visible; their load-order and realm
/// rules are the member item's to decide.
///
/// This is deliberately broader than [`super::resolve_dynamic_field_member`]'s
/// execution-region rule, which answers what a read *yields* at a position: a
/// field a hook assigns further down its own body is a name the table has, even
/// on the call that has not reached the assignment yet.
fn dynamic_field_visible_at_offset(
    db: &DbIndex,
    owner: &crate::DynamicFieldOwner,
    field_name: &str,
    site: &DynamicFieldAccessSite,
    own_writes_only: bool,
) -> bool {
    let index = db.get_dynamic_field_index();
    let definitions = if own_writes_only {
        index.direct_definitions_of(owner, field_name)
    } else {
        index.field_definitions(owner, field_name)
    };
    if definitions.is_empty() {
        return true;
    }

    let member_index = db.get_member_index();
    definitions.iter().any(|definition| {
        if definition.file_id != site.file_id || definition.value.start() <= site.position {
            return true;
        }
        // A caller inside a function body runs after the whole file has loaded,
        // so every same-file definition is visible to it.
        if site.in_function {
            return true;
        }
        // A definition inside a function body runs when that function is
        // called, which load order does not pin down, so it stays visible.
        member_index
            .enclosing_function_scope_range(definition.file_id, definition.value.start())
            .is_some()
    })
}

fn append_keyed_dynamic_field(
    db: &DbIndex,
    ctx: &FindMembersContext,
    owner: &crate::DynamicFieldOwner,
    prefix_type: &LuaType,
    members: &mut Vec<LuaMemberInfo>,
    filter: &FindMemberFilter,
    dynamic_fields_global: bool,
) -> Option<bool> {
    let FindMemberFilter::ByKey { member_key, .. } = filter else {
        return None;
    };
    let LuaMemberKey::Name(field_name) = member_key else {
        return Some(false);
    };

    if members.iter().any(|member| member.key == *member_key) {
        return Some(false);
    }

    let own_writes_only = reads_own_writes_only(ctx, owner);
    let in_file = (!dynamic_fields_global).then(|| ctx.file_id()).flatten();
    let index = db.get_dynamic_field_index();
    let is_visible = if own_writes_only {
        index
            .direct_definitions_of(owner, field_name)
            .iter()
            .any(|definition| in_file.is_none_or(|file_id| definition.file_id == file_id))
    } else if let Some(file_id) = in_file {
        index.has_field_in_file(owner, field_name, file_id)
    } else {
        index.has_field(owner, field_name)
    };
    if !is_visible {
        return Some(false);
    }
    if let Some(site) = ctx.dynamic_field_access_site(db)
        && !dynamic_field_visible_at_offset(db, owner, field_name, &site, own_writes_only)
    {
        return Some(false);
    }

    let resolved = ctx.file_id().and_then(|file_id| {
        resolve_dynamic_field_member_for_file(db, file_id, prefix_type, member_key, own_writes_only)
    });
    members.push(LuaMemberInfo {
        property_owner_id: None,
        key: member_key.clone(),
        typ: resolved
            .map(|resolution| resolution.typ)
            .unwrap_or(LuaType::Any),
        feature: None,
        overload_index: None,
    });

    Some(should_stop_collecting(members.len(), filter))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use flagset::FlagSet;
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::{
        FindMembersContext, find_members, find_members_in_workspace_for_file,
        find_members_with_key, find_members_with_key_in_workspace_for_file,
        find_structural_members, super_types_for_context,
    };
    use crate::{
        DbIndex, DynamicFieldOwner, Emmyrc, FileId, GlobalId, GmodRealm, GmodRealmFileMetadata,
        InFiled, InferGuard, LuaMemberKey, LuaType, LuaTypeCache, LuaTypeDecl, LuaTypeDeclId,
        WorkspaceId,
        db_index::{
            LuaDeclTypeKind, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberOwner,
            WorkspaceKind,
        },
    };

    fn make_db() -> DbIndex {
        let mut db = DbIndex::new();
        db.get_module_index_mut()
            .set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        db
    }

    fn configure_workspaces(db: &mut DbIndex) -> (WorkspaceId, WorkspaceId, WorkspaceId) {
        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

        let module_index = db.get_module_index_mut();
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );

        (workspace_a, workspace_b, library_workspace)
    }

    fn make_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range),
            file_id,
        )
    }

    fn add_type_decl(db: &mut DbIndex, file_id: FileId, type_decl_id: LuaTypeDeclId) {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(TextSize::new(0), TextSize::new(1)),
                type_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                FlagSet::default(),
                type_decl_id,
            ),
        );
    }

    fn bind_member_type(db: &mut DbIndex, member_id: LuaMemberId, typ: LuaType) {
        db.get_type_index_mut()
            .bind_type(member_id.into(), LuaTypeCache::InferType(typ));
    }

    #[test]
    fn visible_super_types_deduplicate_equivalent_edges() {
        let mut db = make_db();
        let (workspace_a, _, _) = configure_workspaces(&mut db);
        let caller_file = FileId::new(1);
        let edge_file = FileId::new(2);
        let module_index = db.get_module_index_mut();
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/caller.lua");
        module_index.add_module_by_path(edge_file, "C:/Users/username/ProjectA/edges.lua");

        let child_id = LuaTypeDeclId::global("Child");
        let parent_type = LuaType::Ref(LuaTypeDeclId::global("Parent"));
        add_type_decl(&mut db, caller_file, child_id.clone());
        db.get_type_index_mut().add_super_type(
            child_id.clone(),
            edge_file,
            TextRange::new(10.into(), 20.into()),
            parent_type.clone(),
        );
        db.get_type_index_mut().add_super_type(
            child_id.clone(),
            edge_file,
            TextRange::new(30.into(), 40.into()),
            parent_type.clone(),
        );

        let context = FindMembersContext::new_with_workspace_file_and_position(
            InferGuard::new(),
            workspace_a,
            caller_file,
            TextSize::new(0),
        );
        let visible = super_types_for_context(&db, &child_id, &context).expect("super types");

        assert_eq!(visible, vec![&parent_type]);
    }

    fn set_file_realms(db: &mut DbIndex, file_realms: &[(FileId, GmodRealm)]) {
        db.get_gmod_infer_index_mut().set_all_realm_file_metadata(
            file_realms
                .iter()
                .map(|(file_id, realm)| {
                    (
                        *file_id,
                        GmodRealmFileMetadata {
                            inferred_realm: *realm,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        );
    }

    #[test]
    fn find_members_in_workspace_for_file_filters_type_members_by_workspace_and_realm() {
        let mut db = make_db();
        let (workspace_a, _, _) = configure_workspaces(&mut db);
        let type_decl_id = LuaTypeDeclId::global("ScopedOwner");

        let caller_file = FileId::new(1);
        let library_file = FileId::new(2);
        let other_main_file = FileId::new(3);
        let module_index = db.get_module_index_mut();
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        add_type_decl(&mut db, library_file, type_decl_id.clone());

        let key = LuaMemberKey::Name("value".into());
        let shared_member = make_member_id(library_file, 1);
        let isolated_member = make_member_id(other_main_file, 2);
        let owner = LuaMemberOwner::Type(type_decl_id.clone());
        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                shared_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                isolated_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        bind_member_type(&mut db, shared_member, LuaType::String);
        bind_member_type(&mut db, isolated_member, LuaType::Integer);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (library_file, GmodRealm::Shared),
                (other_main_file, GmodRealm::Server),
            ],
        );

        let all_members = find_members_in_workspace_for_file(
            &db,
            &LuaType::Ref(type_decl_id.clone()),
            workspace_a,
            caller_file,
        )
        .expect("members should resolve");
        assert_eq!(all_members.len(), 1);
        assert_eq!(all_members[0].key, key);
        assert_eq!(all_members[0].typ, LuaType::String);
        assert_eq!(all_members[0].property_owner_id, Some(shared_member.into()));

        let keyed_members = find_members_with_key_in_workspace_for_file(
            &db,
            &LuaType::Ref(type_decl_id),
            LuaMemberKey::Name("value".into()),
            false,
            workspace_a,
            caller_file,
        )
        .expect("keyed member should resolve");
        assert_eq!(keyed_members.len(), 1);
        assert_eq!(keyed_members[0].typ, LuaType::String);
        assert_eq!(
            keyed_members[0].property_owner_id,
            Some(shared_member.into())
        );
    }

    #[test]
    fn find_members_in_workspace_for_file_filters_global_path_members_by_workspace_and_realm() {
        let mut db = make_db();
        let (workspace_a, _, _) = configure_workspaces(&mut db);
        let type_decl_id = LuaTypeDeclId::global("ScopedFallback");

        let caller_file = FileId::new(10);
        let library_file = FileId::new(11);
        let other_main_file = FileId::new(12);
        let module_index = db.get_module_index_mut();
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        add_type_decl(&mut db, library_file, type_decl_id.clone());

        let key = LuaMemberKey::Name("ctor".into());
        let shared_member = make_member_id(library_file, 1);
        let isolated_member = make_member_id(other_main_file, 2);
        let owner = LuaMemberOwner::GlobalPath(GlobalId::new(type_decl_id.get_name()));
        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                shared_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                isolated_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        bind_member_type(&mut db, shared_member, LuaType::String);
        bind_member_type(&mut db, isolated_member, LuaType::Integer);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (library_file, GmodRealm::Shared),
                (other_main_file, GmodRealm::Server),
            ],
        );

        let members = find_members_with_key_in_workspace_for_file(
            &db,
            &LuaType::Ref(type_decl_id),
            LuaMemberKey::Name("ctor".into()),
            false,
            workspace_a,
            caller_file,
        )
        .expect("global-path fallback member should resolve");

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].key, key);
        assert_eq!(members[0].typ, LuaType::String);
        assert_eq!(members[0].property_owner_id, Some(shared_member.into()));
    }

    #[test]
    fn find_ref_members_include_global_path_without_type_declaration() {
        let mut db = make_db();
        let type_decl_id = LuaTypeDeclId::global("TOOL");
        let member_id = make_member_id(FileId::new(19), 1);
        let key = LuaMemberKey::Name("BuildCPanel".into());
        let owner = LuaMemberOwner::GlobalPath(GlobalId::new(type_decl_id.get_name()));

        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                member_id,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        bind_member_type(&mut db, member_id, LuaType::Function);

        let members = find_members_with_key(&db, &LuaType::Ref(type_decl_id), key.clone(), false)
            .expect("global-path member should resolve without a nominal type declaration");

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].key, key);
        assert_eq!(members[0].typ, LuaType::Function);
        assert_eq!(members[0].property_owner_id, Some(member_id.into()));
    }

    #[test]
    fn find_namespace_members_include_direct_global_path_members() {
        let mut db = make_db();
        let member_id = make_member_id(FileId::new(20), 1);
        let key = LuaMemberKey::Name("Print".into());
        let owner = LuaMemberOwner::GlobalPath(GlobalId::new("marauth.util"));

        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                member_id,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        bind_member_type(&mut db, member_id, LuaType::Function);

        let members = find_members_with_key(
            &db,
            &LuaType::Namespace(smol_str::SmolStr::new("marauth.util").into()),
            key.clone(),
            false,
        )
        .expect("namespace global-path member should resolve");

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].key, key);
        assert_eq!(members[0].typ, LuaType::Function);
        assert_eq!(members[0].property_owner_id, Some(member_id.into()));
    }

    #[test]
    fn find_members_with_key_is_stable_for_reversed_same_key_insertions() {
        let mut db_forward = make_db();
        let mut db_reversed = make_db();
        let type_decl_id = LuaTypeDeclId::global("StableSelectionOwner");
        let owner = LuaMemberOwner::Type(type_decl_id.clone());
        let key = LuaMemberKey::Name("stableField".into());

        let first_member = make_member_id(FileId::new(41), 1);
        let second_member = make_member_id(FileId::new(42), 2);

        add_type_decl(&mut db_forward, first_member.file_id, type_decl_id.clone());
        add_type_decl(&mut db_reversed, first_member.file_id, type_decl_id.clone());
        bind_member_type(&mut db_forward, first_member, LuaType::String);
        bind_member_type(&mut db_forward, second_member, LuaType::Integer);
        bind_member_type(&mut db_reversed, first_member, LuaType::String);
        bind_member_type(&mut db_reversed, second_member, LuaType::Integer);

        db_forward.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                first_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db_forward.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                second_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        db_reversed.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                second_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db_reversed.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                first_member,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        let selected_forward = find_members_with_key(
            &db_forward,
            &LuaType::Ref(type_decl_id.clone()),
            key.clone(),
            false,
        )
        .expect("forward insertion should resolve");
        let selected_reversed =
            find_members_with_key(&db_reversed, &LuaType::Ref(type_decl_id), key, false)
                .expect("reversed insertion should resolve");

        assert_eq!(
            selected_forward[0].property_owner_id,
            selected_reversed[0].property_owner_id
        );
        assert_eq!(selected_forward[0].typ, selected_reversed[0].typ);
    }

    #[test]
    fn keyed_dynamic_fields_respect_local_and_global_visibility() {
        let mut db = make_db();
        let caller_file = FileId::new(51);
        let other_file = FileId::new(52);
        let type_decl_id = LuaTypeDeclId::global("DynamicOwner");
        let field_name = "dynamicValue";
        let field_key = LuaMemberKey::Name(field_name.into());
        let definition_range = TextRange::new(TextSize::new(10), TextSize::new(11));
        add_type_decl(&mut db, caller_file, type_decl_id.clone());

        db.get_dynamic_field_index_mut().add_field(
            &InFiled::new(caller_file, definition_range),
            DynamicFieldOwner::Type(type_decl_id.clone()),
            field_name.into(),
            definition_range,
        );
        let table_range = InFiled::new(
            caller_file,
            TextRange::new(TextSize::new(20), TextSize::new(21)),
        );
        db.get_dynamic_field_index_mut().add_field(
            &InFiled::new(caller_file, definition_range),
            DynamicFieldOwner::Table(table_range.clone()),
            field_name.into(),
            definition_range,
        );

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.dynamic_fields_global = false;
        db.update_config(Arc::new(emmyrc));

        for prefix_type in [
            LuaType::Ref(type_decl_id.clone()),
            LuaType::TableConst(table_range),
        ] {
            let local_members = find_members_with_key_in_workspace_for_file(
                &db,
                &prefix_type,
                field_key.clone(),
                false,
                WorkspaceId::MAIN,
                caller_file,
            )
            .expect("keyed local dynamic-field lookup should return a result");
            assert_eq!(local_members.len(), 1);
            assert_eq!(local_members[0].key, field_key);

            let other_members = find_members_with_key_in_workspace_for_file(
                &db,
                &prefix_type,
                field_key.clone(),
                false,
                WorkspaceId::MAIN,
                other_file,
            )
            .expect("keyed non-contributing lookup should return a result");
            assert!(other_members.is_empty());
        }

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.dynamic_fields_global = true;
        db.update_config(Arc::new(emmyrc));
        let global_members = find_members_with_key_in_workspace_for_file(
            &db,
            &LuaType::Ref(type_decl_id),
            field_key,
            false,
            WorkspaceId::MAIN,
            other_file,
        )
        .expect("keyed global dynamic-field lookup should return a result");
        assert_eq!(global_members.len(), 1);
    }

    #[test]
    fn context_free_dynamic_fields_are_untyped_and_left_out_of_declared_members() {
        let mut db = make_db();
        let file_id = FileId::new(61);
        let type_decl_id = LuaTypeDeclId::global("PanelLike");
        add_type_decl(&mut db, file_id, type_decl_id.clone());
        let declared = make_member_id(file_id, 1);
        db.get_member_index_mut().add_member(
            LuaMemberOwner::Type(type_decl_id.clone()),
            LuaMember::new(
                declared,
                LuaMemberKey::Name("declared".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        bind_member_type(&mut db, declared, LuaType::String);
        for (at, name) in [(10, "dynamicA"), (20, "dynamicB")] {
            let range = TextRange::new(TextSize::new(at), TextSize::new(at + 1));
            db.get_dynamic_field_index_mut().add_field(
                &InFiled::new(file_id, range),
                DynamicFieldOwner::Type(type_decl_id.clone()),
                name.into(),
                range,
            );
        }
        let prefix = LuaType::Ref(type_decl_id);

        let all = find_members(&db, &prefix).expect("members");
        let dynamic = all
            .iter()
            .filter(|member| member.property_owner_id.is_none())
            .collect::<Vec<_>>();
        assert_eq!(dynamic.len(), 2);
        assert!(dynamic.iter().all(|member| member.typ == LuaType::Any));

        let declared_only = find_structural_members(&db, &prefix).expect("members");
        assert_eq!(declared_only.len(), 1);
        assert_eq!(declared_only[0].key, LuaMemberKey::Name("declared".into()));
    }
}
