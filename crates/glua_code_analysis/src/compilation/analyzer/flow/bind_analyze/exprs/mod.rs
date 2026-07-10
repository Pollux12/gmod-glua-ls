mod bind_binary_expr;

use glua_parser::{
    LuaAst, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaIndexExpr, LuaNameExpr,
    LuaTableExpr, LuaUnaryExpr, UnaryOperator,
};

use crate::{
    FlowId, FlowNodeKind,
    compilation::analyzer::flow::{
        bind_analyze::{bind_each_child, exprs::bind_binary_expr::is_binary_logical},
        binder::FlowBinder,
    },
};
pub use bind_binary_expr::bind_binary_expr;

pub fn bind_condition_expr(
    binder: &mut FlowBinder,
    condition_expr: LuaExpr,
    current: FlowId,
    true_target: FlowId,
    false_target: FlowId,
) {
    let old_true_target = binder.true_target;
    let old_false_target = binder.false_target;

    // Condition expressions can narrow any variable they mention; record all
    // referenced names / index paths so the flow-walk skip stays sound.
    binder.record_narrowable_refs_in_expr(&condition_expr);

    binder.true_target = true_target;
    binder.false_target = false_target;
    bind_expr(binder, condition_expr.clone(), current);
    binder.true_target = old_true_target;
    binder.false_target = old_false_target;

    if !is_binary_logical(&condition_expr) {
        let true_condition =
            binder.create_node(FlowNodeKind::TrueCondition(condition_expr.to_ptr()));
        binder.add_antecedent(true_condition, current);
        binder.add_antecedent(true_target, true_condition);

        let false_condition =
            binder.create_node(FlowNodeKind::FalseCondition(condition_expr.to_ptr()));
        binder.add_antecedent(false_condition, current);
        binder.add_antecedent(false_target, false_condition);
    }
}

pub fn bind_expr(binder: &mut FlowBinder, expr: LuaExpr, current: FlowId) -> FlowId {
    match expr {
        LuaExpr::NameExpr(name_expr) => bind_name_expr(binder, name_expr, current),
        LuaExpr::CallExpr(call_expr) => bind_call_expr(binder, call_expr, current),
        LuaExpr::TableExpr(table_expr) => bind_table_expr(binder, table_expr, current),
        LuaExpr::LiteralExpr(_) => Some(()), // Literal expressions do not need binding
        LuaExpr::ClosureExpr(closure_expr) => bind_closure_expr(binder, closure_expr, current),
        LuaExpr::ParenExpr(paren_expr) => bind_paren_expr(binder, paren_expr, current),
        LuaExpr::IndexExpr(index_expr) => bind_index_expr(binder, index_expr, current),
        LuaExpr::BinaryExpr(binary_expr) => bind_binary_expr(binder, binary_expr, current),
        LuaExpr::UnaryExpr(unary_expr) => bind_unary_expr(binder, unary_expr, current),
    };

    current
}

pub fn bind_name_expr(
    binder: &mut FlowBinder,
    name_expr: LuaNameExpr,
    current: FlowId,
) -> Option<()> {
    binder.bind_syntax_node(name_expr.get_syntax_id(), current);
    Some(())
}

pub fn bind_table_expr(
    binder: &mut FlowBinder,
    table_expr: LuaTableExpr,
    current: FlowId,
) -> Option<()> {
    bind_each_child(binder, LuaAst::LuaTableExpr(table_expr), current);
    Some(())
}

pub fn bind_closure_expr(
    binder: &mut FlowBinder,
    closure_expr: LuaClosureExpr,
    current: FlowId,
) -> Option<()> {
    let entry = binder.create_node(FlowNodeKind::ClosureEntry(closure_expr.get_position()));
    binder.add_antecedent(entry, current);

    let old_loop = binder.loop_label;
    let old_break = binder.break_target_label;
    let old_true = binder.true_target;
    let old_false = binder.false_target;
    binder.loop_label = binder.unreachable;
    binder.break_target_label = binder.unreachable;
    binder.true_target = binder.unreachable;
    binder.false_target = binder.unreachable;

    bind_each_child(binder, LuaAst::LuaClosureExpr(closure_expr), entry);

    binder.loop_label = old_loop;
    binder.break_target_label = old_break;
    binder.true_target = old_true;
    binder.false_target = old_false;
    Some(())
}

pub fn bind_index_expr(
    binder: &mut FlowBinder,
    index_expr: LuaIndexExpr,
    current: FlowId,
) -> Option<()> {
    binder.bind_syntax_node(index_expr.get_syntax_id(), current);
    bind_each_child(binder, LuaAst::LuaIndexExpr(index_expr.clone()), current);
    Some(())
}

pub fn bind_paren_expr(
    binder: &mut FlowBinder,
    paren_expr: glua_parser::LuaParenExpr,
    current: FlowId,
) -> Option<()> {
    let inner_expr = paren_expr.get_expr()?;

    bind_expr(binder, inner_expr, current);
    Some(())
}

pub fn bind_unary_expr(
    binder: &mut FlowBinder,
    unary_expr: LuaUnaryExpr,
    current: FlowId,
) -> Option<()> {
    let inner_expr = unary_expr.get_expr()?;

    if unary_expr
        .get_op_token()
        .is_some_and(|op| matches!(op.get_op(), UnaryOperator::OpNot))
    {
        let old_true_target = binder.true_target;
        let old_false_target = binder.false_target;
        binder.true_target = old_false_target;
        binder.false_target = old_true_target;
        bind_expr(binder, inner_expr, current);
        binder.true_target = old_true_target;
        binder.false_target = old_false_target;
        return Some(());
    }

    bind_expr(binder, inner_expr, current);
    Some(())
}

pub fn bind_call_expr(
    binder: &mut FlowBinder,
    call_expr: LuaCallExpr,
    current: FlowId,
) -> Option<()> {
    bind_each_child(binder, LuaAst::LuaCallExpr(call_expr.clone()), current);
    Some(())
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaAstNode, LuaClosureExpr, LuaParser, ParserConfig};

    use super::*;
    use crate::{DbIndex, FileId};

    #[test]
    fn closure_children_do_not_attach_to_enclosing_control_targets() {
        let parser = LuaParser::parse(
            r#"
            local callback = function()
                result = left and right
                break
            end
            "#,
            ParserConfig::default(),
        );
        let closure = parser
            .get_chunk_node()
            .descendants::<LuaClosureExpr>()
            .next()
            .expect("closure expression");
        let db = DbIndex::new();
        let mut binder = FlowBinder::new(&db, FileId::new(1));
        let outer_loop = binder.create_loop_label();
        let outer_break = binder.create_branch_label();
        let outer_true = binder.create_branch_label();
        let outer_false = binder.create_branch_label();
        binder.loop_label = outer_loop;
        binder.break_target_label = outer_break;
        binder.true_target = outer_true;
        binder.false_target = outer_false;

        let start = binder.start;
        bind_closure_expr(&mut binder, closure, start).expect("closure expression should bind");

        assert_eq!(
            [
                binder.loop_label,
                binder.break_target_label,
                binder.true_target,
                binder.false_target,
            ],
            [outer_loop, outer_break, outer_true, outer_false]
        );

        let (tree, errors) = binder.finish();
        let outer_targets_have_antecedents =
            [outer_break, outer_true, outer_false].map(|flow_id| {
                tree.get_flow_node(flow_id)
                    .is_some_and(|node| node.antecedent.is_some())
            });

        assert_eq!(outer_targets_have_antecedents, [false, false, false]);
        assert_eq!(
            errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>(),
            vec!["Break outside loop"]
        );
    }
}
