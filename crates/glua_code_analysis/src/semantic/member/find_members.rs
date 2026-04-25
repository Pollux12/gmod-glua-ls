use std::collections::{HashMap, HashSet};

use rowan::TextSize;
use smol_str::SmolStr;

use crate::{
    DbIndex, FileId, GlobalId, InferGuardRef, LuaGenericType, LuaInstanceType, LuaIntersectionType,
    LuaMemberKey, LuaMemberOwner, LuaObjectType, LuaSemanticDeclId, LuaTupleType, LuaType,
    LuaTypeDeclId, LuaUnionType, WorkspaceId,
    semantic::{
        InferGuard,
        generic::{TypeSubstitutor, instantiate_type_generic},
    },
};

use super::{
    FindMembersResult, LuaMemberInfo, get_buildin_type_map_type_id, intersect_member_types,
};

#[derive(Debug, Clone)]
pub enum FindMemberFilter {
    /// 寻找所有成员
    All,
    /// 根据指定的key寻找成员
    ByKey {
        /// 要搜索的成员key
        member_key: LuaMemberKey,
        /// 是否寻找所有匹配的成员,为`false`时,找到第一个匹配的成员后停止
        find_all: bool,
    },
}

pub fn find_members(db: &DbIndex, prefix_type: &LuaType) -> FindMembersResult {
    let ctx = FindMembersContext::new(InferGuard::new());
    find_members_guard(db, prefix_type, &ctx, &FindMemberFilter::All)
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

#[derive(Clone)]
struct FindMembersContext {
    infer_guard: InferGuardRef,
    substitutor: Option<TypeSubstitutor>,
    workspace_id: Option<WorkspaceId>,
    file_id: Option<FileId>,
    caller_position: Option<TextSize>,
}

impl FindMembersContext {
    fn new(infer_guard: InferGuardRef) -> Self {
        Self {
            infer_guard,
            substitutor: None,
            workspace_id: None,
            file_id: None,
            caller_position: None,
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
        }
    }

    fn with_substitutor(&self, substitutor: TypeSubstitutor) -> Self {
        Self {
            infer_guard: self.infer_guard.clone(),
            substitutor: Some(substitutor),
            workspace_id: self.workspace_id,
            file_id: self.file_id,
            caller_position: self.caller_position,
        }
    }

    fn fork_infer(&self) -> Self {
        Self {
            infer_guard: self.infer_guard.fork(),
            substitutor: self.substitutor.clone(),
            workspace_id: self.workspace_id,
            file_id: self.file_id,
            caller_position: self.caller_position,
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

    fn has_workspace_scope(&self) -> bool {
        self.workspace_id.is_some() && self.file_id.is_some()
    }

    fn caller_position(&self) -> Option<TextSize> {
        self.caller_position
    }
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
            find_owner_members(db, ctx, &member_owner, filter)
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

/// 检查成员是否应该被包含
fn should_include_member(key: &LuaMemberKey, filter: &FindMemberFilter) -> bool {
    match filter {
        FindMemberFilter::All => true,
        FindMemberFilter::ByKey { member_key, .. } => member_key == key,
    }
}

/// 检查是否应该停止收集更多成员
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
    let type_decl = type_index.get_type_decl(type_decl_id)?;
    if type_decl.is_alias() {
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

    if members.is_empty()
        && let Some(global_members) = find_owner_members(
            db,
            ctx,
            &LuaMemberOwner::GlobalPath(GlobalId::new(type_decl_id.get_name())),
            filter,
        )
    {
        members.extend(global_members);

        if should_stop_collecting(members.len(), filter) {
            return Some(members);
        }
    }

    if type_decl.is_class()
        && let Some(super_types) = type_index.get_super_types(type_decl_id)
    {
        for super_type in super_types {
            let instantiated_super = ctx.instantiate_type(db, &super_type);
            if let Some(super_members) = find_members_guard(db, &instantiated_super, ctx, filter) {
                members.extend(super_members);

                if should_stop_collecting(members.len(), filter) {
                    return Some(members);
                }
            }
        }
    }

    if append_dynamic_fields_for_type(db, ctx, type_decl_id, &mut members, filter) {
        return Some(members);
    }

    Some(members)
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
    let owner_members = member_index.get_members(owner)?;

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
        FindMemberFilter::All => {
            let owner_members = member_index.get_members(owner)?;
            let mut seen = HashSet::new();
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
    for typ in union_type.into_vec().iter() {
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
    let mut members: HashMap<LuaMemberKey, LuaMemberInfo> = HashMap::new();

    for typ in intersection_type.get_types().iter() {
        let instantiated_type = ctx.instantiate_type(db, typ);
        let fork_ctx = ctx.fork_infer();
        let sub_members = find_members_guard(db, &instantiated_type, &fork_ctx, filter);
        let Some(sub_members) = sub_members else {
            continue;
        };

        // Within a single component type, treat duplicate keys as overrides (first wins).
        let mut component_seen: HashSet<LuaMemberKey> = HashSet::new();
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
    let substitutor =
        TypeSubstitutor::from_type_array_for_type(db, base_ref_id, instantiated_params);
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
    let global_decls = db.get_global_index().get_all_global_decl_ids();
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
                let ns_type = LuaType::Namespace(SmolStr::new(format!("{}.{}", ns, &name)).into());
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

    Some(members)
}

fn append_dynamic_fields_for_type(
    db: &DbIndex,
    ctx: &FindMembersContext,
    type_decl_id: &LuaTypeDeclId,
    members: &mut Vec<LuaMemberInfo>,
    filter: &FindMemberFilter,
) -> bool {
    let emmyrc = db.get_emmyrc();
    if !emmyrc.gmod.enabled || !emmyrc.gmod.infer_dynamic_fields {
        return false;
    }

    let index = db.get_dynamic_field_index();
    let mut field_names = if emmyrc.gmod.dynamic_fields_global {
        index
            .get_fields(type_decl_id)
            .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    } else if let Some(file_id) = ctx.file_id() {
        index
            .get_fields_in_file(type_decl_id, file_id)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    } else {
        index
            .get_fields(type_decl_id)
            .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };

    field_names.sort_unstable();

    for field_name in field_names {
        let member_key = LuaMemberKey::Name(field_name);
        if !should_include_member(&member_key, filter) {
            continue;
        }

        if members.iter().any(|member| member.key == member_key) {
            continue;
        }

        members.push(LuaMemberInfo {
            property_owner_id: None,
            key: member_key,
            typ: LuaType::Any,
            feature: None,
            overload_index: None,
        });

        if should_stop_collecting(members.len(), filter) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use flagset::FlagSet;
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::{find_members_in_workspace_for_file, find_members_with_key_in_workspace_for_file};
    use crate::{
        DbIndex, FileId, GlobalId, GmodRealm, GmodRealmFileMetadata, LuaMemberKey, LuaType,
        LuaTypeCache, LuaTypeDecl, LuaTypeDeclId, WorkspaceId,
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
}
