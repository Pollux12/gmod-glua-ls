use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use rowan::TextSize;
use smol_str::SmolStr;

use crate::{DbIndex, FileId, LuaMemberKey, LuaMemberOwner, LuaType, LuaTypeDeclId};

pub type PrecomputedSubtypeMembers =
    HashMap<LuaTypeDeclId, HashMap<LuaMemberKey, SubtypeMemberCandidates>>;

#[derive(Debug, Clone, Default)]
pub struct SubtypeMemberCandidates {
    direct_subtype_ids: Arc<Vec<LuaTypeDeclId>>,
    transitive_subtype_ids: Arc<Vec<LuaTypeDeclId>>,
}

#[derive(Debug, Default)]
pub struct SubtypeMemberQueryResult {
    pub direct_candidates: Vec<SmolStr>,
    pub has_transitive_hit: bool,
}

pub fn precompute_subtype_members(db: &DbIndex) -> PrecomputedSubtypeMembers {
    if !db.get_emmyrc().gmod.enabled {
        return HashMap::new();
    }

    let type_index = db.get_type_index();
    let member_index = db.get_member_index();
    let mut fields: HashMap<LuaTypeDeclId, HashMap<LuaMemberKey, SubtypeMemberCandidateSets>> =
        HashMap::new();

    for type_decl in type_index.get_all_types() {
        let type_id = type_decl.get_id();
        let owner = LuaMemberOwner::Type(type_id.clone());
        let Some(members) = member_index.get_members(&owner) else {
            continue;
        };

        let member_keys = members
            .iter()
            .filter_map(|member| match member.get_key() {
                LuaMemberKey::Name(name) => Some(LuaMemberKey::Name(name.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if member_keys.is_empty() {
            continue;
        }

        let mut super_types = Vec::new();
        type_id.collect_super_types(db, &mut super_types);
        for super_type in super_types {
            if let LuaType::Ref(super_id) | LuaType::Def(super_id) = super_type {
                let fields_for_base = fields.entry(super_id).or_default();
                for member_key in &member_keys {
                    fields_for_base
                        .entry(member_key.clone())
                        .or_default()
                        .transitive_subtype_ids
                        .insert(type_id.clone());
                }
            }
        }

        if let Some(super_types) = type_index.get_super_types_iter(&type_id) {
            for super_type in super_types {
                if let LuaType::Ref(super_id) | LuaType::Def(super_id) = super_type {
                    let fields_for_base = fields.entry(super_id.clone()).or_default();
                    for member_key in &member_keys {
                        fields_for_base
                            .entry(member_key.clone())
                            .or_default()
                            .direct_subtype_ids
                            .insert(type_id.clone());
                    }
                }
            }
        }
    }

    fields
        .into_iter()
        .map(|(base_type_id, fields)| {
            let fields = fields
                .into_iter()
                .map(|(member_key, candidates)| (member_key, candidates.into_precomputed(db)))
                .collect();
            (base_type_id, fields)
        })
        .collect()
}

pub fn query_subtype_member(
    db: &DbIndex,
    subtype_members: &PrecomputedSubtypeMembers,
    prefix_typ: &LuaType,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> SubtypeMemberQueryResult {
    if !db.get_emmyrc().gmod.enabled {
        return SubtypeMemberQueryResult::default();
    }

    query_subtype_member_from_index(
        db,
        subtype_members,
        prefix_typ,
        member_key,
        caller_file_id,
        caller_position,
    )
}

fn query_subtype_member_from_index(
    db: &DbIndex,
    subtype_members: &PrecomputedSubtypeMembers,
    prefix_typ: &LuaType,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> SubtypeMemberQueryResult {
    match prefix_typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => query_type_subtype_member(
            db,
            subtype_members,
            type_id,
            member_key,
            caller_file_id,
            caller_position,
        ),
        LuaType::TableOf(inner) => query_subtype_member_from_index(
            db,
            subtype_members,
            inner,
            member_key,
            caller_file_id,
            caller_position,
        ),
        LuaType::Union(union) => {
            let mut result = SubtypeMemberQueryResult::default();
            for typ in union.types().filter(|typ| !typ.is_nil()) {
                let nested = query_subtype_member_from_index(
                    db,
                    subtype_members,
                    typ,
                    member_key,
                    caller_file_id,
                    caller_position,
                );
                result.direct_candidates.extend(nested.direct_candidates);
                result.has_transitive_hit |= nested.has_transitive_hit;
            }
            result.direct_candidates.sort();
            result.direct_candidates.dedup();
            result
        }
        _ => SubtypeMemberQueryResult::default(),
    }
}

fn query_type_subtype_member(
    db: &DbIndex,
    subtype_members: &PrecomputedSubtypeMembers,
    base_type_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> SubtypeMemberQueryResult {
    let Some(candidates) = subtype_members
        .get(base_type_id)
        .and_then(|fields| fields.get(member_key))
    else {
        return SubtypeMemberQueryResult::default();
    };

    let mut direct_candidates = candidates
        .direct_subtype_ids
        .iter()
        .filter(|subtype_id| {
            subtype_has_visible_member(db, subtype_id, member_key, caller_file_id, caller_position)
        })
        .filter_map(|subtype_id| subtype_display_name(db, subtype_id))
        .collect::<Vec<_>>();
    direct_candidates.sort();
    direct_candidates.dedup();

    let has_transitive_hit = direct_candidates.is_empty()
        && candidates.transitive_subtype_ids.iter().any(|subtype_id| {
            subtype_has_visible_member(db, subtype_id, member_key, caller_file_id, caller_position)
        });

    SubtypeMemberQueryResult {
        direct_candidates,
        has_transitive_hit,
    }
}

fn subtype_has_visible_member(
    db: &DbIndex,
    subtype_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> bool {
    let owner = LuaMemberOwner::Type(subtype_id.clone());
    db.get_member_index()
        .get_member_item(&owner, member_key)
        .is_some_and(|member_item| {
            !member_item
                .visible_member_ids_with_realm_at_offset(db, &caller_file_id, caller_position)
                .is_empty()
        })
}

fn subtype_display_name(db: &DbIndex, subtype_id: &LuaTypeDeclId) -> Option<SmolStr> {
    db.get_type_index()
        .get_type_decl(subtype_id)
        .map(|decl| SmolStr::new(decl.get_name()))
}

#[derive(Debug, Default)]
struct SubtypeMemberCandidateSets {
    direct_subtype_ids: HashSet<LuaTypeDeclId>,
    transitive_subtype_ids: HashSet<LuaTypeDeclId>,
}

impl SubtypeMemberCandidateSets {
    fn into_precomputed(self, db: &DbIndex) -> SubtypeMemberCandidates {
        let mut direct_subtype_ids = self.direct_subtype_ids.into_iter().collect::<Vec<_>>();
        sort_type_ids_by_name(db, &mut direct_subtype_ids);

        let mut transitive_subtype_ids =
            self.transitive_subtype_ids.into_iter().collect::<Vec<_>>();
        sort_type_ids_by_name(db, &mut transitive_subtype_ids);

        SubtypeMemberCandidates {
            direct_subtype_ids: Arc::new(direct_subtype_ids),
            transitive_subtype_ids: Arc::new(transitive_subtype_ids),
        }
    }
}

fn sort_type_ids_by_name(db: &DbIndex, type_ids: &mut [LuaTypeDeclId]) {
    type_ids.sort_by(|left, right| {
        let left_name = db
            .get_type_index()
            .get_type_decl(left)
            .map(|decl| decl.get_name())
            .unwrap_or_else(|| left.get_name());
        let right_name = db
            .get_type_index()
            .get_type_decl(right)
            .map(|decl| decl.get_name())
            .unwrap_or_else(|| right.get_name());
        left_name.cmp(right_name)
    });
}
