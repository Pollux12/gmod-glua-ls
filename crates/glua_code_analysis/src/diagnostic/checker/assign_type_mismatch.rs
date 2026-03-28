use std::ops::Deref;

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaExpr, LuaIndexExpr,
    LuaLiteralToken, LuaLocalStat, LuaNameExpr, LuaSyntaxNode, LuaSyntaxToken, LuaTableExpr,
    LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};
use rowan::{NodeOrToken, TextRange};

use crate::{
    DiagnosticCode, LuaDeclExtra, LuaDeclId, LuaMemberKey, LuaSemanticDeclId, LuaType,
    SemanticDeclLevel, SemanticModel, TypeCheckFailReason, TypeCheckResult, VariadicType,
    infer_index_expr,
};

use super::{Checker, DiagnosticContext, humanize_lint_type};

pub struct AssignTypeMismatchChecker;

impl Checker for AssignTypeMismatchChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::AssignTypeMismatch];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        for node in semantic_model.get_root().descendants::<LuaAst>() {
            match node {
                LuaAst::LuaAssignStat(assign) => {
                    check_assign_stat(context, semantic_model, &assign);
                }
                LuaAst::LuaLocalStat(local) => {
                    check_local_stat(context, semantic_model, &local);
                }
                _ => {}
            }
        }
    }
}

fn check_assign_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    assign: &LuaAssignStat,
) -> Option<()> {
    let (vars, exprs) = assign.get_var_and_expr_list();
    let value_types = semantic_model.infer_expr_list_types(&exprs, Some(vars.len()));

    for (idx, var) in vars.iter().enumerate() {
        match var {
            LuaVarExpr::IndexExpr(index_expr) => {
                check_index_expr(
                    context,
                    semantic_model,
                    index_expr,
                    exprs.get(idx).cloned(),
                    value_types.get(idx)?.0.clone(),
                );
            }
            LuaVarExpr::NameExpr(name_expr) => {
                check_name_expr(
                    context,
                    semantic_model,
                    name_expr,
                    exprs.get(idx).cloned(),
                    value_types.get(idx)?.0.clone(),
                );
            }
        }
    }
    Some(())
}

fn check_name_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    name_expr: &LuaNameExpr,
    expr: Option<LuaExpr>,
    value_type: LuaType,
) -> Option<()> {
    let semantic_decl = semantic_model.find_decl(
        rowan::NodeOrToken::Node(name_expr.syntax().clone()),
        SemanticDeclLevel::default(),
    )?;
    let mut source_is_inferred = false;
    let source_type = match semantic_decl.clone() {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = semantic_model
                .get_db()
                .get_decl_index()
                .get_decl(&decl_id)?;
            match decl.extra {
                LuaDeclExtra::Param {
                    idx, signature_id, ..
                } => {
                    let signature = semantic_model
                        .get_db()
                        .get_signature_index()
                        .get(&signature_id)?;
                    let param_type = signature.get_param_info_by_id(idx)?;
                    Some(param_type.type_ref.clone())
                }
                LuaDeclExtra::Local { .. } => {
                    let type_cache = semantic_model
                        .get_db()
                        .get_type_index()
                        .get_type_cache(&decl_id.into())?;
                    source_is_inferred = type_cache.is_infer();
                    Some(type_cache.as_type().clone())
                }
                _ => {
                    let type_cache = semantic_model
                        .get_db()
                        .get_type_index()
                        .get_type_cache(&decl_id.into())?;
                    source_is_inferred = type_cache.is_infer();
                    Some(type_cache.as_type().clone())
                }
            }
        }
        _ => None,
    };
    check_assign_type_mismatch(
        context,
        semantic_model,
        name_expr.get_range(),
        source_type.as_ref(),
        &value_type,
        false,
        source_is_inferred,
    );
    let strict_inferred_mismatch = semantic_model.get_emmyrc().strict.inferred_type_mismatch;
    if let Some(expr) = expr {
        if !source_is_inferred || strict_inferred_mismatch {
            check_table_expr(
                context,
                semantic_model,
                rowan::NodeOrToken::Node(name_expr.syntax().clone()),
                &expr,
                source_type.as_ref(),
            );
        }
    }

    Some(())
}

fn check_index_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    expr: Option<LuaExpr>,
    value_type: LuaType,
) -> Option<()> {
    let inferred_append_collection_target =
        is_inferred_member_collection_append_target(semantic_model, index_expr).unwrap_or(false);
    if inferred_append_collection_target {
        return Some(());
    }

    let source_is_inferred = inferred_member_flags(semantic_model, index_expr)
        .map(|(is_inferred, _)| is_inferred)
        .unwrap_or(false);

    let source_type = infer_index_expr(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        index_expr.clone(),
        false,
    )
    .ok();

    check_assign_type_mismatch(
        context,
        semantic_model,
        index_expr.get_range(),
        source_type.as_ref(),
        &value_type,
        true,
        source_is_inferred,
    );
    let strict_inferred_mismatch = semantic_model.get_emmyrc().strict.inferred_type_mismatch;
    if let Some(expr) = expr {
        if !source_is_inferred || strict_inferred_mismatch {
            check_table_expr(
                context,
                semantic_model,
                rowan::NodeOrToken::Node(index_expr.syntax().clone()),
                &expr,
                source_type.as_ref(),
            );
        }
    }
    Some(())
}

fn is_inferred_member_collection_append_target(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
) -> Option<bool> {
    if !is_collection_append_write(index_expr)? {
        return Some(false);
    }

    let prefix_expr = index_expr.get_prefix_expr()?;
    let LuaExpr::IndexExpr(prefix_index_expr) = prefix_expr else {
        return Some(false);
    };

    is_inferred_member_collection_expr(semantic_model, &prefix_index_expr)
}

fn is_collection_append_write(index_expr: &LuaIndexExpr) -> Option<bool> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let glua_parser::LuaIndexKey::Expr(index_key_expr) = index_expr.get_index_key()? else {
        return Some(false);
    };
    let LuaExpr::BinaryExpr(binary_expr) = index_key_expr else {
        return Some(false);
    };
    if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpAdd {
        return Some(false);
    }

    let (left, right) = binary_expr.get_exprs()?;
    if !is_literal_integer_one(&right) {
        return Some(false);
    }

    let LuaExpr::UnaryExpr(unary_expr) = left else {
        return Some(false);
    };
    if unary_expr.get_op_token()?.get_op() != UnaryOperator::OpLen {
        return Some(false);
    }

    let len_expr = unary_expr.get_expr()?;
    Some(expr_access_path(&prefix_expr) == expr_access_path(&len_expr))
}

fn expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

fn is_literal_integer_one(expr: &LuaExpr) -> bool {
    let LuaExpr::LiteralExpr(literal_expr) = expr else {
        return false;
    };

    matches!(
        literal_expr.get_literal(),
        Some(LuaLiteralToken::Number(number))
            if matches!(number.get_number_value(), NumberResult::Int(1))
    )
}

fn is_inferred_member_collection_expr(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
) -> Option<bool> {
    let owner = member_owner_from_prefix_type(semantic_model, index_expr.get_prefix_expr()?)?;
    let index_key = index_expr.get_index_key()?;
    let member_key = LuaMemberKey::from_index_key(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        &index_key,
    )
    .ok()?;

    member_flags_before_position(
        semantic_model,
        &owner,
        &member_key,
        index_expr.get_range().start(),
        false,
    )
    .map(|(_, is_collection)| is_collection)
}

fn inferred_member_flags(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
) -> Option<(bool, bool)> {
    let owner = member_owner_from_prefix_type(semantic_model, index_expr.get_prefix_expr()?)?;

    let index_key = index_expr.get_index_key()?;
    let member_key = LuaMemberKey::from_index_key(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        &index_key,
    )
    .ok()?;

    let flags = member_flags_before_position(
        semantic_model,
        &owner,
        &member_key,
        index_expr.get_range().start(),
        false,
    )?;
    if flags != (false, false) {
        return Some(flags);
    }

    member_flags_for_member_ids(
        semantic_model,
        vec![crate::LuaMemberId::new(
            index_expr.get_syntax_id(),
            semantic_model.get_file_id(),
        )],
    )
}

fn inferred_table_member_flags(
    semantic_model: &SemanticModel,
    table_type: &LuaType,
    member_key: &LuaMemberKey,
    position: rowan::TextSize,
    current_member_id: crate::LuaMemberId,
) -> Option<(bool, bool)> {
    let owner = member_owner_from_type(table_type)?;
    let flags = member_flags_before_position(semantic_model, &owner, member_key, position, true)?;
    if flags != (false, false) {
        return Some(flags);
    }

    member_flags_for_member_ids(semantic_model, vec![current_member_id])
}

fn member_owner_from_prefix_type(
    semantic_model: &SemanticModel,
    prefix_expr: LuaExpr,
) -> Option<crate::LuaMemberOwner> {
    let prefix_type = semantic_model.infer_expr(prefix_expr).ok()?;
    member_owner_from_type(&prefix_type)
}

fn member_owner_from_type(prefix_type: &LuaType) -> Option<crate::LuaMemberOwner> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => {
            Some(crate::LuaMemberOwner::Element(in_file_range.clone()))
        }
        LuaType::Def(def_id) | LuaType::Ref(def_id) => {
            Some(crate::LuaMemberOwner::Type(def_id.clone()))
        }
        LuaType::Instance(instance) => {
            Some(crate::LuaMemberOwner::Element(instance.get_range().clone()))
        }
        _ => None,
    }
}

fn member_flags_before_position(
    semantic_model: &SemanticModel,
    owner: &crate::LuaMemberOwner,
    member_key: &LuaMemberKey,
    position: rowan::TextSize,
    include_current_position: bool,
) -> Option<(bool, bool)> {
    member_flags_for_member_ids(
        semantic_model,
        semantic_model
            .get_db()
            .get_member_index()
            .get_members_for_owner_key(owner, member_key)
            .into_iter()
            .filter(|member| {
                if member.get_id().file_id != semantic_model.get_file_id() {
                    return true;
                }
                if include_current_position {
                    member.get_id().get_position() <= position
                } else {
                    member.get_id().get_position() < position
                }
            })
            .map(|member| member.get_id()),
    )
}

fn member_flags_for_member_ids<I>(
    semantic_model: &SemanticModel,
    member_ids: I,
) -> Option<(bool, bool)>
where
    I: IntoIterator<Item = crate::LuaMemberId>,
{
    let mut member_ids = member_ids.into_iter().peekable();
    if member_ids.peek().is_none() {
        return Some((false, false));
    }

    let mut all_lenient = true;
    let mut all_collection = true;
    for member_id in member_ids {
        if !all_lenient && !all_collection {
            return Some((false, false));
        }

        let Some(type_cache) = semantic_model
            .get_db()
            .get_type_index()
            .get_type_cache(&member_id.into())
        else {
            return Some((false, false));
        };
        if !type_cache.is_infer() {
            return Some((false, false));
        }

        if !is_lenient_inferred_member_type(type_cache.as_type()) {
            all_lenient = false;
        }

        let is_collection = match type_cache.as_type() {
            LuaType::Array(_) => true,
            LuaType::Tuple(tuple) => tuple.is_infer_resolve(),
            _ => false,
        };
        if !is_collection {
            all_collection = false;
        }
    }

    Some((all_lenient, all_collection))
}

fn is_lenient_inferred_member_type(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Nil | LuaType::Unknown | LuaType::Never | LuaType::Array(_)
    ) || matches!(typ, LuaType::Tuple(tuple) if tuple.is_infer_resolve())
}

fn check_local_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    local: &LuaLocalStat,
) -> Option<()> {
    let vars = local.get_local_name_list().collect::<Vec<_>>();
    let value_exprs = local.get_value_exprs().collect::<Vec<_>>();
    let value_types = semantic_model.infer_expr_list_types(&value_exprs, Some(vars.len()));

    for (idx, var) in vars.iter().enumerate() {
        let name_token = var.get_name_token()?;
        let decl_id = LuaDeclId::new(semantic_model.get_file_id(), name_token.get_position());
        let range = semantic_model
            .get_db()
            .get_decl_index()
            .get_decl(&decl_id)?
            .get_range();
        let var_type = semantic_model
            .get_db()
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|cache| cache.as_type().clone())?;
        let value_type = value_types.get(idx)?.0.clone();
        check_assign_type_mismatch(
            context,
            semantic_model,
            range,
            Some(&var_type),
            &value_type,
            false,
            false,
        );
        if let Some(expr) = value_exprs.get(idx) {
            check_table_expr(
                context,
                semantic_model,
                rowan::NodeOrToken::Node(var.syntax().clone()),
                expr,
                Some(&var_type),
            );
        }
    }
    Some(())
}

/// 检查整个表, 返回`true`表示诊断出异常.
pub fn check_table_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    decl_node: NodeOrToken<LuaSyntaxNode, LuaSyntaxToken>,
    table_expr: &LuaExpr,
    table_type: Option<&LuaType>, // 记录的类型
) -> Option<bool> {
    // 检查是否附加了元数据以跳过诊断
    if let Some(semantic_decl) = semantic_model.find_decl(decl_node, SemanticDeclLevel::default()) {
        if let Some(property) = semantic_model
            .get_db()
            .get_property_index()
            .get_property(&semantic_decl)
        {
            if let Some(lsp_optimization) = property.find_attribute_use("lsp_optimization") {
                if let Some(LuaType::DocStringConst(code)) =
                    lsp_optimization.get_param_by_name("code")
                {
                    if code.as_ref() == "check_table_field" {
                        return Some(false);
                    }
                };
            }
        }
    }

    let table_type = table_type?;
    if let Some(table_expr) = LuaTableExpr::cast(table_expr.syntax().clone()) {
        return check_table_expr_content(context, semantic_model, table_type, &table_expr);
    }
    Some(false)
}

// 处理 value_expr 是 TableExpr 的情况, 但不会处理 `local a = { x = 1 }, local v = a`
fn check_table_expr_content(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    table_type: &LuaType,
    table_expr: &LuaTableExpr,
) -> Option<bool> {
    const MAX_CHECK_COUNT: usize = 250;
    let mut check_count = 0;
    let mut has_diagnostic = false;

    let fields = table_expr.get_fields().collect::<Vec<_>>();

    for (idx, field) in fields.iter().enumerate() {
        check_count += 1;
        if check_count > MAX_CHECK_COUNT {
            return Some(has_diagnostic);
        }
        let Some(value_expr) = field.get_value_expr() else {
            continue;
        };

        let expr_type = semantic_model
            .infer_expr(value_expr.clone())
            .unwrap_or(LuaType::Any);

        // 位于的最后的 TableFieldValue 允许接受函数调用返回的多值, 而且返回的值必然会从下标 1 开始覆盖掉所有索引字段.
        if field.is_value_field()
            && idx == fields.len() - 1
            && let LuaType::Variadic(variadic) = &expr_type
        {
            if let Some(result) = check_table_last_variadic_type(
                context,
                semantic_model,
                table_type,
                idx,
                variadic,
                field.get_range(),
            ) {
                has_diagnostic = has_diagnostic || result;
            }
            continue;
        }

        let Some(field_key) = field.get_field_key() else {
            continue;
        };
        let Some(member_key) = semantic_model.get_member_key(&field_key) else {
            continue;
        };

        let source_type = match semantic_model.infer_member_type(table_type, &member_key) {
            Ok(typ) => typ,
            Err(_) => {
                continue;
            }
        };
        let source_is_inferred = inferred_table_member_flags(
            semantic_model,
            table_type,
            &member_key,
            field.get_range().start(),
            crate::LuaMemberId::new(field.get_syntax_id(), semantic_model.get_file_id()),
        )
        .map(|(is_inferred, _)| is_inferred)
        .unwrap_or(false);
        let strict_inferred_mismatch = semantic_model.get_emmyrc().strict.inferred_type_mismatch;

        if (!source_is_inferred || strict_inferred_mismatch)
            && should_check_nested_table_fields(&source_type)
            && let Some(table_expr) = LuaTableExpr::cast(value_expr.syntax().clone())
        {
            // 检查子表
            if let Some(result) =
                check_table_expr_content(context, semantic_model, &source_type, &table_expr)
            {
                has_diagnostic = has_diagnostic || result;
            }
            continue;
        }

        let allow_nil = matches!(table_type, LuaType::Array(_));

        if let Some(result) = check_assign_type_mismatch(
            context,
            semantic_model,
            field.get_range(),
            Some(&source_type),
            &expr_type,
            allow_nil,
            source_is_inferred,
        ) {
            has_diagnostic = has_diagnostic || result;
        }
    }

    Some(has_diagnostic)
}

fn should_check_nested_table_fields(source_type: &LuaType) -> bool {
    if source_type.is_table() || source_type.is_custom_type() {
        return true;
    }

    match source_type {
        LuaType::Union(union_type) => union_type
            .into_vec()
            .iter()
            .any(should_check_nested_table_fields),
        LuaType::MultiLineUnion(multi_union) => multi_union
            .get_unions()
            .iter()
            .any(|(typ, _)| should_check_nested_table_fields(typ)),
        LuaType::Intersection(intersection_type) => intersection_type
            .get_types()
            .iter()
            .any(should_check_nested_table_fields),
        _ => false,
    }
}

fn check_table_last_variadic_type(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    table_type: &LuaType,
    idx: usize,
    value_variadic: &VariadicType,
    range: TextRange,
) -> Option<bool> {
    // test max 10
    for offset in idx..(idx + 10) {
        let member_key = LuaMemberKey::Integer((idx + offset) as i64 + 1);
        let source_type = semantic_model
            .infer_member_type(table_type, &member_key)
            .ok()?;
        match source_type {
            LuaType::Variadic(source_variadic) => {
                return Some(source_variadic.deref() != value_variadic);
            }
            _ => {
                let expr_type = value_variadic.get_type(offset)?;

                if let Some(result) = check_assign_type_mismatch(
                    context,
                    semantic_model,
                    range,
                    Some(&source_type),
                    expr_type,
                    false,
                    false,
                ) && result
                {
                    return Some(true);
                }
            }
        }
    }

    Some(false)
}

fn check_assign_type_mismatch(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    range: TextRange,
    source_type: Option<&LuaType>,
    value_type: &LuaType,
    allow_nil: bool,
    source_is_inferred: bool,
) -> Option<bool> {
    let source_type = source_type.unwrap_or(&LuaType::Any);
    // 如果一致, 则不进行类型检查
    if source_type == value_type {
        return Some(false);
    }

    if source_is_inferred && !semantic_model.get_emmyrc().strict.inferred_type_mismatch {
        return Some(false);
    }

    // `never` indicates a type inference limitation, not an actual error
    if matches!(source_type, LuaType::Never) || matches!(value_type, LuaType::Never) {
        return Some(false);
    }

    // 某些情况下我们应允许可空, 例如: boolean[]
    if allow_nil && value_type.is_nullable() {
        return Some(false);
    }

    match (&source_type, &value_type) {
        // 如果源类型是定义类型, 则仅在目标类型是定义类型或引用类型时进行类型检查
        (LuaType::Def(_), LuaType::Def(_) | LuaType::Ref(_)) => {}
        (LuaType::Def(_), _) => return Some(false),
        // 此时检查交给 table_field
        (LuaType::Ref(_) | LuaType::Tuple(_), LuaType::TableConst(_)) => return Some(false),
        (LuaType::Nil, _) => return Some(false),
        // Allow nil assignment to reference/class types (common Lua cleanup pattern)
        (LuaType::Ref(_), LuaType::Nil) => return Some(false),
        (LuaType::Ref(_), LuaType::Instance(instance)) => {
            if instance.get_base().is_table() {
                return Some(false);
            }
        }
        _ => {}
    }

    let result = semantic_model.type_check_detail(source_type, value_type);
    if result.is_err() {
        add_type_check_diagnostic(
            context,
            semantic_model,
            range,
            source_type,
            value_type,
            result,
        );
        return Some(true);
    }
    Some(false)
}

fn add_type_check_diagnostic(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    range: TextRange,
    source_type: &LuaType,
    value_type: &LuaType,
    result: TypeCheckResult,
) {
    let db = semantic_model.get_db();
    match result {
        Ok(_) => (),
        Err(reason) => {
            let reason_message = match reason {
                TypeCheckFailReason::TypeNotMatchWithReason(reason) => reason,
                TypeCheckFailReason::TypeRecursion => t!("type recursion").to_string(),
                _ => "".to_string(),
            };

            context.add_diagnostic(
                DiagnosticCode::AssignTypeMismatch,
                range,
                t!(
                    "Cannot assign `%{value}` to `%{source}`. %{reason}",
                    value = humanize_lint_type(db, value_type),
                    source = humanize_lint_type(db, source_type),
                    reason = reason_message
                )
                .to_string(),
                None,
            );
        }
    }
}
