use glua_code_analysis::{
    DbIndex, FileId, GmodRealm, LuaMemberInfo, LuaMemberKey, LuaSemanticDeclId, LuaType,
    LuaTypeDeclId, SemanticModel, enum_variable_is_param, get_tpl_ref_extend_type,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaComment, LuaCommentOwner, LuaDocTag, LuaDocTagRealm, LuaExpr,
    LuaFuncStat, LuaIndexExpr, LuaLocalFuncStat, LuaStringToken,
};
use rowan::TextSize;
use std::collections::{HashMap, HashSet};

use crate::handlers::completion::{
    add_completions::{CompletionTriggerStatus, add_member_completion},
    completion_builder::CompletionBuilder,
};

pub fn add_completion(builder: &mut CompletionBuilder) -> Option<()> {
    if builder.is_cancelled() {
        return None;
    }

    let index_expr = LuaIndexExpr::cast(builder.trigger_token.parent()?)?;
    let index_token = index_expr.get_index_token()?;
    let completion_status = if index_token.is_dot() {
        CompletionTriggerStatus::Dot
    } else if index_token.is_colon() {
        CompletionTriggerStatus::Colon
    } else if LuaStringToken::can_cast(builder.trigger_token.kind().into()) {
        CompletionTriggerStatus::InString
    } else {
        CompletionTriggerStatus::LeftBracket
    };

    let prefix_expr = index_expr.get_prefix_expr()?;
    let prefix_type = match builder
        .semantic_model
        .infer_expr(prefix_expr.clone())
        .ok()?
    {
        LuaType::TplRef(tpl) => get_tpl_ref_extend_type(
            builder.semantic_model.get_db(),
            &mut builder.semantic_model.get_cache().borrow_mut(),
            &LuaType::TplRef(tpl.clone()),
            prefix_expr.clone(),
            0,
        )?,
        prefix_type => prefix_type,
    };
    // 如果是枚举类型且为函数参数, 则不进行补全
    if enum_variable_is_param(
        builder.semantic_model.get_db(),
        &mut builder.semantic_model.get_cache().borrow_mut(),
        &index_expr,
        &prefix_type,
    )
    .is_some()
    {
        return None;
    }

    let mut member_info_map = builder
        .semantic_model
        .get_member_info_map_at_offset(&prefix_type, builder.position_offset)
        .unwrap_or_default();
    extend_gmod_hook_fallback_members(builder, &prefix_expr, &prefix_type, &mut member_info_map);

    add_completions_for_members(builder, &member_info_map, completion_status)
}

fn extend_gmod_hook_fallback_members(
    builder: &CompletionBuilder,
    prefix_expr: &LuaExpr,
    prefix_type: &LuaType,
    members: &mut HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
) {
    if !builder.semantic_model.get_emmyrc().gmod.enabled {
        return;
    }

    let owner_name = match prefix_type {
        LuaType::Ref(owner_type_decl_id) => Some(owner_type_decl_id.get_simple_name().to_string()),
        _ => match prefix_expr {
            LuaExpr::NameExpr(name_expr) => name_expr.get_name_text(),
            _ => None,
        },
    };

    let Some(owner_name) = owner_name else {
        return;
    };

    let owner_candidates = gmod_hook_owner_candidates(owner_name.as_str());
    if owner_candidates.is_empty() {
        return;
    }

    let mut existing: HashMap<LuaMemberKey, HashSet<Option<LuaSemanticDeclId>>> = HashMap::new();
    for (key, infos) in members.iter() {
        let entry = existing.entry(key.clone()).or_default();
        for info in infos {
            entry.insert(info.property_owner_id.clone());
        }
    }

    for owner_candidate in owner_candidates {
        let owner_type = LuaType::Ref(LuaTypeDeclId::global(owner_candidate));
        let Some(fallback_map) = builder
            .semantic_model
            .get_member_info_map_at_offset(&owner_type, builder.position_offset)
        else {
            continue;
        };

        for (key, fallback_infos) in fallback_map {
            let owners = existing.entry(key.clone()).or_default();
            let target = members.entry(key).or_default();
            for info in fallback_infos {
                if owners.insert(info.property_owner_id.clone()) {
                    target.push(info);
                }
            }
        }
    }
}

fn gmod_hook_owner_candidates(owner_name: &str) -> &'static [&'static str] {
    if owner_name.eq_ignore_ascii_case("GM") || owner_name.eq_ignore_ascii_case("GAMEMODE") {
        &["GM", "GAMEMODE", "SANDBOX"]
    } else if owner_name.eq_ignore_ascii_case("PLUGIN") {
        &["PLUGIN", "GM", "GAMEMODE", "SANDBOX"]
    } else if owner_name.eq_ignore_ascii_case("SANDBOX") {
        &["SANDBOX", "GM", "GAMEMODE"]
    } else {
        &[]
    }
}

pub fn add_completions_for_members(
    builder: &mut CompletionBuilder,
    members: &HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
    completion_status: CompletionTriggerStatus,
) -> Option<()> {
    // 排序
    let mut sorted_entries: Vec<_> = members.iter().collect();
    sorted_entries.sort_unstable_by_key(|(name, _)| *name);

    for (_, member_infos) in sorted_entries {
        add_resolve_member_infos(builder, member_infos, completion_status);
    }

    Some(())
}

fn add_resolve_member_infos(
    builder: &mut CompletionBuilder,
    member_infos: &Vec<LuaMemberInfo>,
    completion_status: CompletionTriggerStatus,
) -> Option<()> {
    if member_infos.len() == 1 {
        let member_info = &member_infos[0];
        if !is_member_realm_compatible(builder, member_info) {
            return Some(());
        }
        let overload_count = match &member_info.typ {
            LuaType::DocFunction(_) => None,
            LuaType::Signature(id) => {
                if let Some(signature) = builder
                    .semantic_model
                    .get_db()
                    .get_signature_index()
                    .get(id)
                {
                    let count = signature.overloads.len();
                    if count == 0 { None } else { Some(count) }
                } else {
                    None
                }
            }
            _ => None,
        };
        add_member_completion(
            builder,
            member_info.clone(),
            completion_status,
            overload_count,
        );
        return Some(());
    }

    let (filtered_member_infos, overload_count) =
        filter_member_infos(&builder.semantic_model, member_infos)?;

    let resolve_state = get_resolve_state(builder.semantic_model.get_db(), &filtered_member_infos);

    for member_info in filtered_member_infos {
        if !is_member_realm_compatible(builder, member_info) {
            continue;
        }

        match resolve_state {
            MemberResolveState::All => {
                add_member_completion(
                    builder,
                    member_info.clone(),
                    completion_status,
                    overload_count,
                );
            }
            MemberResolveState::Meta => {
                if let Some(feature) = member_info.feature
                    && feature.is_meta_decl()
                {
                    add_member_completion(
                        builder,
                        member_info.clone(),
                        completion_status,
                        overload_count,
                    );
                }
            }
            MemberResolveState::FileDecl => {
                if let Some(feature) = member_info.feature
                    && feature.is_file_decl()
                {
                    add_member_completion(
                        builder,
                        member_info.clone(),
                        completion_status,
                        overload_count,
                    );
                }
            }
        }
    }

    Some(())
}

/// 过滤成员信息，返回需要的成员列表和重载数量
fn filter_member_infos<'a>(
    semantic_model: &SemanticModel,
    member_infos: &'a Vec<LuaMemberInfo>,
) -> Option<(Vec<&'a LuaMemberInfo>, Option<usize>)> {
    if member_infos.is_empty() {
        return None;
    }

    let mut file_decl_member: Option<&LuaMemberInfo> = None;
    let mut gmod_meta_member: Option<&LuaMemberInfo> = None;
    let mut member_with_owners: Vec<(&LuaMemberInfo, Option<LuaTypeDeclId>)> =
        Vec::with_capacity(member_infos.len());
    let mut all_doc_function = true;
    let mut overload_count = 0;

    // 一次遍历收集所有信息
    for member_info in member_infos {
        let owner_id = get_owner_type_id(semantic_model.get_db(), member_info);
        member_with_owners.push((member_info, owner_id.clone()));

        // 寻找第一个 file_decl 作为参考，如果没有则使用第一个
        if file_decl_member.is_none()
            && let Some(feature) = member_info.feature
            && feature.is_file_decl()
        {
            file_decl_member = Some(member_info);
        }

        if gmod_meta_member.is_none()
            && let Some(feature) = member_info.feature
            && feature.is_meta_decl()
            && is_gmod_hook_member_info(semantic_model.get_db(), member_info)
        {
            gmod_meta_member = Some(member_info);
        }

        // 检查是否全为 DocFunction，同时计算重载数量
        match &member_info.typ {
            LuaType::DocFunction(_) => {
                overload_count += 1;
            }
            LuaType::Signature(id) => {
                all_doc_function = false;
                overload_count += 1;
                if let Some(signature) = semantic_model.get_db().get_signature_index().get(id) {
                    overload_count += signature.overloads.len();
                }
            }
            _ => {
                all_doc_function = false;
            }
        }
    }

    // 确定最终使用的参考 owner
    let final_reference_owner = if let Some(meta_member_info) = gmod_meta_member {
        get_owner_type_id(semantic_model.get_db(), meta_member_info)
    } else if let Some(file_decl_member_info) = file_decl_member {
        // 与第一个成员进行类型检查, 确保子类成员的类型与父类成员的类型一致
        if let Some((first_member, first_owner)) = member_with_owners.first() {
            let type_check_result =
                semantic_model.type_check(&file_decl_member_info.typ, &first_member.typ);
            if type_check_result.is_ok() {
                get_owner_type_id(semantic_model.get_db(), file_decl_member_info)
            } else {
                first_owner.clone()
            }
        } else {
            get_owner_type_id(semantic_model.get_db(), file_decl_member_info)
        }
    } else {
        // 没有找到 file_decl，使用第一个成员作为参考
        member_with_owners
            .first()
            .and_then(|(_, owner)| owner.clone())
    };

    // 过滤出相同 owner_type_id 的成员
    let mut filtered_member_infos: Vec<&LuaMemberInfo> = member_with_owners
        .into_iter()
        .filter_map(|(member_info, owner_id)| {
            if owner_id == final_reference_owner {
                Some(member_info)
            } else {
                None
            }
        })
        .collect();

    // 处理重载计数
    let final_overload_count = if overload_count >= 1 {
        let count = overload_count - 1;
        if count == 0 { None } else { Some(count) }
    } else {
        None
    };

    // 如果全为 DocFunction, 只保留第一个
    if all_doc_function && !filtered_member_infos.is_empty() {
        filtered_member_infos.truncate(1);
    }

    Some((filtered_member_infos, final_overload_count))
}

enum MemberResolveState {
    All,
    Meta,
    FileDecl,
}

fn get_owner_type_id(db: &DbIndex, info: &LuaMemberInfo) -> Option<LuaTypeDeclId> {
    match &info.property_owner_id {
        Some(LuaSemanticDeclId::Member(member_id)) => {
            if let Some(owner) = db.get_member_index().get_current_owner(member_id) {
                return owner.get_type_id().cloned();
            }
            None
        }
        _ => None,
    }
}

fn get_resolve_state(db: &DbIndex, member_infos: &Vec<&LuaMemberInfo>) -> MemberResolveState {
    let mut resolve_state = MemberResolveState::All;
    if db.get_emmyrc().strict.meta_override_file_define {
        for member_info in member_infos.iter() {
            if let Some(feature) = member_info.feature {
                if feature.is_meta_decl() {
                    resolve_state = MemberResolveState::Meta;
                    break;
                } else if feature.is_file_decl() {
                    resolve_state = MemberResolveState::FileDecl;
                }
            }
        }
    }
    resolve_state
}

fn is_gmod_hook_member_info(db: &DbIndex, info: &LuaMemberInfo) -> bool {
    let Some(owner_type_id) = get_owner_type_id(db, info) else {
        return false;
    };

    let owner_name = owner_type_id.get_simple_name();
    owner_name.eq_ignore_ascii_case("GM")
        || owner_name.eq_ignore_ascii_case("GAMEMODE")
        || owner_name.eq_ignore_ascii_case("SANDBOX")
        || owner_name.eq_ignore_ascii_case("PLUGIN")
}

fn is_member_realm_compatible(builder: &CompletionBuilder, info: &LuaMemberInfo) -> bool {
    if !builder.semantic_model.get_emmyrc().gmod.enabled {
        return true;
    }

    let infer_index = builder.semantic_model.get_db().get_gmod_infer_index();
    let call_realm = infer_index.get_realm_at_offset(
        &builder.semantic_model.get_file_id(),
        builder.position_offset,
    );

    if !matches!(call_realm, GmodRealm::Client | GmodRealm::Server) {
        return true;
    }

    let Some(property_owner_id) = &info.property_owner_id else {
        return true;
    };
    let Some((decl_file_id, decl_offset)) = semantic_decl_position(property_owner_id) else {
        return true;
    };

    let decl_realm = resolve_decl_realm(&builder.semantic_model, property_owner_id)
        .unwrap_or_else(|| infer_index.get_realm_at_offset(&decl_file_id, decl_offset));
    !matches!(
        (call_realm, decl_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

fn semantic_decl_position(property_owner_id: &LuaSemanticDeclId) -> Option<(FileId, TextSize)> {
    match property_owner_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => Some((decl_id.file_id, decl_id.position)),
        LuaSemanticDeclId::Member(member_id) => Some((member_id.file_id, member_id.get_position())),
        LuaSemanticDeclId::Signature(signature_id) => {
            Some((signature_id.get_file_id(), signature_id.get_position()))
        }
        LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn resolve_decl_realm(
    semantic_model: &SemanticModel,
    property_owner_id: &LuaSemanticDeclId,
) -> Option<GmodRealm> {
    let (decl_file_id, decl_offset) = semantic_decl_position(property_owner_id)?;
    if let Some(annotation_realm) =
        resolve_decl_annotation_realm_at_offset(semantic_model, &decl_file_id, decl_offset)
    {
        return Some(annotation_realm);
    }

    Some(
        semantic_model
            .get_db()
            .get_gmod_infer_index()
            .get_realm_at_offset(&decl_file_id, decl_offset),
    )
}

fn resolve_decl_annotation_realm_at_offset(
    semantic_model: &SemanticModel,
    file_id: &FileId,
    offset: TextSize,
) -> Option<GmodRealm> {
    let tree = semantic_model.get_db().get_vfs().get_syntax_tree(file_id)?;
    for func_stat in tree.get_chunk_node().descendants::<LuaFuncStat>() {
        if func_stat.get_range().contains(offset)
            && let Some(comment) = func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            return Some(realm);
        }
    }

    for local_func_stat in tree.get_chunk_node().descendants::<LuaLocalFuncStat>() {
        if local_func_stat.get_range().contains(offset)
            && let Some(comment) = local_func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            return Some(realm);
        }
    }

    None
}

fn realm_from_doc_comment(comment: &LuaComment) -> Option<GmodRealm> {
    for tag in comment.get_doc_tags() {
        if let LuaDocTag::Realm(realm_tag) = tag
            && let Some(realm) = realm_from_doc_tag(&realm_tag)
        {
            return Some(realm);
        }
    }

    None
}

fn realm_from_doc_tag(tag: &LuaDocTagRealm) -> Option<GmodRealm> {
    let name = tag.get_name_token()?;
    match name.get_name_text() {
        "client" => Some(GmodRealm::Client),
        "server" => Some(GmodRealm::Server),
        "shared" => Some(GmodRealm::Shared),
        _ => None,
    }
}
