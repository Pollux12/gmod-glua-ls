use rustc_hash::FxHashMap;

use crate::{DbIndex, FileId, LuaMemberKey, LuaObjectType, LuaType, WorkspaceId};

use super::{
    LuaMemberInfo,
    find_members::{self},
};

pub fn get_member_map(
    db: &DbIndex,
    prefix_type: &LuaType,
) -> Option<FxHashMap<LuaMemberKey, Vec<LuaMemberInfo>>> {
    let members = find_members::find_members(db, prefix_type)?;
    build_member_map(members)
}

pub fn get_member_map_in_workspace_for_file(
    db: &DbIndex,
    prefix_type: &LuaType,
    workspace_id: WorkspaceId,
    file_id: FileId,
) -> Option<FxHashMap<LuaMemberKey, Vec<LuaMemberInfo>>> {
    let members =
        find_members::find_members_in_workspace_for_file(db, prefix_type, workspace_id, file_id)?;
    build_member_map(members)
}

pub fn get_member_map_in_workspace_for_file_at_offset(
    db: &DbIndex,
    prefix_type: &LuaType,
    workspace_id: WorkspaceId,
    file_id: FileId,
    caller_position: rowan::TextSize,
) -> Option<FxHashMap<LuaMemberKey, Vec<LuaMemberInfo>>> {
    let members = find_members::find_members_in_workspace_for_file_at_offset(
        db,
        prefix_type,
        workspace_id,
        file_id,
        caller_position,
    )?;
    build_member_map(members)
}

/// The fields of the table literal `typ` as a shape of their own, for a value
/// copied out of the file that holds the literal.
pub(crate) fn snapshot_table_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    if !matches!(typ, LuaType::TableConst(_)) {
        return None;
    }
    let fields = get_member_map(db, typ)?
        .into_iter()
        .filter_map(|(key, members)| {
            let member_type = LuaType::from_inferred_vec(
                members
                    .into_iter()
                    .map(|member| member.typ)
                    .collect::<Vec<_>>(),
            );
            (!member_type.is_never()).then_some((key, member_type))
        })
        .collect();
    Some(LuaObjectType::new_with_fields(fields, Vec::new()).into())
}

fn build_member_map(
    members: Vec<LuaMemberInfo>,
) -> Option<FxHashMap<LuaMemberKey, Vec<LuaMemberInfo>>> {
    let mut member_map = FxHashMap::default();
    for member in members {
        let key = member.key.clone();
        let typ = &member.typ;
        // Usually a generic instantiation inference result
        if let LuaType::Union(u) = typ
            && u.types().all(|f| f.is_function())
        {
            for (index, f) in u.types().enumerate() {
                let new_member = LuaMemberInfo {
                    key: key.clone(),
                    typ: f.clone(),
                    property_owner_id: member.property_owner_id.clone(),
                    feature: member.feature,
                    overload_index: Some(index),
                };

                member_map
                    .entry(key.clone())
                    .or_insert_with(Vec::new)
                    .push(new_member);
            }
            continue;
        }
        member_map.entry(key).or_insert_with(Vec::new).push(member);
    }

    Some(member_map)
}
