use glua_parser::{
    LuaBlock, LuaCallExpr, LuaCallExprStat, LuaDoStat, LuaExpr, LuaForRangeStat, LuaForStat,
    LuaIfClauseStat, LuaIfStat, LuaRepeatStat, LuaReturnStat, LuaStat, LuaWhileStat,
};

use crate::{DbIndex, FileId};

#[derive(Debug, Clone)]
pub enum LuaReturnPoint {
    Expr(LuaExpr),
    MuliExpr(Vec<LuaExpr>),
    Nil,
    Error(LuaCallExpr),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ChangeFlow {
    None,
    Break,
    Error,
    Return,
}

pub fn analyze_func_body_returns(
    context: Option<(&DbIndex, FileId)>,
    body: LuaBlock,
) -> Vec<LuaReturnPoint> {
    let mut returns = Vec::new();

    let flow = analyze_block_returns(context, body, &mut returns);
    match flow {
        Some(ChangeFlow::Break) | Some(ChangeFlow::None) => {
            returns.push(LuaReturnPoint::Nil);
        }
        _ => {}
    }

    returns
}

fn analyze_block_returns(
    context: Option<(&DbIndex, FileId)>,
    block: LuaBlock,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    for stat in block.get_stats() {
        match stat {
            LuaStat::DoStat(do_stat) => {
                let flow = analyze_do_stat_returns(context, do_stat, returns);
                match flow {
                    Some(ChangeFlow::None) => {}
                    _ => return flow,
                }
            }
            LuaStat::WhileStat(while_stat) => {
                analyze_while_stat_returns(context, while_stat, returns);
            }
            LuaStat::RepeatStat(repeat_stat) => {
                analyze_repeat_stat_returns(context, repeat_stat, returns);
            }
            LuaStat::IfStat(if_stat) => {
                let flow = analyze_if_stat_returns(context, if_stat, returns);
                match flow {
                    Some(ChangeFlow::None) => {}
                    _ => return flow,
                }
            }
            LuaStat::ForStat(for_stat) => {
                analyze_for_stat_returns(context, for_stat, returns);
            }
            LuaStat::ForRangeStat(for_range_stat) => {
                analyze_for_range_stat_returns(context, for_range_stat, returns);
            }
            LuaStat::CallExprStat(call_expr) => {
                let flow = analyze_call_expr_stat_returns(context, call_expr, returns);
                if let Some(ChangeFlow::Error) = flow {
                    return Some(ChangeFlow::Error);
                }
            }
            LuaStat::BreakStat(_) => {
                return Some(ChangeFlow::Break);
            }
            LuaStat::ReturnStat(return_stat) => {
                return analyze_return_stat_returns(return_stat, returns);
            }
            _ => {}
        };
    }

    Some(ChangeFlow::None)
}

fn analyze_do_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    do_stat: LuaDoStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    analyze_block_returns(context, do_stat.get_block()?, returns)
}

fn analyze_while_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    while_stat: LuaWhileStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let flow = analyze_block_returns(context, while_stat.get_block()?, returns);
    match flow {
        Some(ChangeFlow::Break) => Some(ChangeFlow::None),
        _ => flow,
    }
}

fn analyze_repeat_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    repeat_stat: LuaRepeatStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let flow = analyze_block_returns(context, repeat_stat.get_block()?, returns);
    match flow {
        Some(ChangeFlow::Break) => Some(ChangeFlow::None),
        _ => flow,
    }
}

fn analyze_for_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    for_stat: LuaForStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let flow = analyze_block_returns(context, for_stat.get_block()?, returns);
    match flow {
        Some(ChangeFlow::Break) => Some(ChangeFlow::None),
        _ => flow,
    }
}

fn analyze_if_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    if_stat: LuaIfStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let mut branch_flow = |block: Option<LuaBlock>| {
        block
            .and_then(|block| analyze_block_returns(context, block, returns))
            .unwrap_or(ChangeFlow::None)
    };
    let mut flow = branch_flow(if_stat.get_block());
    let mut has_else = false;
    for clause in if_stat.get_all_clause() {
        has_else |= matches!(clause, LuaIfClauseStat::Else(_));
        flow = flow.min(branch_flow(clause.get_block()));
    }

    Some(if has_else { flow } else { ChangeFlow::None })
}

fn analyze_for_range_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    for_range_stat: LuaForRangeStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let flow = analyze_block_returns(context, for_range_stat.get_block()?, returns);
    match flow {
        Some(ChangeFlow::Break) => Some(ChangeFlow::None),
        _ => flow,
    }
}

fn analyze_call_expr_stat_returns(
    context: Option<(&DbIndex, FileId)>,
    call_expr_stat: LuaCallExprStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let call_expr = call_expr_stat.get_call_expr()?;
    if context.is_some_and(|(db, file_id)| crate::calls_intrinsic_error(db, file_id, &call_expr)) {
        returns.push(LuaReturnPoint::Error(call_expr));
        return Some(ChangeFlow::Error);
    }
    Some(ChangeFlow::None)
}

fn analyze_return_stat_returns(
    return_stat: LuaReturnStat,
    returns: &mut Vec<LuaReturnPoint>,
) -> Option<ChangeFlow> {
    let exprs: Vec<LuaExpr> = return_stat.get_expr_list().collect();
    match exprs.len() {
        0 => returns.push(LuaReturnPoint::Nil),
        1 => returns.push(LuaReturnPoint::Expr(exprs[0].clone())),
        _ => returns.push(LuaReturnPoint::MuliExpr(exprs)),
    }

    Some(ChangeFlow::Return)
}
