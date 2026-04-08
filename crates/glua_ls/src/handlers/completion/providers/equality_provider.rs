use glua_code_analysis::{
    DbIndex, InferGuard, LuaDeclId, LuaType, get_real_type, infer_table_field_value_should_be,
    infer_table_should_be,
};
use glua_parser::{
    BinaryOperator, LuaAst, LuaAstNode, LuaAstToken, LuaBinaryExpr, LuaBlock, LuaLiteralExpr,
    LuaSyntaxNode, LuaSyntaxToken, LuaTokenKind,
};

use crate::handlers::completion::{
    completion_builder::CompletionBuilder, providers::function_provider::dispatch_type,
};

pub fn add_completion(builder: &mut CompletionBuilder) -> Option<()> {
    if builder.is_cancelled() {
        return None;
    }
    if !check_can_add_completion(builder) {
        return None;
    }
    let types = get_token_should_type(builder)?;
    for typ in &types {
        dispatch_type(builder, typ.clone(), &InferGuard::new());
    }

    if !types.is_empty() && !builder.is_invoked() {
        builder.stop_here();
    }
    Some(())
}

fn check_can_add_completion(builder: &CompletionBuilder) -> bool {
    // 允许空格字符触发补全
    if builder.is_space_trigger_character {
        return true;
    }

    true
}

fn get_token_should_type(builder: &mut CompletionBuilder) -> Option<Vec<LuaType>> {
    let token = builder.trigger_token.clone();
    let mut parent_node = token.parent()?;
    // 如果父节点是块, 则可能是输入未完全, 语法树缺失
    if LuaBlock::cast(parent_node.clone()).is_some() {
        if let Some(node) = token.prev_token()?.parent() {
            parent_node = node;
        }
    } else {
        // 输入`""`时允许往上找
        if LuaLiteralExpr::can_cast(parent_node.kind().into()) {
            parent_node = parent_node.parent()?;
        }
    }

    if let Some(typ) = get_equality_should_type(builder, &token, &parent_node) {
        return Some(vec![typ]);
    }

    match LuaAst::cast(parent_node)? {
        LuaAst::LuaLocalStat(local_stat) => {
            let locals = local_stat.get_local_name_list().collect::<Vec<_>>();
            if locals.len() != 1 {
                return None;
            }

            let position = builder.trigger_token.text_range().start();
            let eq = local_stat.token_by_kind(LuaTokenKind::TkAssign)?;
            if position < eq.get_position() {
                return None;
            }
            let local = locals.first()?;
            let decl_id =
                LuaDeclId::new(builder.semantic_model.get_file_id(), local.get_position());
            let decl_type = builder
                .semantic_model
                .get_db()
                .get_type_index()
                .get_type_cache(&decl_id.into())?;
            let typ = decl_type.as_type().clone();
            if contain_function_types(builder.semantic_model.get_db(), &typ).is_none() {
                return Some(vec![typ]);
            }
        }
        LuaAst::LuaAssignStat(assign_stat) => {
            let (vars, _) = assign_stat.get_var_and_expr_list();

            if vars.len() != 1 {
                return None;
            }

            let position = builder.trigger_token.text_range().start();
            let eq = assign_stat.token_by_kind(LuaTokenKind::TkAssign)?;
            if position < eq.get_position() {
                return None;
            }

            let var = vars.first()?;
            let var_type = builder.semantic_model.infer_expr(var.to_expr());
            if let Ok(typ) = var_type
            // this is to avoid repeating function types in completion
                && contain_function_types(builder.semantic_model.get_db(), &typ).is_none()
            {
                return Some(vec![typ]);
            }
        }
        LuaAst::LuaTableExpr(table_expr) => {
            let table_type = infer_table_should_be(
                builder.semantic_model.get_db(),
                &mut builder.semantic_model.get_cache().borrow_mut(),
                table_expr,
            );
            if let Ok(typ) = table_type
                && let LuaType::Array(array_type) = typ
            {
                return Some(vec![array_type.get_base().clone()]);
            }
        }
        LuaAst::LuaTableField(table_field) => {
            if table_field.is_value_field() {
                return None;
            }

            let typ = infer_table_field_value_should_be(
                builder.semantic_model.get_db(),
                &mut builder.semantic_model.get_cache().borrow_mut(),
                table_field,
            )
            .ok()?;
            return Some(vec![typ]);
        }
        _ => {}
    }

    None
}

fn get_equality_should_type(
    builder: &CompletionBuilder,
    token: &LuaSyntaxToken,
    parent_node: &LuaSyntaxNode,
) -> Option<LuaType> {
    // Fast path for the old/direct tree shape.
    if let Some(binary_expr) = LuaBinaryExpr::cast(parent_node.clone())
        && let Some(typ) = infer_left_type_if_equality(builder, &binary_expr)
    {
        return Some(typ);
    }

    // Recovery-tolerant path: any binary-expression ancestor of the trigger token.
    if let Some(binary_expr) = token
        .parent_ancestors()
        .filter_map(LuaBinaryExpr::cast)
        .find(is_equality_binary_expr)
        && let Some(typ) = infer_left_type_if_equality(builder, &binary_expr)
    {
        return Some(typ);
    }

    // Fallback for newer recovery shapes: locate nearby equality operator token,
    // then resolve its binary-expression ancestor.
    let op_token = find_previous_equality_op_token(token.clone())?;
    let binary_expr = op_token
        .parent_ancestors()
        .filter_map(LuaBinaryExpr::cast)
        .find(is_equality_binary_expr)?;
    infer_left_type_if_equality(builder, &binary_expr)
}

fn infer_left_type_if_equality(
    builder: &CompletionBuilder,
    binary_expr: &LuaBinaryExpr,
) -> Option<LuaType> {
    if !is_equality_binary_expr(binary_expr) {
        return None;
    }

    let left = binary_expr.get_left_expr()?;
    builder.semantic_model.infer_expr(left).ok()
}

fn is_equality_binary_expr(binary_expr: &LuaBinaryExpr) -> bool {
    let Some(op_token) = binary_expr.get_op_token() else {
        return false;
    };

    matches!(
        op_token.get_op(),
        BinaryOperator::OpEq | BinaryOperator::OpNe
    )
}

fn find_previous_equality_op_token(token: LuaSyntaxToken) -> Option<LuaSyntaxToken> {
    let mut cursor = token;

    // Keep this local and bounded to avoid pulling in unrelated context.
    for _ in 0..32 {
        let prev = cursor.prev_token()?;
        cursor = prev.clone();

        match prev.kind().into() {
            LuaTokenKind::TkWhitespace => continue,
            LuaTokenKind::TkEq | LuaTokenKind::TkNe => return Some(prev),
            LuaTokenKind::TkEndOfLine | LuaTokenKind::TkSemicolon => return None,
            _ => {}
        }
    }

    None
}

pub fn contain_function_types(db: &DbIndex, typ: &LuaType) -> Option<()> {
    match typ {
        LuaType::Union(union_typ) => {
            for member in union_typ.into_vec().iter() {
                match member {
                    _ if member.is_function() => {
                        return Some(());
                    }
                    _ if member.is_custom_type() => {
                        let real_type = get_real_type(db, member)?;
                        if real_type.is_function() {
                            return Some(());
                        }
                    }
                    _ => {
                        continue;
                    }
                }
            }

            None
        }
        _ if typ.is_function() => Some(()),
        _ => None,
    }
}
