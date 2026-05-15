use std::collections::HashMap;

use glua_code_analysis::{
    LuaCompilation, LuaDeclId, LuaMemberId, LuaSemanticDeclId, LuaType, LuaTypeDeclId,
    SemanticDeclLevel, SemanticModel,
};
use glua_parser::{
    LuaAstNode, LuaDocTagField, LuaExpr, LuaIndexExpr, LuaStat, LuaSyntaxNode, LuaSyntaxToken,
    LuaTableField,
};
use lsp_types::Location;
use tokio_util::sync::CancellationToken;

use crate::handlers::hover::find_member_origin_owners;

pub fn search_implementations(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    token: LuaSyntaxToken,
    cancel_token: &CancellationToken,
) -> Option<Vec<Location>> {
    let mut result = Vec::new();
    if let Some(semantic_decl) =
        semantic_model.find_decl(token.clone().into(), SemanticDeclLevel::NoTrace)
    {
        match semantic_decl {
            LuaSemanticDeclId::TypeDecl(type_decl_id) => {
                search_type_implementations(semantic_model, compilation, type_decl_id, &mut result);
            }
            LuaSemanticDeclId::Member(member_id) => {
                search_member_implementations(
                    semantic_model,
                    compilation,
                    member_id,
                    &mut result,
                    cancel_token,
                );
            }
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                search_decl_implementations(
                    semantic_model,
                    compilation,
                    decl_id,
                    &mut result,
                    cancel_token,
                );
            }
            _ => {}
        }
    }

    Some(result)
}

pub fn search_member_implementations(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    member_id: LuaMemberId,
    result: &mut Vec<Location>,
    cancel_token: &CancellationToken,
) -> Option<()> {
    let member = semantic_model
        .get_db()
        .get_member_index()
        .get_member(&member_id)?;
    let member_key = member.get_key();

    let index_references = semantic_model
        .get_db()
        .get_reference_index()
        .get_index_references(member_key)?;

    let mut semantic_cache = HashMap::new();

    // Collect all same-named member IDs to check references against.
    let property_owners =
        match find_member_origin_owners(compilation, semantic_model, member_id, true, None) {
            crate::handlers::hover::DeclOriginResult::Multiple(ids) => ids,
            crate::handlers::hover::DeclOriginResult::Single(id) => vec![id],
        };

    // Resolve the class type that owns the field, so we can match
    // assignments on tables associated with the class.
    let origin_class_id = property_owners.iter().find_map(|id| match id {
        LuaSemanticDeclId::Member(mid) => semantic_model
            .get_db()
            .get_member_index()
            .get_current_owner(mid)
            .and_then(|owner| match owner {
                glua_code_analysis::LuaMemberOwner::Type(type_decl_id) => {
                    Some(type_decl_id.clone())
                }
                _ => None,
            }),
        _ => None,
    });

    for in_filed_syntax_id in index_references {
        if cancel_token.is_cancelled() {
            return None;
        }
        let reference_semantic_model =
            if let Some(semantic_model) = semantic_cache.get_mut(&in_filed_syntax_id.file_id) {
                semantic_model
            } else {
                let semantic_model = compilation.get_semantic_model(in_filed_syntax_id.file_id)?;
                semantic_cache.insert(in_filed_syntax_id.file_id, semantic_model);
                semantic_cache.get_mut(&in_filed_syntax_id.file_id)?
            };
        let root = reference_semantic_model.get_root();
        let node = in_filed_syntax_id.value.to_node_from_root(root.syntax())?;
        if let Some(is_signature) = check_member_reference(reference_semantic_model, node.clone()) {
            // Check if this reference semantically matches any of the origin owners.
            let matches_owner = property_owners.iter().any(|owner| {
                reference_semantic_model.is_reference_to(
                    node.clone(),
                    owner.clone(),
                    SemanticDeclLevel::default(),
                )
            });

            // Also check if the prefix of the index expression resolves to a
            // decl whose type corresponds to the origin class. This catches
            // cases like `local MyClass = {}; MyClass.myField = 1` where
            // `MyClass` is a local table that implements the @class MyClass,
            // but does NOT match by name text alone (which would cause false
            // positives from unrelated locals/tables with the same name).
            let matches_prefix = if !matches_owner && LuaIndexExpr::can_cast(node.kind().into()) {
                if let Some(class_id) = &origin_class_id {
                    let expr = LuaIndexExpr::cast(node.clone());
                    expr.and_then(|e| e.get_prefix_expr())
                        .is_some_and(|prefix| {
                            if let LuaExpr::NameExpr(name_expr) = prefix {
                                // Resolve the prefix to its semantic decl and
                                // verify the decl's type corresponds to the
                                // origin class, rather than just matching the
                                // name text.
                                reference_semantic_model
                                    .find_decl(
                                        name_expr.syntax().clone().into(),
                                        SemanticDeclLevel::default(),
                                    )
                                    .is_some_and(|prefix_decl| match prefix_decl {
                                        LuaSemanticDeclId::LuaDecl(prefix_decl_id) => {
                                            let prefix_type = reference_semantic_model
                                                .get_type(prefix_decl_id.into());
                                            match &prefix_type {
                                                LuaType::Ref(tid) | LuaType::Def(tid) => {
                                                    tid == class_id
                                                }
                                                LuaType::TableConst(_) => {
                                                    // Pure name-text matching for
                                                    // TableConst is too loose — it
                                                    // accepts unrelated locals/tables
                                                    // that happen to share the class
                                                    // name. Require a semantic type
                                                    // annotation (Ref/Def) instead.
                                                    false
                                                }
                                                _ => false,
                                            }
                                        }
                                        _ => false,
                                    })
                            } else {
                                false
                            }
                        })
                } else {
                    false
                }
            } else {
                false
            };

            if !matches_owner && !matches_prefix {
                continue;
            }

            let document = reference_semantic_model.get_document();
            let range = in_filed_syntax_id.value.get_range();
            let location = document.to_lsp_location(range)?;
            // 由于允许函数声明重载, 所以需要将签名放在前面
            if is_signature {
                result.insert(0, location);
            } else {
                result.push(location);
            }
        }
    }
    Some(())
}

/// 检查成员引用是否符合实现
fn check_member_reference(semantic_model: &SemanticModel, node: LuaSyntaxNode) -> Option<bool> {
    match &node {
        expr_node if LuaIndexExpr::can_cast(expr_node.kind().into()) => {
            let expr = LuaIndexExpr::cast(expr_node.clone())?;
            let _prefix_type = semantic_model.infer_expr(expr.get_prefix_expr()?).ok()?;
            let mut is_signature = false;
            if let Some(current_type) = semantic_model
                .infer_expr(LuaExpr::IndexExpr(expr.clone()))
                .ok()
                && current_type.is_signature()
            {
                is_signature = true;
            }
            // 往上寻找 stat 节点
            let stat = expr.ancestors::<LuaStat>().next()?;
            match stat {
                LuaStat::FuncStat(_) => {
                    return Some(is_signature);
                }
                LuaStat::AssignStat(assign_stat) => {
                    // 判断是否在左侧
                    let (vars, _) = assign_stat.get_var_and_expr_list();
                    for var in vars {
                        if var
                            .syntax()
                            .text_range()
                            .contains(node.text_range().start())
                        {
                            return Some(is_signature);
                        }
                    }
                    return None;
                }
                _ => {
                    return None;
                }
            }
        }
        tag_field_node if LuaDocTagField::can_cast(tag_field_node.kind().into()) => {
            return Some(false);
        }
        table_field_node if LuaTableField::can_cast(table_field_node.kind().into()) => {
            let table_field = LuaTableField::cast(table_field_node.clone())?;
            if table_field.is_assign_field() {
                return Some(false);
            } else {
                return None;
            }
        }
        _ => {}
    }

    Some(false)
}

pub fn search_type_implementations(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    type_decl_id: LuaTypeDeclId,
    result: &mut Vec<Location>,
) -> Option<()> {
    let db = semantic_model.get_db();
    let type_index = db.get_type_index();
    let type_decl = type_index.get_type_decl(&type_decl_id)?;
    let locations = type_decl.get_locations();
    let mut semantic_cache = HashMap::new();
    for location in locations {
        let semantic_model = if let Some(semantic_model) = semantic_cache.get_mut(&location.file_id)
        {
            semantic_model
        } else {
            let semantic_model = compilation.get_semantic_model(location.file_id)?;
            semantic_cache.insert(location.file_id, semantic_model);
            semantic_cache.get_mut(&location.file_id)?
        };
        let document = semantic_model.get_document();
        let range = location.range;
        let location = document.to_lsp_location(range)?;
        result.push(location);
    }

    Some(())
}

pub fn search_decl_implementations(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    decl_id: LuaDeclId,
    result: &mut Vec<Location>,
    cancel_token: &CancellationToken,
) -> Option<()> {
    let decl = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;

    if decl.is_local() {
        let document = semantic_model.get_document();
        let decl_refs = semantic_model
            .get_db()
            .get_reference_index()
            .get_decl_references(&decl_id.file_id, &decl_id)?;

        let range = decl.get_range();
        let location = document.to_lsp_location(range)?;
        result.push(location);

        for decl_ref in &decl_refs.cells {
            if decl_ref.is_write
                && let Some(location) = document.to_lsp_location(decl_ref.range)
            {
                result.push(location);
            }
        }

        return Some(());
    } else {
        let name = decl.get_name();
        let global_decl_ids = semantic_model
            .get_db()
            .get_global_index()
            .get_global_decl_ids(name)?;

        let mut semantic_cache = HashMap::new();

        for global_decl_id in global_decl_ids {
            if cancel_token.is_cancelled() {
                return None;
            }
            let semantic_model =
                if let Some(semantic_model) = semantic_cache.get_mut(&global_decl_id.file_id) {
                    semantic_model
                } else {
                    let semantic_model = compilation.get_semantic_model(global_decl_id.file_id)?;
                    semantic_cache.insert(global_decl_id.file_id, semantic_model);
                    semantic_cache.get_mut(&global_decl_id.file_id)?
                };
            let Some(decl) = semantic_model
                .get_db()
                .get_decl_index()
                .get_decl(global_decl_id)
            else {
                continue;
            };

            let document = semantic_model.get_document();
            let range = decl.get_range();
            let location = document.to_lsp_location(range)?;
            result.push(location);
        }
    }

    Some(())
}
