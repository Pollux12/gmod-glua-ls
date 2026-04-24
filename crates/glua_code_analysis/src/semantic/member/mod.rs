mod find_index;
mod find_members;
mod get_member_map;
mod infer_raw_member;

use std::collections::HashSet;

use crate::{
    DbIndex, LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaSemanticDeclId, TypeOps,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::type_check::check_type_compact,
};
pub use find_index::find_index_operations;
pub use find_members::{
    find_members, find_members_in_workspace_for_file, find_members_in_workspace_for_file_at_offset,
    find_members_with_key, find_members_with_key_in_workspace_for_file,
    find_members_with_key_in_workspace_for_file_at_offset,
};
pub use get_member_map::{
    get_member_map, get_member_map_in_workspace_for_file,
    get_member_map_in_workspace_for_file_at_offset,
};
use glua_parser::{LuaAssignStat, LuaAstNode, LuaSyntaxKind, LuaTableExpr, LuaTableField};
pub use infer_raw_member::infer_raw_member_type;

use super::{
    InferFailReason, LuaInferCache, SemanticDeclLevel, infer_node_semantic_decl,
    infer_table_should_be,
};

pub fn get_buildin_type_map_type_id(type_: &LuaType) -> Option<LuaTypeDeclId> {
    match type_ {
        LuaType::String
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => Some(LuaTypeDeclId::global("string")),
        LuaType::Io => Some(LuaTypeDeclId::global("io")),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaMemberInfo {
    pub property_owner_id: Option<LuaSemanticDeclId>,
    pub key: LuaMemberKey,
    pub typ: LuaType,
    pub feature: Option<LuaMemberFeature>,
    pub overload_index: Option<usize>,
}

type FindMembersResult = Option<Vec<LuaMemberInfo>>;
type RawGetMemberTypeResult = Result<LuaType, InferFailReason>;

pub(crate) fn intersect_member_types(db: &DbIndex, left: LuaType, right: LuaType) -> LuaType {
    if left == right {
        left
    } else {
        TypeOps::Intersect.apply(db, &left, &right)
    }
}

pub(crate) fn member_key_as_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::None => None,
        LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
        LuaMemberKey::Name(name) => Some(LuaType::StringConst(name.clone().into())),
        LuaMemberKey::ExprType(typ) => Some(typ.clone()),
    }
}

pub(crate) fn member_key_matches_type(
    db: &DbIndex,
    access_key_type: &LuaType,
    member_key: &LuaMemberKey,
) -> bool {
    let Some(member_key_type) = member_key_as_type(member_key) else {
        return false;
    };

    check_type_compact(db, access_key_type, &member_key_type).is_ok()
        || check_type_compact(db, &member_key_type, access_key_type).is_ok()
}

pub fn find_member_origin_owner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
) -> Option<LuaSemanticDeclId> {
    find_member_origin_owner_inner(db, infer_config, member_id, None)
}

pub fn find_member_origin_owner_at_offset(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
    caller_position: rowan::TextSize,
) -> Option<LuaSemanticDeclId> {
    find_member_origin_owner_inner(db, infer_config, member_id, Some(caller_position))
}

fn find_member_origin_owner_inner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    const MAX_ITERATIONS: usize = 50;
    let mut visited_members = HashSet::new();

    let mut current_owner = resolve_member_owner(db, infer_config, &member_id, caller_position);
    let mut final_owner = current_owner.clone();
    let mut iteration_count = 0;

    while let Some(LuaSemanticDeclId::Member(current_member_id)) = &current_owner {
        if visited_members.contains(current_member_id) || iteration_count >= MAX_ITERATIONS {
            break;
        }

        visited_members.insert(*current_member_id);
        iteration_count += 1;

        match resolve_member_owner(db, infer_config, current_member_id, caller_position) {
            Some(next_owner) => {
                final_owner = Some(next_owner.clone());
                current_owner = Some(next_owner);
            }
            None => break,
        }
    }

    final_owner
}

fn resolve_member_owner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: &LuaMemberId,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let current_node = member_id.get_syntax_id().to_node_from_root(&root)?;
    match member_id.get_syntax_id().get_kind() {
        LuaSyntaxKind::TableFieldAssign => {
            if LuaTableField::can_cast(current_node.kind().into()) {
                let table_field = LuaTableField::cast(current_node.clone())?;
                // 如果表是类, 那么通过类型推断获取 owner
                if let Some(owner_id) = resolve_table_field_through_type_inference(
                    db,
                    infer_config,
                    &table_field,
                    caller_position,
                ) {
                    return Some(owner_id);
                }
                // 非类, 那么通过右值推断
                let value_expr = table_field.get_value_expr()?;
                let value_node = value_expr.get_syntax_id().to_node_from_root(&root)?;
                infer_node_semantic_decl(db, infer_config, value_node, SemanticDeclLevel::default())
            } else {
                None
            }
        }
        LuaSyntaxKind::IndexExpr => {
            let assign_node = current_node.parent()?;
            let assign_stat = LuaAssignStat::cast(assign_node)?;
            let (vars, exprs) = assign_stat.get_var_and_expr_list();

            for (var, expr) in vars.iter().zip(exprs.iter()) {
                if var.syntax().text_range() == current_node.text_range() {
                    let expr_node = expr.get_syntax_id().to_node_from_root(&root)?;
                    return infer_node_semantic_decl(
                        db,
                        infer_config,
                        expr_node,
                        SemanticDeclLevel::default(),
                    );
                }
            }
            None
        }
        _ => None,
    }
}

fn resolve_table_field_through_type_inference(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    table_field: &LuaTableField,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let parent = table_field.syntax().parent()?;
    let table_expr = LuaTableExpr::cast(parent)?;
    let table_type = infer_table_should_be(db, infer_config, table_expr).ok()?;

    if !matches!(table_type, LuaType::Ref(_) | LuaType::Def(_)) {
        return None;
    }

    let field_key = table_field.get_field_key()?;
    let key = LuaMemberKey::from_index_key(db, infer_config, &field_key).ok()?;
    let caller_file_id = infer_config.get_file_id();
    let workspace_id = db.get_module_index().get_workspace_id(caller_file_id);
    let member_infos = match (workspace_id, caller_position) {
        (Some(workspace_id), Some(caller_position)) => {
            find_members_with_key_in_workspace_for_file_at_offset(
                db,
                &table_type,
                key,
                false,
                workspace_id,
                caller_file_id,
                caller_position,
            )?
        }
        (Some(workspace_id), None) => find_members_with_key_in_workspace_for_file(
            db,
            &table_type,
            key,
            false,
            workspace_id,
            caller_file_id,
        )?,
        (None, _) => find_members_with_key(db, &table_type, key, false)?,
    };

    member_infos
        .first()
        .cloned()
        .and_then(|m| m.property_owner_id)
}
