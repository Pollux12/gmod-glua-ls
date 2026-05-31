use std::collections::HashMap;

use crate::handlers::command::make_auto_doc_tag_command;
use glua_code_analysis::{LuaDocument, SemanticModel};
use glua_parser::{BinaryOperator, LuaAstNode, LuaBinaryExpr, LuaExpr};
use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, Range, TextEdit, WorkspaceEdit};
use rowan::{NodeOrToken, TokenAtOffset};

pub fn build_need_check_nil(
    semantic_model: &SemanticModel,
    actions: &mut Vec<CodeActionOrCommand>,
    range: Range,
    _data: &Option<serde_json::Value>,
) -> Option<()> {
    let document = semantic_model.get_document();
    let offset = document.get_offset(range.end.line as usize, range.end.character as usize)?;
    let root = semantic_model.get_root();
    let token = match root.syntax().token_at_offset(offset) {
        TokenAtOffset::Single(token) => token,
        TokenAtOffset::Between(_, token) => token,
        _ => return None,
    };
    // 取上一个token的父节点
    let node_or_token = token.prev_sibling_or_token()?;
    if let NodeOrToken::Node(expr_node) = node_or_token
        && LuaExpr::can_cast(expr_node.kind().into())
    {
        let expr = LuaExpr::cast(expr_node)?;
        let range = expr.syntax().text_range();
        let mut lsp_range = document.to_lsp_range(range)?;
        // 将范围缩小到最尾部的字符
        lsp_range.start.line = lsp_range.end.line;
        lsp_range.start.character = lsp_range.end.character;

        let text_edit = TextEdit {
            range: lsp_range,
            new_text: "--[[@cast -?]]".to_string(),
        };

        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: t!("use cast to remove nil").to_string(),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(WorkspaceEdit {
                changes: Some(HashMap::from([(document.get_uri(), vec![text_edit])])),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }

    Some(())
}

pub fn build_add_doc_tag(
    _semantic_model: &SemanticModel,
    actions: &mut Vec<CodeActionOrCommand>,
    _range: Range,
    data: &Option<serde_json::Value>,
) -> Option<()> {
    let Some(data) = data else {
        return None;
    };

    let tag_name = data.as_str()?;
    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: t!("Add @%{name} to the list of known tags", name = tag_name).to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        command: Some(make_auto_doc_tag_command(
            t!("Add @%{name} to the list of known tags", name = tag_name).as_ref(),
            tag_name,
        )),

        ..Default::default()
    }));

    Some(())
}

pub fn build_gmod_null_check(
    semantic_model: &SemanticModel,
    actions: &mut Vec<CodeActionOrCommand>,
    range: Range,
    _data: &Option<serde_json::Value>,
) -> Option<()> {
    let document = semantic_model.get_document();
    let target_range = document.to_rowan_range(range)?;
    let root = semantic_model.get_root();
    let token = match root.syntax().token_at_offset(target_range.start()) {
        TokenAtOffset::Single(token) => token,
        TokenAtOffset::Between(_, token) => token,
        _ => return None,
    };

    let mut current_node = token.parent();
    while let Some(node) = current_node {
        if node.text_range() == target_range {
            if let Some(binary_expr) = LuaBinaryExpr::cast(node.clone()) {
                if let Some(replacement) =
                    build_gmod_null_binary_replacement(semantic_model, &document, &binary_expr)
                {
                    push_gmod_null_check_action(actions, &document, range, replacement);
                }
                return Some(());
            }

            if LuaExpr::can_cast(node.kind().into()) {
                let expr_text = document.get_text_slice(target_range).trim();
                if !expr_text.is_empty() {
                    push_gmod_null_check_action(
                        actions,
                        &document,
                        range,
                        format!("IsValid({expr_text})"),
                    );
                }
                return Some(());
            }
        }

        current_node = node.parent();
    }

    Some(())
}

fn build_gmod_null_binary_replacement(
    semantic_model: &SemanticModel,
    document: &LuaDocument,
    binary_expr: &LuaBinaryExpr,
) -> Option<String> {
    let op = binary_expr.get_op_token()?.get_op();
    if !matches!(op, BinaryOperator::OpEq | BinaryOperator::OpNe) {
        return None;
    }

    let (left, right) = binary_expr.get_exprs()?;
    let checked_expr = match (
        is_nil_expr(semantic_model, &left),
        is_nil_expr(semantic_model, &right),
    ) {
        (false, true) => left,
        (true, false) => right,
        _ => return None,
    };

    let expr_text = document
        .get_text_slice(checked_expr.syntax().text_range())
        .trim();
    if expr_text.is_empty() {
        return None;
    }

    let is_valid_call = format!("IsValid({expr_text})");
    match op {
        BinaryOperator::OpEq => Some(format!("not {is_valid_call}")),
        BinaryOperator::OpNe => Some(is_valid_call),
        _ => None,
    }
}

fn is_nil_expr(semantic_model: &SemanticModel, expr: &LuaExpr) -> bool {
    semantic_model
        .infer_expr(expr.clone())
        .is_ok_and(|expr_type| expr_type.is_nil())
}

fn push_gmod_null_check_action(
    actions: &mut Vec<CodeActionOrCommand>,
    document: &LuaDocument,
    range: Range,
    new_text: String,
) {
    let text_edit = TextEdit { range, new_text };

    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: t!("Use IsValid(...) for GMod NULL check").to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(document.get_uri(), vec![text_edit])])),
            ..Default::default()
        }),
        ..Default::default()
    }));
}

pub fn build_preferred_local_alias_fix(
    semantic_model: &SemanticModel,
    actions: &mut Vec<CodeActionOrCommand>,
    range: Range,
    data: &Option<serde_json::Value>,
) -> Option<()> {
    let alias_name = data.as_ref()?.get("preferredAlias")?.as_str()?;
    let document = semantic_model.get_document();
    let text_edit = TextEdit {
        range,
        new_text: alias_name.to_string(),
    };

    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: t!("Replace with local alias '%{name}'", name = alias_name).to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(document.get_uri(), vec![text_edit])])),
            ..Default::default()
        }),
        ..Default::default()
    }));

    Some(())
}
