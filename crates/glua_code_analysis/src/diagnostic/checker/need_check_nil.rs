use glua_parser::{
    BinaryOperator, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaIndexExpr,
    LuaIndexKey,
};

use crate::{DiagnosticCode, LuaType, LuaUnionType, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct NeedCheckNilChecker;

impl Checker for NeedCheckNilChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::NeedCheckNil,
        DiagnosticCode::UncheckedNilAccess,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        for expr in root.descendants::<LuaExpr>() {
            match expr {
                LuaExpr::CallExpr(call_expr) => {
                    check_call_expr(context, semantic_model, call_expr);
                }
                LuaExpr::BinaryExpr(binary_expr) => {
                    check_binary_expr(context, semantic_model, binary_expr);
                }
                LuaExpr::IndexExpr(index_expr) => {
                    check_index_expr(context, semantic_model, index_expr);
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

    // For member/method calls where the resolved callable is non-nullable but the
    // receiver may be nil (e.g. `ent:GetPos()` with `ent: Entity?`), still report
    // nil access at the call prefix. `check_index_expr` skips call prefixes to
    // avoid duplicate diagnostics, so call_expr owns this case.
    if let LuaExpr::IndexExpr(index_expr) = &prefix {
        let receiver = index_expr.get_prefix_expr()?;
        let receiver_type = semantic_model.infer_expr(receiver.clone()).ok()?;
        if receiver_type.is_nullable() {
            // Definite nil receivers should be warning-level unchecked access.
            // Nullable-but-not-definite receivers remain NeedCheckNil.
            let diagnostic_code = if receiver_type.is_nil()
                || should_report_unchecked_nil_access(&receiver, &receiver_type)
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
        }
    }

    Some(())
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
) -> Option<()> {
    // Call prefixes are handled in check_call_expr. Skipping here prevents
    // duplicate diagnostics for call expressions like `test.meow()`.
    if is_call_prefix(&index_expr) {
        return Some(());
    }

    let prefix = index_expr.get_prefix_expr()?;
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

fn is_short_circuit_isvalid_guard_for_receiver(call_expr: &LuaCallExpr, receiver: &LuaExpr) -> bool {
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
