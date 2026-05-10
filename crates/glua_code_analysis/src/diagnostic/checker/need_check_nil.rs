use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaClosureExpr, LuaExpr,
    LuaIndexExpr, LuaIndexKey,
};
use rowan::TextRange;

use crate::{
    DiagnosticCode, LuaType, LuaUnionType, SemanticModel,
    semantic::contains_gmod_invalid_entity_type,
};

use super::{
    AssignmentPrefixEvents, Checker, DiagnosticContext,
    is_initialized_assignment_prefix as shared_is_initialized_assignment_prefix,
};

pub struct NeedCheckNilChecker;

impl Checker for NeedCheckNilChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::NeedCheckNil,
        DiagnosticCode::UncheckedNilAccess,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let assignment_prefixes = context.get_assignment_prefix_events(&root);
        for expr in root.descendants::<LuaExpr>() {
            match expr {
                LuaExpr::CallExpr(call_expr) => {
                    check_call_expr(context, semantic_model, call_expr);
                }
                LuaExpr::BinaryExpr(binary_expr) => {
                    check_binary_expr(context, semantic_model, binary_expr);
                }
                LuaExpr::IndexExpr(index_expr) => {
                    check_index_expr(context, semantic_model, index_expr, &assignment_prefixes);
                }
                _ => {}
            }
        }
    }
}

fn check_call_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let prefix = call_expr.get_prefix_expr()?;

    if let LuaExpr::IndexExpr(index_expr) = &prefix
        && let Some(receiver) = index_expr.get_prefix_expr()
        && is_short_circuit_isvalid_guard_for_receiver(&call_expr, &receiver)
    {
        return Some(());
    }

    if let LuaExpr::IndexExpr(index_expr) = &prefix {
        let receiver = index_expr.get_prefix_expr()?;
        if report_unsafe_receiver(context, semantic_model, &receiver) {
            return Some(());
        }
    }

    let func = semantic_model.infer_expr(prefix.clone()).ok()?;
    if func.is_nullable() {
        if should_report_unchecked_nil_access(&prefix, &func) {
            context.add_diagnostic(
                DiagnosticCode::UncheckedNilAccess,
                prefix.get_range(),
                t!("%{name} may be nil", name = prefix.syntax().text()).to_string(),
                None,
            );
        } else if !should_skip_deferred_nullable_function_call(&call_expr, &prefix) {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                prefix.get_range(),
                t!("function %{name} may be nil", name = prefix.syntax().text()).to_string(),
                None,
            );
        }

        return Some(());
    }

    Some(())
}

fn report_unsafe_receiver(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    receiver: &LuaExpr,
) -> bool {
    let Ok(receiver_type) = semantic_model.infer_expr(receiver.clone()) else {
        return false;
    };
    if receiver_type.is_nullable() {
        // Definite nil receivers should be warning-level unchecked access.
        // Nullable-but-not-definite receivers remain NeedCheckNil.
        let diagnostic_code = if receiver_type.is_nil()
            || should_report_unchecked_nil_access(receiver, &receiver_type)
        {
            DiagnosticCode::UncheckedNilAccess
        } else {
            DiagnosticCode::NeedCheckNil
        };

        context.add_diagnostic(
            diagnostic_code,
            receiver.get_range(),
            t!("%{name} may be nil", name = receiver.syntax().text()).to_string(),
            None,
        );
        return true;
    }

    if !contains_gmod_invalid_entity_type(semantic_model.get_db(), &receiver_type) {
        return false;
    }

    context.add_diagnostic(
        DiagnosticCode::NeedCheckNil,
        receiver.get_range(),
        t!(
            "%{name} may be an invalid entity; check IsValid before calling Entity methods",
            name = receiver.syntax().text()
        )
        .to_string(),
        None,
    );
    true
}

fn should_skip_deferred_nullable_function_call(call_expr: &LuaCallExpr, prefix: &LuaExpr) -> bool {
    let in_closure = call_expr
        .syntax()
        .ancestors()
        .skip(1)
        .any(|node| LuaClosureExpr::cast(node).is_some());
    if !in_closure {
        return false;
    }

    matches!(prefix, LuaExpr::NameExpr(_))
}

fn check_index_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: LuaIndexExpr,
    assignment_prefixes: &AssignmentPrefixEvents,
) -> Option<()> {
    // Call prefixes are handled in check_call_expr. Skipping here prevents
    // duplicate diagnostics for call expressions like `test.meow()`.
    if is_call_prefix(&index_expr) {
        return Some(());
    }

    let prefix = index_expr.get_prefix_expr()?;
    if is_initialized_assignment_lhs_prefix(&index_expr, &prefix, assignment_prefixes) {
        return Some(());
    }

    let prefix_type = semantic_model.infer_expr(prefix.clone()).ok()?;
    if prefix_type.is_nullable() {
        let diagnostic_code = if should_report_unchecked_nil_access(&prefix, &prefix_type) {
            DiagnosticCode::UncheckedNilAccess
        } else {
            DiagnosticCode::NeedCheckNil
        };

        context.add_diagnostic(
            diagnostic_code,
            prefix.get_range(),
            t!("%{name} may be nil", name = prefix.syntax().text()).to_string(),
            None,
        );
    }

    Some(())
}

fn is_initialized_assignment_lhs_prefix(
    index_expr: &LuaIndexExpr,
    _prefix: &LuaExpr,
    assignment_prefixes: &AssignmentPrefixEvents,
) -> bool {
    let Some(assign_stat) = index_expr
        .syntax()
        .ancestors()
        .find_map(LuaAssignStat::cast)
    else {
        return false;
    };

    if !is_index_expr_in_assign_lhs(index_expr, &assign_stat) {
        return false;
    }

    shared_is_initialized_assignment_prefix(index_expr, &assign_stat, assignment_prefixes)
}

fn is_index_expr_in_assign_lhs(index_expr: &LuaIndexExpr, assign_stat: &LuaAssignStat) -> bool {
    let index_range = index_expr.syntax().text_range();
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter()
        .any(|var| range_contains(var.syntax().text_range(), index_range))
}

fn range_contains(outer: TextRange, inner: TextRange) -> bool {
    outer.start() <= inner.start() && outer.end() >= inner.end()
}

/// Returns `true` when `index_expr` is the direct prefix of a `CallExpr`.
/// In that case, nil diagnostics for the call are owned by `check_call_expr`.
fn is_call_prefix(index_expr: &LuaIndexExpr) -> bool {
    let Some(call) = index_expr.get_parent::<LuaCallExpr>() else {
        return false;
    };
    let Some(call_prefix) = call.get_prefix_expr() else {
        return false;
    };
    // Only suppress when this IndexExpr IS the call's prefix (not an arg, etc.)
    call_prefix.syntax() == index_expr.syntax()
}

fn is_short_circuit_isvalid_guard_for_receiver(
    call_expr: &LuaCallExpr,
    receiver: &LuaExpr,
) -> bool {
    let Some(binary_expr) = call_expr.get_parent::<LuaBinaryExpr>() else {
        return false;
    };

    let Some(op) = binary_expr.get_op_token().map(|token| token.get_op()) else {
        return false;
    };
    if op != BinaryOperator::OpAnd {
        return false;
    }

    let Some((left, right)) = binary_expr.get_exprs() else {
        return false;
    };
    if right.syntax() != call_expr.syntax() {
        return false;
    }

    match left {
        LuaExpr::CallExpr(guard_call) => is_isvalid_call_guarding_expr(&guard_call, receiver),
        _ => false,
    }
}

fn is_isvalid_call_guarding_expr(guard_call: &LuaCallExpr, receiver: &LuaExpr) -> bool {
    let Some(prefix) = guard_call.get_prefix_expr() else {
        return false;
    };

    match prefix {
        LuaExpr::NameExpr(name_expr) => {
            if name_expr.get_name_text().as_deref() != Some("IsValid") {
                return false;
            }

            let Some(args_list) = guard_call.get_args_list() else {
                return false;
            };
            let Some(first_arg) = args_list.get_args().next() else {
                return false;
            };
            first_arg.syntax() == receiver.syntax()
        }
        LuaExpr::IndexExpr(index_expr) => {
            if !guard_call.is_colon_call() {
                return false;
            }

            let is_isvalid_method = match index_expr.get_index_key() {
                Some(LuaIndexKey::Name(name_token)) => name_token.get_name_text() == "IsValid",
                _ => false,
            };
            if !is_isvalid_method {
                return false;
            }

            let Some(self_expr) = index_expr.get_prefix_expr() else {
                return false;
            };
            self_expr.syntax() == receiver.syntax()
        }
        _ => false,
    }
}

fn should_report_unchecked_nil_access(prefix_expr: &LuaExpr, prefix_type: &LuaType) -> bool {
    matches!(prefix_expr, LuaExpr::IndexExpr(_)) && is_opaque_nullable_any(prefix_type)
}

fn is_opaque_nullable_any(ty: &LuaType) -> bool {
    let LuaType::Union(union) = ty else {
        return false;
    };

    matches!(union.as_ref(), LuaUnionType::Nullable(LuaType::Any))
}

fn check_binary_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    binary_expr: LuaBinaryExpr,
) -> Option<()> {
    let op = binary_expr.get_op_token()?.get_op();
    if matches!(
        op,
        BinaryOperator::OpAdd
            | BinaryOperator::OpSub
            | BinaryOperator::OpMul
            | BinaryOperator::OpDiv
            | BinaryOperator::OpMod
    ) {
        let (left, right) = binary_expr.get_exprs()?;
        let left_type = semantic_model.infer_expr(left.clone()).ok()?;

        if left_type.is_nullable() {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                left.get_range(),
                t!("%{name} value may be nil", name = left.syntax().text()).to_string(),
                None,
            );
        }

        let right_type = semantic_model.infer_expr(right.clone()).ok()?;
        if right_type.is_nullable() {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                right.get_range(),
                t!("%{name} value may be nil", name = right.syntax().text()).to_string(),
                None,
            );
        }
    }

    Some(())
}
