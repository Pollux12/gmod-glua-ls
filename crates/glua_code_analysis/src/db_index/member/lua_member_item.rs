use std::collections::BTreeMap;

use crate::{
    DbIndex, FileId, InferFailReason, LuaSemanticDeclId, LuaType, TypeOps,
    db_index::gmod_infer::GmodRealm,
};
use rowan::TextSize;

use super::LuaMemberId;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaMemberIndexItem {
    One(LuaMemberId),
    Many(Vec<LuaMemberId>),
}

impl LuaMemberIndexItem {
    pub fn resolve_type(&self, db: &DbIndex) -> Result<LuaType, InferFailReason> {
        resolve_member_type(db, self)
    }

    pub fn resolve_type_with_realm(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
    ) -> Result<LuaType, InferFailReason> {
        resolve_member_type_with_realm(db, self, caller_file_id)
    }

    pub fn resolve_type_with_realm_at_offset(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Result<LuaType, InferFailReason> {
        resolve_member_type_with_realm_at_offset(db, self, caller_file_id, caller_position)
    }

    pub fn resolve_semantic_decl(&self, db: &DbIndex) -> Option<LuaSemanticDeclId> {
        resolve_member_semantic_id(db, self)
    }

    pub fn resolve_semantic_decl_with_realm(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
    ) -> Option<LuaSemanticDeclId> {
        resolve_member_semantic_id_with_realm(db, self, caller_file_id)
    }

    pub fn resolve_semantic_decl_with_realm_at_offset(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Option<LuaSemanticDeclId> {
        resolve_member_semantic_id_with_realm_at_offset(db, self, caller_file_id, caller_position)
    }

    #[allow(unused)]
    pub fn resolve_type_owner_member_id(&self, db: &DbIndex) -> Option<LuaMemberId> {
        resolve_type_owner_member_id(db, self)
    }

    pub fn is_one(&self) -> bool {
        matches!(self, LuaMemberIndexItem::One(_))
    }

    pub fn visible_member_ids_with_realm(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
    ) -> Vec<LuaMemberId> {
        let member_ids = self.get_member_ids();
        let priority_tiers = get_member_id_priority_tiers(db, caller_file_id, &member_ids);
        select_member_ids_by_workspace_and_realm(
            db,
            priority_tiers,
            infer_caller_file_realm(db, caller_file_id),
        )
    }

    pub fn visible_member_ids_with_realm_at_offset(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Vec<LuaMemberId> {
        let member_ids = self.get_member_ids();
        let priority_tiers = get_member_id_priority_tiers(db, caller_file_id, &member_ids);
        select_member_ids_by_workspace_and_realm(
            db,
            priority_tiers,
            db.get_gmod_infer_index()
                .get_realm_at_offset(caller_file_id, caller_position),
        )
    }

    pub fn get_member_ids(&self) -> Vec<LuaMemberId> {
        match self {
            LuaMemberIndexItem::One(member_id) => vec![*member_id],
            LuaMemberIndexItem::Many(member_ids) => member_ids.clone(),
        }
    }
}

fn resolve_member_type(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
) -> Result<LuaType, InferFailReason> {
    match member_item {
        LuaMemberIndexItem::One(member_id) => {
            let member_type_cache = db.get_type_index().get_type_cache(&(*member_id).into());
            match member_type_cache {
                Some(cache) => Ok(cache.as_type().clone()),
                None => Err(InferFailReason::UnResolveMemberType(*member_id)),
            }
        }
        LuaMemberIndexItem::Many(member_ids) => {
            let mut resolve_state = MemberTypeResolveState::All;
            let mut members = vec![];
            for member_id in member_ids {
                if let Some(member) = db.get_member_index().get_member(member_id) {
                    members.push(member);
                } else {
                    return Err(InferFailReason::None);
                }
            }
            let all_file_defines = members
                .iter()
                .all(|member| member.get_feature().is_file_define());
            let should_prefer_doc_file_defines = all_file_defines
                && members.iter().any(|member| {
                    db.get_type_index()
                        .get_type_cache(&member.get_id().into())
                        .is_some_and(|cache| cache.is_doc())
                });
            let should_widen_file_defines =
                !should_prefer_doc_file_defines && members.len() > 1 && all_file_defines;
            if db.get_emmyrc().strict.meta_override_file_define {
                for member in &members {
                    let feature = member.get_feature();
                    if feature.is_meta_decl() {
                        resolve_state = MemberTypeResolveState::Meta;
                        break;
                    } else if feature.is_file_decl() {
                        resolve_state = MemberTypeResolveState::FileDecl;
                    }
                }
            }

            match resolve_state {
                MemberTypeResolveState::All => {
                    let mut typ = LuaType::Unknown;
                    for member in members {
                        let member_type_cache = db
                            .get_type_index()
                            .get_type_cache(&member.get_id().into())
                            .ok_or(InferFailReason::UnResolveMemberType(member.get_id()))?;
                        if should_prefer_doc_file_defines && !member_type_cache.is_doc() {
                            continue;
                        }

                        let member_type = member_type_cache.as_type();
                        let member_type = if should_widen_file_defines {
                            crate::widen_literal_type_for_assignment(member_type)
                        } else {
                            member_type.clone()
                        };
                        typ = TypeOps::Union.apply(db, &typ, &member_type);
                    }
                    Ok(typ)
                }
                MemberTypeResolveState::Meta => {
                    let mut typ = LuaType::Unknown;
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_meta_decl() {
                            typ = TypeOps::Union.apply(
                                db,
                                &typ,
                                db.get_type_index()
                                    .get_type_cache(&member.get_id().into())
                                    .ok_or(InferFailReason::UnResolveMemberType(member.get_id()))?
                                    .as_type(),
                            );
                        }
                    }
                    Ok(typ)
                }
                MemberTypeResolveState::FileDecl => {
                    let mut typ = LuaType::Unknown;
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_decl() {
                            typ = TypeOps::Union.apply(
                                db,
                                &typ,
                                db.get_type_index()
                                    .get_type_cache(&member.get_id().into())
                                    .ok_or(InferFailReason::UnResolveMemberType(member.get_id()))?
                                    .as_type(),
                            );
                        }
                    }
                    Ok(typ)
                }
            }
        }
    }
}

fn resolve_member_type_with_realm(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
) -> Result<LuaType, InferFailReason> {
    let visible_member_ids = member_item.visible_member_ids_with_realm(db, caller_file_id);
    if visible_member_ids.is_empty() {
        return resolve_member_type(db, &LuaMemberIndexItem::Many(vec![]));
    }

    resolve_member_type(db, &member_item_from_ids(visible_member_ids))
}

fn resolve_member_type_with_realm_at_offset(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> Result<LuaType, InferFailReason> {
    let visible_member_ids =
        member_item.visible_member_ids_with_realm_at_offset(db, caller_file_id, caller_position);
    if visible_member_ids.is_empty() {
        return resolve_member_type(db, &LuaMemberIndexItem::Many(vec![]));
    }

    resolve_member_type(db, &member_item_from_ids(visible_member_ids))
}

fn resolve_member_semantic_id_with_realm(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
) -> Option<LuaSemanticDeclId> {
    let visible_member_ids = member_item.visible_member_ids_with_realm(db, caller_file_id);

    resolve_member_semantic_id(db, &member_item_from_ids(visible_member_ids))
}

fn resolve_member_semantic_id_with_realm_at_offset(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> Option<LuaSemanticDeclId> {
    let visible_member_ids =
        member_item.visible_member_ids_with_realm_at_offset(db, caller_file_id, caller_position);

    resolve_member_semantic_id(db, &member_item_from_ids(visible_member_ids))
}

fn infer_caller_file_realm(db: &DbIndex, caller_file_id: &FileId) -> GmodRealm {
    db.get_gmod_infer_index()
        .get_realm_file_metadata(caller_file_id)
        .map(|metadata| metadata.inferred_realm)
        .unwrap_or(GmodRealm::Unknown)
}

fn get_member_id_priority_tiers(
    db: &DbIndex,
    caller_file_id: &FileId,
    member_ids: &[LuaMemberId],
) -> Vec<(u8, Vec<LuaMemberId>)> {
    let module_index = db.get_module_index();
    let Some(caller_workspace_id) = module_index.get_workspace_id(*caller_file_id) else {
        return vec![(0, member_ids.to_vec())];
    };

    let mut priority_tiers = BTreeMap::new();
    for member_id in member_ids {
        let candidate_workspace_id = module_index
            .get_workspace_id(member_id.file_id)
            .unwrap_or(crate::WorkspaceId::MAIN);
        let Some(priority) =
            module_index.workspace_resolution_priority(caller_workspace_id, candidate_workspace_id)
        else {
            continue;
        };

        priority_tiers
            .entry(priority)
            .or_insert_with(Vec::new)
            .push(*member_id);
    }

    priority_tiers.into_iter().collect()
}

fn select_member_ids_by_workspace_and_realm(
    db: &DbIndex,
    priority_tiers: Vec<(u8, Vec<LuaMemberId>)>,
    caller_realm: GmodRealm,
) -> Vec<LuaMemberId> {
    let fallback_member_ids = priority_tiers
        .first()
        .map(|(_, member_ids)| member_ids.clone())
        .unwrap_or_default();

    if !db.get_emmyrc().gmod.enabled {
        return fallback_member_ids;
    }

    let infer_index = db.get_gmod_infer_index();
    for (_, tier_member_ids) in priority_tiers {
        let compatible_member_ids = tier_member_ids
            .into_iter()
            .filter(|member_id| {
                let member_realm =
                    infer_index.get_realm_at_offset(&member_id.file_id, member_id.get_position());
                is_realm_compatible(caller_realm, member_realm)
            })
            .collect::<Vec<_>>();
        if !compatible_member_ids.is_empty() {
            return compatible_member_ids;
        }
    }

    fallback_member_ids
}

fn member_item_from_ids(member_ids: Vec<LuaMemberId>) -> LuaMemberIndexItem {
    match member_ids.len() {
        0 => LuaMemberIndexItem::Many(vec![]),
        1 => LuaMemberIndexItem::One(member_ids[0]),
        _ => LuaMemberIndexItem::Many(member_ids),
    }
}

fn is_realm_compatible(call_realm: GmodRealm, decl_realm: GmodRealm) -> bool {
    !matches!(
        (call_realm, decl_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

fn resolve_type_owner_member_id(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
) -> Option<LuaMemberId> {
    match member_item {
        LuaMemberIndexItem::One(member_id) => Some(*member_id),
        LuaMemberIndexItem::Many(member_ids) => {
            let member_index = db.get_member_index();
            let mut resolve_state = MemberTypeResolveState::All;
            let members = member_ids
                .iter()
                .map(|id| member_index.get_member(id))
                .collect::<Option<Vec<_>>>()?;
            for member in &members {
                let feature = member.get_feature();
                if feature.is_meta_decl() {
                    resolve_state = MemberTypeResolveState::Meta;
                    break;
                } else if feature.is_file_decl() {
                    resolve_state = MemberTypeResolveState::FileDecl;
                }
            }

            match resolve_state {
                MemberTypeResolveState::All => {
                    for member in members {
                        let member_type_cache = db
                            .get_type_index()
                            .get_type_cache(&member.get_id().into())?;
                        if member_type_cache.as_type().is_member_owner() {
                            return Some(member.get_id());
                        }
                    }

                    None
                }
                MemberTypeResolveState::Meta => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_meta_decl() {
                            return Some(member.get_id());
                        }
                    }

                    None
                }
                MemberTypeResolveState::FileDecl => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_decl() {
                            return Some(member.get_id());
                        }
                    }

                    None
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberTypeResolveState {
    All,
    Meta,
    FileDecl,
}

fn resolve_member_semantic_id(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
) -> Option<LuaSemanticDeclId> {
    match member_item {
        LuaMemberIndexItem::One(member_id) => Some(LuaSemanticDeclId::Member(*member_id)),
        LuaMemberIndexItem::Many(member_ids) => {
            let mut resolve_state = MemberSemanticDeclResolveState::MetaOrNone;
            let members = member_ids
                .iter()
                .map(|id| db.get_member_index().get_member(id))
                .collect::<Option<Vec<_>>>()?;
            for member in &members {
                let feature = member.get_feature();
                if feature.is_file_define() {
                    resolve_state = MemberSemanticDeclResolveState::FirstDefine;
                } else if feature.is_file_decl() {
                    resolve_state = MemberSemanticDeclResolveState::FileDecl;
                    break;
                }
            }

            match resolve_state {
                MemberSemanticDeclResolveState::MetaOrNone => {
                    let mut last_valid_member =
                        LuaSemanticDeclId::Member(members.first()?.get_id());
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_meta_decl() {
                            let semantic_id = LuaSemanticDeclId::Member(member.get_id());
                            last_valid_member = semantic_id.clone();
                            if check_member_version(db, semantic_id.clone()) {
                                return Some(semantic_id);
                            }
                        }
                    }

                    Some(last_valid_member)
                }
                MemberSemanticDeclResolveState::FirstDefine => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_define() {
                            return Some(LuaSemanticDeclId::Member(member.get_id()));
                        }
                    }

                    None
                }
                MemberSemanticDeclResolveState::FileDecl => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_decl() {
                            return Some(LuaSemanticDeclId::Member(member.get_id()));
                        }
                    }

                    None
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberSemanticDeclResolveState {
    MetaOrNone,
    FirstDefine,
    FileDecl,
}

fn check_member_version(db: &DbIndex, semantic_id: LuaSemanticDeclId) -> bool {
    let Some(property) = db.get_property_index().get_property(&semantic_id) else {
        return true;
    };

    if let Some(version) = property.version_conds() {
        let version_number = db.get_emmyrc().runtime.version.to_lua_version_number();
        return version.iter().any(|cond| cond.check(&version_number));
    }

    true
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::{
        LuaMemberIndexItem, get_member_id_priority_tiers, select_member_ids_by_workspace_and_realm,
    };
    use crate::{
        DbIndex, FileId, GmodRealm, GmodRealmFileMetadata, GmodRealmRange, LuaSemanticDeclId,
        LuaTypeDeclId, WorkspaceId,
        db_index::{
            LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaMemberOwner, WorkspaceKind,
        },
    };

    fn make_db() -> DbIndex {
        let mut db = DbIndex::new();
        db.get_module_index_mut()
            .set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        db
    }

    fn make_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range),
            file_id,
        )
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
    fn member_id_priority_tiers_keep_workspace_priority_order() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

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
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/.lua/std").into(),
            WorkspaceId::STD,
            WorkspaceKind::Std,
        );

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let library_file = FileId::new(2);
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );

        let std_file = FileId::new(3);
        module_index.add_module_by_path(std_file, "C:/Users/username/.lua/std/math.lua");

        let other_main_file = FileId::new(4);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let library_member = make_member_id(library_file, 1);
        let std_member = make_member_id(std_file, 2);
        let other_main_member = make_member_id(other_main_file, 3);

        let tiers = get_member_id_priority_tiers(
            &db,
            &caller_file,
            &[other_main_member, std_member, library_member],
        );

        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0], (1, vec![library_member]));
        assert_eq!(tiers[1], (2, vec![std_member]));
    }

    #[test]
    fn select_member_ids_by_workspace_and_realm_uses_first_compatible_tier() {
        let mut db = make_db();
        let tier_one_member = make_member_id(FileId::new(10), 1);
        let tier_two_member = make_member_id(FileId::new(11), 2);

        set_file_realms(
            &mut db,
            &[
                (tier_one_member.file_id, GmodRealm::Shared),
                (tier_two_member.file_id, GmodRealm::Unknown),
            ],
        );

        let selected = select_member_ids_by_workspace_and_realm(
            &db,
            vec![(0, vec![tier_one_member]), (1, vec![tier_two_member])],
            GmodRealm::Client,
        );

        assert_eq!(selected, vec![tier_one_member]);
    }

    #[test]
    fn select_member_ids_by_workspace_and_realm_falls_back_to_best_tier_when_needed() {
        let mut db = make_db();
        let server_member = make_member_id(FileId::new(20), 1);
        let unknown_member = make_member_id(FileId::new(21), 2);

        set_file_realms(
            &mut db,
            &[
                (server_member.file_id, GmodRealm::Server),
                (unknown_member.file_id, GmodRealm::Server),
            ],
        );

        let selected = select_member_ids_by_workspace_and_realm(
            &db,
            vec![(0, vec![server_member]), (1, vec![unknown_member])],
            GmodRealm::Client,
        );

        assert_eq!(selected, vec![server_member]);
    }

    #[test]
    fn resolve_semantic_decl_with_realm_prefers_first_compatible_tier() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

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

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let library_file = FileId::new(2);
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );

        let other_main_file = FileId::new(3);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let library_member = make_member_id(library_file, 1);
        let other_main_member = make_member_id(other_main_file, 2);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("Owner"));

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                library_member,
                LuaMemberKey::Name("value".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                other_main_member,
                LuaMemberKey::Name("value".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (library_file, GmodRealm::Shared),
                (other_main_file, GmodRealm::Server),
            ],
        );

        let item = LuaMemberIndexItem::Many(vec![other_main_member, library_member]);
        let semantic_decl = item.resolve_semantic_decl_with_realm(&db, &caller_file);

        assert_eq!(
            semantic_decl,
            Some(LuaSemanticDeclId::Member(library_member))
        );
    }

    #[test]
    fn resolve_semantic_decl_with_realm_at_offset_prefers_branch_compatible_member() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let branch_file = FileId::new(11);
        let client_member = make_member_id(branch_file, 1);
        let server_member = make_member_id(branch_file, 20);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("BranchOwner"));

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                client_member,
                LuaMemberKey::Name("branchValue".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                server_member,
                LuaMemberKey::Name("branchValue".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        db.get_gmod_infer_index_mut().set_all_realm_file_metadata(
            [
                (
                    caller_file,
                    GmodRealmFileMetadata {
                        inferred_realm: GmodRealm::Shared,
                        branch_realm_ranges: vec![
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(0), TextSize::new(10)),
                                realm: GmodRealm::Client,
                            },
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(10), TextSize::new(30)),
                                realm: GmodRealm::Server,
                            },
                        ],
                        ..Default::default()
                    },
                ),
                (
                    branch_file,
                    GmodRealmFileMetadata {
                        inferred_realm: GmodRealm::Shared,
                        branch_realm_ranges: vec![
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(0), TextSize::new(10)),
                                realm: GmodRealm::Client,
                            },
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(10), TextSize::new(30)),
                                realm: GmodRealm::Server,
                            },
                        ],
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
        );

        let item = LuaMemberIndexItem::Many(vec![client_member, server_member]);

        let client_decl =
            item.resolve_semantic_decl_with_realm_at_offset(&db, &caller_file, TextSize::new(1));
        let server_decl =
            item.resolve_semantic_decl_with_realm_at_offset(&db, &caller_file, TextSize::new(20));

        assert_eq!(client_decl, Some(LuaSemanticDeclId::Member(client_member)));
        assert_eq!(server_decl, Some(LuaSemanticDeclId::Member(server_member)));
    }
}
