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

/// Maximum nesting depth of materialized by-value [`LuaObjectType`] trees.
/// Matches `bounded_depth::MAX_DEPTH`.
const MAX_SNAPSHOT_DEPTH: usize = 6;

/// Maximum expanded [`LuaObjectType`] nodes in a single materialized tree,
/// bounding downstream tree traversals (`PartialEq`, `visit_type`, `humanize`).
const MAX_SNAPSHOT_NODES: usize = 256;

struct SnapshotOutcome {
    typ: Option<LuaType>,
    nodes: usize,
    cacheable: bool,
}

struct SnapshotState {
    visited: rustc_hash::FxHashSet<LuaType>,
    memo: FxHashMap<(LuaType, usize), (Option<LuaType>, usize)>,
    remaining_nodes: usize,
}

/// The fields of the table literal `typ` as a shape of their own, for a value
/// copied out of the file that holds the literal.
pub(crate) fn snapshot_table_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    let mut state = SnapshotState {
        visited: rustc_hash::FxHashSet::default(),
        memo: FxHashMap::default(),
        remaining_nodes: MAX_SNAPSHOT_NODES,
    };
    snapshot_table_type_inner(db, typ, MAX_SNAPSHOT_DEPTH, &mut state).typ
}

fn snapshot_table_type_inner(
    db: &DbIndex,
    typ: &LuaType,
    depth: usize,
    state: &mut SnapshotState,
) -> SnapshotOutcome {
    if let LuaType::Union(union) = typ {
        let mut changed = false;
        let mut total_nodes = 0usize;
        let mut cacheable = true;
        let members = union
            .types()
            .map(|arm| {
                let outcome = snapshot_table_type_inner(db, arm, depth, state);
                total_nodes = total_nodes.saturating_add(outcome.nodes);
                cacheable &= outcome.cacheable;
                match outcome.typ {
                    Some(snap) => {
                        changed = true;
                        snap
                    }
                    None => arm.clone(),
                }
            })
            .collect::<Vec<_>>();
        return SnapshotOutcome {
            typ: changed.then(|| LuaType::from_vec(members)),
            nodes: total_nodes,
            cacheable,
        };
    }
    if !matches!(typ, LuaType::TableConst(_)) {
        return SnapshotOutcome {
            typ: None,
            nodes: 0,
            cacheable: true,
        };
    }
    if depth == 0 {
        return SnapshotOutcome {
            typ: Some(LuaType::Table),
            nodes: 0,
            cacheable: true,
        };
    }
    if state.visited.contains(typ) {
        return SnapshotOutcome {
            typ: None,
            nodes: 0,
            cacheable: false,
        };
    }
    let cache_key = (typ.clone(), depth);
    if let Some((cached, nodes)) = state.memo.get(&cache_key) {
        if *nodes <= state.remaining_nodes {
            state.remaining_nodes -= *nodes;
            return SnapshotOutcome {
                typ: cached.clone(),
                nodes: *nodes,
                cacheable: true,
            };
        }
        return SnapshotOutcome {
            typ: Some(LuaType::Table),
            nodes: 0,
            cacheable: false,
        };
    }
    if state.remaining_nodes == 0 {
        return SnapshotOutcome {
            typ: Some(LuaType::Table),
            nodes: 0,
            cacheable: false,
        };
    }

    state.visited.insert(typ.clone());
    let Some(member_map) = get_member_map(db, typ) else {
        state.visited.remove(typ);
        state.memo.insert(cache_key, (None, 0));
        return SnapshotOutcome {
            typ: None,
            nodes: 0,
            cacheable: true,
        };
    };

    state.remaining_nodes -= 1;
    let mut fields = std::collections::BTreeMap::new();
    let mut index_access = Vec::new();
    let mut entries = member_map.into_iter().collect::<Vec<_>>();
    entries.sort_by_cached_key(|(key, _)| format!("{key:?}"));
    let mut cacheable = true;
    let mut tree_nodes = 1usize;
    for (key, members) in entries {
        let member_type = LuaType::from_inferred_vec(
            members
                .into_iter()
                .map(|member| member.typ)
                .collect::<Vec<_>>(),
        );
        if member_type.is_never() {
            continue;
        }
        let outcome = snapshot_table_type_inner(db, &member_type, depth - 1, state);
        cacheable &= outcome.cacheable;
        tree_nodes = tree_nodes.saturating_add(outcome.nodes);
        let member_type = outcome.typ.unwrap_or(member_type);
        match key {
            LuaMemberKey::ExprType(key_type) => index_access.push((key_type, member_type)),
            other => {
                fields.insert(other, member_type);
            }
        }
    }
    state.visited.remove(typ);
    let result: LuaType = LuaObjectType::new_with_fields(fields, index_access).into();
    if cacheable {
        state
            .memo
            .insert(cache_key, (Some(result.clone()), tree_nodes));
    }
    SnapshotOutcome {
        typ: Some(result),
        nodes: tree_nodes,
        cacheable,
    }
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
