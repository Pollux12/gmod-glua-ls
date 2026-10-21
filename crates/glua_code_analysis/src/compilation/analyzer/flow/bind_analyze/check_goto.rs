use glua_parser::{LuaAstNode, LuaAstToken, LuaBlock, LuaClosureExpr, LuaSyntaxId};

use crate::{AnalyzeError, DiagnosticCode, compilation::analyzer::flow::binder::FlowBinder};

/// Joins each `goto` to the label it names: the one declared by the nearest
/// block around it, within its function. A label written after the `goto`
/// takes its state; one before it takes none, as a loop's head takes no state
/// from its body's end.
pub fn check_goto_label(binder: &mut FlowBinder) {
    let goto_stat_caches = binder.get_goto_caches();
    for goto_stat_cache in goto_stat_caches {
        let label_token = goto_stat_cache.label_token;
        let label_name = goto_stat_cache.label.as_str();
        let label = label_token
            .syntax()
            .parent_ancestors()
            .take_while(|node| !LuaClosureExpr::can_cast(node.kind().into()))
            .filter_map(LuaBlock::cast)
            .find_map(|block| binder.get_label(LuaSyntaxId::from_node(block.syntax()), label_name));
        match label {
            Some((label, position)) => {
                if position > label_token.get_position() {
                    binder.add_antecedent(label, goto_stat_cache.flow_id);
                }
            }
            None => binder.report_error(AnalyzeError::new(
                DiagnosticCode::SyntaxError,
                &format!(
                    "goto label '{label_name}' not found",
                    label_name = label_name
                ),
                label_token.get_range(),
            )),
        }
    }
}
