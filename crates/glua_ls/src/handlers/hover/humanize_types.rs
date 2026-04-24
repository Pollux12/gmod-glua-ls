use std::collections::HashSet;

use glua_code_analysis::{
    DbIndex, FileId, GmodRealm, InFiled, LuaMember, LuaMultiLineUnion, LuaSemanticDeclId, LuaType,
    LuaTypeDeclId, LuaUnionType, RenderLevel, SemanticDeclLevel, SemanticModel, format_union_type,
};

use glua_code_analysis::humanize_type;
use glua_parser::{
    LuaAstNode, LuaComment, LuaCommentOwner, LuaDocTag, LuaDocTagRealm, LuaExpr, LuaFuncStat,
    LuaIndexExpr, LuaLocalFuncStat, LuaStat, LuaSyntaxId, LuaSyntaxKind, LuaTableExpr, LuaVarExpr,
};
use rowan::{TextRange, TextSize};

use super::hover_builder::HoverBuilder;

pub fn hover_const_type(db: &DbIndex, typ: &LuaType) -> String {
    let const_value = humanize_type(db, typ, RenderLevel::Detailed);

    match typ {
        LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => {
            format!("integer = {}", const_value)
        }
        LuaType::FloatConst(_) => format!("number = {}", const_value),
        LuaType::StringConst(_) | LuaType::DocStringConst(_) => format!("string = {}", const_value),
        _ => const_value,
    }
}

pub fn hover_humanize_type(
    builder: &mut HoverBuilder,
    ty: &LuaType,
    fallback_level: Option<RenderLevel>, // 当有值时, 若获取类型描述为空会回退到使用`humanize_type()`
) -> String {
    let db = builder.semantic_model.get_db();
    match ty {
        LuaType::Ref(type_decl_id) => {
            if let Some(type_decl) = db.get_type_index().get_type_decl(type_decl_id)
                && let Some(LuaType::MultiLineUnion(multi_union)) =
                    type_decl.get_alias_origin(db, None)
            {
                return hover_multi_line_union_type(
                    builder,
                    db,
                    multi_union.as_ref(),
                    Some(type_decl.get_full_name()),
                )
                .unwrap_or_default();
            }
            hover_ref_type_with_inheritance(
                db,
                ty,
                type_decl_id,
                fallback_level.unwrap_or(RenderLevel::Simple),
            )
        }
        LuaType::MultiLineUnion(multi_union) => {
            hover_multi_line_union_type(builder, db, multi_union.as_ref(), None).unwrap_or_default()
        }
        LuaType::Union(union) => hover_union_type(builder, union, RenderLevel::Detailed),
        _ => humanize_type(db, ty, fallback_level.unwrap_or(RenderLevel::Simple)),
    }
}

fn hover_ref_type_with_inheritance(
    db: &DbIndex,
    ty: &LuaType,
    type_decl_id: &LuaTypeDeclId,
    fallback_level: RenderLevel,
) -> String {
    let base_type = humanize_type(db, ty, fallback_level);
    let inheritance_suffix = build_inheritance_suffix(db, type_decl_id);
    if inheritance_suffix.is_empty() {
        base_type
    } else {
        format!("{base_type}{inheritance_suffix}")
    }
}

fn build_inheritance_suffix(db: &DbIndex, type_decl_id: &LuaTypeDeclId) -> String {
    let mut current_type_id = type_decl_id.clone();
    let mut visited = HashSet::from([current_type_id.clone()]);
    let mut chain_parts = Vec::new();

    while let Some(super_type) = first_super_type(db, &current_type_id) {
        let next_type_id = if let LuaType::Ref(next_type_id) = &super_type {
            Some(next_type_id.clone())
        } else {
            None
        };

        chain_parts.push(humanize_type(db, &super_type, RenderLevel::Simple));

        let Some(next_type_id) = next_type_id else {
            break;
        };
        if !visited.insert(next_type_id.clone()) {
            break;
        }
        current_type_id = next_type_id;
    }

    if chain_parts.is_empty() {
        return String::new();
    }

    format!(" : {}", chain_parts.join(" : "))
}

fn first_super_type(db: &DbIndex, type_decl_id: &LuaTypeDeclId) -> Option<LuaType> {
    db.get_type_index()
        .get_super_types_iter(type_decl_id)?
        .next()
        .cloned()
}

fn hover_union_type(
    builder: &mut HoverBuilder,
    union: &LuaUnionType,
    level: RenderLevel,
) -> String {
    format_union_type(union, level, |ty, level| {
        hover_humanize_type(builder, ty, Some(level))
    })
}

fn hover_multi_line_union_type(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    multi_union: &LuaMultiLineUnion,
    ty_name: Option<&str>,
) -> Option<String> {
    let members = multi_union.get_unions();
    let type_name = if ty_name.is_none() {
        let members = multi_union.get_unions();
        let type_str = members
            .iter()
            .take(10)
            .map(|(ty, _)| humanize_type(db, ty, RenderLevel::Simple))
            .collect::<Vec<_>>()
            .join("|");
        Some(format!("({})", type_str))
    } else {
        ty_name.map(|name| name.to_string())
    };
    let mut text = format!("{}:\n", type_name.clone().unwrap_or_default());
    for (typ, description) in members {
        let type_humanize_text = humanize_type(db, typ, RenderLevel::Minimal);
        if let Some(description) = description {
            text.push_str(&format!(
                "    | {} -- {}\n",
                type_humanize_text, description
            ));
        } else {
            text.push_str(&format!("    | {}\n", type_humanize_text));
        }
    }
    builder.add_type_expansion(text);
    type_name
}

/// 推断前缀是否为全局定义, 如果是, 则返回全局名称, 否则返回 None
pub fn infer_prefix_global_name<'a>(
    semantic_model: &'a SemanticModel,
    member: &LuaMember,
) -> Option<&'a str> {
    let root = semantic_model
        .get_db()
        .get_vfs()
        .get_syntax_tree(&member.get_file_id())?
        .get_red_root();
    let cur_node = member.get_syntax_id().to_node_from_root(&root)?;

    if Into::<LuaSyntaxKind>::into(cur_node.kind()) == LuaSyntaxKind::IndexExpr {
        let index_expr = LuaIndexExpr::cast(cur_node)?;
        let semantic_decl = semantic_model.find_decl(
            index_expr
                .get_prefix_expr()?
                .get_syntax_id()
                .to_node_from_root(&root)
                .unwrap()
                .into(),
            SemanticDeclLevel::default(),
        );
        if let Some(LuaSemanticDeclId::LuaDecl(id)) = semantic_decl
            && let Some(decl) = semantic_model.get_db().get_decl_index().get_decl(&id)
            && decl.is_global()
        {
            return Some(decl.get_name());
        }
    }
    None
}

/// 描述信息结构体
#[derive(Debug, Clone)]
pub struct DescriptionInfo {
    pub description: Option<String>,
    pub source: Option<String>,
    pub tag_content: Option<Vec<(String, String)>>,
    pub realm: Option<GmodRealm>,
    pub explicit_realm: bool,
}

impl DescriptionInfo {
    pub fn new() -> Self {
        Self {
            description: None,
            source: None,
            tag_content: None,
            realm: None,
            explicit_realm: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.source.is_none()
            && self.tag_content.is_none()
            && self.realm.is_none()
    }
}

/// 从属性所有者获取描述信息
pub fn extract_description_from_property_owner(
    semantic_model: &SemanticModel,
    property_owner: &LuaSemanticDeclId,
) -> Option<DescriptionInfo> {
    let property = semantic_model
        .get_db()
        .get_property_index()
        .get_property(property_owner)?;

    let mut result = DescriptionInfo::new();

    result.description = property.description().map(|detail| detail.to_string());
    result.source = property.source().map(|source| source.to_string());
    let (realm, explicit_realm) = infer_description_realm(semantic_model, property_owner);
    result.realm = realm;
    result.explicit_realm = explicit_realm;

    if let Some(tag_content) = property.tag_content() {
        for (tag_name, description) in tag_content.get_all_tags() {
            if result.tag_content.is_none() {
                result.tag_content = Some(Vec::new());
            }
            if let Some(tag_content) = &mut result.tag_content {
                tag_content.push((tag_name.clone(), description.clone()));
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn infer_description_realm(
    semantic_model: &SemanticModel,
    property_owner: &LuaSemanticDeclId,
) -> (Option<GmodRealm>, bool) {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return (None, false);
    }

    let db = semantic_model.get_db();
    let (file_id, offset) = match property_owner {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let Some(decl) = db.get_decl_index().get_decl(decl_id) else {
                return (None, false);
            };
            (decl.get_file_id(), decl.get_range().start())
        }
        LuaSemanticDeclId::Member(member_id) => {
            let Some(member) = db.get_member_index().get_member(member_id) else {
                return (None, false);
            };
            (member.get_file_id(), member.get_range().start())
        }
        _ => return (None, false),
    };

    if let Some(annotation_realm) =
        resolve_decl_annotation_realm_at_offset(semantic_model, &file_id, offset)
    {
        return (Some(annotation_realm), true);
    }

    if let Some(metadata) = db.get_gmod_infer_index().get_realm_file_metadata(&file_id)
        && let Some(annotation_realm) = metadata.annotation_realm
    {
        return (Some(annotation_realm), true);
    }

    match db
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, offset)
    {
        GmodRealm::Unknown => (None, false),
        realm => (Some(realm), false),
    }
}

pub(crate) fn infer_property_owner_realm(
    semantic_model: &SemanticModel,
    property_owner: &LuaSemanticDeclId,
) -> Option<GmodRealm> {
    let db = semantic_model.get_db();
    let (file_id, offset) = match property_owner {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(decl_id)?;
            (decl.get_file_id(), decl.get_range().start())
        }
        LuaSemanticDeclId::Member(member_id) => {
            let member = db.get_member_index().get_member(member_id)?;
            (member.get_file_id(), member.get_range().start())
        }
        _ => return None,
    };

    if let Some(annotation_realm) =
        resolve_decl_annotation_realm_at_offset(semantic_model, &file_id, offset)
    {
        return Some(annotation_realm);
    }

    if let Some(metadata) = db.get_gmod_infer_index().get_realm_file_metadata(&file_id)
        && let Some(annotation_realm) = metadata.annotation_realm
    {
        return Some(annotation_realm);
    }

    Some(
        db.get_gmod_infer_index()
            .get_realm_at_offset(&file_id, offset),
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

/// 从 element_id 中提取所有者名称
pub fn extract_owner_name_from_element(
    semantic_model: &SemanticModel,
    element_id: &InFiled<TextRange>,
) -> Option<String> {
    let root = semantic_model
        .get_db()
        .get_vfs()
        .get_syntax_tree(&element_id.file_id)?
        .get_red_root();

    // 通过 TextRange 找到对应的 AST 节点
    let node = LuaSyntaxId::to_node_at_range(&root, element_id.value)?;
    let stat = LuaStat::cast(node.clone().parent()?)?;
    match stat {
        LuaStat::LocalStat(local_stat) => {
            let value = LuaExpr::cast(node)?;
            let local_name = local_stat.get_local_name_by_value(value);
            if let Some(local_name) = local_name {
                return Some(local_name.get_name_token()?.get_name_text().to_string());
            }
        }
        LuaStat::AssignStat(assign_stat) => {
            let value = LuaExpr::cast(node)?;
            let (vars, values) = assign_stat.get_var_and_expr_list();
            let idx = values
                .iter()
                .position(|v| v.get_syntax_id() == value.get_syntax_id())?;
            let var = vars.get(idx)?;
            match var {
                LuaVarExpr::NameExpr(name_expr) => {
                    return Some(name_expr.get_name_token()?.get_name_text().to_string());
                }
                LuaVarExpr::IndexExpr(index_expr) => {
                    if let Some(index_key) = index_expr.get_index_key() {
                        return Some(index_key.get_path_part());
                    }
                }
            }
        }
        _ => {}
    }
    None
}

pub fn extract_parent_type_from_element(
    semantic_model: &SemanticModel,
    element_id: &InFiled<TextRange>,
) -> Option<LuaType> {
    let root = semantic_model
        .get_db()
        .get_vfs()
        .get_syntax_tree(&element_id.file_id)?
        .get_red_root();

    let node = LuaSyntaxId::to_node_at_range(&root, element_id.value)?;
    let stat = LuaStat::cast(node.clone().parent()?)?;
    if let LuaStat::LocalStat(_) = stat {
        let table_expr = LuaTableExpr::cast(node)?;
        let ty = semantic_model.infer_table_should_be(table_expr);
        return ty;
    }
    None
}
