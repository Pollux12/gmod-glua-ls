use std::ops::Deref;

use glua_parser::{
    LuaAst, LuaAstNode, LuaBlock, LuaCallArgList, LuaCallExpr, LuaClosureExpr, LuaComment,
    LuaDocTagReturn, LuaExpr, LuaFuncStat, LuaIfStat, LuaLiteralToken, LuaLocalStat, LuaReturnStat,
    LuaStat, LuaSyntaxKind, LuaVarExpr,
};
use rowan::{TextRange, TextSize};

use crate::{
    DbIndex, InferFailReason, LuaDeclId, LuaInferCache, LuaType, ReturnTypeKind,
    SignatureReturnStatus, TypeOps, VariadicType,
    compilation::analyzer::unresolve::{
        UnResolveCallClosureParams, UnResolveClosureReturn, UnResolveParentAst,
        UnResolveParentClosureParams, UnResolveReturn,
    },
    db_index::{LuaDocReturnInfo, LuaMemberOwner, LuaSignatureId},
    infer_expr,
};

use super::{LuaAnalyzer, LuaReturnPoint, func_body::analyze_func_body_returns};

pub fn analyze_closure(analyzer: &mut LuaAnalyzer, closure: LuaClosureExpr) -> Option<()> {
    let signature_id = LuaSignatureId::from_closure(analyzer.file_id, &closure);

    analyze_colon_define(analyzer, &signature_id, &closure);
    analyze_lambda_params(analyzer, &signature_id, &closure);
    analyze_require_guard_param(analyzer, &signature_id, &closure);
    analyze_return(analyzer, &signature_id, &closure);
    Some(())
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
            return Some(());
        }
    };

    let return_points = analyze_func_body_returns(block);
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
            let first_union_type = TypeOps::Union.apply(db, &first_type, &right);

            match variadic.deref() {
                VariadicType::Base(base) => {
                    let union_base = TypeOps::Union.apply(db, base, &LuaType::Nil);
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
                            *mult = TypeOps::Union.apply(db, mult, &LuaType::Nil);
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
            let first_union_type = TypeOps::Union.apply(db, &left, &first_type);
            match variadic.deref() {
                VariadicType::Base(base) => {
                    let union_base = TypeOps::Union.apply(db, base, &LuaType::Nil);
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
                            *mult = TypeOps::Union.apply(db, mult, &LuaType::Nil);
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
    !matches!(typ, LuaType::Any | LuaType::Unknown | LuaType::Nil) && !typ.is_nullable()
}
