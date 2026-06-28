use glua_code_analysis::{
    LuaDeclId, LuaType, LuaUnionType, RenderLevel, SemanticModel, format_union_type, humanize_type,
    infer_param_with_cache, resolve_alias_type,
};
use glua_parser::{LuaAstNode, LuaAstToken, LuaClosureExpr, LuaParamName};
use itertools::Itertools;
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, InlayHintLabelPart, Location};

pub fn build_closure_hint(
    semantic_model: &SemanticModel,
    result: &mut Vec<InlayHint>,
    closure: LuaClosureExpr,
) -> Option<()> {
    if !semantic_model.get_emmyrc().hint.param_hint {
        return Some(());
    }
    let lua_params = closure.get_params_list()?;
    let document = semantic_model.get_document();
    for lua_param in lua_params.get_params() {
        let typ = infer_lua_param_hint_type(semantic_model, &lua_param)?;
        if typ.is_any() || typ.is_unknown() {
            continue;
        }

        let lsp_range = document.to_lsp_range(lua_param.get_range())?;
        let mut label_parts = build_label_parts(semantic_model, &typ);
        if label_parts.is_empty() {
            let typ_desc = format!(
                ": {}",
                hint_humanize_type(semantic_model, &typ, RenderLevel::Simple)
            );
            label_parts.push(InlayHintLabelPart {
                value: typ_desc,
                location: Some(
                    get_type_location(semantic_model, &typ, 0)
                        .unwrap_or(Location::new(document.get_uri(), lsp_range)),
                ),
                ..Default::default()
            });
        }
        let hint = InlayHint {
            kind: Some(InlayHintKind::TYPE),
            label: InlayHintLabel::LabelParts(label_parts),
            position: lsp_range.end,
            text_edits: None,
            tooltip: None,
            padding_left: Some(true),
            padding_right: None,
            data: None,
        };
        result.push(hint);
    }

    Some(())
}

fn infer_lua_param_hint_type(
    semantic_model: &SemanticModel,
    lua_param: &LuaParamName,
) -> Option<LuaType> {
    let token = lua_param
        .get_name_token()
        .map(|token| token.syntax().clone())
        .or_else(|| lua_param.syntax().first_token())?;
    let decl_id = LuaDeclId::new(semantic_model.get_file_id(), token.text_range().start());
    let decl = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;
    infer_param_with_cache(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        decl,
    )
    .ok()
}

pub fn build_label_parts(semantic_model: &SemanticModel, typ: &LuaType) -> Vec<InlayHintLabelPart> {
    let mut parts: Vec<InlayHintLabelPart> = Vec::new();
    match typ {
        LuaType::Union(union) => {
            for typ in union.types() {
                if let Some(part) = get_part(semantic_model, typ) {
                    parts.push(part);
                }
            }
        }
        _ => {
            if let Some(part) = get_part(semantic_model, typ) {
                parts.push(part);
            }
        }
    }
    // 去重
    let parts: Vec<InlayHintLabelPart> = parts
        .into_iter()
        .unique_by(|part| part.value.clone())
        .collect();
    // 将 "?" 标签移到最后
    let mut normal_parts = Vec::new();
    let mut nil_parts = Vec::new();
    for part in parts {
        if part.value == "?" {
            nil_parts.push(part);
        } else {
            normal_parts.push(part);
        }
    }
    normal_parts.append(&mut nil_parts);
    let mut result = Vec::new();
    for (i, part) in normal_parts.into_iter().enumerate() {
        let mut part = part;
        if part.value != "?" {
            part.value = format!("{}{}", if i == 0 { ": " } else { "|" }, part.value);
        }
        result.push(part);
    }
    // 如果只有一个`nil`标签, 那么将其改为": nil"
    if result.len() == 1 && result[0].value == "?" {
        result[0].value = ": nil".to_string();
    }
    result
}

fn get_part(semantic_model: &SemanticModel, typ: &LuaType) -> Option<InlayHintLabelPart> {
    match typ {
        LuaType::Union(_) => None,
        LuaType::Nil => Some(InlayHintLabelPart {
            value: "?".to_string(),
            location: get_type_location(semantic_model, typ, 0),
            ..Default::default()
        }),
        _ => {
            let value = hint_humanize_type(semantic_model, typ, RenderLevel::Simple);
            let location = get_type_location(semantic_model, typ, 0);
            Some(InlayHintLabelPart {
                value,
                location,
                ..Default::default()
            })
        }
    }
}

fn get_type_location(
    semantic_model: &SemanticModel,
    typ: &LuaType,
    depth: usize,
) -> Option<Location> {
    if depth > 10 {
        return None;
    }
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            let type_decl = semantic_model.get_db().get_type_index().get_type_decl(id)?;
            let location = type_decl.get_locations().first()?;
            let document = semantic_model.get_document_by_file_id(location.file_id)?;
            let lsp_range = document.to_lsp_range(location.range)?;
            Some(Location::new(document.get_uri(), lsp_range))
        }
        LuaType::Generic(generic) => {
            let base_type_id = generic.get_base_type_id();
            get_type_location(semantic_model, &LuaType::Ref(base_type_id), depth + 1)
        }
        LuaType::Array(array_type) => {
            get_type_location(semantic_model, array_type.get_base(), depth + 1)
        }
        LuaType::Any => get_base_type_location(semantic_model, "any"),
        LuaType::Nil => get_base_type_location(semantic_model, "nil"),
        LuaType::Unknown => get_base_type_location(semantic_model, "unknown"),
        LuaType::Userdata => get_base_type_location(semantic_model, "userdata"),
        LuaType::Function => get_base_type_location(semantic_model, "function"),
        LuaType::Thread => get_base_type_location(semantic_model, "thread"),
        LuaType::Table => get_base_type_location(semantic_model, "table"),
        _ if typ.is_string() => get_base_type_location(semantic_model, "string"),
        _ if typ.is_integer() => get_base_type_location(semantic_model, "integer"),
        _ if typ.is_number() => get_base_type_location(semantic_model, "number"),
        _ if typ.is_boolean() => get_base_type_location(semantic_model, "boolean"),
        _ => None,
    }
}

fn get_base_type_location(semantic_model: &SemanticModel, name: &str) -> Option<Location> {
    let type_decl = semantic_model
        .get_db()
        .get_type_index()
        .find_type_decl(semantic_model.get_file_id(), name)?;
    let location = type_decl.get_locations().first()?;
    let document = semantic_model.get_document_by_file_id(location.file_id)?;
    let lsp_range = document.to_lsp_range(location.range)?;
    Some(Location::new(document.get_uri(), lsp_range))
}

fn hint_humanize_type(semantic_model: &SemanticModel, typ: &LuaType, level: RenderLevel) -> String {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            let resolved = resolve_alias_type(semantic_model.get_db(), typ);
            if let Some(alias_id) = resolved.alias_id
                && resolved.typ != *typ
            {
                return format!(
                    "{} = {}",
                    alias_id.get_simple_name(),
                    hint_humanize_type(semantic_model, &resolved.typ, level)
                );
            }

            id.get_simple_name().to_string()
        }
        LuaType::Generic(generic) => {
            let base_type_id = generic.get_base_type_id();
            let base_type_name =
                hint_humanize_type(semantic_model, &LuaType::Ref(base_type_id), level);

            let generic_params = generic
                .get_params()
                .iter()
                .map(|ty| hint_humanize_type(semantic_model, ty, level.next_level()))
                .collect::<Vec<_>>()
                .join(",");

            format!("{}<{}>", base_type_name, generic_params)
        }
        LuaType::Union(union) => hint_humanize_union_type(semantic_model, union, level),
        _ => humanize_type(semantic_model.get_db(), typ, level),
    }
}

fn hint_humanize_union_type(
    semantic_model: &SemanticModel,
    union: &LuaUnionType,
    level: RenderLevel,
) -> String {
    format_union_type(union, level, |ty, _| {
        hint_humanize_type(semantic_model, ty, level)
    })
}
