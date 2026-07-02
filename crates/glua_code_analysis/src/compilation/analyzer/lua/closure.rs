use std::ops::Deref;

use glua_parser::{
    BinaryOperator, LuaAst, LuaAstNode, LuaAstToken, LuaBlock, LuaCallArgList, LuaCallExpr,
    LuaClosureExpr, LuaComment, LuaDocTagReturn, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexKey,
    LuaLiteralToken, LuaLocalStat, LuaReturnStat, LuaStat, LuaSyntaxKind, LuaVarExpr, PathTrait,
    UnaryOperator,
};
use rowan::{TextRange, TextSize};

use crate::{
    DbIndex, InferFailReason, LuaDeclId, LuaInferCache, LuaType, ReturnTypeKind,
    SignatureReturnStatus, TypeOps, VariadicType,
    compilation::analyzer::unresolve::{
        UnResolveCallClosureParams, UnResolveClosureReturn, UnResolveParentAst,
        UnResolveParentClosureParams, UnResolveReturn,
    },
    db_index::{LuaDocReturnInfo, LuaMemberOwner, LuaReturnCorrelation, LuaSignatureId},
    infer_expr,
};

use super::{LuaAnalyzer, LuaReturnPoint, func_body::analyze_func_body_returns};

pub fn analyze_closure(analyzer: &mut LuaAnalyzer, closure: LuaClosureExpr) -> Option<()> {
    let signature_id = LuaSignatureId::from_closure(analyzer.file_id, &closure);

    analyze_colon_define(analyzer, &signature_id, &closure);
    analyze_lambda_params(analyzer, &signature_id, &closure);
    analyze_require_guard_param(analyzer, &signature_id, &closure);
    analyze_nil_return_guard_params(analyzer, &signature_id, &closure);
    analyze_return(analyzer, &signature_id, &closure);
    Some(())
}

fn analyze_nil_return_guard_params(
    analyzer: &mut LuaAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let params = closure
        .get_params_list()?
        .get_params()
        .filter_map(|param| {
            param
                .get_name_token()
                .map(|name| name.get_name_text().to_string())
        })
        .collect::<Vec<_>>();
    let block = closure.get_block()?;
    let mut guard_param_indices = Vec::new();
    let mut guard_return_ranges = Vec::new();
    for stat in block.get_stats() {
        let LuaStat::IfStat(if_stat) = stat else {
            continue;
        };
        let Some(param_name) = nil_return_guard_param_name(&if_stat) else {
            continue;
        };
        let Some(param_idx) = params.iter().position(|param| param == &param_name) else {
            continue;
        };

        guard_param_indices.push(param_idx);
        if let Some(return_stat) = nil_return_guard_return_stat(&if_stat) {
            guard_return_ranges.push(return_stat.get_range());
        }
    }

    if guard_param_indices.is_empty()
        || !non_guard_returns_are_proven_non_nil(analyzer, closure, &block, &guard_return_ranges)
    {
        return Some(());
    }

    for param_idx in guard_param_indices {
        analyzer
            .db
            .get_signature_index_mut()
            .get_or_create(*signature_id)
            .add_nil_return_guard_param(param_idx);
    }

    Some(())
}

fn nil_return_guard_param_name(if_stat: &LuaIfStat) -> Option<String> {
    if nil_return_guard_return_stat(if_stat).is_none() {
        return None;
    }

    let LuaExpr::UnaryExpr(unary_expr) = if_stat.get_condition_expr()? else {
        return None;
    };
    if unary_expr.get_op_token()?.get_op() != UnaryOperator::OpNot {
        return None;
    }
    expr_name_text(&unary_expr.get_expr()?)
}

fn nil_return_guard_return_stat(if_stat: &LuaIfStat) -> Option<LuaReturnStat> {
    if if_stat.get_else_if_clause_list().next().is_some() || if_stat.get_else_clause().is_some() {
        return None;
    }

    let block = if_stat.get_block()?;
    let mut stats = block.get_stats();
    let Some(LuaStat::ReturnStat(return_stat)) = stats.next() else {
        return None;
    };

    if stats.next().is_none() && return_stat.get_expr_list().next().is_none() {
        Some(return_stat)
    } else {
        None
    }
}

fn non_guard_returns_are_proven_non_nil(
    analyzer: &mut LuaAnalyzer,
    closure: &LuaClosureExpr,
    block: &LuaBlock,
    guard_return_ranges: &[TextRange],
) -> bool {
    let mut saw_non_guard_return = false;
    let return_stats = block.descendants::<LuaReturnStat>().collect::<Vec<_>>();
    for return_stat in return_stats {
        if return_stat.ancestors::<LuaClosureExpr>().next().as_ref() != Some(closure) {
            continue;
        }
        if guard_return_ranges.contains(&return_stat.get_range()) {
            continue;
        }

        saw_non_guard_return = true;
        let Some(first_expr) = return_stat.get_expr_list().next() else {
            return false;
        };
        if matches!(
            first_expr.clone(),
            LuaExpr::LiteralExpr(ref literal_expr)
                if matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Nil(_)))
        ) {
            return false;
        }
        if matches!(first_expr, LuaExpr::IndexExpr(_))
            && expr_contains_immediately_executed_call(&first_expr)
        {
            return false;
        }

        if return_index_expr_proven_non_nil_by_prior_branch(
            closure,
            block,
            &return_stat,
            &first_expr,
        ) {
            continue;
        }
        if return_index_expr_has_matching_initializer_branch(
            closure,
            block,
            &return_stat,
            &first_expr,
        ) {
            return false;
        }

        let Ok(ret_type) = infer_expr(
            analyzer.db,
            analyzer
                .context
                .infer_manager
                .get_infer_cache(analyzer.file_id),
            first_expr.clone(),
        ) else {
            return false;
        };
        if ret_type.is_nullable() {
            return false;
        }
    }

    saw_non_guard_return
}

fn return_index_expr_proven_non_nil_by_prior_branch(
    closure: &LuaClosureExpr,
    block: &LuaBlock,
    return_stat: &LuaReturnStat,
    return_expr: &LuaExpr,
) -> bool {
    if !matches!(return_expr, LuaExpr::IndexExpr(_)) {
        return false;
    }
    if expr_contains_immediately_executed_call(return_expr) {
        return false;
    }

    let return_text = expr_source_text(return_expr);
    for stat in block.get_stats() {
        if stat.get_range().start() >= return_stat.get_range().start() {
            break;
        }

        let LuaStat::IfStat(if_stat) = stat else {
            continue;
        };
        if !if_block_directly_assigns_non_nil_expr(&if_stat, &return_text) {
            continue;
        }

        let Some(condition_expr) = if_stat.get_condition_expr() else {
            continue;
        };
        if negated_condition_proves_index_exists(closure, &condition_expr, &return_text) {
            if intervening_statement_mutates_index(
                block,
                if_stat.get_range().end(),
                return_stat.get_range().start(),
                return_expr,
                &return_text,
            ) {
                continue;
            }

            return true;
        }
    }

    false
}

fn return_index_expr_has_matching_initializer_branch(
    closure: &LuaClosureExpr,
    block: &LuaBlock,
    return_stat: &LuaReturnStat,
    return_expr: &LuaExpr,
) -> bool {
    if !matches!(return_expr, LuaExpr::IndexExpr(_)) {
        return false;
    }

    let return_text = expr_source_text(return_expr);
    for stat in block.get_stats() {
        if stat.get_range().start() >= return_stat.get_range().start() {
            break;
        }

        let LuaStat::IfStat(if_stat) = stat else {
            continue;
        };
        if !if_block_directly_assigns_expr(&if_stat, &return_text) {
            continue;
        }

        let Some(condition_expr) = if_stat.get_condition_expr() else {
            continue;
        };
        if negated_condition_proves_index_exists(closure, &condition_expr, &return_text) {
            return true;
        }
    }

    false
}

fn intervening_statement_mutates_index(
    block: &LuaBlock,
    after: TextSize,
    before: TextSize,
    index_expr: &LuaExpr,
    index_expr_text: &str,
) -> bool {
    let dependency_names = index_expr_dependency_names(index_expr);
    for stat in block.get_stats() {
        let range = stat.get_range();
        if range.start() <= after || range.start() >= before {
            continue;
        }

        match stat {
            LuaStat::AssignStat(assign) => {
                let (_, exprs) = assign.get_var_and_expr_list();
                if exprs.iter().any(expr_contains_immediately_executed_call) {
                    return true;
                }

                let (vars, _) = assign.get_var_and_expr_list();
                if vars.into_iter().any(|var| {
                    written_expr_invalidates_index(
                        &var_source_text(&var),
                        index_expr_text,
                        &dependency_names,
                    )
                }) {
                    return true;
                }
            }
            LuaStat::LocalStat(local_stat) => {
                if local_stat
                    .get_value_exprs()
                    .any(|expr| expr_contains_immediately_executed_call(&expr))
                {
                    return true;
                }

                if local_stat
                    .get_local_name_list()
                    .into_iter()
                    .filter_map(|name| name.get_name_token())
                    .map(|name| name.get_name_text().to_string())
                    .any(|name| dependency_names.iter().any(|dep| dep == &name))
                {
                    return true;
                }
            }
            LuaStat::IfStat(if_stat) => {
                if !if_stat_is_harmless_between_guard_and_return(
                    &if_stat,
                    index_expr_text,
                    &dependency_names,
                ) {
                    return true;
                }
            }
            LuaStat::EmptyStat(_) => {}
            _ => return true,
        }
    }

    false
}

fn if_stat_is_harmless_between_guard_and_return(
    if_stat: &LuaIfStat,
    index_expr_text: &str,
    dependency_names: &[String],
) -> bool {
    if if_stat.get_else_if_clause_list().next().is_some() || if_stat.get_else_clause().is_some() {
        return false;
    }
    if if_stat
        .get_condition_expr()
        .is_none_or(|condition| expr_contains_immediately_executed_call(&condition))
    {
        return false;
    }
    let Some(block) = if_stat.get_block() else {
        return false;
    };

    block.get_stats().all(|stat| {
        direct_stat_is_harmless_between_guard_and_return(&stat, index_expr_text, dependency_names)
    })
}

fn direct_stat_is_harmless_between_guard_and_return(
    stat: &LuaStat,
    index_expr_text: &str,
    dependency_names: &[String],
) -> bool {
    match stat {
        LuaStat::AssignStat(assign) => {
            let (vars, exprs) = assign.get_var_and_expr_list();
            !exprs.iter().any(expr_contains_immediately_executed_call)
                && !vars.into_iter().any(|var| {
                    written_expr_invalidates_index(
                        &var_source_text(&var),
                        index_expr_text,
                        dependency_names,
                    )
                })
        }
        LuaStat::LocalStat(local_stat) => {
            !local_stat
                .get_value_exprs()
                .any(|expr| expr_contains_immediately_executed_call(&expr))
                && !local_stat
                    .get_local_name_list()
                    .into_iter()
                    .filter_map(|name| name.get_name_token())
                    .map(|name| name.get_name_text().to_string())
                    .any(|name| dependency_names.iter().any(|dep| dep == &name))
        }
        LuaStat::CallExprStat(call_stat) => call_stat
            .get_call_expr()
            .is_some_and(|call_expr| call_is_harmless_setmetatable(&call_expr, index_expr_text)),
        LuaStat::EmptyStat(_) => true,
        _ => false,
    }
}

fn call_is_harmless_setmetatable(call_expr: &LuaCallExpr, index_expr_text: &str) -> bool {
    if call_expr
        .get_prefix_expr()
        .is_none_or(|prefix| expr_source_text(&prefix) != "setmetatable")
    {
        return false;
    }
    let Some(args) = call_expr.get_args_list() else {
        return false;
    };
    let args = args.get_args().collect::<Vec<_>>();
    args.first()
        .is_some_and(|arg| expr_source_text(arg) == index_expr_text)
        && args
            .iter()
            .skip(1)
            .all(|arg| !expr_contains_immediately_executed_call(arg))
}

fn expr_contains_immediately_executed_call(expr: &LuaExpr) -> bool {
    let expr_range = expr.get_range();
    matches!(expr, LuaExpr::CallExpr(_))
        || expr
            .descendants::<LuaCallExpr>()
            .any(|call_expr| !call_is_inside_nested_closure_expr(&call_expr, expr_range))
}

fn call_is_inside_nested_closure_expr(call_expr: &LuaCallExpr, expr_range: TextRange) -> bool {
    call_expr.ancestors::<LuaClosureExpr>().any(|closure| {
        let closure_range = closure.get_range();
        closure_range.start() >= expr_range.start() && closure_range.end() <= expr_range.end()
    })
}

fn written_expr_invalidates_index(
    written_text: &str,
    index_expr_text: &str,
    dependency_names: &[String],
) -> bool {
    written_text == index_expr_text
        || index_expr_text
            .strip_prefix(written_text)
            .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('['))
        || dependency_names.iter().any(|name| name == written_text)
}

fn index_expr_dependency_names(expr: &LuaExpr) -> Vec<String> {
    let mut names = Vec::new();
    collect_index_expr_dependency_names(expr, &mut names);
    names
}

fn collect_index_expr_dependency_names(expr: &LuaExpr, names: &mut Vec<String>) {
    match expr {
        LuaExpr::NameExpr(_) => names.push(expr_source_text(expr)),
        LuaExpr::IndexExpr(index_expr) => {
            if let Some(prefix_expr) = index_expr.get_prefix_expr() {
                collect_index_expr_dependency_names(&prefix_expr, names);
            }
            if let Some(LuaIndexKey::Expr(key_expr)) = index_expr.get_index_key() {
                collect_index_expr_dependency_names(&key_expr, names);
            }
        }
        _ => {}
    }
}

fn if_block_directly_assigns_non_nil_expr(if_stat: &LuaIfStat, expr_text: &str) -> bool {
    if if_stat.get_else_if_clause_list().next().is_some() || if_stat.get_else_clause().is_some() {
        return false;
    }

    let Some(block) = if_stat.get_block() else {
        return false;
    };
    let mut stats = block.get_stats();
    let Some(LuaStat::AssignStat(assign)) = stats.next() else {
        return false;
    };
    if stats.next().is_some() {
        return false;
    }

    let (vars, exprs) = assign.get_var_and_expr_list();
    if vars.len() != 1 || exprs.len() != 1 {
        return false;
    }

    var_source_text(&vars[0]) == expr_text && expr_is_syntactically_non_nil(&exprs[0])
}

fn if_block_directly_assigns_expr(if_stat: &LuaIfStat, expr_text: &str) -> bool {
    if if_stat.get_else_if_clause_list().next().is_some() || if_stat.get_else_clause().is_some() {
        return false;
    }

    let Some(block) = if_stat.get_block() else {
        return false;
    };
    let mut stats = block.get_stats();
    let Some(LuaStat::AssignStat(assign)) = stats.next() else {
        return false;
    };
    if stats.next().is_some() {
        return false;
    }

    let (vars, exprs) = assign.get_var_and_expr_list();
    vars.len() == 1 && exprs.len() == 1 && var_source_text(&vars[0]) == expr_text
}

fn negated_condition_proves_index_exists(
    closure: &LuaClosureExpr,
    condition_expr: &LuaExpr,
    index_expr_text: &str,
) -> bool {
    let LuaExpr::UnaryExpr(unary_expr) = condition_expr else {
        return false;
    };
    if unary_expr
        .get_op_token()
        .is_none_or(|op| op.get_op() != UnaryOperator::OpNot)
    {
        return false;
    }

    match unary_expr.get_expr() {
        Some(expr) if expr_source_text(&expr) == index_expr_text => true,
        Some(LuaExpr::CallExpr(call_expr)) => {
            predicate_call_proves_index_exists(closure, &call_expr, index_expr_text)
        }
        _ => false,
    }
}

fn predicate_call_proves_index_exists(
    closure: &LuaClosureExpr,
    call_expr: &LuaCallExpr,
    index_expr_text: &str,
) -> bool {
    if call_expr.is_colon_call() {
        return false;
    }

    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };
    let predicate_path = expr_source_text(&prefix_expr);

    let Some(root) = closure.syntax().ancestors().last() else {
        return false;
    };

    let matching_predicates = root
        .descendants()
        .filter_map(LuaFuncStat::cast)
        .filter(|func_stat| func_stat.ancestors::<LuaClosureExpr>().next().is_none())
        .filter(|func_stat| !func_stat_is_colon_defined(func_stat))
        .filter(|func_stat| {
            func_stat
                .get_func_name()
                .and_then(|name| name.get_access_path().map(|path| path.to_string()))
                .as_deref()
                == Some(predicate_path.as_str())
        })
        .filter_map(|func_stat| func_stat.get_closure())
        .collect::<Vec<_>>();

    if matching_predicates.len() != 1 {
        return false;
    }

    predicate_returns_index_non_nil(&matching_predicates[0], call_expr, index_expr_text)
}

fn func_stat_is_colon_defined(func_stat: &LuaFuncStat) -> bool {
    matches!(
        func_stat.get_func_name(),
        Some(LuaVarExpr::IndexExpr(index_expr))
            if index_expr.get_index_token().is_some_and(|token| token.is_colon())
    )
}

fn predicate_returns_index_non_nil(
    closure: &LuaClosureExpr,
    call_expr: &LuaCallExpr,
    index_expr_text: &str,
) -> bool {
    let Some(block) = closure.get_block() else {
        return false;
    };
    let substitutions = predicate_param_substitutions(closure, call_expr);

    let mut saw_proving_return = false;
    for return_stat in block.descendants::<LuaReturnStat>().filter(|return_stat| {
        return_stat.ancestors::<LuaClosureExpr>().next().as_ref() == Some(closure)
    }) {
        let Some(return_expr) = return_stat.get_expr_list().next() else {
            continue;
        };
        if expr_is_falsy_literal(&return_expr) {
            continue;
        }
        if !predicate_return_expr_proves_index(return_expr, &substitutions, index_expr_text) {
            return false;
        }
        saw_proving_return = true;
    }

    saw_proving_return
}

fn predicate_return_expr_proves_index(
    return_expr: LuaExpr,
    substitutions: &[(String, String)],
    index_expr_text: &str,
) -> bool {
    let return_text = expr_source_text_with_param_subs(&return_expr, substitutions);
    if return_text == index_expr_text {
        return true;
    }

    let LuaExpr::BinaryExpr(binary_expr) = return_expr else {
        return false;
    };
    if binary_expr
        .get_op_token()
        .is_none_or(|op| op.get_op() != BinaryOperator::OpNe)
    {
        return false;
    }
    let Some((left, right)) = binary_expr.get_exprs() else {
        return false;
    };

    let left_text = expr_source_text_with_param_subs(&left, substitutions);
    let right_text = expr_source_text_with_param_subs(&right, substitutions);

    (left_text == index_expr_text && expr_is_nil(&right))
        || (expr_is_nil(&left) && right_text == index_expr_text)
}

fn predicate_param_substitutions(
    closure: &LuaClosureExpr,
    call_expr: &LuaCallExpr,
) -> Vec<(String, String)> {
    let Some(params) = closure.get_params_list() else {
        return Vec::new();
    };
    let Some(args) = call_expr.get_args_list() else {
        return Vec::new();
    };

    params
        .get_params()
        .filter_map(|param| {
            param
                .get_name_token()
                .map(|name| name.get_name_text().to_string())
        })
        .zip(args.get_args().map(|arg| expr_source_text(&arg)))
        .collect()
}

fn expr_is_nil(expr: &LuaExpr) -> bool {
    matches!(
        expr,
        LuaExpr::LiteralExpr(literal_expr)
            if matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Nil(_)))
    )
}

fn expr_is_syntactically_non_nil(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::TableExpr(_) => !expr_contains_immediately_executed_call(expr),
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal() {
            Some(LuaLiteralToken::String(_)) | Some(LuaLiteralToken::Number(_)) => true,
            Some(LuaLiteralToken::Bool(token)) => token.is_true(),
            _ => false,
        },
        _ => false,
    }
}

fn expr_is_falsy_literal(expr: &LuaExpr) -> bool {
    matches!(
        expr,
        LuaExpr::LiteralExpr(literal_expr)
            if matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Nil(_)))
                || matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Bool(ref token)) if !token.is_true())
    )
}

fn expr_source_text(expr: &LuaExpr) -> String {
    expr.syntax()
        .text()
        .to_string()
        .split_whitespace()
        .collect()
}

fn expr_source_text_with_param_subs(expr: &LuaExpr, substitutions: &[(String, String)]) -> String {
    match expr {
        LuaExpr::NameExpr(_) => {
            let name = expr_source_text(expr);
            substitutions
                .iter()
                .find_map(|(param, arg)| (param == &name).then(|| arg.clone()))
                .unwrap_or(name)
        }
        LuaExpr::IndexExpr(index_expr) => {
            let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                return expr_source_text(expr);
            };
            let prefix_text = expr_source_text_with_param_subs(&prefix_expr, substitutions);
            match index_expr.get_index_key() {
                Some(LuaIndexKey::Name(name)) => {
                    format!("{prefix_text}.{}", name.get_name_text())
                }
                Some(LuaIndexKey::String(string)) => {
                    format!("{prefix_text}[{}]", string.syntax().text())
                }
                Some(LuaIndexKey::Integer(integer)) => {
                    format!("{prefix_text}[{}]", integer.syntax().text())
                }
                Some(LuaIndexKey::Expr(key_expr)) => format!(
                    "{prefix_text}[{}]",
                    expr_source_text_with_param_subs(&key_expr, substitutions)
                ),
                Some(LuaIndexKey::Idx(_)) | None => expr_source_text(expr),
            }
        }
        _ => expr_source_text(expr),
    }
}

fn var_source_text(var: &LuaVarExpr) -> String {
    var.syntax().text().to_string().split_whitespace().collect()
}

fn analyze_colon_define(
    analyzer: &mut LuaAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let signature = analyzer
        .db
        .get_signature_index_mut()
        .get_or_create(*signature_id);

    let func_stat = closure.get_parent::<LuaFuncStat>()?;
    let func_name = func_stat.get_func_name()?;
    if let LuaVarExpr::IndexExpr(index_expr) = func_name {
        let index_token = index_expr.get_index_token()?;
        signature.is_colon_define = index_token.is_colon();
    }

    Some(())
}

fn analyze_lambda_params(
    analyzer: &mut LuaAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let ast_node = closure.get_parent::<LuaAst>()?;
    match ast_node {
        LuaAst::LuaCallArgList(call_arg_list) => {
            let call_expr = call_arg_list.get_parent::<LuaCallExpr>()?;
            let pos = closure.get_position();
            let founded_idx = call_arg_list
                .get_args()
                .position(|arg| arg.get_position() == pos)?;

            let unresolved = UnResolveCallClosureParams {
                file_id: analyzer.file_id,
                signature_id: *signature_id,
                call_expr,
                param_idx: founded_idx,
            };

            analyzer
                .context
                .add_unresolve(unresolved.into(), InferFailReason::None);
        }
        LuaAst::LuaFuncStat(func_stat) => {
            let unresolved = UnResolveParentClosureParams {
                file_id: analyzer.file_id,
                signature_id: *signature_id,
                parent_ast: UnResolveParentAst::LuaFuncStat(func_stat.clone()),
            };

            analyzer
                .context
                .add_unresolve(unresolved.into(), InferFailReason::None);
        }
        LuaAst::LuaTableField(table_field) => {
            let unresolved = UnResolveParentClosureParams {
                file_id: analyzer.file_id,
                signature_id: *signature_id,
                parent_ast: UnResolveParentAst::LuaTableField(table_field.clone()),
            };

            analyzer
                .context
                .add_unresolve(unresolved.into(), InferFailReason::None);
        }
        LuaAst::LuaAssignStat(assign_stat) => {
            let unresolved = UnResolveParentClosureParams {
                file_id: analyzer.file_id,
                signature_id: *signature_id,
                parent_ast: UnResolveParentAst::LuaAssignStat(assign_stat.clone()),
            };

            analyzer
                .context
                .add_unresolve(unresolved.into(), InferFailReason::None);
        }
        _ => {}
    }

    Some(())
}

fn analyze_require_guard_param(
    analyzer: &mut LuaAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let params = closure
        .get_params_list()?
        .get_params()
        .filter_map(|param| {
            if param.is_dots() {
                Some("...".to_string())
            } else {
                param
                    .get_name_token()
                    .map(|name| name.get_name_text().to_string())
            }
        })
        .collect::<Vec<_>>();

    let block = closure.get_block()?;
    let mut candidates = vec![];
    collect_require_guard_candidates(&block, &params, &mut candidates);

    let param_idx = candidates
        .into_iter()
        .find(|candidate| {
            !is_require_guard_local_mutable(analyzer, candidate.decl_pos)
                && is_require_guard_return_shape(&block, &candidate.guard_name)
        })
        .map(|candidate| candidate.param_idx);

    if let Some(param_idx) = param_idx {
        let signature = analyzer
            .db
            .get_signature_index_mut()
            .get_or_create(*signature_id);
        signature.set_require_guard_param(param_idx);
    }

    Some(())
}

#[derive(Debug, Clone)]
struct RequireGuardCandidate {
    guard_name: String,
    param_idx: usize,
    decl_pos: TextSize,
}

fn collect_require_guard_candidates(
    block: &LuaBlock,
    params: &[String],
    candidates: &mut Vec<RequireGuardCandidate>,
) {
    for stat in block.get_stats() {
        match stat {
            LuaStat::LocalStat(local) => {
                if let Some(candidate) = get_require_guard_from_local_stat(&local, params) {
                    candidates.push(candidate);
                }
            }
            LuaStat::IfStat(if_stat) => {
                collect_require_guard_candidates_from_if(if_stat, params, candidates);
            }
            LuaStat::DoStat(do_stat) => {
                if let Some(block) = do_stat.get_block() {
                    collect_require_guard_candidates(&block, params, candidates);
                }
            }
            LuaStat::WhileStat(while_stat) => {
                if let Some(block) = while_stat.get_block() {
                    collect_require_guard_candidates(&block, params, candidates);
                }
            }
            LuaStat::RepeatStat(repeat_stat) => {
                if let Some(block) = repeat_stat.get_block() {
                    collect_require_guard_candidates(&block, params, candidates);
                }
            }
            LuaStat::ForStat(for_stat) => {
                if let Some(block) = for_stat.get_block() {
                    collect_require_guard_candidates(&block, params, candidates);
                }
            }
            LuaStat::ForRangeStat(for_range_stat) => {
                if let Some(block) = for_range_stat.get_block() {
                    collect_require_guard_candidates(&block, params, candidates);
                }
            }
            _ => {}
        }
    }
}

fn collect_require_guard_candidates_from_if(
    if_stat: LuaIfStat,
    params: &[String],
    candidates: &mut Vec<RequireGuardCandidate>,
) {
    if let Some(block) = if_stat.get_block() {
        collect_require_guard_candidates(&block, params, candidates);
    }
    for else_if in if_stat.get_else_if_clause_list() {
        if let Some(block) = else_if.get_block() {
            collect_require_guard_candidates(&block, params, candidates);
        }
    }
    if let Some(else_clause) = if_stat.get_else_clause() {
        if let Some(block) = else_clause.get_block() {
            collect_require_guard_candidates(&block, params, candidates);
        }
    }
}

fn get_require_guard_from_local_stat(
    local: &LuaLocalStat,
    params: &[String],
) -> Option<RequireGuardCandidate> {
    let mut local_names = local.get_local_name_list();
    let local_name = local_names.next()?;

    let guard_name = local_name.get_name_token()?.get_name_text().to_string();

    let mut value_exprs = local.get_value_exprs();
    let first = value_exprs.next()?;
    let LuaExpr::CallExpr(call_expr) = first else {
        return None;
    };

    let required_param = match_require_call(&call_expr, "pcall", "require")?;
    let param_idx = params.iter().position(|param| param == &required_param)?;

    Some(RequireGuardCandidate {
        guard_name,
        param_idx,
        decl_pos: local_name.get_position(),
    })
}

fn match_require_call(call_expr: &LuaCallExpr, callee: &str, require_fn: &str) -> Option<String> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let prefix_name = expr_name_text(&prefix_expr)?;
    if prefix_name != callee {
        return None;
    }

    let args = call_expr.get_args_list()?;
    let args = args.get_args().collect::<Vec<_>>();
    if args.len() < 2 {
        return None;
    }

    let first_arg = expr_name_text(&args[0])?;
    if first_arg != require_fn {
        return None;
    }

    expr_name_text(&args[1])
}

fn expr_name_text(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_name_text().map(|name| name.to_string()),
        LuaExpr::ParenExpr(paren_expr) => {
            paren_expr.get_expr().and_then(|expr| expr_name_text(&expr))
        }
        _ => None,
    }
}

fn is_require_guard_local_mutable(analyzer: &LuaAnalyzer, local_decl_pos: TextSize) -> bool {
    analyzer
        .db
        .get_reference_index()
        .get_decl_references(
            &analyzer.file_id,
            &LuaDeclId::new(analyzer.file_id, local_decl_pos),
        )
        .is_some_and(|decl_ref| decl_ref.mutable)
}

fn is_require_guard_return_shape(block: &LuaBlock, guard_name: &str) -> bool {
    is_block_return_shape_safe(block, guard_name, false)
}

fn is_block_return_shape_safe(block: &LuaBlock, guard_name: &str, in_guard: bool) -> bool {
    for stat in block.get_stats() {
        match stat {
            LuaStat::ReturnStat(return_stat) => {
                if !is_return_exprs_safe(&return_stat, in_guard, guard_name) {
                    return false;
                }
                return true;
            }
            LuaStat::IfStat(if_stat) => {
                if !is_if_return_shape_safe(if_stat, guard_name, in_guard) {
                    return false;
                }
            }
            LuaStat::DoStat(do_stat) => {
                if let Some(block) = do_stat.get_block() {
                    if !is_block_return_shape_safe(&block, guard_name, in_guard) {
                        return false;
                    }
                }
            }
            LuaStat::WhileStat(while_stat) => {
                if let Some(block) = while_stat.get_block() {
                    if !is_block_return_shape_safe(&block, guard_name, in_guard) {
                        return false;
                    }
                }
            }
            LuaStat::RepeatStat(repeat_stat) => {
                if let Some(block) = repeat_stat.get_block() {
                    if !is_block_return_shape_safe(&block, guard_name, in_guard) {
                        return false;
                    }
                }
            }
            LuaStat::ForStat(for_stat) => {
                if let Some(block) = for_stat.get_block() {
                    if !is_block_return_shape_safe(&block, guard_name, in_guard) {
                        return false;
                    }
                }
            }
            LuaStat::ForRangeStat(for_range_stat) => {
                if let Some(block) = for_range_stat.get_block() {
                    if !is_block_return_shape_safe(&block, guard_name, in_guard) {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    true
}

fn is_if_return_shape_safe(if_stat: LuaIfStat, guard_name: &str, in_guard: bool) -> bool {
    let then_guard = if_stat
        .get_condition_expr()
        .is_some_and(|expr| is_expression_var(&expr, guard_name));

    if let Some(block) = if_stat.get_block() {
        if !is_block_return_shape_safe(&block, guard_name, in_guard || then_guard) {
            return false;
        }
    }

    for else_if in if_stat.get_else_if_clause_list() {
        let else_if_guard = else_if
            .get_condition_expr()
            .is_some_and(|expr| is_expression_var(&expr, guard_name));

        if let Some(block) = else_if.get_block() {
            if !is_block_return_shape_safe(&block, guard_name, in_guard || else_if_guard) {
                return false;
            }
        }
    }

    if let Some(else_clause) = if_stat.get_else_clause() {
        if let Some(block) = else_clause.get_block() {
            if !is_block_return_shape_safe(&block, guard_name, in_guard) {
                return false;
            }
        }
    }

    true
}

fn is_return_exprs_safe(return_stat: &LuaReturnStat, in_guard: bool, guard_name: &str) -> bool {
    let exprs = return_stat.get_expr_list().collect::<Vec<_>>();
    match exprs.len() {
        0 => true,
        1 => is_single_return_expr_safe(&exprs[0], in_guard, guard_name),
        _ => exprs.into_iter().all(|expr| is_false_or_nil_expr(&expr)),
    }
}

fn is_single_return_expr_safe(expr: &LuaExpr, in_guard: bool, guard_name: &str) -> bool {
    if is_false_or_nil_expr(expr) {
        return true;
    }

    if is_expression_var(expr, guard_name) {
        return true;
    }

    if is_true_expr(expr) {
        return in_guard;
    }

    false
}

fn is_false_or_nil_expr(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal() {
            Some(LuaLiteralToken::Nil(_)) => true,
            Some(LuaLiteralToken::Bool(bool_token)) => !bool_token.is_true(),
            _ => false,
        },
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|expr| is_false_or_nil_expr(&expr)),
        _ => false,
    }
}

fn is_true_expr(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => {
            matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Bool(token)) if token.is_true())
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|expr| is_true_expr(&expr)),
        _ => false,
    }
}

fn is_expression_var(expr: &LuaExpr, name: &str) -> bool {
    expr_name_text(expr).is_some_and(|var| var == name)
}

fn analyze_return(
    analyzer: &mut LuaAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let signature = analyzer.db.get_signature_index().get(signature_id)?;
    if signature.resolve_return == SignatureReturnStatus::DocResolve
        && (!signature_has_uninformative_return(signature)
            || closure_has_direct_return_doc(closure))
    {
        return None;
    }

    let parent = closure.get_parent::<LuaAst>()?;
    if let LuaAst::LuaCallArgList(_) = &parent {
        analyze_lambda_returns(analyzer, signature_id, closure);
    };

    let block = match closure.get_block() {
        Some(block) => block,
        None => {
            let signature = analyzer
                .db
                .get_signature_index_mut()
                .get_or_create(*signature_id);
            signature.resolve_return = SignatureReturnStatus::InferResolve;
            signature.set_return_correlations(Vec::new());
            return Some(());
        }
    };

    let return_points = analyze_func_body_returns(block);
    let return_correlations = analyze_return_correlations(
        analyzer.db,
        analyzer
            .context
            .infer_manager
            .get_infer_cache(analyzer.file_id),
        &return_points,
    );
    let returns = match analyze_return_point(
        analyzer.db,
        analyzer
            .context
            .infer_manager
            .get_infer_cache(analyzer.file_id),
        &return_points,
    ) {
        Ok(returns) => returns,
        Err(InferFailReason::None) => {
            vec![LuaDocReturnInfo {
                type_ref: LuaType::Unknown,
                default_value: None,
                description: None,
                name: None,
                attributes: None,
                return_kind: ReturnTypeKind::default(),
            }]
        }
        Err(reason) => {
            let unresolve = UnResolveReturn {
                file_id: analyzer.file_id,
                signature_id: *signature_id,
                return_points,
            };

            analyzer.context.add_unresolve(unresolve.into(), reason);
            return None;
        }
    };
    let signature = analyzer
        .db
        .get_signature_index_mut()
        .get_or_create(*signature_id);

    signature.resolve_return = SignatureReturnStatus::InferResolve;

    signature.return_docs = returns;
    signature.set_return_correlations(return_correlations);

    Some(())
}

fn signature_has_uninformative_return(signature: &crate::LuaSignature) -> bool {
    let return_type = signature.get_return_type();
    return_type.is_any() || return_type.is_unknown()
}

fn closure_has_direct_return_doc(closure: &LuaClosureExpr) -> bool {
    let Some(comment) = closure
        .ancestors::<LuaStat>()
        .next()
        .and_then(|stat| stat.syntax().prev_sibling())
    else {
        return false;
    };

    let kind: LuaSyntaxKind = comment.kind().into();
    if kind != LuaSyntaxKind::Comment {
        return false;
    }

    LuaComment::cast(comment)
        .is_some_and(|comment| comment.children::<LuaDocTagReturn>().next().is_some())
}

fn analyze_lambda_returns(
    analyzer: &mut LuaAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let call_arg_list = closure.get_parent::<LuaCallArgList>()?;
    let call_expr = call_arg_list.get_parent::<LuaCallExpr>()?;
    let pos = closure.get_position();
    let founded_idx = call_arg_list
        .get_args()
        .position(|arg| arg.get_position() == pos)?;
    let block = closure.get_block()?;
    let return_points = analyze_func_body_returns(block);
    let unresolved = UnResolveClosureReturn {
        file_id: analyzer.file_id,
        signature_id: *signature_id,
        call_expr,
        param_idx: founded_idx,
        return_points,
    };

    analyzer
        .context
        .add_unresolve(unresolved.into(), InferFailReason::None);

    Some(())
}

pub fn analyze_return_point(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_points: &Vec<LuaReturnPoint>,
) -> Result<Vec<LuaDocReturnInfo>, InferFailReason> {
    let mut return_type = None;
    for point in return_points {
        match point {
            LuaReturnPoint::Expr(expr) => {
                let expr_type = infer_expr(db, cache, expr.clone())?;
                return_type = Some(match return_type {
                    Some(current) => union_return_expr(db, current, expr_type),
                    None => expr_type,
                });
            }
            LuaReturnPoint::MuliExpr(exprs) => {
                let mut multi_return = vec![];
                for expr in exprs {
                    let expr_type = infer_expr(db, cache, expr.clone())?;
                    multi_return.push(expr_type);
                }
                let typ = LuaType::Variadic(VariadicType::Multi(multi_return).into());
                return_type = Some(match return_type {
                    Some(current) => union_return_expr(db, current, typ),
                    None => typ,
                });
            }
            LuaReturnPoint::Nil => {
                return_type = Some(match return_type {
                    Some(current) => union_return_expr(db, current, LuaType::Nil),
                    None => LuaType::Nil,
                });
            }
            LuaReturnPoint::Error => {}
        }
    }

    Ok(vec![LuaDocReturnInfo {
        type_ref: return_type.unwrap_or(LuaType::Never),
        default_value: None,
        description: None,
        name: None,
        attributes: None,
        return_kind: ReturnTypeKind::default(),
    }])
}

fn analyze_return_correlations(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_points: &[LuaReturnPoint],
) -> Vec<LuaReturnCorrelation> {
    let mut shapes = Vec::new();
    for point in return_points {
        match return_point_shape(db, cache, point) {
            Some(shape) => shapes.push(shape),
            None => return Vec::new(),
        }
    }

    let max_len = shapes.iter().map(Vec::len).max().unwrap_or(0);
    let mut correlations = Vec::new();
    for discriminant_slot in 0..max_len {
        let mut implied_non_nil_slots = Vec::new();
        for implied_slot in 0..max_len {
            if implied_slot == discriminant_slot {
                continue;
            }
            if return_shapes_imply_non_nil_slot(&shapes, discriminant_slot, implied_slot) {
                implied_non_nil_slots.push(implied_slot);
            }
        }
        if !implied_non_nil_slots.is_empty() {
            correlations.push(LuaReturnCorrelation {
                discriminant_slot,
                implied_non_nil_slots,
            });
        }
    }

    correlations
}

fn return_point_shape(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    point: &LuaReturnPoint,
) -> Option<Vec<LuaType>> {
    match point {
        LuaReturnPoint::Nil => Some(vec![LuaType::Nil]),
        LuaReturnPoint::Error => Some(Vec::new()),
        LuaReturnPoint::Expr(expr) => Some(first_class_return_shape(
            infer_expr(db, cache, expr.clone()).ok()?,
        )),
        LuaReturnPoint::MuliExpr(exprs) => {
            let mut shape = Vec::with_capacity(exprs.len());
            for expr in exprs {
                shape.push(infer_expr(db, cache, expr.clone()).ok()?);
            }
            Some(shape)
        }
    }
}

fn first_class_return_shape(typ: LuaType) -> Vec<LuaType> {
    match typ {
        LuaType::Variadic(variadic) => match variadic.deref() {
            VariadicType::Multi(types) => types.clone(),
            VariadicType::Base(base) => vec![base.clone()],
        },
        other => vec![other],
    }
}

fn return_shapes_imply_non_nil_slot(
    shapes: &[Vec<LuaType>],
    discriminant_slot: usize,
    implied_slot: usize,
) -> bool {
    let mut saw_truthy_discriminant = false;
    let mut saw_falsy_discriminant = false;

    for shape in shapes {
        let discriminant = shape.get(discriminant_slot).unwrap_or(&LuaType::Nil);
        if discriminant.is_always_falsy() {
            saw_falsy_discriminant = true;
            continue;
        }
        if !discriminant.is_always_truthy() {
            return false;
        }

        saw_truthy_discriminant = true;
        let implied = shape.get(implied_slot).unwrap_or(&LuaType::Nil);
        if implied.is_optional() {
            return false;
        }
    }

    saw_truthy_discriminant && saw_falsy_discriminant
}

fn union_return_expr(db: &DbIndex, left: LuaType, right: LuaType) -> LuaType {
    match (&left, &right) {
        (LuaType::TableConst(empty), LuaType::Table)
            if table_const_has_no_known_members(db, empty) =>
        {
            LuaType::Table
        }
        (LuaType::Table, LuaType::TableConst(empty))
            if table_const_has_no_known_members(db, empty) =>
        {
            LuaType::Table
        }
        (LuaType::TableConst(empty), LuaType::Any | LuaType::Unknown)
            if table_const_has_no_known_members(db, empty) =>
        {
            right.clone()
        }
        (LuaType::Any | LuaType::Unknown, LuaType::TableConst(empty))
            if table_const_has_no_known_members(db, empty) =>
        {
            left.clone()
        }
        (LuaType::Any, right) if should_union_any_as_unknown(right) => {
            LuaType::from_vec(vec![LuaType::Unknown, right.clone()])
        }
        (left, LuaType::Any) if should_union_any_as_unknown(left) => {
            LuaType::from_vec(vec![left.clone(), LuaType::Unknown])
        }
        (LuaType::Unknown, LuaType::Unknown) => LuaType::Unknown,
        (LuaType::Unknown, _) | (_, LuaType::Unknown) => LuaType::from_vec(vec![left, right]),
        (LuaType::Variadic(left_variadic), LuaType::Variadic(right_variadic)) => {
            match (&left_variadic.deref(), &right_variadic.deref()) {
                (VariadicType::Base(left_base), VariadicType::Base(right_base)) => {
                    let union_base = TypeOps::Union.apply(db, left_base, right_base);
                    LuaType::Variadic(VariadicType::Base(union_base).into())
                }
                (VariadicType::Multi(left_multi), VariadicType::Multi(right_multi)) => {
                    let mut new_multi = vec![];
                    let max_len = left_multi.len().max(right_multi.len());
                    for i in 0..max_len {
                        let left_type = left_multi.get(i).cloned().unwrap_or(LuaType::Nil);
                        let right_type = right_multi.get(i).cloned().unwrap_or(LuaType::Nil);
                        new_multi.push(TypeOps::Union.apply(db, &left_type, &right_type));
                    }
                    LuaType::Variadic(VariadicType::Multi(new_multi).into())
                }
                // difficult to merge the type, use let
                _ => left.clone(),
            }
        }
        (LuaType::Variadic(variadic), _) => {
            let first_type = variadic.get_type(0).cloned().unwrap_or(LuaType::Unknown);
            let first_union_type = union_return_expr(db, first_type, right.clone());

            match variadic.deref() {
                VariadicType::Base(base) => {
                    let union_base = union_return_expr(db, base.clone(), LuaType::Nil);
                    LuaType::Variadic(
                        VariadicType::Multi(vec![
                            first_union_type,
                            LuaType::Variadic(VariadicType::Base(union_base).into()),
                        ])
                        .into(),
                    )
                }
                VariadicType::Multi(multi) => {
                    let mut new_multi = multi.clone();
                    if !new_multi.is_empty() {
                        new_multi[0] = first_union_type;
                        for mult in new_multi.iter_mut().skip(1) {
                            *mult = union_return_expr(db, mult.clone(), LuaType::Nil);
                        }
                    } else {
                        new_multi.push(first_union_type);
                    }

                    LuaType::Variadic(VariadicType::Multi(new_multi).into())
                }
            }
        }
        (_, LuaType::Variadic(variadic)) => {
            let first_type = variadic.get_type(0).cloned().unwrap_or(LuaType::Unknown);
            let first_union_type = union_return_expr(db, left.clone(), first_type);
            match variadic.deref() {
                VariadicType::Base(base) => {
                    let union_base = union_return_expr(db, base.clone(), LuaType::Nil);
                    LuaType::Variadic(
                        VariadicType::Multi(vec![
                            first_union_type,
                            LuaType::Variadic(VariadicType::Base(union_base).into()),
                        ])
                        .into(),
                    )
                }
                VariadicType::Multi(multi) => {
                    let mut new_multi = multi.clone();
                    if !new_multi.is_empty() {
                        new_multi[0] = first_union_type;
                        for mult in new_multi.iter_mut().skip(1) {
                            *mult = union_return_expr(db, mult.clone(), LuaType::Nil);
                        }
                    } else {
                        new_multi.push(first_union_type);
                    }

                    LuaType::Variadic(VariadicType::Multi(new_multi).into())
                }
            }
        }
        _ => TypeOps::Union.apply(db, &left, &right),
    }
}

fn table_const_has_no_known_members(db: &DbIndex, table: &crate::InFiled<TextRange>) -> bool {
    db.get_member_index()
        .get_members(&LuaMemberOwner::Element(table.clone()))
        .is_none_or(|members| members.is_empty())
}

fn should_union_any_as_unknown(typ: &LuaType) -> bool {
    // Broad Boolean joins the Any→Unknown path so real bool branches survive `any | boolean`.
    // Boolean literals stay excluded so `any | false` preserves the original any fallback.
    !matches!(
        typ,
        LuaType::Any
            | LuaType::Unknown
            | LuaType::Nil
            | LuaType::BooleanConst(_)
            | LuaType::DocBooleanConst(_)
    ) && !typ.is_nullable()
}
