use glua_code_analysis::{SemanticModel, is_require_call};
use glua_parser::{LuaAstNode, LuaChunk, LuaExpr, LuaStat};
use lsp_types::{FoldingRange, FoldingRangeKind};
use rowan::TextSize;

use super::builder::FoldingRangeBuilder;

pub fn build_imports_fold_range(
    builder: &mut FoldingRangeBuilder,
    root: LuaChunk,
    semantic_model: &SemanticModel,
) -> Option<()> {
    let root_block = root.get_block()?;

    let mut start: Option<TextSize> = None;
    let mut end: Option<TextSize> = None;
    for stat in root_block.get_stats() {
        if is_require_stat(semantic_model, &stat) {
            let range = stat.get_range();
            if start.is_none() {
                start = Some(range.start());
            }
            end = Some(range.end());
        } else if start.is_some() && end.is_some() {
            let start_pos = start.unwrap();
            let end_pos = end.unwrap();
            let docucment = builder.get_document();
            let start_line_col = docucment.get_line_col(start_pos)?;
            let end_line_col = docucment.get_line_col(end_pos)?;
            let fold_range = FoldingRange {
                start_line: start_line_col.0 as u32,
                start_character: Some(start_line_col.1 as u32),
                end_line: end_line_col.0 as u32,
                end_character: Some(end_line_col.1 as u32),
                kind: Some(FoldingRangeKind::Imports),
                collapsed_text: Some("imports ...".to_string()),
            };
            builder.push(fold_range);
            start = None;
            end = None;
        }
    }

    // if just only require stat, then donot fold it
    Some(())
}

/// Whether `stat` takes a value from a `require`, as `local m = require("m")`
/// or `require("m").setup()` do.
pub(crate) fn is_require_stat(semantic_model: &SemanticModel, stat: &LuaStat) -> bool {
    match stat {
        LuaStat::LocalStat(local_stat) => local_stat
            .get_value_exprs()
            .any(|expr| is_require_expr(semantic_model, expr)),
        LuaStat::AssignStat(assign_stat) => assign_stat
            .get_var_and_expr_list()
            .1
            .into_iter()
            .any(|expr| is_require_expr(semantic_model, expr)),
        LuaStat::CallExprStat(call_expr_stat) => call_expr_stat
            .get_call_expr()
            .is_some_and(|call_expr| is_require_expr(semantic_model, call_expr.into())),
        _ => false,
    }
}

fn is_require_expr(semantic_model: &SemanticModel, expr: LuaExpr) -> bool {
    match expr {
        LuaExpr::CallExpr(call_expr) => {
            is_require_call(
                semantic_model.get_db(),
                semantic_model.get_file_id(),
                &call_expr,
            ) || call_expr
                .get_prefix_expr()
                .is_some_and(|prefix| is_require_expr(semantic_model, prefix))
        }
        LuaExpr::IndexExpr(index_expr) => index_expr
            .get_prefix_expr()
            .is_some_and(|prefix| is_require_expr(semantic_model, prefix)),
        _ => false,
    }
}
