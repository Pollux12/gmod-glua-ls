use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Arc,
    vec,
};

use crate::handlers::hover::{
    HoverBuilder,
    humanize_types::{
        DescriptionInfo, extract_description_from_property_owner, extract_owner_name_from_element,
        extract_parent_type_from_element, hover_humanize_type,
    },
    infer_prefix_global_name,
};
use glua_code_analysis::{
    AsyncState, DbIndex, InferGuard, LuaDocDefaultValue, LuaDocParamInfo, LuaDocReturnInfo,
    LuaFunctionType, LuaMember, LuaMemberOwner, LuaSemanticDeclId, LuaType, RenderLevel,
    ReturnTypeKind, SemanticDeclLevel, TypeSubstitutor, VariadicType, humanize_type,
    infer_call_expr_func, infer_self_type, instantiate_doc_function,
    try_extract_signature_id_from_field,
};

pub fn build_function_hover(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    semantic_decls: &[(LuaSemanticDeclId, LuaType)],
) -> Option<()> {
    let (function_name, is_local) = {
        let (semantic_decl, _) = semantic_decls.first()?;
        match semantic_decl {
            LuaSemanticDeclId::LuaDecl(id) => {
                let decl = db.get_decl_index().get_decl(id)?;
                (decl.get_name().to_string(), decl.is_local())
            }
            LuaSemanticDeclId::Member(id) => {
                let member = db.get_member_index().get_member(id)?;
                (member.get_key().to_path(), false)
            }
            _ => {
                return None;
            }
        }
    };

    // 如果是函数调用, 那么我们需要根据上下文实例化出实际类型
    if let Some(call_expr) = builder.get_call_expr() {
        build_function_call_hover(
            builder,
            db,
            semantic_decls,
            &call_expr,
            &function_name,
            is_local,
        );
    } else {
        build_function_define_hover(builder, db, semantic_decls, &function_name, is_local);
    }

    Some(())
}

fn build_function_call_hover(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    semantic_decls: &[(LuaSemanticDeclId, LuaType)],
    call_expr: &glua_parser::LuaCallExpr,
    function_name: &str,
    is_local: bool,
) -> Option<()> {
    let final_type = infer_call_expr_func(
        db,
        &mut builder.semantic_model.get_cache().borrow_mut(),
        call_expr.clone(),
        semantic_decls.last()?.1.clone(),
        &InferGuard::new(),
        None,
    )
    .ok()?;

    // 根据推断出来的类型确定哪个 semantic_decl 是匹配的
    let mut match_semantic_decl = &semantic_decls.last()?.0;
    for (semantic_decl, typ) in semantic_decls.iter() {
        if let LuaType::DocFunction(f) = typ {
            if f == &final_type {
                match_semantic_decl = semantic_decl;
                break;
            }
        }
    }

    let function_member = match match_semantic_decl {
        LuaSemanticDeclId::Member(id) => {
            let member = db.get_member_index().get_member(&id)?;
            Some(member)
        }
        _ => None,
    };

    let is_field = function_member_is_field(db, semantic_decls);
    let concrete_owner_type = infer_call_owner_type(builder, db, call_expr);
    let contents = if let Some((param_docs, return_docs)) =
        get_signature_hover_docs(db, match_semantic_decl, function_member)
    {
        vec![hover_doc_function_type(
            builder,
            db,
            &final_type,
            function_member,
            function_name,
            is_local,
            is_field,
            concrete_owner_type.as_ref(),
            Some(param_docs),
            merge_function_return_defaults(&final_type, return_docs),
        )]
    } else {
        process_function_type(
            builder,
            db,
            &LuaType::DocFunction(final_type),
            function_member,
            function_name,
            is_local,
            is_field,
            concrete_owner_type.as_ref(),
        )?
    };
    let description = get_function_description(builder, db, &match_semantic_decl);
    builder.set_type_description(contents.first()?.clone());
    builder.add_description_from_info_with_realm(description, true);

    Some(())
}

#[derive(Debug, Clone)]
struct HoverFunctionInfo {
    primary: String,
    overloads: Option<Vec<String>>,
    description: Option<DescriptionInfo>,
    is_trigger_owner: bool,
}

#[allow(unused)]
fn build_function_define_hover(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    semantic_decls: &[(LuaSemanticDeclId, LuaType)],
    function_name: &str,
    is_local: bool,
) -> Option<()> {
    let is_field = function_member_is_field(db, semantic_decls);
    let trigger_decl = builder.get_trigger_token().and_then(|token| {
        builder
            .semantic_model
            .find_decl(token.into(), SemanticDeclLevel::default())
    });
    let mut function_infos = Vec::new();
    for (semantic_decl_id, typ) in semantic_decls {
        let mut typ = typ.clone();
        let function_member = match semantic_decl_id {
            LuaSemanticDeclId::Member(id) => {
                let member = db.get_member_index().get_member(&id)?;
                Some(member)
            }
            _ => None,
        };

        if let Some(substitutor) = &builder.substitutor {
            if let Some(lua_func) = hover_instantiate_function_type(db, &typ, substitutor) {
                typ = LuaType::DocFunction(lua_func);
            }
        }

        let Some(contents) = process_function_type(
            builder,
            db,
            &typ,
            function_member,
            function_name,
            is_local,
            is_field,
            None,
        ) else {
            continue;
        };
        if contents.is_empty() {
            continue;
        }
        let description = get_function_description(builder, db, &semantic_decl_id);
        function_infos.push(HoverFunctionInfo {
            primary: contents.first()?.clone(),
            overloads: if contents.len() > 1 {
                Some(contents[1..].to_vec())
            } else {
                None
            },
            description,
            is_trigger_owner: trigger_decl.as_ref() == Some(semantic_decl_id),
        });
    }

    let caller_realm = builder
        .get_trigger_token()
        .map(|token| token.text_range().start())
        .map_or_else(
            || {
                db.get_gmod_infer_index()
                    .get_realm_file_metadata(&builder.semantic_model.get_file_id())
                    .map(|metadata| metadata.inferred_realm)
                    .unwrap_or(glua_code_analysis::GmodRealm::Unknown)
            },
            |trigger_position| {
                db.get_gmod_infer_index()
                    .get_realm_at_offset(&builder.semantic_model.get_file_id(), trigger_position)
            },
        );
    // 去重, 这是必须的.
    // Keep the last occurrence for each signature so the active symbol remains the primary entry.
    dedup_function_infos(&mut function_infos, caller_realm);
    // 需要显示重载的情况
    match function_infos.len() {
        0 => {
            return None;
        }
        1 => {
            builder.set_type_description(function_infos[0].primary.clone());
            builder
                .add_description_from_info_with_realm(function_infos[0].description.clone(), true);
        }
        _ => {
            let main_type = if let Some(trigger_idx) =
                function_infos.iter().position(|info| info.is_trigger_owner)
            {
                function_infos.remove(trigger_idx)
            } else {
                function_infos.pop()?
            };
            builder.set_type_description(main_type.primary.clone());
            builder.add_description_from_info_with_realm(main_type.description.clone(), true);

            let mut seen_signatures = HashSet::new();
            seen_signatures.insert(main_type.primary.clone());
            if let Some(overloads) = &main_type.overloads {
                for overload in overloads {
                    if seen_signatures.insert(overload.clone()) {
                        builder.add_signature_overload(overload.clone());
                    }
                }
            }

            function_infos.sort_by_key(|info| {
                Reverse(info.overloads.as_ref().is_some_and(|v| !v.is_empty()))
            });
            for type_desc in &function_infos {
                if seen_signatures.insert(type_desc.primary.clone()) {
                    builder.add_signature_overload(type_desc.primary.clone());
                }
                if let Some(overloads) = &type_desc.overloads {
                    for overload in overloads {
                        if seen_signatures.insert(overload.clone()) {
                            builder.add_signature_overload(overload.clone());
                        }
                    }
                }
            }

            for type_desc in function_infos {
                builder.add_description_from_info_with_realm(type_desc.description.clone(), true);
            }
        }
    }
    Some(())
}

fn merge_preferred_description(
    existing: &mut HoverFunctionInfo,
    incoming: &HoverFunctionInfo,
    caller_realm: glua_code_analysis::GmodRealm,
) {
    existing.is_trigger_owner |= incoming.is_trigger_owner;
    match (&mut existing.description, &incoming.description) {
        (None, Some(incoming_description)) => {
            existing.description = Some(incoming_description.clone());
        }
        (Some(existing_description), Some(incoming_description)) => {
            if existing_description.realm.is_none() && incoming_description.realm.is_some() {
                existing_description.realm = incoming_description.realm;
                existing_description.explicit_realm = incoming_description.explicit_realm;
            } else if existing_description.realm.is_some()
                && incoming_description.realm.is_some()
                && incoming_description.explicit_realm
                && !existing_description.explicit_realm
            {
                existing_description.realm = incoming_description.realm;
                existing_description.explicit_realm = true;
            }

            if existing_description.description.is_none()
                && incoming_description.description.is_some()
            {
                existing_description.description = incoming_description.description.clone();
            }

            if existing_description.source.is_none() && incoming_description.source.is_some() {
                existing_description.source = incoming_description.source.clone();
            }

            if existing_description.tag_content.is_none()
                && incoming_description.tag_content.is_some()
            {
                existing_description.tag_content = incoming_description.tag_content.clone();
            }

            if existing_description.description.is_none()
                && existing_description.source.is_none()
                && incoming_description.description.is_none()
                && incoming_description.source.is_none()
                && existing_description.tag_content.is_none()
                && incoming_description.tag_content.is_none()
                && !existing_description.explicit_realm
                && !incoming_description.explicit_realm
            {
                existing_description.realm = merge_docless_realms(
                    caller_realm,
                    existing_description.realm,
                    existing_description.explicit_realm,
                    incoming_description.realm,
                    incoming_description.explicit_realm,
                );
            }
        }
        _ => {}
    }
}

fn dedup_function_infos(
    function_infos: &mut Vec<HoverFunctionInfo>,
    caller_realm: glua_code_analysis::GmodRealm,
) {
    let mut deduped_reversed: Vec<HoverFunctionInfo> = Vec::with_capacity(function_infos.len());
    let mut index_by_primary: HashMap<String, usize> = HashMap::with_capacity(function_infos.len());
    for function_info in function_infos.drain(..).rev() {
        if let Some(existing_index) = index_by_primary.get(&function_info.primary).copied() {
            let existing = &mut deduped_reversed[existing_index];
            merge_preferred_description(existing, &function_info, caller_realm);
            continue;
        }
        index_by_primary.insert(function_info.primary.clone(), deduped_reversed.len());
        deduped_reversed.push(function_info);
    }
    deduped_reversed.reverse();
    *function_infos = deduped_reversed;
}

fn merge_docless_realms(
    caller_realm: glua_code_analysis::GmodRealm,
    existing_realm: Option<glua_code_analysis::GmodRealm>,
    existing_explicit_realm: bool,
    incoming_realm: Option<glua_code_analysis::GmodRealm>,
    incoming_explicit_realm: bool,
) -> Option<glua_code_analysis::GmodRealm> {
    let existing_realm = existing_realm.or(incoming_realm)?;
    let incoming_realm = incoming_realm.unwrap_or(existing_realm);

    if !super::is_realm_compatible(caller_realm, incoming_realm) {
        return Some(existing_realm);
    }
    if !super::is_realm_compatible(caller_realm, existing_realm) {
        return Some(incoming_realm);
    }

    let merged = match caller_realm {
        glua_code_analysis::GmodRealm::Server
        | glua_code_analysis::GmodRealm::Client
        | glua_code_analysis::GmodRealm::Menu => {
            if incoming_realm == glua_code_analysis::GmodRealm::Shared {
                glua_code_analysis::GmodRealm::Shared
            } else {
                existing_realm
            }
        }
        glua_code_analysis::GmodRealm::Shared | glua_code_analysis::GmodRealm::Unknown => {
            if existing_realm == glua_code_analysis::GmodRealm::Shared
                && incoming_realm == glua_code_analysis::GmodRealm::Client
                && incoming_explicit_realm
            {
                glua_code_analysis::GmodRealm::Client
            } else if incoming_realm == glua_code_analysis::GmodRealm::Shared
                || existing_realm == glua_code_analysis::GmodRealm::Shared
            {
                glua_code_analysis::GmodRealm::Shared
            } else if matches!(
                (existing_realm, incoming_realm),
                (
                    glua_code_analysis::GmodRealm::Server,
                    glua_code_analysis::GmodRealm::Client
                ) | (
                    glua_code_analysis::GmodRealm::Client,
                    glua_code_analysis::GmodRealm::Server
                )
            ) {
                glua_code_analysis::GmodRealm::Unknown
            } else if existing_realm == glua_code_analysis::GmodRealm::Client
                && existing_explicit_realm
            {
                glua_code_analysis::GmodRealm::Client
            } else if existing_realm == glua_code_analysis::GmodRealm::Unknown {
                incoming_realm
            } else {
                existing_realm
            }
        }
    };

    Some(merged)
}

fn process_function_type(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    typ: &LuaType,
    function_member: Option<&LuaMember>,
    function_name: &str,
    is_local: bool,
    is_field: bool,
    concrete_owner_type: Option<&LuaType>,
) -> Option<Vec<String>> {
    match typ {
        LuaType::DocFunction(lua_func) => {
            let content = hover_doc_function_type(
                builder,
                db,
                lua_func,
                function_member,
                &function_name,
                is_local,
                is_field,
                concrete_owner_type,
                None,
                convert_function_return_to_docs(lua_func),
            );
            Some(vec![content])
        }
        LuaType::Signature(signature_id) => {
            let signature = db.get_signature_index().get(&signature_id)?;
            let mut new_overloads = signature.overloads.clone();
            let fake_doc_function = Arc::new(
                LuaFunctionType::new(
                    signature.async_state,
                    signature.is_colon_define,
                    signature.is_vararg,
                    signature.get_type_params(),
                    signature.get_return_type(),
                )
                .with_optional_params(signature.get_param_optional_flags()),
            );
            new_overloads.insert(0, fake_doc_function);
            let mut contents = Vec::with_capacity(new_overloads.len());
            for (i, overload) in new_overloads.iter().enumerate() {
                contents.push(hover_doc_function_type(
                    builder,
                    db,
                    overload,
                    function_member,
                    function_name,
                    is_local,
                    is_field,
                    concrete_owner_type,
                    if i == 0 {
                        Some(&signature.param_docs)
                    } else {
                        None
                    },
                    if i == 0 {
                        merge_function_return_docs(overload, &signature.return_docs)
                    } else {
                        convert_function_return_to_docs(overload)
                    },
                ));
            }
            Some(contents)
        }
        LuaType::Union(union) => {
            let mut contents = Vec::new();
            for typ in union.types() {
                if let Some(content) = process_function_type(
                    builder,
                    db,
                    typ,
                    function_member,
                    function_name,
                    is_local,
                    is_field,
                    concrete_owner_type,
                ) {
                    contents.extend(content);
                }
            }
            Some(contents)
        }
        _ => None,
    }
}

fn hover_doc_function_type(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    func: &LuaFunctionType,
    owner_member: Option<&LuaMember>,
    func_name: &str,
    is_local: bool,
    is_field: bool, /* 是否为类字段 */
    concrete_owner_type: Option<&LuaType>,
    param_docs: Option<&HashMap<usize, LuaDocParamInfo>>,
    return_docs: Vec<LuaDocReturnInfo>, /* 返回值以此为准 */
) -> String {
    let async_label = match func.get_async_state() {
        AsyncState::Async => "async ",
        AsyncState::Sync => "sync ",
        _ => "",
    };
    let mut is_method = func.is_colon_define();
    let mut type_label = if is_local && owner_member.is_none() {
        "local function "
    } else {
        "function "
    };

    // 有可能来源于类. 例如: `local add = class.add`, `add()`应被视为类方法
    let full_name = if let Some(owner_member) = owner_member {
        if is_field {
            type_label = "(field) ";
        }

        let member_key = owner_member.get_key().to_path();
        let mut name = String::with_capacity(member_key.len() + 16);

        let mut push_typed_owner_prefix = |prefix: &str, owner_ty: LuaType| {
            name.push_str(prefix);
            is_method = func.is_method(builder.semantic_model, Some(&owner_ty));
            if is_method {
                type_label = "(method) ";
            }
            name.push(if is_method { ':' } else { '.' });
        };

        let parent_owner = db
            .get_member_index()
            .get_current_owner(&owner_member.get_id());
        if let Some(parent_owner) = parent_owner {
            match parent_owner {
                LuaMemberOwner::Type(type_decl_id) => {
                    if let Some(owner_ty) = concrete_owner_type.cloned() {
                        if let Some(prefix) = owner_type_display_name(&owner_ty) {
                            push_typed_owner_prefix(&prefix, owner_ty);
                        } else {
                            let prefix =
                                infer_prefix_global_name(builder.semantic_model, owner_member)
                                    .unwrap_or_else(|| type_decl_id.get_simple_name());
                            push_typed_owner_prefix(&prefix, LuaType::Ref(type_decl_id.clone()));
                        }
                    } else {
                        let prefix = infer_prefix_global_name(builder.semantic_model, owner_member)
                            .unwrap_or_else(|| type_decl_id.get_simple_name());
                        push_typed_owner_prefix(&prefix, LuaType::Ref(type_decl_id.clone()));
                    }
                }
                LuaMemberOwner::Element(element_id) => {
                    if let Some(LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id)) =
                        extract_parent_type_from_element(builder.semantic_model, element_id)
                    {
                        push_typed_owner_prefix(
                            type_decl_id.get_simple_name(),
                            LuaType::Ref(type_decl_id.clone()),
                        );
                    } else if let Some(owner_name) =
                        extract_owner_name_from_element(builder.semantic_model, element_id)
                    {
                        name.push_str(&owner_name);
                        if is_method {
                            type_label = "(method) ";
                        }
                        name.push(if is_method { ':' } else { '.' });
                    }
                }
                _ => {}
            }
        }

        name.push_str(&member_key);
        name
    } else {
        func_name.to_string()
    };

    let params = func
        .get_params()
        .iter()
        .enumerate()
        .map(|(index, param)| build_function_param(db, func, param_docs, index, param, is_method))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");

    let ret_detail = build_function_returns(builder, return_docs);
    format_function_type(type_label, async_label, full_name, params, ret_detail)
}

fn infer_call_owner_type(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    call_expr: &glua_parser::LuaCallExpr,
) -> Option<LuaType> {
    if !matches!(
        call_expr.get_prefix_expr()?,
        glua_parser::LuaExpr::NameExpr(_)
    ) {
        return None;
    }

    let mut cache = builder.semantic_model.get_cache().borrow_mut();
    infer_self_type(db, &mut cache, call_expr)
}

fn owner_type_display_name(owner_type: &LuaType) -> Option<String> {
    match owner_type {
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            Some(type_decl_id.get_simple_name().to_string())
        }
        LuaType::Generic(generic) => Some(generic.get_base_type_id().get_simple_name().to_string()),
        _ => None,
    }
}

fn convert_function_return_to_docs(func: &LuaFunctionType) -> Vec<LuaDocReturnInfo> {
    match func.get_ret() {
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Base(base) => vec![LuaDocReturnInfo {
                name: None,
                type_ref: base.clone(),
                description: None,
                default_value: None,
                attributes: None,
                return_kind: ReturnTypeKind::default(),
            }],
            VariadicType::Multi(types) => types
                .iter()
                .map(|ty| LuaDocReturnInfo {
                    name: None,
                    type_ref: ty.clone(),
                    description: None,
                    default_value: None,
                    attributes: None,
                    return_kind: ReturnTypeKind::default(),
                })
                .collect(),
        },
        _ => vec![LuaDocReturnInfo {
            name: None,
            type_ref: func.get_ret().clone(),
            description: None,
            default_value: None,
            attributes: None,
            return_kind: ReturnTypeKind::default(),
        }],
    }
}

fn merge_function_return_docs(
    func: &LuaFunctionType,
    return_docs: &[LuaDocReturnInfo],
) -> Vec<LuaDocReturnInfo> {
    if return_docs.is_empty() {
        return convert_function_return_to_docs(func);
    }

    let mut merged = convert_function_return_to_docs(func);
    if merged.is_empty() {
        return return_docs.to_vec();
    }

    for (idx, return_doc) in return_docs.iter().enumerate() {
        if let Some(merged_return) = merged.get_mut(idx) {
            merged_return.name = return_doc.name.clone();
            merged_return.default_value = return_doc.default_value.clone();
            merged_return.description = return_doc.description.clone();
            merged_return.attributes = return_doc.attributes.clone();
            merged_return.return_kind = return_doc.return_kind;
        } else {
            merged.push(return_doc.clone());
        }
    }

    merged
}

fn merge_function_return_defaults(
    func: &LuaFunctionType,
    return_docs: &[LuaDocReturnInfo],
) -> Vec<LuaDocReturnInfo> {
    let mut merged = convert_function_return_to_docs(func);
    for (idx, return_doc) in return_docs.iter().enumerate() {
        if let Some(merged_return) = merged.get_mut(idx) {
            merged_return.name = return_doc.name.clone();
            merged_return.default_value = return_doc.default_value.clone();
            merged_return.return_kind = return_doc.return_kind;
        }
    }

    merged
}

fn get_signature_hover_docs<'a>(
    db: &'a DbIndex,
    semantic_decl: &LuaSemanticDeclId,
    function_member: Option<&LuaMember>,
) -> Option<(&'a HashMap<usize, LuaDocParamInfo>, &'a [LuaDocReturnInfo])> {
    let signature_id = match semantic_decl {
        LuaSemanticDeclId::Signature(signature_id) => Some(*signature_id),
        LuaSemanticDeclId::Member(_) => db
            .get_property_index()
            .get_signature_owner(semantic_decl)
            .or_else(|| {
                function_member.and_then(|member| try_extract_signature_id_from_field(db, member))
            }),
        _ => db.get_property_index().get_signature_owner(semantic_decl),
    }?;
    let signature = db.get_signature_index().get(&signature_id)?;
    Some((&signature.param_docs, signature.return_docs.as_slice()))
}

fn build_function_param(
    db: &DbIndex,
    func: &LuaFunctionType,
    param_docs: Option<&HashMap<usize, LuaDocParamInfo>>,
    index: usize,
    param: &(String, Option<LuaType>),
    is_method: bool,
) -> String {
    if index == 0 && is_method && !func.is_colon_define() {
        return "".to_string();
    }

    let param_doc = param_docs.and_then(|docs| docs.get(&index));
    let name = param_doc
        .map(|doc| doc.name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&param.0);
    let type_ref = param
        .1
        .as_ref()
        .or_else(|| param_doc.map(|doc| &doc.type_ref));

    let mut rendered = if let Some(ty) = type_ref {
        format!("{}: {}", name, humanize_type(db, ty, RenderLevel::Simple))
    } else {
        name.to_string()
    };

    if let Some(default_value) = param_doc.and_then(|doc| doc.default_value.as_ref()) {
        rendered.push('=');
        rendered.push_str(&format_doc_default_value(default_value));
    }

    rendered
}

pub(crate) fn format_doc_default_value(default_value: &LuaDocDefaultValue) -> String {
    match default_value {
        LuaDocDefaultValue::Nil => "nil".to_string(),
        LuaDocDefaultValue::Boolean(value) => value.to_string(),
        LuaDocDefaultValue::Number(value) => value.clone(),
        LuaDocDefaultValue::String(value) => format!("{value:?}"),
    }
}

fn format_function_type(
    type_label: &str,
    async_label: &str,
    full_name: String,
    params: String,
    rets: String,
) -> String {
    let prefix = if type_label.starts_with("function") {
        format!("{}{}", async_label, type_label)
    } else {
        format!("{}{}", type_label, async_label)
    };
    format!("{}{}({}){}", prefix, full_name, params, rets)
}

fn get_function_description(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    semantic_decl_id: &LuaSemanticDeclId,
) -> Option<DescriptionInfo> {
    let mut description =
        extract_description_from_property_owner(builder.semantic_model, semantic_decl_id);
    match semantic_decl_id {
        LuaSemanticDeclId::Member(id) => {
            let member = db.get_member_index().get_member(id)?;
            // 以 @field 定义的 function 描述信息绑定的 id 并不是 member, 需要特殊处理
            if description.is_none()
                && let Some(signature_id) = try_extract_signature_id_from_field(db, member)
            {
                description = extract_description_from_property_owner(
                    builder.semantic_model,
                    &LuaSemanticDeclId::Signature(signature_id),
                );
            }
            Some(member)
        }
        _ => None,
    };
    description
}

fn build_function_returns(
    builder: &mut HoverBuilder,
    return_docs: Vec<LuaDocReturnInfo>,
) -> String {
    let mut result = String::new();
    // 如果不是补全且存在名称, 我们需要多行显示
    let has_multiline = !builder.is_completion
        && return_docs
            .iter()
            .any(|return_info| return_info.name.is_some());

    for (i, return_info) in return_docs.iter().enumerate() {
        if i == 0 && return_info.type_ref.is_nil() {
            continue;
        }
        let type_text = build_function_return_type(builder, return_info, i);

        if has_multiline {
            // 存在返回值名称时使用多行模式
            let prefix = if i == 0 {
                result.push('\n');
                "-> ".to_string()
            } else {
                format!("{}. ", i + 1)
            };
            let name = return_info.name.clone().unwrap_or_default();

            result.push_str(&format!(
                "  {}{}{}\n",
                prefix,
                if !name.is_empty() {
                    format!("{}: ", name)
                } else {
                    "".to_string()
                },
                type_text,
            ));
        } else {
            // 不存在返回值名称时使用单行模式
            if i == 0 {
                result.push_str(&format!(" -> {}", type_text));
            } else {
                result.push_str(&format!(", {}", type_text));
            }
        }
    }

    result
}

fn build_function_return_type(
    builder: &mut HoverBuilder,
    ret_info: &LuaDocReturnInfo,
    i: usize,
) -> String {
    let type_expansion_count = builder.get_type_expansion_count();
    // 在这个过程中可能会设置`type_expansion`
    let type_text = hover_humanize_type(builder, &ret_info.type_ref, Some(RenderLevel::Simple));
    if builder.get_type_expansion_count() > type_expansion_count {
        // 重新设置`type_expansion`
        if let Some(pop_type_expansion) =
            builder.pop_type_expansion(type_expansion_count, builder.get_type_expansion_count())
        {
            let mut new_type_expansion = format!("return #{}", i + 1);
            let mut seen = HashSet::new();
            for type_expansion in pop_type_expansion {
                for line in type_expansion.lines().skip(1) {
                    if seen.insert(line.to_string()) {
                        new_type_expansion.push('\n');
                        new_type_expansion.push_str(line);
                    }
                }
            }
            builder.add_type_expansion(new_type_expansion);
        }
    };
    if let Some(default_value) = &ret_info.default_value {
        format!("{}={}", type_text, format_doc_default_value(default_value))
    } else {
        type_text
    }
}

// 函数是否为类字段, 任意一个为类字段我们都认为全部为类字段
fn function_member_is_field(db: &DbIndex, semantic_decls: &[(LuaSemanticDeclId, LuaType)]) -> bool {
    semantic_decls.iter().any(|(semantic_decl, _)| {
        if let LuaSemanticDeclId::Member(id) = semantic_decl {
            let member = db.get_member_index().get_member(id);
            member.is_some() && member.unwrap().is_field()
        } else {
            false
        }
    })
}

fn hover_instantiate_function_type(
    db: &DbIndex,
    typ: &LuaType,
    substitutor: &TypeSubstitutor,
) -> Option<Arc<LuaFunctionType>> {
    if !typ.contain_tpl() {
        return None;
    }
    match typ {
        LuaType::DocFunction(f) => {
            if let LuaType::DocFunction(f) = instantiate_doc_function(db, f, substitutor) {
                Some(f)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn is_function(typ: &LuaType) -> bool {
    typ.is_function()
        || match &typ {
            LuaType::Union(union) => union
                .types()
                .all(|t| matches!(t, LuaType::DocFunction(_) | LuaType::Signature(_))),
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use glua_code_analysis::GmodRealm;

    use crate::handlers::hover::humanize_types::DescriptionInfo;

    use super::{HoverFunctionInfo, merge_preferred_description};

    #[test]
    fn merge_preferred_description_explicit_realm_overrides_implicit_realm() {
        let mut existing = HoverFunctionInfo {
            primary: "(function) ents.Create(class: string)".to_string(),
            overloads: None,
            description: Some(DescriptionInfo {
                description: Some("override docs".to_string()),
                source: None,
                tag_content: None,
                realm: Some(GmodRealm::Shared),
                explicit_realm: false,
            }),
            is_trigger_owner: true,
        };
        let incoming = HoverFunctionInfo {
            primary: "(function) ents.Create(class: string)".to_string(),
            overloads: None,
            description: Some(DescriptionInfo {
                description: Some("annotated docs".to_string()),
                source: None,
                tag_content: None,
                realm: Some(GmodRealm::Server),
                explicit_realm: true,
            }),
            is_trigger_owner: false,
        };

        merge_preferred_description(&mut existing, &incoming, GmodRealm::Shared);

        let merged = existing
            .description
            .expect("description should remain present");
        assert_eq!(merged.realm, Some(GmodRealm::Server));
        assert!(merged.explicit_realm);
    }

    #[test]
    fn merge_preferred_description_preserves_existing_explicit_docless_realm() {
        let mut existing = HoverFunctionInfo {
            primary: "(method) GM:Spawn()".to_string(),
            overloads: None,
            description: Some(DescriptionInfo {
                description: None,
                source: None,
                tag_content: None,
                realm: Some(GmodRealm::Server),
                explicit_realm: true,
            }),
            is_trigger_owner: true,
        };
        let incoming = HoverFunctionInfo {
            primary: "(method) GM:Spawn()".to_string(),
            overloads: None,
            description: Some(DescriptionInfo {
                description: None,
                source: None,
                tag_content: None,
                realm: Some(GmodRealm::Shared),
                explicit_realm: false,
            }),
            is_trigger_owner: false,
        };

        merge_preferred_description(&mut existing, &incoming, GmodRealm::Shared);

        let merged = existing
            .description
            .expect("description should remain present");
        assert_eq!(merged.realm, Some(GmodRealm::Server));
        assert!(merged.explicit_realm);
    }
}
