use std::collections::HashSet;

use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaElseIfClauseStat, LuaExpr,
    LuaForRangeStat, LuaForStat, LuaIfStat, LuaIndexExpr, LuaIndexKey, LuaLocalStat, LuaRepeatStat,
    LuaSyntaxKind, LuaSyntaxNode, LuaTokenKind, LuaVarExpr, LuaWhileStat,
};

use crate::{
    DbIndex, DiagnosticCode, InferFailReason, LuaAliasCallKind, LuaAliasCallType, LuaMemberKey,
    LuaMemberOwner, LuaType, SemanticModel, enum_variable_is_param, get_keyof_members,
    semantic::member_key_matches_type,
};

use super::{Checker, DiagnosticContext, humanize_lint_type};

pub struct CheckFieldChecker;

impl Checker for CheckFieldChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::InjectField, DiagnosticCode::UndefinedField];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let mut checked_index_expr = HashSet::new();
        for node in root.descendants::<LuaAst>() {
            if context.is_cancelled() {
                return;
            }
            match node {
                LuaAst::LuaAssignStat(assign) => {
                    let (vars, _) = assign.get_var_and_expr_list();
                    for var in vars.iter() {
                        if let LuaVarExpr::IndexExpr(index_expr) = var {
                            checked_index_expr.insert(index_expr.syntax().clone());
                            check_index_expr(
                                context,
                                semantic_model,
                                index_expr,
                                DiagnosticCode::InjectField,
                            );
                        }
                    }
                }
                LuaAst::LuaFuncStat(func_stat) => {
                    if let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name() {
                        checked_index_expr.insert(index_expr.syntax().clone());
                        check_index_expr(
                            context,
                            semantic_model,
                            &index_expr,
                            DiagnosticCode::InjectField,
                        );
                    }
                }
                LuaAst::LuaIndexExpr(index_expr) => {
                    if checked_index_expr.contains(index_expr.syntax()) {
                        continue;
                    }
                    check_index_expr(
                        context,
                        semantic_model,
                        &index_expr,
                        DiagnosticCode::UndefinedField,
                    );
                }
                _ => {}
            }
        }
    }
}

fn check_index_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    code: DiagnosticCode,
) -> Option<()> {
    if context.is_cancelled() {
        return Some(());
    }

    let db = context.db;
    let prefix_typ = semantic_model
        .infer_expr(index_expr.get_prefix_expr()?)
        .unwrap_or(LuaType::Unknown);

    if is_invalid_prefix_type(&prefix_typ, code) {
        return Some(());
    }

    let index_key = index_expr.get_index_key()?;

    if is_valid_member(
        context,
        semantic_model,
        &prefix_typ,
        index_expr,
        &index_key,
        code,
    )
    .is_some()
    {
        // TableOf types allow dot-access to all members but flag colon method calls.
        // GetTable() returns a plain table with fields; colon calls pass the wrong self.
        if is_tableof_colon_access(&prefix_typ, index_expr) {
            context.add_diagnostic(
                DiagnosticCode::UndefinedField,
                index_key.get_range()?,
                t!(
                    "Cannot call methods via `:` on a table returned by GetTable(). Use dot-access `.%{field}` instead. ",
                    field = index_key.get_path_part(),
                )
                .to_string(),
                None,
            );
        }
        return Some(());
    }

    if is_dynamic_field(db, &prefix_typ, &index_key) {
        return Some(());
    }

    if matches!(code, DiagnosticCode::UndefinedField)
        && !is_enum_type(db, &prefix_typ)
        && is_nil_guarded_in_scope(index_expr)
    {
        return Some(());
    }

    if matches!(code, DiagnosticCode::UndefinedField)
        && field_exists_on_subclass(db, &prefix_typ, &index_key.get_path_part())
    {
        return Some(());
    }

    // Bracket access with non-literal expression keys (e.g., `tbl[entity]`) is a dynamic
    // table access pattern that cannot be statically validated. Suppress undefined-field
    // unless the prefix is an enum or a typed table (where key validation is meaningful).
    if matches!(code, DiagnosticCode::UndefinedField) {
        if let LuaIndexKey::Expr(expr) = &index_key {
            let key_type = semantic_model.infer_expr(expr.clone()).ok();
            let is_literal_key = key_type.as_ref().is_some_and(|t| {
                t.is_string() || t.is_integer() || matches!(t, LuaType::StringConst(_))
            });
            if !is_literal_key {
                let has_strict_key_type = match &prefix_typ {
                    LuaType::Ref(id) | LuaType::Def(id) => db
                        .get_type_index()
                        .get_type_decl(id)
                        .is_some_and(|decl| decl.is_enum()),
                    LuaType::TableGeneric(_) => true,
                    _ => false,
                };
                if !has_strict_key_type {
                    return Some(());
                }
            }
        }
    }

    let index_name = index_key.get_path_part();
    match code {
        DiagnosticCode::InjectField => {
            context.add_diagnostic(
                DiagnosticCode::InjectField,
                index_key.get_range()?,
                t!(
                    "Fields cannot be injected into the reference of `%{class}` for `%{field}`. ",
                    class = humanize_lint_type(db, &prefix_typ),
                    field = index_name,
                )
                .to_string(),
                None,
            );
        }
        DiagnosticCode::UndefinedField => {
            context.add_diagnostic(
                DiagnosticCode::UndefinedField,
                index_key.get_range()?,
                t!("Undefined field `%{field}`. ", field = index_name,).to_string(),
                None,
            );
        }
        _ => {}
    }

    Some(())
}

fn is_invalid_prefix_type(typ: &LuaType, code: DiagnosticCode) -> bool {
    let mut current_typ = typ;
    loop {
        match current_typ {
            LuaType::Any
            | LuaType::Unknown
            | LuaType::Table
            | LuaType::Never
            | LuaType::SelfInfer
            | LuaType::TplRef(_)
            | LuaType::StrTplRef(_) => return true,
            LuaType::TableConst(_) => return code == DiagnosticCode::InjectField,
            LuaType::Instance(instance_typ) => {
                current_typ = instance_typ.get_base();
            }
            LuaType::TableOf(inner) => {
                current_typ = inner;
            }
            _ => return false,
        }
    }
}

/// Check if this is a colon method call on a `tableof(T)` type.
fn is_tableof_colon_access(prefix_typ: &LuaType, index_expr: &LuaIndexExpr) -> bool {
    let is_tableof = match prefix_typ {
        LuaType::TableOf(_) => true,
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .any(|t| matches!(t, LuaType::TableOf(_))),
        _ => false,
    };
    if !is_tableof {
        return false;
    }
    index_expr
        .get_index_token()
        .is_some_and(|token| token.is_colon())
}

pub(super) fn is_valid_member(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    prefix_typ: &LuaType,
    index_expr: &LuaIndexExpr,
    index_key: &LuaIndexKey,
    code: DiagnosticCode,
) -> Option<()> {
    match prefix_typ {
        LuaType::Global | LuaType::Userdata => return Some(()),
        LuaType::Array(typ) => {
            if typ.get_base().is_unknown() {
                return Some(());
            }
            // For arrays with a known element type, any numeric index is valid.
            // Integer literals and Idx keys are always fine.
            if matches!(index_key, LuaIndexKey::Integer(_) | LuaIndexKey::Idx(_)) {
                return Some(());
            }
            // Expression keys: accept integer/number types (Lua 5.1/GLua uses
            // `number` for all numeric values, including integer indices).
            if let LuaIndexKey::Expr(expr) = index_key {
                match semantic_model.infer_expr(expr.clone()) {
                    Ok(key_type)
                        if key_type.is_integer() || matches!(key_type, LuaType::Number) =>
                    {
                        return Some(());
                    }
                    Err(_) => return Some(()),
                    _ => {}
                }
            }
        }
        LuaType::Tuple(_) => {
            // Tuple types are array-like; integer and number index access is always valid.
            if matches!(index_key, LuaIndexKey::Integer(_) | LuaIndexKey::Idx(_)) {
                return Some(());
            }
            if let LuaIndexKey::Expr(expr) = index_key {
                match semantic_model.infer_expr(expr.clone()) {
                    Ok(key_type)
                        if key_type.is_integer() || matches!(key_type, LuaType::Number) =>
                    {
                        return Some(());
                    }
                    Err(_) => return Some(()),
                    _ => {}
                }
            }
        }
        // In GMod mode, strings support numeric byte indexing (e.g. `str[2]`, `str[i]`).
        // This mirrors the inference behaviour in `infer_raw_member.rs`.
        LuaType::String
        | LuaType::Io
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => {
            if semantic_model.get_db().get_emmyrc().gmod.enabled {
                if matches!(index_key, LuaIndexKey::Integer(_) | LuaIndexKey::Idx(_)) {
                    return Some(());
                }
                if let LuaIndexKey::Expr(expr) = index_key {
                    match semantic_model.infer_expr(expr.clone()) {
                        Ok(key_type)
                            if key_type.is_integer()
                                || key_type.is_number()
                                || matches!(key_type, LuaType::Number | LuaType::Integer) =>
                        {
                            return Some(());
                        }
                        Err(_) => return Some(()),
                        _ => {}
                    }
                }
            }
        }
        LuaType::Ref(_) => {
            // 如果类型是 Ref 的 enum, 那么需要检查变量是否为参数, 因为作为参数的 enum 本质上是 value 而不是 enum
            if check_enum_is_param(semantic_model, prefix_typ, index_expr).is_some() {
                return None;
            }
        }
        LuaType::Union(union) => {
            // For union types (e.g., Player|number from realm merge), check if the
            // field exists as a member of ANY non-nil union member using the member index.
            // This handles cases where runtime type checks guard the field access.
            let db = semantic_model.get_db();
            let field_name = index_key.get_path_part();
            let key = LuaMemberKey::Name(field_name.into());
            for member in union.into_vec().iter() {
                if member.is_nil() {
                    continue;
                }
                if let LuaType::Ref(id) | LuaType::Def(id) = member {
                    let owner = LuaMemberOwner::Type(id.clone());
                    if db
                        .get_member_index()
                        .get_member_item(&owner, &key)
                        .is_some()
                    {
                        return Some(());
                    }
                    // Also check parent types (e.g. Player inherits from Entity)
                    let mut supers = Vec::new();
                    id.collect_super_types(db, &mut supers);
                    for st in &supers {
                        if let LuaType::Ref(sid) | LuaType::Def(sid) = st {
                            let sowner = LuaMemberOwner::Type(sid.clone());
                            if db
                                .get_member_index()
                                .get_member_item(&sowner, &key)
                                .is_some()
                            {
                                return Some(());
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    // Check flow-based semantic info FIRST (cheap cached lookup) before
    // expensive AST walks like is_nil_safe_expr_context.
    let need_add_diagnostic =
        match semantic_model.get_semantic_info(index_expr.syntax().clone().into()) {
            Some(info) => {
                let mut need = info.semantic_decl.is_none();
                if need && code == DiagnosticCode::UndefinedField {
                    // For UndefinedField, if flow analysis resolved the type to a
                    // Signature (func-stat method definitions on Ref-typed variables),
                    // the field is genuinely defined on this variable and no diagnostic
                    // should be reported. This is more targeted than checking for any
                    // non-Unknown type, which would suppress legitimate undefined-field
                    // diagnostics from condition narrowing.
                    if matches!(info.typ, LuaType::Signature(_)) {
                        need = false;
                    }
                }
                if need {
                    let decl_type = semantic_model.get_index_decl_type(index_expr.clone());
                    if decl_type.is_some_and(|typ| !typ.is_unknown()) {
                        need = false;
                    };
                }

                need
            }
            None => true,
        };

    if !need_add_diagnostic {
        return Some(());
    }

    // nil-safe context check (ancestor walk) — only needed when flow analysis
    // didn't already resolve the field above.
    if matches!(code, DiagnosticCode::UndefinedField) && is_nil_safe_expr_context(index_expr) {
        if is_non_enum_custom_type(semantic_model, prefix_typ) {
            return Some(());
        }

        for child in index_expr.syntax().children_with_tokens() {
            if child.kind() == LuaTokenKind::TkLeftBracket.into() {
                // 此时为 [] 访问, 大部分类型都可以直接通行
                match prefix_typ {
                    LuaType::Ref(id) | LuaType::Def(id) => {
                        if let Some(decl) =
                            semantic_model.get_db().get_type_index().get_type_decl(id)
                        {
                            // enum 仍然需要检查
                            if decl.is_enum() {
                                break;
                            } else {
                                return Some(());
                            }
                        }
                    }
                    _ => return Some(()),
                }
            }
        }
    }

    let key_type = if let LuaIndexKey::Expr(expr) = index_key {
        match semantic_model.infer_expr(expr.clone()) {
            Ok(
                LuaType::Any
                | LuaType::Unknown
                | LuaType::Table
                | LuaType::TplRef(_)
                | LuaType::StrTplRef(_),
            ) => {
                return Some(());
            }
            Ok(typ) => typ,
            // 解析失败时认为其是合法的, 因为他可能没有标注类型
            Err(InferFailReason::UnResolveDeclType(_)) => {
                return Some(());
            }
            Err(_) => {
                return None;
            }
        }
    } else {
        return None;
    };

    // 一些类型组合需要特殊处理
    if let (LuaType::Def(id), _) = (prefix_typ, &key_type)
        && let Some(decl) = semantic_model.get_db().get_type_index().get_type_decl(id)
        && decl.is_class()
    {
        if code == DiagnosticCode::InjectField {
            return Some(());
        }
        if index_key.is_string() || matches!(key_type, LuaType::String) {
            return Some(());
        }
    }

    /*
    允许这种写法
            ---@type string?
            local field
            local a = Class[field]
    */
    let key_types = get_key_types(context, &semantic_model.get_db(), &key_type);
    if key_types.is_empty() {
        return None;
    }

    let prefix_types = get_prefix_types(context, prefix_typ);
    for prefix_type in prefix_types {
        if context.is_cancelled() {
            return Some(());
        }
        if let Some(members) = semantic_model.get_member_infos(&prefix_type) {
            for info in &members {
                if context.is_cancelled() {
                    return Some(());
                }
                for key_type in &key_types {
                    if member_key_matches_type(semantic_model.get_db(), key_type, &info.key) {
                        return Some(());
                    }
                }
            }
            if members.is_empty() {
                // 当没有任何成员信息且是 enum 类型时, 需要检查参数是否为自己
                if check_enum_self_reference(semantic_model, &prefix_type, &key_types).is_some() {
                    return Some(());
                }
            }
        } else if check_enum_self_reference(semantic_model, &prefix_type, &key_types).is_some() {
            return Some(());
        }
    }

    None
}

/// 检查枚举类型的自引用
fn check_enum_self_reference(
    semantic_model: &SemanticModel,
    prefix_type: &LuaType,
    key_types: &HashSet<LuaType>,
) -> Option<()> {
    if let LuaType::Ref(id) | LuaType::Def(id) = prefix_type
        && let Some(decl) = semantic_model.get_db().get_type_index().get_type_decl(id)
        && decl.is_enum()
        && key_types.iter().any(|typ| match typ {
            LuaType::Ref(key_id) | LuaType::Def(key_id) => *id == *key_id,
            _ => false,
        })
    {
        return Some(());
    }
    None
}

fn get_prefix_types(context: &DiagnosticContext, prefix_typ: &LuaType) -> HashSet<LuaType> {
    let mut type_set = HashSet::new();
    let mut stack = vec![prefix_typ.clone()];
    let mut visited = HashSet::new();

    while let Some(current_type) = stack.pop() {
        if context.is_cancelled() {
            return type_set;
        }
        if visited.contains(&current_type) {
            continue;
        }
        visited.insert(current_type.clone());
        match &current_type {
            LuaType::Union(union_typ) => {
                for t in union_typ.into_vec() {
                    stack.push(t.clone());
                }
            }
            LuaType::Any | LuaType::Unknown | LuaType::Nil => {}
            _ => {
                type_set.insert(current_type.clone());
            }
        }
    }
    type_set
}

fn get_key_types(context: &DiagnosticContext, db: &DbIndex, typ: &LuaType) -> HashSet<LuaType> {
    let mut type_set = HashSet::new();
    let mut stack = vec![typ.clone()];
    let mut visited = HashSet::new();

    while let Some(current_type) = stack.pop() {
        if context.is_cancelled() {
            return type_set;
        }
        if visited.contains(&current_type) {
            continue;
        }
        visited.insert(current_type.clone());
        match &current_type {
            LuaType::String => {
                type_set.insert(current_type);
            }
            LuaType::Integer => {
                type_set.insert(current_type);
            }
            LuaType::Union(union_typ) => {
                for t in union_typ.into_vec() {
                    stack.push(t.clone());
                }
            }
            LuaType::StrTplRef(_) | LuaType::Ref(_) => {
                type_set.insert(current_type);
            }
            LuaType::DocStringConst(_) | LuaType::DocIntegerConst(_) => {
                type_set.insert(current_type);
            }
            LuaType::Call(alias_call) => {
                if let Some(key_types) = get_keyof_keys(db, alias_call) {
                    for t in key_types {
                        stack.push(t.clone());
                    }
                }
            }
            _ => {}
        }
    }
    type_set
}

/// 判断给定的AST节点是否位于判断语句的条件表达式中
///
/// 该函数检查节点是否位于以下语句的条件部分：
/// - if语句的条件表达式
/// - while循环的条件表达式
/// - for循环的迭代表达式
/// - repeat循环的条件表达式
/// - elseif子句的条件表达式
///
/// # 参数
/// * `node` - 要检查的AST节点
///
/// # 返回值
/// * `true` - 节点位于判断语句的条件表达式中
/// * `false` - 节点不在判断语句的条件表达式中
fn in_conditional_statement<T: LuaAstNode>(node: &T) -> bool {
    let node_range = node.get_range();

    // 遍历所有祖先节点，查找条件语句
    for ancestor in node.syntax().ancestors() {
        match ancestor.kind().into() {
            LuaSyntaxKind::IfStat => {
                if let Some(if_stat) = LuaIfStat::cast(ancestor)
                    && let Some(condition_expr) = if_stat.get_condition_expr()
                    && condition_expr.get_range().contains_range(node_range)
                {
                    return true;
                }
            }
            LuaSyntaxKind::WhileStat => {
                if let Some(while_stat) = LuaWhileStat::cast(ancestor)
                    && let Some(condition_expr) = while_stat.get_condition_expr()
                    && condition_expr.get_range().contains_range(node_range)
                {
                    return true;
                }
            }
            LuaSyntaxKind::ForStat => {
                if let Some(for_stat) = LuaForStat::cast(ancestor) {
                    for iter_expr in for_stat.get_iter_expr() {
                        if iter_expr.get_range().contains_range(node_range) {
                            return true;
                        }
                    }
                }
            }
            LuaSyntaxKind::ForRangeStat => {
                if let Some(for_range_stat) = LuaForRangeStat::cast(ancestor) {
                    for expr in for_range_stat.get_expr_list() {
                        if expr.get_range().contains_range(node_range) {
                            return true;
                        }
                    }
                }
            }
            LuaSyntaxKind::RepeatStat => {
                if let Some(repeat_stat) = LuaRepeatStat::cast(ancestor)
                    && let Some(condition_expr) = repeat_stat.get_condition_expr()
                    && condition_expr.get_range().contains_range(node_range)
                {
                    return true;
                }
            }
            LuaSyntaxKind::ElseIfClauseStat => {
                if let Some(elseif_clause) = LuaElseIfClauseStat::cast(ancestor)
                    && let Some(condition_expr) = elseif_clause.get_condition_expr()
                    && condition_expr.get_range().contains_range(node_range)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn is_nil_safe_expr_context<T: LuaAstNode>(node: &T) -> bool {
    if in_conditional_statement(node) {
        return true;
    }

    for ancestor in node.syntax().ancestors().skip(1) {
        match ancestor.kind().into() {
            LuaSyntaxKind::CallExpr => {
                if let Some(call_expr) = LuaCallExpr::cast(ancestor.clone()) {
                    let node_range = node.syntax().text_range();
                    if is_known_member_guard_call_argument(&call_expr, node_range) {
                        return true;
                    }

                    if call_expr
                        .get_prefix_expr()
                        .is_some_and(|prefix| prefix.get_range().contains_range(node_range))
                    {
                        // The field is being called (e.g. `x:method()`).
                        // Only suppress if this call is on the right side of an `and`
                        // expression (short-circuit guard: `x.method and x:method()`).
                        if let Some(parent) = ancestor.parent() {
                            if let Some(binary) = LuaBinaryExpr::cast(parent) {
                                let has_and = binary
                                    .syntax()
                                    .children_with_tokens()
                                    .any(|child| child.kind() == LuaTokenKind::TkAnd.into());
                                if has_and {
                                    // Check that the call is the right operand (guarded side)
                                    if let Some((_, right)) = binary.get_exprs() {
                                        let call_range = call_expr.syntax().text_range();
                                        if right.syntax().text_range() == call_range {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                        return false;
                    }
                }

                return false;
            }
            LuaSyntaxKind::RequireCallExpr
            | LuaSyntaxKind::ErrorCallExpr
            | LuaSyntaxKind::AssertCallExpr
            | LuaSyntaxKind::TypeCallExpr
            | LuaSyntaxKind::SetmetatableCallExpr
            | LuaSyntaxKind::IndexExpr => {
                return false;
            }
            LuaSyntaxKind::BinaryExpr => {
                return ancestor.children_with_tokens().any(|child| {
                    child.kind() == LuaTokenKind::TkAnd.into()
                        || child.kind() == LuaTokenKind::TkEq.into()
                        || child.kind() == LuaTokenKind::TkNe.into()
                        || child.kind() == LuaTokenKind::TkOr.into()
                });
            }
            LuaSyntaxKind::UnaryExpr => {
                return ancestor
                    .children_with_tokens()
                    .any(|child| child.kind() == LuaTokenKind::TkNot.into());
            }
            kind if is_expression_boundary(kind) => return false,
            _ => {}
        }
    }

    false
}

fn is_known_member_guard_call_argument(
    call_expr: &LuaCallExpr,
    node_range: rowan::TextRange,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };

    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return false;
    };
    if name_expr.get_name_text().as_deref() != Some("isfunction") {
        return false;
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };

    args_list
        .get_args()
        .any(|arg| arg.get_range().contains_range(node_range))
}

fn is_expression_boundary(kind: LuaSyntaxKind) -> bool {
    matches!(
        kind,
        LuaSyntaxKind::Chunk
            | LuaSyntaxKind::Block
            | LuaSyntaxKind::EmptyStat
            | LuaSyntaxKind::LocalStat
            | LuaSyntaxKind::LocalFuncStat
            | LuaSyntaxKind::IfStat
            | LuaSyntaxKind::ElseIfClauseStat
            | LuaSyntaxKind::ElseClauseStat
            | LuaSyntaxKind::WhileStat
            | LuaSyntaxKind::DoStat
            | LuaSyntaxKind::ForStat
            | LuaSyntaxKind::ForRangeStat
            | LuaSyntaxKind::RepeatStat
            | LuaSyntaxKind::FuncStat
            | LuaSyntaxKind::LabelStat
            | LuaSyntaxKind::BreakStat
            | LuaSyntaxKind::ReturnStat
            | LuaSyntaxKind::GotoStat
            | LuaSyntaxKind::CallExprStat
            | LuaSyntaxKind::AssignStat
            | LuaSyntaxKind::GlobalStat
            | LuaSyntaxKind::UnknownStat
    )
}

fn is_non_enum_custom_type(semantic_model: &SemanticModel, typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => semantic_model
            .get_db()
            .get_type_index()
            .get_type_decl(id)
            .is_some_and(|decl| !decl.is_enum()),
        LuaType::Instance(instance_type) => {
            is_non_enum_custom_type(semantic_model, instance_type.get_base())
        }
        LuaType::Union(union_type) => {
            let members = union_type.into_vec();
            // Treat `T | nil` as equivalent to `T` for nil-safe context checks:
            // accessing a field on `T?` in an `and`/`or` expression is safe because
            // the surrounding boolean expression guards against the nil case.
            let non_nil: Vec<_> = members
                .iter()
                .filter(|t| !matches!(t, LuaType::Nil))
                .collect();
            !non_nil.is_empty()
                && non_nil
                    .iter()
                    .all(|t| is_non_enum_custom_type(semantic_model, t))
        }
        _ => false,
    }
}

fn check_enum_is_param(
    semantic_model: &SemanticModel,
    prefix_typ: &LuaType,
    index_expr: &LuaIndexExpr,
) -> Option<()> {
    enum_variable_is_param(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        index_expr,
        prefix_typ,
    )
}

fn get_keyof_keys(db: &DbIndex, alias_call: &LuaAliasCallType) -> Option<Vec<LuaType>> {
    if alias_call.get_call_kind() != LuaAliasCallKind::KeyOf {
        return None;
    }
    let source_operands = alias_call.get_operands().iter().collect::<Vec<_>>();
    if source_operands.len() != 1 {
        return None;
    }
    let members = get_keyof_members(db, &source_operands[0]).unwrap_or_default();
    let key_types = members
        .iter()
        .filter_map(|m| match &m.key {
            LuaMemberKey::Integer(i) => Some(LuaType::DocIntegerConst(*i)),
            LuaMemberKey::Name(s) => Some(LuaType::DocStringConst(s.clone().into())),
            _ => None,
        })
        .collect::<Vec<_>>();
    Some(key_types)
}

/// Check if this index expression is inside an if-body where the condition
/// guards the same field against nil (e.g., `if x.field ~= nil then ... end`
/// or `if x.field then ... end`).
fn is_nil_guarded_in_scope(index_expr: &LuaIndexExpr) -> bool {
    let target_text = index_expr.syntax().text().to_string();
    // Normalize colon-access to dot-access so that `obj:Method` matches `obj.Method`
    let normalized_target = target_text.replacen(':', ".", 1);
    let node_range = index_expr.syntax().text_range();
    let target_root_name = extract_root_identifier(&normalized_target);

    for ancestor in index_expr.syntax().ancestors() {
        match ancestor.kind().into() {
            LuaSyntaxKind::IfStat => {
                if let Some(if_stat) = LuaIfStat::cast(ancestor) {
                    if let Some(condition_expr) = if_stat.get_condition_expr() {
                        let cond_range = condition_expr.get_range();
                        if cond_range.contains_range(node_range) {
                            // Field IS in the condition — it's a nil/truthy check itself.
                            // Suppress if the field is used as a truthy check (direct, or, and, not).
                            if is_truthy_check_in_condition(&condition_expr, &normalized_target) {
                                return true;
                            }
                        } else {
                            // Field is in the body — check if the condition guards it.
                            if condition_nil_guards_field(&condition_expr, &normalized_target) {
                                if let Some(root_name) = target_root_name
                                    && has_root_reassignment_before_usage(index_expr, root_name)
                                {
                                    break;
                                }
                                return true;
                            }
                        }
                    }
                }
                break;
            }
            LuaSyntaxKind::ElseIfClauseStat => {
                if let Some(elseif_clause) = LuaElseIfClauseStat::cast(ancestor) {
                    if let Some(condition_expr) = elseif_clause.get_condition_expr() {
                        let cond_range = condition_expr.get_range();
                        if cond_range.contains_range(node_range) {
                            if is_truthy_check_in_condition(&condition_expr, &normalized_target) {
                                return true;
                            }
                        } else if condition_nil_guards_field(&condition_expr, &normalized_target) {
                            if let Some(root_name) = target_root_name
                                && has_root_reassignment_before_usage(index_expr, root_name)
                            {
                                break;
                            }
                            return true;
                        }
                    }
                }
                break;
            }
            // `or` default pattern: `field or DEFAULT` — field is nil-checked by the or.
            LuaSyntaxKind::BinaryExpr => {
                if let Some(binary) = LuaBinaryExpr::cast(ancestor.clone()) {
                    let has_or = binary
                        .syntax()
                        .children_with_tokens()
                        .any(|child| child.kind() == LuaTokenKind::TkOr.into());
                    if has_or {
                        // Check if the field is on the left side of `or`
                        let exprs: Vec<LuaExpr> = binary
                            .syntax()
                            .children()
                            .filter_map(LuaExpr::cast)
                            .collect();
                        if let Some(first) = exprs.first() {
                            let first_range = first.get_range();
                            if first_range.contains_range(node_range) {
                                return true;
                            }
                        }
                    }
                    let has_and = binary
                        .syntax()
                        .children_with_tokens()
                        .any(|child| child.kind() == LuaTokenKind::TkAnd.into());
                    if has_and {
                        // `field and expr` — field is being used as a guard
                        let exprs: Vec<LuaExpr> = binary
                            .syntax()
                            .children()
                            .filter_map(LuaExpr::cast)
                            .collect();
                        if let Some(first) = exprs.first() {
                            let first_range = first.get_range();
                            if first_range.contains_range(node_range) {
                                return true;
                            }
                        }
                    }
                }
                continue;
            }
            LuaSyntaxKind::ClosureExpr | LuaSyntaxKind::FuncStat | LuaSyntaxKind::LocalFuncStat => {
                break;
            }
            _ => {}
        }
    }

    // Pattern: local assignment followed by nil-check of the assigned variable.
    // e.g., `local x = obj.field; if x then ...`
    if is_local_assign_with_nil_check(index_expr, &normalized_target) {
        return true;
    }

    // Pattern: early return guard — a preceding `if not field then return end`
    // e.g., `if not obj.field then return end; ... obj.field`
    if is_guarded_by_early_return(index_expr, &normalized_target) {
        return true;
    }

    false
}

fn extract_root_identifier(field_text: &str) -> Option<&str> {
    let root = field_text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()?;
    if root.is_empty() {
        return None;
    }

    Some(root)
}

fn has_root_reassignment_before_usage(index_expr: &LuaIndexExpr, root_name: &str) -> bool {
    let containing_stat = match index_expr.syntax().ancestors().find(|n| {
        let k: LuaSyntaxKind = n.kind().into();
        matches!(
            k,
            LuaSyntaxKind::LocalStat
                | LuaSyntaxKind::AssignStat
                | LuaSyntaxKind::CallExprStat
                | LuaSyntaxKind::IfStat
                | LuaSyntaxKind::ReturnStat
        )
    }) {
        Some(s) => s,
        None => return false,
    };

    let parent = match containing_stat.parent() {
        Some(p) => p,
        None => return false,
    };
    let stat_start = containing_stat.text_range().start();
    for child in parent.children() {
        if child.text_range().start() >= stat_start {
            break;
        }
        if node_reassigns_root_name(&child, root_name) {
            return true;
        }
    }

    false
}

fn node_reassigns_root_name(node: &LuaSyntaxNode, root_name: &str) -> bool {
    if let Some(assign_stat) = LuaAssignStat::cast(node.clone()) {
        let (vars, _) = assign_stat.get_var_and_expr_list();
        for var in vars {
            if let LuaVarExpr::NameExpr(name_expr) = var
                && name_expr.syntax().text().to_string().trim() == root_name
            {
                return true;
            }
        }
    }

    if let Some(local_stat) = LuaLocalStat::cast(node.clone()) {
        for local_name in local_stat.get_local_name_list() {
            if local_name.syntax().text().to_string().trim() == root_name {
                return true;
            }
        }
    }

    for child in node.children() {
        if node_reassigns_root_name(&child, root_name) {
            return true;
        }
    }

    false
}

/// Check if a field is being used as a direct truthy/nil check in a condition.
/// Handles: `field`, `not field`, `field or other`, `field and other`, `a or field`, etc.
fn is_truthy_check_in_condition(condition: &LuaExpr, field_text: &str) -> bool {
    match condition {
        LuaExpr::IndexExpr(idx) => {
            idx.syntax().text().to_string().replacen(':', ".", 1) == field_text
        }
        LuaExpr::BinaryExpr(binary) => {
            let has_and_or = binary.syntax().children_with_tokens().any(|child| {
                let kind = child.kind();
                kind == LuaTokenKind::TkAnd.into() || kind == LuaTokenKind::TkOr.into()
            });
            if has_and_or {
                for expr in binary.syntax().children().filter_map(LuaExpr::cast) {
                    if is_truthy_check_in_condition(&expr, field_text) {
                        return true;
                    }
                }
            }
            // Also handle comparison: field ~= nil, field == nil
            let has_eq_ne = binary.syntax().children_with_tokens().any(|child| {
                let kind = child.kind();
                kind == LuaTokenKind::TkEq.into() || kind == LuaTokenKind::TkNe.into()
            });
            if has_eq_ne {
                let exprs: Vec<LuaExpr> = binary
                    .syntax()
                    .children()
                    .filter_map(LuaExpr::cast)
                    .collect();
                if exprs.len() == 2 {
                    let lhs = exprs[0].syntax().text().to_string();
                    let rhs = exprs[1].syntax().text().to_string();
                    if (lhs.replacen(':', ".", 1) == field_text && rhs.trim() == "nil")
                        || (rhs.replacen(':', ".", 1) == field_text && lhs.trim() == "nil")
                    {
                        return true;
                    }
                    // Also check if the field is nested inside either side,
                    // e.g., `type(obj.field) == "table"` — field is inside the call.
                    for expr in &exprs {
                        if is_truthy_check_in_condition(expr, field_text) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        LuaExpr::UnaryExpr(unary) => {
            for child in unary.syntax().children().filter_map(LuaExpr::cast) {
                if is_truthy_check_in_condition(&child, field_text) {
                    return true;
                }
            }
            false
        }
        LuaExpr::ParenExpr(paren) => {
            if let Some(inner) = paren.get_expr() {
                is_truthy_check_in_condition(&inner, field_text)
            } else {
                false
            }
        }
        LuaExpr::CallExpr(call) => {
            // Handle guard calls like IsValid(field), isfunction(field), etc.
            if let Some(args) = call.get_args_list() {
                for arg in args.get_args() {
                    if is_truthy_check_in_condition(&arg, field_text) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Check if a condition expression guards a field against nil.
/// Handles: `field ~= nil`, `field` (truthy), `isfunction(field)`, and compound `and` conditions.
fn condition_nil_guards_field(condition: &LuaExpr, field_text: &str) -> bool {
    match condition {
        LuaExpr::BinaryExpr(binary) => {
            let has_ne_nil = binary
                .syntax()
                .children_with_tokens()
                .any(|child| child.kind() == LuaTokenKind::TkNe.into());
            if has_ne_nil {
                let exprs: Vec<LuaExpr> = binary
                    .syntax()
                    .children()
                    .filter_map(LuaExpr::cast)
                    .collect();
                if exprs.len() == 2 {
                    let lhs_text = exprs[0].syntax().text().to_string();
                    let rhs_text = exprs[1].syntax().text().to_string();
                    if (lhs_text == field_text && rhs_text.trim() == "nil")
                        || (rhs_text == field_text && lhs_text.trim() == "nil")
                    {
                        return true;
                    }
                }
            }
            let has_and = binary
                .syntax()
                .children_with_tokens()
                .any(|child| child.kind() == LuaTokenKind::TkAnd.into());
            if has_and {
                let exprs: Vec<LuaExpr> = binary
                    .syntax()
                    .children()
                    .filter_map(LuaExpr::cast)
                    .collect();
                for expr in &exprs {
                    if condition_nil_guards_field(expr, field_text) {
                        return true;
                    }
                }
            }
            // For == or ~= comparisons, check if the field is nested inside a function call
            // on either side of the comparison, e.g. `string.sub(data.text, 1, 1) == "#"`
            // guards data.text (the call would have errored if data.text was nil).
            // Do NOT match direct field operands here — `field == "x"` does not guarantee
            // the field is non-nil in the body (nil ~= "x" is true in Lua).
            let has_eq_ne = binary.syntax().children_with_tokens().any(|child| {
                let k = child.kind();
                k == LuaTokenKind::TkEq.into() || k == LuaTokenKind::TkNe.into()
            });
            if has_eq_ne {
                let exprs: Vec<LuaExpr> = binary
                    .syntax()
                    .children()
                    .filter_map(LuaExpr::cast)
                    .collect();
                for expr in &exprs {
                    if let LuaExpr::CallExpr(call) = expr {
                        if let Some(args) = call.get_args_list() {
                            for arg in args.get_args() {
                                if condition_nil_guards_field(&arg, field_text) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            false
        }
        LuaExpr::IndexExpr(idx) => {
            idx.syntax().text().to_string().replacen(':', ".", 1) == field_text
        }
        LuaExpr::ParenExpr(paren) => {
            if let Some(inner) = paren.get_expr() {
                condition_nil_guards_field(&inner, field_text)
            } else {
                false
            }
        }
        LuaExpr::CallExpr(call) => {
            // Handle guard calls like isfunction(obj.field), istable(obj.field), etc.
            if let Some(args) = call.get_args_list() {
                for arg in args.get_args() {
                    if condition_nil_guards_field(&arg, field_text) {
                        return true;
                    }
                }
            }
            false
        }
        LuaExpr::UnaryExpr(unary) => {
            // Handle `not field` patterns
            for child in unary.syntax().children().filter_map(LuaExpr::cast) {
                if condition_nil_guards_field(&child, field_text) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Check if the field access is on the RHS of a local assignment, and the assigned
/// variable is nil-checked in a following sibling statement.
/// e.g., `local x = obj.field; if x then ...` or `local x = obj.field; if not x then return end`
fn is_local_assign_with_nil_check(index_expr: &LuaIndexExpr, _field_text: &str) -> bool {
    // Walk up to find the parent LocalStat
    let local_stat = match index_expr.syntax().ancestors().find_map(LuaLocalStat::cast) {
        Some(s) => s,
        None => return false,
    };

    // Get the variable name assigned in the local statement
    let local_names: Vec<_> = local_stat.get_local_name_list().collect();
    if local_names.is_empty() {
        return false;
    }
    let var_name = local_names[0].syntax().text().to_string();
    let var_name = var_name.trim();

    // Look at following sibling statements (up to 5) for a nil-check of the variable
    let local_stat_node = local_stat.syntax().clone();
    let parent = match local_stat_node.parent() {
        Some(p) => p,
        None => return false,
    };
    let mut found_self = false;
    let mut checked = 0;

    for sibling in parent.children() {
        if !found_self {
            if sibling == local_stat_node {
                found_self = true;
            }
            continue;
        }
        checked += 1;
        if checked > 5 {
            break;
        }

        let kind: LuaSyntaxKind = sibling.kind().into();
        if kind == LuaSyntaxKind::IfStat {
            // Check if the if-condition references the variable
            if let Some(if_stat) = LuaIfStat::cast(sibling) {
                if let Some(cond) = if_stat.get_condition_expr() {
                    let cond_text = cond.syntax().text().to_string();
                    if condition_references_var(&cond_text, var_name) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Check if there is a preceding if-statement with an early return that guards the field.
/// e.g., `if not obj.field then return end; ... obj.field`
fn is_guarded_by_early_return(index_expr: &LuaIndexExpr, field_text: &str) -> bool {
    // Find the statement containing the field access
    let containing_stat = match index_expr.syntax().ancestors().find(|n| {
        let k: LuaSyntaxKind = n.kind().into();
        matches!(
            k,
            LuaSyntaxKind::LocalStat
                | LuaSyntaxKind::AssignStat
                | LuaSyntaxKind::CallExprStat
                | LuaSyntaxKind::IfStat
                | LuaSyntaxKind::ReturnStat
        )
    }) {
        Some(s) => s,
        None => return false,
    };

    let parent = match containing_stat.parent() {
        Some(p) => p,
        None => return false,
    };
    let stat_range = containing_stat.text_range();

    // Look at preceding siblings for early-return guards
    for sibling in parent.children() {
        // Only look at siblings BEFORE the containing statement
        if sibling.text_range().start() >= stat_range.start() {
            break;
        }

        let kind: LuaSyntaxKind = sibling.kind().into();
        if kind != LuaSyntaxKind::IfStat {
            continue;
        }

        if let Some(if_stat) = LuaIfStat::cast(sibling) {
            // Check if the condition references our field
            if let Some(cond) = if_stat.get_condition_expr() {
                let cond_text = cond.syntax().text().to_string();
                let cond_text_normalized = cond_text.replacen(':', ".", 1);
                if !cond_text_contains_field_exact(&cond_text_normalized, field_text) {
                    continue;
                }
                // Check if the if-body contains a return statement (early return pattern)
                if if_body_has_return(&if_stat) {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if a condition text contains a field access expression with proper word boundaries.
/// Avoids false positives where `obj.a` would match inside `obj.aa`.
fn cond_text_contains_field_exact(cond_text: &str, field_text: &str) -> bool {
    let bytes = cond_text.as_bytes();
    let field_len = field_text.len();
    if field_len == 0 {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = cond_text[start..].find(field_text) {
        let abs_pos = start + pos;
        // Check boundary before the match
        let before_ok = abs_pos == 0 || {
            let c = bytes[abs_pos - 1] as char;
            !c.is_alphanumeric() && c != '_'
        };
        // Check boundary after the match
        let after_ok = abs_pos + field_len >= bytes.len() || {
            let c = bytes[abs_pos + field_len] as char;
            !c.is_alphanumeric() && c != '_'
        };
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

/// Check if a condition text references a variable name.
fn condition_references_var(cond_text: &str, var_name: &str) -> bool {
    // Simple text search: the variable appears as a word boundary in the condition
    // This handles: `if x then`, `if not x then`, `if x ~= nil then`, `IsValid(x)`, etc.
    for part in cond_text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if part == var_name {
            return true;
        }
    }
    false
}

/// Check if the body of an if-statement contains a return statement.
fn if_body_has_return(if_stat: &LuaIfStat) -> bool {
    if let Some(block) = if_stat.get_block() {
        for child in block.syntax().children() {
            let kind: LuaSyntaxKind = child.kind().into();
            if kind == LuaSyntaxKind::ReturnStat {
                return true;
            }
        }
    }
    false
}

fn is_dynamic_field(db: &DbIndex, prefix_typ: &LuaType, index_key: &LuaIndexKey) -> bool {
    let emmyrc = db.get_emmyrc();
    if !emmyrc.gmod.enabled || !emmyrc.gmod.infer_dynamic_fields {
        return false;
    }

    let field_name = index_key.get_path_part();
    let index = db.get_dynamic_field_index();

    has_dynamic_field_for_type(db, index, prefix_typ, &field_name)
}

/// Check if a type is an enum type. Enum members are finite and known,
/// so nil-guard suppression should not apply to them.
fn is_enum_type(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => db
            .get_type_index()
            .get_type_decl(id)
            .is_some_and(|decl| decl.is_enum()),
        _ => false,
    }
}

fn has_dynamic_field_for_type(
    db: &DbIndex,
    index: &crate::DynamicFieldIndex,
    typ: &LuaType,
    field_name: &str,
) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            if index.has_field(id, field_name) {
                return true;
            }
            // Walk parent types: dynamic fields registered on a parent class
            // (e.g. base_glide_car) should also be visible on child classes
            // (e.g. base_glide_motorcycle).
            let mut super_types = Vec::new();
            id.collect_super_types(db, &mut super_types);
            for super_type in &super_types {
                if let LuaType::Ref(super_id) | LuaType::Def(super_id) = super_type {
                    if index.has_field(super_id, field_name) {
                        return true;
                    }
                }
            }
            false
        }
        LuaType::Instance(instance) => {
            has_dynamic_field_for_type(db, index, instance.get_base(), field_name)
        }
        LuaType::TableOf(inner) => has_dynamic_field_for_type(db, index, inner, field_name),
        LuaType::Union(union_type) => union_type
            .into_vec()
            .iter()
            .any(|t| has_dynamic_field_for_type(db, index, t, field_name)),
        _ => false,
    }
}

/// Check if a field exists on any subclass of the given prefix type.
/// In GMod, entities are commonly passed around as their base type (e.g. Entity)
/// even though they are actually a specific subclass (e.g. Vehicle, Player).
fn field_exists_on_subclass(db: &DbIndex, prefix_typ: &LuaType, field_name: &str) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return false;
    }

    let type_id = match prefix_typ {
        LuaType::Ref(id) | LuaType::Def(id) => id,
        LuaType::TableOf(inner) => return field_exists_on_subclass(db, inner, field_name),
        LuaType::Union(union) => {
            return union
                .into_vec()
                .iter()
                .any(|t| field_exists_on_subclass(db, t, field_name));
        }
        _ => return false,
    };

    let sub_types = db.get_type_index().get_all_sub_types(type_id);
    for sub_decl in sub_types {
        let owner = LuaMemberOwner::Type(sub_decl.get_id());
        let key = LuaMemberKey::Name(field_name.into());
        if db
            .get_member_index()
            .get_member_item(&owner, &key)
            .is_some()
        {
            return true;
        }
    }
    false
}
