use glua_parser::{
    LuaAst, LuaAstNode, LuaAstToken, LuaBlock, LuaCallExprStat, LuaClosureExpr, LuaGeneralToken,
    LuaIfStat, LuaReturnStat, LuaTokenKind, LuaWhileStat,
};

use crate::{DiagnosticCode, LuaSignatureId, LuaType, SemanticModel, SignatureReturnStatus};

use super::{Checker, DiagnosticContext, get_return_stats};

pub struct CheckReturnCount;

impl Checker for CheckReturnCount {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::RedundantReturnValue,
        DiagnosticCode::MissingReturnValue,
        DiagnosticCode::MissingReturn,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };

        for closure_expr in buckets.closure_exprs.iter() {
            check_missing_return(context, semantic_model, closure_expr);
        }
    }
}

// Get (whether the return values are doc-annotated, the return value types)
fn get_function_return_info(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    closure_expr: &LuaClosureExpr,
) -> Option<(bool, LuaType)> {
    let typ = semantic_model
        .infer_bind_value_type(closure_expr.clone().into())
        .unwrap_or(LuaType::Unknown);

    match typ {
        LuaType::DocFunction(func_type) => {
            return Some((true, func_type.get_ret().clone()));
        }
        LuaType::Signature(signature) => {
            let signature = context.db.get_signature_index().get(&signature)?;
            return Some((
                signature.resolve_return == SignatureReturnStatus::DocResolve,
                signature.get_return_type(),
            ));
        }
        _ => {}
    };

    let signature_id = LuaSignatureId::from_closure(semantic_model.get_file_id(), closure_expr);
    let signature = context.db.get_signature_index().get(&signature_id)?;

    Some((
        signature.resolve_return == SignatureReturnStatus::DocResolve,
        signature.get_return_type(),
    ))
}

fn check_missing_return(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    closure_expr: &LuaClosureExpr,
) -> Option<()> {
    let (is_doc_resolve_return, return_type) =
        get_function_return_info(context, semantic_model, closure_expr)?;

    // Skip the check if the return status is not DocResolve
    if !is_doc_resolve_return {
        return None;
    }

    // Minimum number of return values
    let min_expected_return_count = match &return_type {
        LuaType::Variadic(variadic) => {
            let min_len = variadic.get_min_len()?;
            let mut real_min_len = min_len;
            // Check in reverse order
            if min_len > 0 {
                for i in (0..min_len).rev() {
                    if let Some(ty) = variadic.get_type(i) {
                        if ty.is_optional() {
                            real_min_len -= 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            real_min_len
        }
        LuaType::Nil | LuaType::Any | LuaType::Unknown => 0,
        _ if return_type.is_nullable() => 0,
        _ => 1,
    };

    for return_stat in get_return_stats(closure_expr) {
        check_return_count(
            context,
            semantic_model,
            &return_stat,
            &return_type,
            min_expected_return_count,
        );
    }

    // Detecting a missing return statement needs to handle if and while
    if min_expected_return_count > 0 {
        let range = if let Some(block) = closure_expr.get_block() {
            let result = check_return_block(context, semantic_model, block);
            match result {
                Ok(_) => return Some(()),
                Err(block) => {
                    let token = get_block_end_token(&block)
                        .unwrap_or(block.tokens::<LuaGeneralToken>().last()?);
                    Some(token.get_range())
                }
            }
        } else {
            Some(closure_expr.token_by_kind(LuaTokenKind::TkEnd)?.get_range())
        };
        if let Some(range) = range {
            context.add_diagnostic(
                DiagnosticCode::MissingReturn,
                range,
                "Annotations specify that a return value is required here.".to_string(),
                None,
            );
        }
    }

    Some(())
}

fn get_block_end_token(block: &LuaBlock) -> Option<LuaGeneralToken> {
    let token = block
        .token_by_kind(LuaTokenKind::TkEnd)
        .unwrap_or(LuaAst::cast(block.syntax().parent()?)?.token_by_kind(LuaTokenKind::TkEnd)?);
    Some(token)
}

fn check_return_block(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    block: LuaBlock,
) -> Result<(), LuaBlock> {
    // Check whether a return statement exists
    if block.children::<LuaReturnStat>().count() > 0 {
        return Ok(());
    }

    // Check whether error() is called
    for call_expr_stat in block.children::<LuaCallExprStat>() {
        if let Some(call_expr) = call_expr_stat.get_call_expr()
            && crate::calls_intrinsic_error(context.db, semantic_model.get_file_id(), &call_expr)
        {
            return Ok(());
        }
    }

    // Check `if` and `while` statements
    let has_return = check_if_stat(context, semantic_model, &block)?
        | check_while_stat(context, semantic_model, &block)?;

    if has_return { Ok(()) } else { Err(block) }
}

fn check_if_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    block: &LuaBlock,
) -> Result<bool, LuaBlock> {
    let mut has_return = false;
    for if_stat in block.children::<LuaIfStat>() {
        // Check the main block of the `if`
        if let Some(if_block) = if_stat.get_block() {
            if check_return_block(context, semantic_model, if_block.clone()).is_err() {
                has_return = false;
            }
        } else {
            return Err(block.clone());
        }

        // Check all conditional branches
        for clause in if_stat.get_all_clause() {
            if let Some(clause_block) = clause.get_block() {
                if check_return_block(context, semantic_model, clause_block.clone()).is_err() {
                    has_return = false;
                }
            } else {
                return Err(block.clone());
            }
        }

        // Check whether an `else` branch exists; if so it was already checked above
        if if_stat.get_else_clause().is_some() {
            has_return = true;
        }
    }

    if has_return {
        Ok(has_return)
    } else {
        Err(block.clone())
    }
}

fn check_while_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    block: &LuaBlock,
) -> Result<bool, LuaBlock> {
    let mut has_return = false;
    for while_stat in block.children::<LuaWhileStat>() {
        if let Some(while_block) = while_stat.get_block() {
            check_return_block(context, semantic_model, while_block.clone())?;
        } else {
            return Err(block.clone());
        }

        // Check whether the `while` condition is always true; if so a return statement exists (the sub-block was checked above)
        if is_while_condition_true(semantic_model, &while_stat).is_some() {
            has_return = true;
        }
    }
    Ok(has_return)
}

/// Determines whether the condition expression of a LuaWhileStat is `true`
fn is_while_condition_true(
    semantic_model: &SemanticModel,
    while_stat: &LuaWhileStat,
) -> Option<()> {
    let condition_expr = while_stat.get_condition_expr()?;
    let condition_type = semantic_model
        .infer_expr(condition_expr.clone())
        .unwrap_or(LuaType::Any);
    match condition_type {
        LuaType::BooleanConst(value) => {
            if value {
                Some(())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Checks the number of return values
fn check_return_count(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    return_stat: &LuaReturnStat,
    return_type: &LuaType,
    min_expected_return_count: usize,
) -> Option<()> {
    let max_expected_return_count = match return_type {
        LuaType::Variadic(variadic) => variadic.get_max_len(),
        LuaType::Any | LuaType::Unknown => Some(1),
        LuaType::Nil => Some(0),
        _ => Some(1),
    };

    // Count the expressions actually returned and record the excess range
    let expr_list = return_stat.get_expr_list().collect::<Vec<_>>();
    let mut total_return_count = 0;
    let mut tail_return_nil = false;
    let mut redundant_ranges = Vec::new();

    for (index, expr) in expr_list.iter().enumerate() {
        let expr_type = semantic_model
            .infer_expr(expr.clone())
            .unwrap_or(LuaType::Unknown);
        match expr_type {
            LuaType::Variadic(variadic) => {
                total_return_count += variadic.get_max_len()?;
            }
            LuaType::Nil => {
                if index == expr_list.len() - 1 {
                    tail_return_nil = true;
                }
                total_return_count += 1;
            }
            _ => total_return_count += 1,
        };

        if max_expected_return_count.is_some() && total_return_count > max_expected_return_count? {
            if tail_return_nil && total_return_count - 1 == max_expected_return_count? {
                continue;
            }
            redundant_ranges.push(expr.get_range());
        }
    }

    // Bare `return` (no expressions) is always valid in Lua - it is an early exit returning nil.
    if expr_list.is_empty() {
        return Some(());
    }

    // Check for missing return values
    if total_return_count < min_expected_return_count {
        context.add_diagnostic(
            DiagnosticCode::MissingReturnValue,
            return_stat.get_range(),
            format!(
                "Annotations specify that at least {min} return value(s) are required, found {rmin} returned here instead.",
                min = min_expected_return_count,
                rmin = total_return_count
            )
            .to_string(),
            None,
        );
    }

    // Check for excess return values
    for range in redundant_ranges {
        context.add_diagnostic(
            DiagnosticCode::RedundantReturnValue,
            range,
            format!(
                "Annotations specify that at most {max} return value(s) are required, found {rmax} returned here instead.",
                max = max_expected_return_count?,
                rmax = total_return_count
            )
            .to_string(),
            None,
        );
    }

    Some(())
}
