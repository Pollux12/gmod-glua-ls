use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaClosureExpr,
    LuaElseIfClauseStat, LuaExpr, LuaIfStat, LuaIndexExpr, LuaIndexKey, LuaNameExpr, LuaRepeatStat,
    LuaSyntaxKind, LuaSyntaxNode, LuaWhileStat, UnaryOperator,
};
use rowan::TextRange;

use crate::{
    DiagnosticCode, InferFailReason, LuaMemberKey, LuaMemberOwner, LuaType, LuaUnionType,
    SemanticModel,
    semantic::{contains_gmod_null_type, get_var_expr_var_ref_id},
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
        DiagnosticCode::GmodNullCheck,
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

        for if_stat in root.descendants::<LuaIfStat>() {
            if let Some(condition) = if_stat.get_condition_expr() {
                check_condition_expr(context, semantic_model, condition);
            }
        }

        for elseif_stat in root.descendants::<LuaElseIfClauseStat>() {
            if let Some(condition) = elseif_stat.get_condition_expr() {
                check_condition_expr(context, semantic_model, condition);
            }
        }

        for while_stat in root.descendants::<LuaWhileStat>() {
            if let Some(condition) = while_stat.get_condition_expr() {
                check_condition_expr(context, semantic_model, condition);
            }
        }

        for repeat_stat in root.descendants::<LuaRepeatStat>() {
            if let Some(condition) = repeat_stat.get_condition_expr() {
                check_condition_expr(context, semantic_model, condition);
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

    let is_receiver_guarded_by_type_guard = if let LuaExpr::IndexExpr(index_expr) = &prefix
        && let Some(receiver) = index_expr.get_prefix_expr()
        && is_short_circuit_type_guard_for_receiver(semantic_model, &call_expr, &receiver)
    {
        true
    } else {
        false
    };

    if let LuaExpr::IndexExpr(index_expr) = &prefix
        && let Some(receiver) = index_expr.get_prefix_expr()
        && is_short_circuit_isvalid_guard_for_receiver(&call_expr, &receiver)
    {
        return Some(());
    }

    if let LuaExpr::IndexExpr(index_expr) = &prefix {
        let receiver = index_expr.get_prefix_expr()?;
        if !is_receiver_guarded_by_type_guard
            && report_unsafe_receiver(context, semantic_model, &receiver)
        {
            return Some(());
        }
    }

    let func = semantic_model.infer_expr(prefix.clone()).ok()?;
    if func.is_nullable() {
        if should_report_unchecked_nil_access(&prefix, &func) {
            context.add_diagnostic(
                DiagnosticCode::UncheckedNilAccess,
                prefix.get_range(),
                format!("{name} may be nil", name = prefix.syntax().text()).to_string(),
                None,
            );
        } else if nullable_callable_is_from_non_nullable_receiver(semantic_model, &prefix) {
            return Some(());
        } else if !should_skip_deferred_nullable_function_call(&call_expr, &prefix) {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                prefix.get_range(),
                format!("function {name} may be nil", name = prefix.syntax().text()).to_string(),
                None,
            );
        }

        return Some(());
    }

    Some(())
}

fn nullable_callable_is_from_non_nullable_receiver(
    semantic_model: &SemanticModel,
    prefix: &LuaExpr,
) -> bool {
    let LuaExpr::IndexExpr(index_expr) = prefix else {
        return false;
    };
    let Some(receiver) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Ok(receiver_type) = semantic_model.infer_expr(receiver) else {
        return false;
    };

    if index_expr_has_explicit_nullable_member(semantic_model, index_expr, receiver_type.clone()) {
        return false;
    }

    !receiver_type.is_nullable()
        && !contains_gmod_null_type(semantic_model.get_db(), &receiver_type)
}

fn index_expr_has_explicit_nullable_member(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    receiver_type: LuaType,
) -> bool {
    let Some(owner) = member_owner_for_type(receiver_type) else {
        return false;
    };
    let Some(key) = literal_member_key(index_expr) else {
        return false;
    };

    let db = semantic_model.get_db();
    let Some(member_item) = db.get_member_index().get_member_item(&owner, &key) else {
        return false;
    };
    member_item
        .resolve_type_with_realm_at_offset(
            db,
            &semantic_model.get_file_id(),
            index_expr.get_position(),
        )
        .is_ok_and(|member_type| member_type.is_nullable())
}

fn report_unsafe_receiver(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    receiver: &LuaExpr,
) -> bool {
    let receiver_type = match semantic_model.infer_expr(receiver.clone()) {
        Ok(receiver_type) => receiver_type,
        // During diagnostics, unresolved fields keep a `FieldNotFound` error to avoid
        // mutating caches globally. For nil-access checks on method receivers we still
        // need runtime semantics: a missing field read evaluates to `nil`.
        Err(InferFailReason::FieldNotFound) if matches!(receiver, LuaExpr::IndexExpr(_)) => {
            LuaType::Nil
        }
        Err(_) => return false,
    };
    if receiver_type.is_nullable() {
        if is_expr_guarded_by_prior_isvalid_early_return(semantic_model, receiver) {
            return false;
        }

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
            format!("{name} may be nil", name = receiver.syntax().text()).to_string(),
            None,
        );
        return true;
    }

    if !contains_gmod_null_type(semantic_model.get_db(), &receiver_type) {
        return false;
    }

    if is_expr_guarded_by_prior_isvalid_early_return(semantic_model, receiver) {
        return false;
    }

    context.add_diagnostic(
        DiagnosticCode::NeedCheckNil,
        receiver.get_range(),
        format!(
            "{name} may be NULL; check IsValid before calling Entity methods",
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
        if !prefix_type.is_nil()
            && let LuaExpr::IndexExpr(prefix_index_expr) = &prefix
            && index_expr_has_non_nullable_current_member(semantic_model, prefix_index_expr)
        {
            return Some(());
        }

        if is_expr_guarded_by_prior_isvalid_early_return(semantic_model, &prefix) {
            return Some(());
        }

        let diagnostic_code = if should_report_unchecked_nil_access(&prefix, &prefix_type) {
            DiagnosticCode::UncheckedNilAccess
        } else {
            DiagnosticCode::NeedCheckNil
        };

        context.add_diagnostic(
            diagnostic_code,
            prefix.get_range(),
            format!("{name} may be nil", name = prefix.syntax().text()).to_string(),
            None,
        );
    }

    Some(())
}

fn index_expr_has_non_nullable_current_member(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Ok(prefix_type) = semantic_model.infer_expr(prefix_expr) else {
        return false;
    };
    let Some(owner) = member_owner_for_type(prefix_type) else {
        return false;
    };
    let Some(key) = literal_member_key(index_expr) else {
        return false;
    };

    let db = semantic_model.get_db();
    let Some(member_item) = db.get_member_index().get_member_item(&owner, &key) else {
        return false;
    };
    let Ok(member_type) = member_item.resolve_type_with_realm_at_offset(
        db,
        &semantic_model.get_file_id(),
        index_expr.get_position(),
    ) else {
        return false;
    };

    !member_type.is_nullable()
}

fn member_owner_for_type(typ: LuaType) -> Option<LuaMemberOwner> {
    match typ {
        LuaType::TableConst(in_file_range) => Some(LuaMemberOwner::Element(in_file_range)),
        LuaType::Def(def_id) | LuaType::Ref(def_id) => Some(LuaMemberOwner::Type(def_id)),
        LuaType::Instance(instance) => Some(LuaMemberOwner::Element(instance.get_range().clone())),
        _ => None,
    }
}

fn literal_member_key(index_expr: &LuaIndexExpr) -> Option<LuaMemberKey> {
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(LuaMemberKey::Name(name.get_name_text().into())),
        LuaIndexKey::String(string) => Some(LuaMemberKey::Name(string.get_value().into())),
        _ => None,
    }
}

fn is_expr_guarded_by_prior_isvalid_early_return(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    let Some(containing_stat) = expr.syntax().ancestors().find(|node| {
        let kind: LuaSyntaxKind = node.kind().into();
        matches!(
            kind,
            LuaSyntaxKind::LocalStat
                | LuaSyntaxKind::AssignStat
                | LuaSyntaxKind::CallExprStat
                | LuaSyntaxKind::IfStat
                | LuaSyntaxKind::ReturnStat
        )
    }) else {
        return false;
    };

    let Some(parent) = containing_stat.parent() else {
        return false;
    };
    let stat_start = containing_stat.text_range().start();

    for sibling in parent.children() {
        if sibling.text_range().start() >= stat_start {
            break;
        }

        let kind: LuaSyntaxKind = sibling.kind().into();
        if kind != LuaSyntaxKind::IfStat {
            continue;
        }

        let Some(if_stat) = LuaIfStat::cast(sibling) else {
            continue;
        };
        if !if_body_has_return(&if_stat) {
            continue;
        }
        let Some(condition) = if_stat.get_condition_expr() else {
            continue;
        };
        if condition_is_negative_isvalid_guard(semantic_model, &condition, expr)
            && !guard_continuing_clauses_reassign_guarded_expr(semantic_model, expr, &if_stat)
            && !guarded_expr_reassigned_between(
                semantic_model,
                expr,
                &parent,
                if_stat.syntax().text_range(),
                stat_start,
            )
        {
            return true;
        }
    }

    false
}

fn guard_continuing_clauses_reassign_guarded_expr(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    if_stat: &LuaIfStat,
) -> bool {
    for elseif_clause in if_stat.get_else_if_clause_list() {
        if elseif_clause.get_block().is_some_and(|block| {
            block.descendants::<LuaAssignStat>().any(|assign_stat| {
                assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, guarded_expr)
            })
        }) {
            return true;
        }
    }

    if if_stat.get_else_clause().is_some_and(|else_clause| {
        else_clause.get_block().is_some_and(|block| {
            block.descendants::<LuaAssignStat>().any(|assign_stat| {
                assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, guarded_expr)
            })
        })
    }) {
        return true;
    }

    false
}

fn guarded_expr_reassigned_between(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    parent: &LuaSyntaxNode,
    guard_range: TextRange,
    stat_start: rowan::TextSize,
) -> bool {
    for sibling in parent.children() {
        let sibling_range = sibling.text_range();
        if sibling_range.start() <= guard_range.end() {
            continue;
        }
        if sibling_range.start() >= stat_start {
            break;
        }

        if let Some(assign_stat) = LuaAssignStat::cast(sibling.clone())
            && assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, guarded_expr)
        {
            return true;
        }

        if sibling.descendants().any(|node| {
            LuaAssignStat::cast(node).is_some_and(|assign_stat| {
                assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, guarded_expr)
            })
        }) {
            return true;
        }
    }

    false
}

fn assign_stat_reassigns_guarded_expr(
    semantic_model: &SemanticModel,
    assign_stat: &LuaAssignStat,
    guarded_expr: &LuaExpr,
) -> bool {
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter().any(|var| {
        assigned_expr_invalidates_guarded_expr(semantic_model, &var.to_expr(), guarded_expr)
    })
}

fn assigned_expr_invalidates_guarded_expr(
    semantic_model: &SemanticModel,
    assigned_expr: &LuaExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    if exprs_reference_same_var(semantic_model, assigned_expr, guarded_expr) {
        return true;
    }

    let mut current = guarded_expr.clone();
    while let LuaExpr::IndexExpr(index_expr) = current {
        let Some(prefix) = index_expr.get_prefix_expr() else {
            return false;
        };
        if exprs_reference_same_var(semantic_model, assigned_expr, &prefix) {
            return true;
        }
        current = prefix;
    }

    false
}

fn if_body_has_return(if_stat: &LuaIfStat) -> bool {
    if_stat.get_block().is_some_and(|block| {
        block
            .syntax()
            .children()
            .any(|child| LuaSyntaxKind::from(child.kind()) == LuaSyntaxKind::ReturnStat)
    })
}

fn condition_is_negative_isvalid_guard(
    semantic_model: &SemanticModel,
    condition: &LuaExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    match condition {
        LuaExpr::UnaryExpr(unary_expr) => {
            if !unary_expr
                .get_op_token()
                .is_some_and(|token| token.get_op() == UnaryOperator::OpNot)
            {
                return false;
            }
            let Some(inner_expr) = unary_expr.get_expr() else {
                return false;
            };
            match inner_expr {
                LuaExpr::CallExpr(call_expr) => {
                    is_isvalid_call_guarding_var(semantic_model, &call_expr, guarded_expr)
                }
                LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
                    condition_is_negative_isvalid_guard(semantic_model, &expr, guarded_expr)
                }),
                _ => false,
            }
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let Some(op) = binary_expr.get_op_token().map(|token| token.get_op()) else {
                return false;
            };
            if op != BinaryOperator::OpOr {
                return false;
            }
            let Some((left, right)) = binary_expr.get_exprs() else {
                return false;
            };
            condition_is_negative_isvalid_guard(semantic_model, &left, guarded_expr)
                || condition_is_negative_isvalid_guard(semantic_model, &right, guarded_expr)
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
            condition_is_negative_isvalid_guard(semantic_model, &expr, guarded_expr)
        }),
        _ => false,
    }
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
    let Some(guard_call) = short_circuit_left_call_for_right_call(call_expr) else {
        return false;
    };

    is_isvalid_call_guarding_expr(&guard_call, receiver)
}

fn is_short_circuit_type_guard_for_receiver(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    receiver: &LuaExpr,
) -> bool {
    let Some(guard_call) = short_circuit_left_call_for_right_call(call_expr) else {
        return false;
    };

    is_type_guard_call_guarding_expr(semantic_model, &guard_call, receiver)
}

fn short_circuit_left_call_for_right_call(call_expr: &LuaCallExpr) -> Option<LuaCallExpr> {
    let binary_expr = call_expr.get_parent::<LuaBinaryExpr>()?;

    let op = binary_expr.get_op_token().map(|token| token.get_op())?;
    if op != BinaryOperator::OpAnd {
        return None;
    }

    let (left, right) = binary_expr.get_exprs()?;
    if right.syntax() != call_expr.syntax() {
        return None;
    }

    match left {
        LuaExpr::CallExpr(guard_call) => Some(guard_call),
        _ => None,
    }
}

fn is_type_guard_call_guarding_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    receiver: &LuaExpr,
) -> bool {
    if !call_returns_non_nullable_type_guard(semantic_model, guard_call) {
        return false;
    };

    let Some(first_arg) = guard_call
        .get_args_list()
        .and_then(|args| args.get_args().next())
    else {
        return false;
    };

    exprs_reference_same_var(semantic_model, &first_arg, receiver)
}

fn call_returns_non_nullable_type_guard(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
) -> bool {
    match semantic_model.infer_expr(LuaExpr::CallExpr(guard_call.clone())) {
        Ok(LuaType::TypeGuard(inner)) => !inner.is_nullable(),
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
    let (left, right) = binary_expr.get_exprs()?;

    if matches!(op, BinaryOperator::OpEq | BinaryOperator::OpNe) {
        if let Some(non_nil_side) = get_null_nil_comparison_operand(semantic_model, &left, &right) {
            if is_nil_comparison_guarded_by_isvalid(semantic_model, &binary_expr, non_nil_side, op)
            {
                return Some(());
            }

            context.add_diagnostic(
                DiagnosticCode::GmodNullCheck,
                binary_expr.get_range(),
                format!(
                    "{name} may be NULL; comparing to nil does not prove entity validity, use IsValid(...) instead",
                    name = non_nil_side.syntax().text()
                )
                .to_string(),
                None,
            );
            return Some(());
        }
    }

    if matches!(
        op,
        BinaryOperator::OpAdd
            | BinaryOperator::OpSub
            | BinaryOperator::OpMul
            | BinaryOperator::OpDiv
            | BinaryOperator::OpMod
    ) {
        let left_type = semantic_model.infer_expr(left.clone()).ok()?;

        if left_type.is_nullable() {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                left.get_range(),
                format!("{name} value may be nil", name = left.syntax().text()).to_string(),
                None,
            );
        }

        let right_type = semantic_model.infer_expr(right.clone()).ok()?;
        if right_type.is_nullable() {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                right.get_range(),
                format!("{name} value may be nil", name = right.syntax().text()).to_string(),
                None,
            );
        }
    }

    Some(())
}

fn check_condition_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    condition: LuaExpr,
) {
    match condition {
        LuaExpr::UnaryExpr(unary_expr) => {
            if unary_expr
                .get_op_token()
                .is_some_and(|token| token.get_op() == UnaryOperator::OpNot)
                && let Some(inner_expr) = unary_expr.get_expr()
            {
                check_condition_expr(context, semantic_model, inner_expr);
            }
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            if let Some(op) = binary_expr.get_op_token().map(|token| token.get_op())
                && matches!(op, BinaryOperator::OpAnd | BinaryOperator::OpOr)
                && let Some((left, right)) = binary_expr.get_exprs()
            {
                if is_short_circuit_isvalid_condition_guard(semantic_model, op, &left, &right) {
                    return;
                }

                check_condition_expr(context, semantic_model, left);
                check_condition_expr(context, semantic_model, right);
            }
        }
        expr => {
            if let Ok(expr_type) = semantic_model.infer_expr(expr.clone())
                && contains_gmod_null_type(semantic_model.get_db(), &expr_type)
            {
                context.add_diagnostic(
                    DiagnosticCode::GmodNullCheck,
                    expr.get_range(),
                    format!(
                        "{name} may be NULL; NULL is truthy, use IsValid(...) to check entity validity",
                        name = expr.syntax().text()
                    )
                    .to_string(),
                    None,
                );
            }
        }
    }
}

fn is_short_circuit_isvalid_condition_guard(
    semantic_model: &SemanticModel,
    op: BinaryOperator,
    left: &LuaExpr,
    right: &LuaExpr,
) -> bool {
    match op {
        BinaryOperator::OpAnd => {
            expr_matches_isvalid_condition_guard(semantic_model, left, right, false)
                || expr_matches_isvalid_condition_guard(semantic_model, right, left, false)
        }
        BinaryOperator::OpOr => {
            expr_matches_isvalid_condition_guard(semantic_model, left, right, true)
                || expr_matches_isvalid_condition_guard(semantic_model, right, left, true)
        }
        _ => false,
    }
}

fn expr_matches_isvalid_condition_guard(
    semantic_model: &SemanticModel,
    maybe_truthy_expr: &LuaExpr,
    maybe_guard_expr: &LuaExpr,
    negated: bool,
) -> bool {
    let Some(truthy_expr) = unwrap_optional_not_expr(maybe_truthy_expr.clone(), negated) else {
        return false;
    };
    let Some(guard_call) = unwrap_optional_not_call(maybe_guard_expr.clone(), negated) else {
        return false;
    };

    is_isvalid_call_guarding_var(semantic_model, &guard_call, &truthy_expr)
}

fn unwrap_optional_not_expr(expr: LuaExpr, negated: bool) -> Option<LuaExpr> {
    if !negated {
        return Some(expr);
    }

    let LuaExpr::UnaryExpr(unary_expr) = expr else {
        return None;
    };
    if unary_expr
        .get_op_token()
        .is_some_and(|token| token.get_op() == UnaryOperator::OpNot)
    {
        return unary_expr.get_expr();
    }

    None
}

fn unwrap_optional_not_call(expr: LuaExpr, negated: bool) -> Option<LuaCallExpr> {
    match unwrap_optional_not_expr(expr, negated)? {
        LuaExpr::CallExpr(call_expr) => Some(call_expr),
        _ => None,
    }
}

fn is_nil_comparison_guarded_by_isvalid(
    semantic_model: &SemanticModel,
    comparison_expr: &LuaBinaryExpr,
    non_nil_side: &LuaExpr,
    comparison_op: BinaryOperator,
) -> bool {
    let Some(parent_binary_expr) = comparison_expr.get_parent::<LuaBinaryExpr>() else {
        return false;
    };
    let Some(parent_op) = parent_binary_expr
        .get_op_token()
        .map(|token| token.get_op())
    else {
        return false;
    };
    let Some((left, right)) = parent_binary_expr.get_exprs() else {
        return false;
    };

    let (guard_expr, negated_guard) = if left.syntax() == comparison_expr.syntax() {
        (
            right,
            matches!(comparison_op, BinaryOperator::OpEq) && parent_op == BinaryOperator::OpOr,
        )
    } else if right.syntax() == comparison_expr.syntax() {
        (
            left,
            matches!(comparison_op, BinaryOperator::OpEq) && parent_op == BinaryOperator::OpOr,
        )
    } else {
        return false;
    };

    match comparison_op {
        BinaryOperator::OpNe if parent_op != BinaryOperator::OpAnd => return false,
        BinaryOperator::OpEq if parent_op != BinaryOperator::OpOr => return false,
        BinaryOperator::OpEq | BinaryOperator::OpNe => {}
        _ => return false,
    }

    let Some(guard_call) = unwrap_optional_not_call(guard_expr, negated_guard) else {
        return false;
    };

    is_isvalid_call_guarding_var(semantic_model, &guard_call, non_nil_side)
}

fn is_isvalid_call_guarding_var(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    let Some(prefix) = guard_call.get_prefix_expr() else {
        return false;
    };

    match prefix {
        LuaExpr::NameExpr(name_expr) => {
            if !is_unshadowed_isvalid_name(semantic_model, &name_expr) {
                return false;
            }

            let Some(args_list) = guard_call.get_args_list() else {
                return false;
            };
            let Some(first_arg) = args_list.get_args().next() else {
                return false;
            };

            exprs_reference_same_var(semantic_model, &first_arg, guarded_expr)
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

            exprs_reference_same_var(semantic_model, &self_expr, guarded_expr)
        }
        _ => false,
    }
}

fn is_unshadowed_isvalid_name(semantic_model: &SemanticModel, name_expr: &LuaNameExpr) -> bool {
    if name_expr.get_name_text().as_deref() != Some("IsValid") {
        return false;
    }

    let db = semantic_model.get_db();
    let file_id = semantic_model.get_file_id();
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .or_else(|| {
            db.get_decl_index()
                .get_decl_tree(&file_id)
                .and_then(|decl_tree| {
                    decl_tree
                        .find_local_decl("IsValid", name_expr.get_position())
                        .map(|decl| decl.get_id())
                })
        });

    let Some(decl_id) = decl_id else {
        return is_unshadowed_global_isvalid_name(name_expr);
    };

    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    if db
        .get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .is_some_and(|decl_refs| decl_refs.mutable)
    {
        return false;
    }

    let Some(value_syntax_id) = decl.get_value_syntax_id() else {
        return false;
    };
    let Some(node) = value_syntax_id.to_node_from_root(semantic_model.get_root().syntax()) else {
        return false;
    };
    let Some(LuaExpr::NameExpr(alias_name_expr)) = LuaExpr::cast(node) else {
        return false;
    };

    is_unshadowed_isvalid_name(semantic_model, &alias_name_expr)
}

fn is_unshadowed_global_isvalid_name(name_expr: &LuaNameExpr) -> bool {
    if name_expr.get_name_text().as_deref() != Some("IsValid") {
        return false;
    }

    true
}

fn exprs_reference_same_var(
    semantic_model: &SemanticModel,
    left: &LuaExpr,
    right: &LuaExpr,
) -> bool {
    let mut cache = semantic_model.get_cache().borrow_mut();
    let Some(left_ref_id) =
        get_var_expr_var_ref_id(semantic_model.get_db(), &mut cache, left.clone())
    else {
        return false;
    };
    let Some(right_ref_id) =
        get_var_expr_var_ref_id(semantic_model.get_db(), &mut cache, right.clone())
    else {
        return false;
    };

    left_ref_id == right_ref_id
}

fn get_null_nil_comparison_operand<'a>(
    semantic_model: &SemanticModel,
    left: &'a LuaExpr,
    right: &'a LuaExpr,
) -> Option<&'a LuaExpr> {
    if is_nil_expr(semantic_model, left) && expr_can_be_gmod_null(semantic_model, right) {
        return Some(right);
    }

    if is_nil_expr(semantic_model, right) && expr_can_be_gmod_null(semantic_model, left) {
        return Some(left);
    }

    None
}

fn is_nil_expr(semantic_model: &SemanticModel, expr: &LuaExpr) -> bool {
    semantic_model
        .infer_expr(expr.clone())
        .is_ok_and(|expr_type| expr_type.is_nil())
}

fn expr_can_be_gmod_null(semantic_model: &SemanticModel, expr: &LuaExpr) -> bool {
    semantic_model
        .infer_expr(expr.clone())
        .is_ok_and(|expr_type| contains_gmod_null_type(semantic_model.get_db(), &expr_type))
}
