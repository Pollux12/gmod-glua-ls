use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAst, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaCallExprStat,
    LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexExpr, LuaIndexKey, LuaLocalFuncStat,
    LuaLocalStat, LuaNameExpr, LuaSyntaxKind, LuaSyntaxNode, LuaVarExpr, UnaryOperator,
};
use rowan::TextRange;
use rustc_hash::FxHashSet;

use crate::{
    DiagnosticCode, FileId, GMOD_ATTR_SELF_CALL_VALID, GMOD_CALL_ARG_DOMAINS, GMOD_DOMAIN_CONVAR,
    GMOD_ROLE_EXISTS, GMOD_ROLE_REFERENCE, GmodLoadEdgeKind, InferFailReason, LuaDeclId,
    LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaSignatureCast, LuaSignatureId, LuaType,
    LuaUnionType, SemanticDeclLevel, SemanticModel, find_best_call_arg_role_from_type,
    find_signature_attribute_use, get_var_expr_var_ref_id,
    semantic::{
        InferConditionFlow, cast_type, contains_gmod_null_type, get_member_value_expr,
        remove_false_or_nil,
    },
    signature_is_valid_guard_in_realm, signature_is_valid_guard_or_base_runtime_isvalid_in_realm,
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

        for node in root.descendants::<LuaAst>() {
            match node {
                LuaAst::LuaIfStat(if_stat) => {
                    if let Some(condition) = if_stat.get_condition_expr() {
                        check_condition_expr(context, semantic_model, condition);
                    }
                }
                LuaAst::LuaElseIfClauseStat(elseif_stat) => {
                    if let Some(condition) = elseif_stat.get_condition_expr() {
                        check_condition_expr(context, semantic_model, condition);
                    }
                }
                LuaAst::LuaWhileStat(while_stat) => {
                    if let Some(condition) = while_stat.get_condition_expr() {
                        check_condition_expr(context, semantic_model, condition);
                    }
                }
                LuaAst::LuaRepeatStat(repeat_stat) => {
                    if let Some(condition) = repeat_stat.get_condition_expr() {
                        check_condition_expr(context, semantic_model, condition);
                    }
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

    let is_receiver_guarded_by_type_guard = if let LuaExpr::IndexExpr(index_expr) = &prefix
        && let Some(receiver) = index_expr.get_prefix_expr()
    {
        is_short_circuit_type_guard_for_receiver(semantic_model, &call_expr, &receiver)
            || is_non_nullable_receiver_self_type_guard(semantic_model, &call_expr, &receiver)
    } else {
        false
    };

    let is_receiver_known_non_nullable = if let LuaExpr::IndexExpr(index_expr) = &prefix
        && let Some(receiver) = index_expr.get_prefix_expr()
    {
        is_receiver_guarded_by_type_guard
            || is_load_ordered_convar_reference_local(semantic_model, &receiver)
    } else {
        is_receiver_guarded_by_type_guard
    };

    if let LuaExpr::IndexExpr(index_expr) = &prefix {
        let receiver = index_expr.get_prefix_expr()?;
        if !is_receiver_known_non_nullable
            && expr_has_invalidated_prior_nil_early_return(semantic_model, &receiver)
        {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                receiver.get_range(),
                format!("{name} may be nil", name = receiver.syntax().text()).to_string(),
                None,
            );
            return Some(());
        }
        if !is_receiver_known_non_nullable
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
        } else if nullable_callable_is_from_guarded_receiver(semantic_model, &prefix) {
            // The receiver is nil-checked by a prior type-guard early-return (e.g.
            // `if not ent:IsAlive() then return end`), so the function access is safe
            // even though the receiver type still appears nullable in the semantic model
            // (flow narrowing for colon-call guard calls is deferred to the diagnostic
            // layer rather than the flow graph).
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

/// Returns `true` when the member access `prefix` has a receiver that is nil-checked
/// by a TypeGuard. This handles guard patterns such as `if IsValid(ent) then
/// ent:Nick() end` and `if not ent:IsAlive() then return end; ent:GetEditingData()`:
/// the receiver is guarded, so calling a method on it is safe even if the receiver type
/// still appears nullable in the semantic model (some guard narrowing is deferred to the
/// diagnostic layer rather than the flow graph).
fn nullable_callable_is_from_guarded_receiver(
    semantic_model: &SemanticModel,
    prefix: &LuaExpr,
) -> bool {
    let LuaExpr::IndexExpr(index_expr) = prefix else {
        return false;
    };
    let Some(receiver) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Ok(receiver_type) = semantic_model.infer_expr(receiver.clone()) else {
        return false;
    };

    if index_expr_has_explicit_nullable_member(semantic_model, index_expr, receiver_type.clone()) {
        return false;
    }

    if contains_gmod_null_type(semantic_model.get_db(), &receiver_type) {
        // GMod NULL is truthy, so nullable-callable suppression for NULL-typed
        // receivers must use the same explicit validity guards as receiver
        // diagnostics instead of the plain `not expr then return` nil path.
        is_expr_guarded_by_prior_null_excluding_type_guard_early_return(
            semantic_model,
            &receiver,
            &receiver_type,
        ) || is_expr_guarded_by_current_null_excluding_type_guard_condition(
            semantic_model,
            &receiver,
            &receiver_type,
        ) || is_expr_valid_by_enclosing_self_call_context(semantic_model, &receiver)
    } else {
        is_expr_guarded_by_prior_nil_early_return(semantic_model, &receiver)
            || is_expr_guarded_by_correlated_multi_return(semantic_model, &receiver)
            || is_expr_guarded_by_current_type_guard_condition(semantic_model, &receiver)
    }
}

fn index_expr_has_explicit_nullable_member(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    receiver_type: LuaType,
) -> bool {
    let Some(key) = literal_member_key(index_expr) else {
        return false;
    };

    type_has_explicit_nullable_member(semantic_model, index_expr, receiver_type, &key)
}

fn type_has_explicit_nullable_member(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    receiver_type: LuaType,
    key: &LuaMemberKey,
) -> bool {
    match receiver_type {
        LuaType::Union(union) => union.types().any(|component| {
            !matches!(component, LuaType::Nil | LuaType::BooleanConst(false))
                && type_has_explicit_nullable_member(
                    semantic_model,
                    index_expr,
                    component.clone(),
                    key,
                )
        }),
        LuaType::Intersection(intersection) => intersection.get_types().iter().any(|component| {
            type_has_explicit_nullable_member(semantic_model, index_expr, component.clone(), key)
        }),
        receiver_type => {
            owner_has_explicit_nullable_member(semantic_model, index_expr, receiver_type, key)
        }
    }
}

fn owner_has_explicit_nullable_member(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    receiver_type: LuaType,
    key: &LuaMemberKey,
) -> bool {
    let Some(owner) = member_owner_for_type(receiver_type) else {
        return false;
    };

    let db = semantic_model.get_db();
    let Some(member_item) = db.get_member_index().get_member_item(&owner, key) else {
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
        // If the type contains GMod NULL, use TypeGuard-only matching.
        // NULL is truthy, so `not expr` does not prove validity.
        let has_gmod_null = contains_gmod_null_type(semantic_model.get_db(), &receiver_type);
        let guarded = if has_gmod_null {
            is_expr_guarded_by_prior_null_excluding_type_guard_early_return(
                semantic_model,
                receiver,
                &receiver_type,
            ) || is_expr_guarded_by_current_null_excluding_type_guard_condition(
                semantic_model,
                receiver,
                &receiver_type,
            )
        } else {
            is_expr_guarded_by_prior_nil_early_return(semantic_model, receiver)
                || is_expr_guarded_by_correlated_multi_return(semantic_model, receiver)
                || is_expr_guarded_by_current_assigned_value_type_guard_condition(
                    semantic_model,
                    receiver,
                )
                || is_expr_guarded_by_current_type_guard_condition(semantic_model, receiver)
        };
        if guarded {
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

    // NULL is truthy in GLua, so `not ent` does NOT prove entity validity.
    // Only annotation-backed TypeGuard calls suppress NULL diagnostics.
    if is_expr_guarded_by_prior_null_excluding_type_guard_early_return(
        semantic_model,
        receiver,
        &receiver_type,
    ) || is_expr_guarded_by_current_null_excluding_type_guard_condition(
        semantic_model,
        receiver,
        &receiver_type,
    ) || is_expr_valid_by_enclosing_self_call_context(semantic_model, receiver)
    {
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

fn is_expr_valid_by_enclosing_self_call_context(
    semantic_model: &SemanticModel,
    receiver: &LuaExpr,
) -> bool {
    if let Some((call_expr, method_name)) = self_call_method(receiver) {
        return enclosing_callback_marks_self_call_valid(semantic_model, receiver, &method_name)
            && self_call_uses_unshadowed_callback_self(semantic_model, receiver, &call_expr);
    }

    if let Some((assigned_call, method_name, assign_range)) =
        prior_local_self_call_assignment(receiver)
    {
        return enclosing_callback_marks_self_call_valid(semantic_model, receiver, &method_name)
            && self_call_uses_unshadowed_callback_self(semantic_model, receiver, &assigned_call)
            && !local_alias_reassigned_after_assignment(semantic_model, receiver, assign_range);
    }

    false
}

fn self_call_method(expr: &LuaExpr) -> Option<(LuaCallExpr, String)> {
    let LuaExpr::CallExpr(call_expr) = expr else {
        return None;
    };
    if !call_expr.is_colon_call() {
        return None;
    }

    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    let self_expr = index_expr.get_prefix_expr()?;
    if normalized_expr_text(&self_expr).as_deref() != Some("self") {
        return None;
    }

    let method_name = literal_member_name(&index_expr)?;
    Some((call_expr.clone(), method_name))
}

fn prior_local_self_call_assignment(
    receiver: &LuaExpr,
) -> Option<(LuaCallExpr, String, TextRange)> {
    let receiver_text = normalized_expr_text(receiver)?;
    let mut candidate = None;

    let mut current = receiver.syntax().clone();
    while let Some(parent) = current.parent() {
        if LuaClosureExpr::cast(parent.clone()).is_some() {
            break;
        }

        if LuaSyntaxKind::from(parent.kind()) == LuaSyntaxKind::Block {
            for sibling in parent.children() {
                let sibling_range = sibling.text_range();
                if sibling_range.start() >= current.text_range().start() {
                    break;
                }
                let Some(local_stat) = LuaLocalStat::cast(sibling) else {
                    continue;
                };

                let names = local_stat.get_local_name_list().collect::<Vec<_>>();
                let values = local_stat.get_value_exprs().collect::<Vec<_>>();
                for (index, local_name) in names.iter().enumerate() {
                    if local_name.get_text() != receiver_text {
                        continue;
                    }
                    let Some(value_expr) = values.get(index) else {
                        continue;
                    };
                    let Some((call_expr, method_name)) = self_call_method(value_expr) else {
                        continue;
                    };
                    // Keep the nearest visible alias candidate found while walking
                    // outward. If an inner `local owner = ...` shadows an outer
                    // callback-owner alias, `local_alias_reassigned_after_assignment`
                    // must invalidate the outer candidate before suppression.
                    candidate = Some((call_expr, method_name, local_stat.syntax().text_range()));
                }
            }
        }

        current = parent;
    }

    candidate
}

fn local_alias_reassigned_after_assignment(
    semantic_model: &SemanticModel,
    receiver: &LuaExpr,
    assign_range: TextRange,
) -> bool {
    let mut current = receiver.syntax().clone();
    while let Some(parent) = current.parent() {
        if LuaClosureExpr::cast(parent.clone()).is_some() {
            break;
        }

        if LuaSyntaxKind::from(parent.kind()) == LuaSyntaxKind::Block {
            for sibling in parent.children() {
                let sibling_range = sibling.text_range();
                if sibling_range.end() <= assign_range.end() {
                    continue;
                }
                if sibling_range.start() >= current.text_range().start() {
                    break;
                }

                if let Some(assign_stat) = LuaAssignStat::cast(sibling.clone())
                    && assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, receiver)
                {
                    return true;
                }

                if let Some(local_stat) = LuaLocalStat::cast(sibling.clone())
                    && local_stat_shadows_guarded_expr(semantic_model, &local_stat, receiver)
                {
                    return true;
                }

                if sibling.descendants().any(|node| {
                    LuaAssignStat::cast(node).is_some_and(|assign_stat| {
                        assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, receiver)
                    })
                }) {
                    return true;
                }
            }
        }

        current = parent;
    }

    false
}

fn self_call_uses_unshadowed_callback_self(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    call_expr: &LuaCallExpr,
) -> bool {
    if enclosing_colon_method(guarded_expr).is_none() {
        return false;
    }
    let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr() else {
        return false;
    };
    let Some(call_receiver) = index_expr.get_prefix_expr() else {
        return false;
    };
    if normalized_expr_text(&call_receiver).as_deref() != Some("self") {
        return false;
    }

    !self_receiver_shadowed_before_call(semantic_model, guarded_expr, &call_receiver)
}

fn self_receiver_shadowed_before_call(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    self_expr: &LuaExpr,
) -> bool {
    let Some(parent) = guarded_expr
        .syntax()
        .ancestors()
        .find(|node| LuaSyntaxKind::from(node.kind()) == LuaSyntaxKind::Block)
    else {
        return true;
    };
    let guard_start = guarded_expr.syntax().text_range().start();

    parent.children().any(|sibling| {
        sibling.text_range().start() < guard_start
            && LuaLocalStat::cast(sibling).is_some_and(|local_stat| {
                local_stat_shadows_guarded_expr(semantic_model, &local_stat, self_expr)
            })
    })
}

fn enclosing_callback_marks_self_call_valid(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
    valid_method_name: &str,
) -> bool {
    let Some((_, receiver_expr, callback_key)) = enclosing_colon_method(expr) else {
        return false;
    };
    let Ok(receiver_type) = semantic_model.infer_expr(receiver_expr) else {
        return false;
    };
    semantic_model
        .infer_member_type(&receiver_type, &callback_key)
        .is_ok_and(|callback_type| {
            type_has_self_call_valid_attribute(semantic_model, &callback_type, valid_method_name)
        })
        // User callback definitions can shadow library callback signatures. Fall
        // back to the global member-key metadata index so annotation-provided
        // callback markers still apply to overrides, matching the existing
        // self_guard member-key fallback. This is intentionally key-scoped, not
        // type-scoped; only annotate callback names whose owner-valid contract is
        // safe for that method name.
        || indexed_member_key_has_self_call_valid_attribute(
            semantic_model,
            &callback_key,
            valid_method_name,
        )
}

fn enclosing_colon_method(expr: &LuaExpr) -> Option<(LuaFuncStat, LuaExpr, LuaMemberKey)> {
    for ancestor in expr.syntax().ancestors() {
        if let Some(closure_expr) = LuaClosureExpr::cast(ancestor.clone()) {
            let func_stat = closure_expr.get_parent::<LuaFuncStat>()?;
            let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
                return None;
            };
            let receiver_expr = index_expr.get_prefix_expr()?;
            let member_key = literal_member_key(&index_expr)?;
            return Some((func_stat, receiver_expr, member_key));
        }
    }

    None
}

fn literal_member_name(index_expr: &LuaIndexExpr) -> Option<String> {
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
        LuaIndexKey::String(string) => Some(string.get_value().to_string()),
        _ => None,
    }
}

fn type_has_self_call_valid_attribute(
    semantic_model: &SemanticModel,
    typ: &LuaType,
    method_name: &str,
) -> bool {
    match typ {
        LuaType::Signature(signature_id) => {
            signature_has_self_call_valid_attribute(semantic_model, *signature_id, method_name)
        }
        LuaType::Union(union_type) => union_type
            .types()
            .any(|typ| type_has_self_call_valid_attribute(semantic_model, typ, method_name)),
        LuaType::Intersection(intersection_type) => intersection_type
            .get_types()
            .iter()
            .any(|typ| type_has_self_call_valid_attribute(semantic_model, typ, method_name)),
        _ => false,
    }
}

fn indexed_member_key_has_self_call_valid_attribute(
    semantic_model: &SemanticModel,
    member_key: &LuaMemberKey,
    method_name: &str,
) -> bool {
    let db = semantic_model.get_db();
    db.get_member_index()
        .get_current_members_for_key(member_key)
        .into_iter()
        .filter_map(|member| {
            db.get_type_index()
                .get_type_cache(&member.get_id().into())
                .map(|type_cache| type_cache.as_type().clone())
        })
        .any(|typ| type_has_self_call_valid_attribute(semantic_model, &typ, method_name))
}

fn signature_has_self_call_valid_attribute(
    semantic_model: &SemanticModel,
    signature_id: LuaSignatureId,
    method_name: &str,
) -> bool {
    let Some(attribute_use) = find_signature_attribute_use(
        semantic_model.get_db(),
        signature_id,
        GMOD_ATTR_SELF_CALL_VALID,
    ) else {
        return false;
    };
    // `find_signature_attribute_use` returns the first matching standalone
    // attribute. Current callback metadata uses one valid self-call method per
    // callback; if a future callback needs multiple methods, iterate all
    // signature attributes instead of stacking repeated `self_call_valid` tags.
    let Some(method_arg) = attribute_use.get_param_by_name("method") else {
        return false;
    };
    matches!(
        method_arg,
        LuaType::DocStringConst(value) | LuaType::StringConst(value) if value.as_str() == method_name
    )
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

        // If the type contains GMod NULL, use TypeGuard-only matching.
        // NULL is truthy, so `not expr` does not prove validity.
        let has_gmod_null = contains_gmod_null_type(semantic_model.get_db(), &prefix_type);
        let guarded = if has_gmod_null {
            is_expr_guarded_by_prior_null_excluding_type_guard_early_return(
                semantic_model,
                &prefix,
                &prefix_type,
            ) || is_expr_guarded_by_current_null_excluding_type_guard_condition(
                semantic_model,
                &prefix,
                &prefix_type,
            )
        } else {
            is_expr_guarded_by_prior_nil_early_return(semantic_model, &prefix)
        };
        if guarded {
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

fn is_expr_guarded_by_prior_type_guard_early_return(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    is_expr_guarded_by_prior_early_return(semantic_model, expr, |condition, guarded| {
        condition_is_negative_type_guard(semantic_model, condition, guarded)
    })
}

fn is_expr_guarded_by_prior_null_excluding_type_guard_early_return(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    is_expr_guarded_by_prior_early_return(semantic_model, expr, |condition, guarded| {
        condition_is_negative_null_excluding_type_guard(
            semantic_model,
            condition,
            guarded,
            expr_type,
        )
    })
}

fn is_expr_guarded_by_current_null_excluding_type_guard_condition(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    let expr_range = expr.syntax().text_range();
    for ancestor in expr.syntax().ancestors() {
        if let Some(if_stat) = LuaIfStat::cast(ancestor.clone()) {
            if let Some(condition) = if_stat.get_condition_expr()
                && if_stat
                    .get_block()
                    .is_some_and(|block| range_contains(block.syntax().text_range(), expr_range))
                && condition_is_positive_null_excluding_type_guard(
                    semantic_model,
                    &condition,
                    expr,
                    expr_type,
                )
            {
                return true;
            }

            for elseif_clause in if_stat.get_else_if_clause_list() {
                if let Some(condition) = elseif_clause.get_condition_expr()
                    && elseif_clause.get_block().is_some_and(|block| {
                        range_contains(block.syntax().text_range(), expr_range)
                    })
                    && condition_is_positive_null_excluding_type_guard(
                        semantic_model,
                        &condition,
                        expr,
                        expr_type,
                    )
                {
                    return true;
                }
            }
        }
    }

    false
}

fn is_expr_guarded_by_current_type_guard_condition(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    let expr_range = expr.syntax().text_range();
    for ancestor in expr.syntax().ancestors() {
        if let Some(if_stat) = LuaIfStat::cast(ancestor.clone()) {
            if let Some(condition) = if_stat.get_condition_expr()
                && if_stat
                    .get_block()
                    .is_some_and(|block| range_contains(block.syntax().text_range(), expr_range))
                && condition_is_positive_type_guard_call(semantic_model, &condition, expr)
                && !then_block_reassigns_guarded_expr_before_access(semantic_model, &if_stat, expr)
                && !loop_back_edge_reassigns_guarded_expr_after_if(semantic_model, &if_stat, expr)
            {
                return true;
            }

            for elseif_clause in if_stat.get_else_if_clause_list() {
                if let Some(condition) = elseif_clause.get_condition_expr()
                    && elseif_clause.get_block().is_some_and(|block| {
                        range_contains(block.syntax().text_range(), expr_range)
                    })
                    && condition_is_positive_type_guard_call(semantic_model, &condition, expr)
                {
                    return true;
                }
            }
        }
    }

    false
}

fn is_expr_guarded_by_current_assigned_value_type_guard_condition(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    let expr_range = expr.syntax().text_range();
    for ancestor in expr.syntax().ancestors() {
        let Some(if_stat) = LuaIfStat::cast(ancestor.clone()) else {
            continue;
        };

        let Some(condition) = if_stat.get_condition_expr() else {
            continue;
        };

        let in_then_block = if_stat
            .get_block()
            .is_some_and(|block| range_contains(block.syntax().text_range(), expr_range));
        if !in_then_block {
            continue;
        }

        let Some(assigned_expr) = prior_assignment_value_for_expr(semantic_model, expr, &if_stat)
        else {
            continue;
        };
        if !is_stable_guard_expr(&assigned_expr) {
            continue;
        }
        if !condition_is_positive_type_guard_call(semantic_model, &condition, &assigned_expr) {
            continue;
        }
        if then_block_reassigns_guarded_expr_before_access(semantic_model, &if_stat, expr) {
            continue;
        }
        if loop_back_edge_reassigns_guarded_expr_after_if(semantic_model, &if_stat, expr) {
            continue;
        }
        return true;
    }

    false
}

fn prior_assignment_value_for_expr(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    if_stat: &LuaIfStat,
) -> Option<LuaExpr> {
    let guarded_text = normalized_expr_text(guarded_expr)?;
    let path_nodes = preceding_path_sibling_nodes(if_stat);
    let mut candidate = None;

    for node in &path_nodes {
        let Some(assign_stat) = LuaAssignStat::cast(node.clone()) else {
            continue;
        };
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (index, var) in vars.iter().enumerate() {
            let Some(var_text) = normalized_expr_text(&var.to_expr()) else {
                continue;
            };
            if var_text == guarded_text
                && let Some(value_expr) = exprs.get(index).cloned()
            {
                candidate = Some((value_expr, assign_stat.syntax().text_range()));
            }
        }
    }

    let (assigned_expr, assign_range) = candidate?;
    for node in &path_nodes {
        let node_range = node.text_range();
        if node_range.start() <= assign_range.end() {
            continue;
        }

        if let Some(assign_stat) = LuaAssignStat::cast(node.clone())
            && (assign_stat_reassigns_guarded_receiver_value(
                semantic_model,
                &assign_stat,
                guarded_expr,
            ) || assign_stat_reassigns_guarded_expr(
                semantic_model,
                &assign_stat,
                &assigned_expr,
            ))
        {
            return None;
        }
        if node
            .descendants()
            .filter_map(LuaAssignStat::cast)
            .any(|assign_stat| {
                assign_stat_reassigns_guarded_receiver_value(
                    semantic_model,
                    &assign_stat,
                    guarded_expr,
                ) || assign_stat_reassigns_guarded_expr(
                    semantic_model,
                    &assign_stat,
                    &assigned_expr,
                )
            })
        {
            return None;
        }

        if let Some(local_stat) = LuaLocalStat::cast(node.clone())
            && (local_stat_shadows_guarded_expr(semantic_model, &local_stat, guarded_expr)
                || local_stat_shadows_guarded_expr(semantic_model, &local_stat, &assigned_expr))
        {
            return None;
        }
    }

    Some(assigned_expr)
}

fn then_block_reassigns_guarded_expr_before_access(
    semantic_model: &SemanticModel,
    if_stat: &LuaIfStat,
    guarded_expr: &LuaExpr,
) -> bool {
    let Some(block) = if_stat.get_block() else {
        return false;
    };
    let access_start = guarded_expr.syntax().text_range().start();

    for node in block.syntax().children() {
        if node.text_range().end() > access_start {
            continue;
        }

        if node
            .descendants()
            .filter_map(LuaAssignStat::cast)
            .any(|assign_stat| {
                assign_stat_reassigns_guarded_receiver_value(
                    semantic_model,
                    &assign_stat,
                    guarded_expr,
                )
            })
        {
            return true;
        }

        if let Some(local_stat) = LuaLocalStat::cast(node.clone())
            && local_stat_shadows_guarded_expr(semantic_model, &local_stat, guarded_expr)
        {
            return true;
        }
    }

    false
}

fn loop_back_edge_reassigns_guarded_expr_after_if(
    semantic_model: &SemanticModel,
    if_stat: &LuaIfStat,
    guarded_expr: &LuaExpr,
) -> bool {
    let mut current = if_stat.syntax().clone();

    while let Some(parent) = current.parent() {
        if LuaSyntaxKind::from(parent.kind()) == LuaSyntaxKind::Block
            && parent
                .parent()
                .is_some_and(|loop_node| is_loop_stat_kind(LuaSyntaxKind::from(loop_node.kind())))
            && following_siblings_reassign_guarded_receiver(
                semantic_model,
                &parent,
                &current,
                guarded_expr,
            )
        {
            return true;
        }

        current = parent;
    }

    false
}

fn following_siblings_reassign_guarded_receiver(
    semantic_model: &SemanticModel,
    block: &LuaSyntaxNode,
    current: &LuaSyntaxNode,
    guarded_expr: &LuaExpr,
) -> bool {
    let current_end = current.text_range().end();

    for sibling in block.children() {
        if sibling.text_range().start() <= current_end {
            continue;
        }

        if sibling
            .descendants()
            .filter_map(LuaAssignStat::cast)
            .any(|assign_stat| {
                assign_stat_reassigns_guarded_receiver_value(
                    semantic_model,
                    &assign_stat,
                    guarded_expr,
                )
            })
        {
            return true;
        }

        if let Some(local_stat) = LuaLocalStat::cast(sibling.clone())
            && local_stat_shadows_guarded_expr(semantic_model, &local_stat, guarded_expr)
        {
            return true;
        }
    }

    false
}

fn is_loop_stat_kind(kind: LuaSyntaxKind) -> bool {
    matches!(
        kind,
        LuaSyntaxKind::WhileStat
            | LuaSyntaxKind::RepeatStat
            | LuaSyntaxKind::ForStat
            | LuaSyntaxKind::ForRangeStat
    )
}

fn assign_stat_reassigns_guarded_receiver_value(
    semantic_model: &SemanticModel,
    assign_stat: &LuaAssignStat,
    guarded_expr: &LuaExpr,
) -> bool {
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter().any(|var| {
        assigned_expr_invalidates_guarded_receiver_value(
            semantic_model,
            &var.to_expr(),
            guarded_expr,
        )
    })
}

fn assigned_expr_invalidates_guarded_receiver_value(
    semantic_model: &SemanticModel,
    assigned_expr: &LuaExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    // Assignment-backed TypeGuard suppression proves the receiver value copied
    // from the guarded source. This invalidation is intentionally narrower than
    // `assigned_expr_invalidates_guarded_expr`: sibling field writes such as
    // `slot.Normal = slot.Phys:...` do not replace `slot.Phys`, but exact
    // receiver writes, receiver-prefix writes, and index-key reassignments do.
    if exprs_reference_same_var(semantic_model, assigned_expr, guarded_expr) {
        return true;
    }

    if normalized_expr_text(assigned_expr) == normalized_expr_text(guarded_expr) {
        return true;
    }

    let mut current = guarded_expr.clone();
    while let LuaExpr::IndexExpr(index_expr) = current {
        let Some(prefix) = index_expr.get_prefix_expr() else {
            return false;
        };

        if normalized_expr_text(assigned_expr) == normalized_expr_text(&prefix) {
            return true;
        }
        if index_expr_key_reassigned_by_lhs(semantic_model, assigned_expr, &index_expr) {
            return true;
        }

        current = prefix;
    }

    false
}

fn index_expr_key_reassigned_by_lhs(
    semantic_model: &SemanticModel,
    assigned_expr: &LuaExpr,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(LuaIndexKey::Expr(key_expr)) = index_expr.get_index_key() else {
        return false;
    };

    if exprs_reference_same_var(semantic_model, assigned_expr, &key_expr) {
        return true;
    }

    let assigned_text = normalized_expr_text(assigned_expr);
    if assigned_text == normalized_expr_text(&key_expr) {
        return true;
    }

    let mut cache = semantic_model.get_cache().borrow_mut();
    let assigned_ref_id =
        get_var_expr_var_ref_id(semantic_model.get_db(), &mut cache, assigned_expr.clone());
    drop(cache);

    key_expr
        .syntax()
        .descendants()
        .filter_map(LuaExpr::cast)
        .filter(|descendant_expr| {
            matches!(
                descendant_expr,
                LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_)
            )
        })
        .any(|descendant_expr| {
            if assigned_text == normalized_expr_text(&descendant_expr) {
                return true;
            }
            let Some(ref assigned_ref_id) = assigned_ref_id else {
                return false;
            };
            let mut cache = semantic_model.get_cache().borrow_mut();
            let descendant_ref_id = get_var_expr_var_ref_id(
                semantic_model.get_db(),
                &mut cache,
                descendant_expr.clone(),
            );
            drop(cache);
            descendant_ref_id.is_some_and(|id| &id == assigned_ref_id)
        })
}

fn preceding_path_sibling_nodes(if_stat: &LuaIfStat) -> Vec<LuaSyntaxNode> {
    let mut nodes = Vec::new();
    let mut current = if_stat.syntax().clone();

    while let Some(parent) = current.parent() {
        if LuaSyntaxKind::from(parent.kind()) == LuaSyntaxKind::Block {
            let current_start = current.text_range().start();
            for sibling in parent.children() {
                if sibling.text_range().start() >= current_start {
                    break;
                }
                nodes.push(sibling);
            }
        }
        current = parent;
    }

    nodes.sort_by_key(|node| (node.text_range().start(), node.text_range().end()));
    nodes
}

fn condition_is_positive_type_guard_call(
    semantic_model: &SemanticModel,
    condition: &LuaExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    match condition {
        LuaExpr::CallExpr(call_expr) => {
            is_string_registry_exists_call_guarding_reference_expr(
                semantic_model,
                call_expr,
                guarded_expr,
            ) || is_type_guard_call_guarding_expr(semantic_model, call_expr, guarded_expr)
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let Some(op) = binary_expr.get_op_token().map(|op| op.get_op()) else {
                return false;
            };
            if op != BinaryOperator::OpAnd {
                return false;
            }
            let Some((left, right)) = binary_expr.get_exprs() else {
                return false;
            };
            condition_is_positive_type_guard_call(semantic_model, &left, guarded_expr)
                || condition_is_positive_type_guard_call(semantic_model, &right, guarded_expr)
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
            condition_is_positive_type_guard_call(semantic_model, &expr, guarded_expr)
        }),
        _ => false,
    }
}

fn is_stable_guard_expr(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::NameExpr(_) => true,
        LuaExpr::IndexExpr(index_expr) => {
            let Some(prefix) = index_expr.get_prefix_expr() else {
                return false;
            };
            is_stable_guard_expr(&prefix)
                && index_expr
                    .get_index_key()
                    .is_some_and(|key| is_stable_guard_index_key(&key))
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|inner| is_stable_guard_expr(&inner)),
        _ => false,
    }
}

fn is_stable_guard_index_key(key: &LuaIndexKey) -> bool {
    match key {
        LuaIndexKey::Name(_)
        | LuaIndexKey::String(_)
        | LuaIndexKey::Integer(_)
        | LuaIndexKey::Idx(_) => true,
        LuaIndexKey::Expr(expr) => is_stable_guard_key_expr(expr),
    }
}

fn is_stable_guard_key_expr(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::LiteralExpr(_) | LuaExpr::NameExpr(_) => true,
        LuaExpr::IndexExpr(_) => is_stable_guard_expr(expr),
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|inner| is_stable_guard_key_expr(&inner)),
        LuaExpr::UnaryExpr(unary_expr) => unary_expr
            .get_expr()
            .is_some_and(|inner| is_stable_guard_key_expr(&inner)),
        LuaExpr::BinaryExpr(binary_expr) => binary_expr.get_exprs().is_some_and(|(left, right)| {
            is_stable_guard_key_expr(&left) && is_stable_guard_key_expr(&right)
        }),
        _ => false,
    }
}

/// Combined guard: checks both TypeGuard-specific and general negation guards.
/// The TypeGuard guard uses structural var_ref_id matching (precise but limited);
/// the general negation guard uses text-based matching (handles dynamic-key
/// index expressions like `self.Objects[i]` that var_ref_id can't track).
fn is_expr_guarded_by_prior_nil_early_return(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    // Try annotation-backed guard first (structural, more precise)
    if is_expr_guarded_by_prior_type_guard_early_return(semantic_model, expr) {
        return true;
    }
    // Fall back to general negation guard (text-based, handles dynamic keys)
    is_expr_guarded_by_prior_early_return(semantic_model, expr, |condition, guarded| {
        condition_is_negative_expr_guard(condition, guarded)
    })
}

fn is_load_ordered_convar_reference_local(semantic_model: &SemanticModel, expr: &LuaExpr) -> bool {
    let Some((call_expr, ret_idx)) = local_call_initializer_slot(semantic_model, expr) else {
        return false;
    };
    if ret_idx != 0 {
        return false;
    }

    let Some(convar_name) = static_convar_reference_key(semantic_model, &call_expr) else {
        return false;
    };

    load_ancestor_registers_convar(semantic_model, convar_name.as_str())
}

fn static_convar_reference_key(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<String> {
    if let Some((domain, key)) =
        static_string_call_arg_role(semantic_model, call_expr, &[GMOD_ROLE_REFERENCE])
    {
        return (domain == GMOD_DOMAIN_CONVAR).then_some(key);
    }

    if !base_runtime_getconvar_reference_call(semantic_model, call_expr) {
        return None;
    }
    crate::ast_util::literal_string_arg_value(call_expr, 0)
}

fn base_runtime_getconvar_reference_call(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };
    if normalized_expr_text(&prefix_expr).as_deref() != Some("GetConVar") {
        return false;
    }
    let Some(signature_id) = call_signature_id(semantic_model, call_expr) else {
        return false;
    };
    if !semantic_model.get_emmyrc().gmod.enabled {
        return false;
    }
    semantic_model
        .get_db()
        .get_vfs()
        .get_file_path(&signature_id.get_file_id())
        .and_then(|path| path.to_str())
        .map(|path| path.replace('\\', "/"))
        .is_some_and(|path| path.ends_with("/lua/includes/util.lua"))
}

fn load_ancestor_registers_convar(semantic_model: &SemanticModel, convar_name: &str) -> bool {
    let convar_name = convar_name.trim();
    if convar_name.is_empty() {
        return false;
    }

    let db = semantic_model.get_db();
    let mut pending = vec![semantic_model.get_file_id()];
    let mut visited = FxHashSet::default();
    while let Some(file_id) = pending.pop() {
        if !visited.insert(file_id) {
            continue;
        }

        // Registration order inside an ancestor is intentionally not rechecked here: a resolved
        // executing load edge proves the current file is reached from that loader, and the common
        // base-game pattern registers systems before including dependent files.
        if file_registers_convar(db.get_gmod_infer_index(), file_id, convar_name) {
            return true;
        }

        let Some(load_info) = db.get_gmod_load_index().get_file_info(&file_id) else {
            continue;
        };
        for edge in &load_info.incoming_edges {
            if load_edge_executes_target(edge.kind) {
                pending.push(edge.source_file_id);
            }
        }
    }

    false
}

fn file_registers_convar(
    infer_index: &crate::GmodInferIndex,
    file_id: FileId,
    convar_name: &str,
) -> bool {
    infer_index
        .get_system_file_metadata(&file_id)
        .is_some_and(|metadata| {
            metadata.convar_create_calls.iter().any(|site| {
                site.convar_name
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|registered| !registered.is_empty() && registered == convar_name)
            })
        })
}

fn load_edge_executes_target(kind: GmodLoadEdgeKind) -> bool {
    matches!(
        kind,
        GmodLoadEdgeKind::Include
            | GmodLoadEdgeKind::IncludeCS
            | GmodLoadEdgeKind::Require
            | GmodLoadEdgeKind::WrapperInclude
            | GmodLoadEdgeKind::DynamicInclude
    )
}

fn is_expr_guarded_by_correlated_multi_return(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    let Some((call_expr, implied_slot)) = local_call_initializer_slot(semantic_model, expr) else {
        return false;
    };
    let Some(signature_id) = call_signature_id(semantic_model, &call_expr) else {
        return false;
    };
    let Some(signature) = semantic_model
        .get_db()
        .get_signature_index()
        .get(&signature_id)
    else {
        return false;
    };

    let Some(local_stat) = call_expr.get_parent::<LuaLocalStat>() else {
        return false;
    };

    for discriminant_slot in
        local_stat
            .get_local_name_list()
            .enumerate()
            .filter_map(|(index, local_name)| {
                let decl_id =
                    LuaDeclId::new(semantic_model.get_file_id(), local_name.get_position());
                let decl = semantic_model
                    .get_db()
                    .get_decl_index()
                    .get_decl(&decl_id)?;
                let initializer = decl.get_initializer()?;
                if initializer.get_expr_syntax_id() == call_expr.get_syntax_id()
                    && signature.return_correlation_implies(initializer.get_ret_idx(), implied_slot)
                {
                    Some((index, decl_id, initializer.get_ret_idx()))
                } else {
                    None
                }
            })
    {
        let (_, discriminant_decl_id, _) = discriminant_slot;
        if expr_has_prior_correlated_return_guard(semantic_model, expr, discriminant_decl_id) {
            return true;
        }
    }

    false
}

fn local_call_initializer_slot(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> Option<(LuaCallExpr, usize)> {
    if !matches!(expr, LuaExpr::NameExpr(_)) {
        return None;
    }

    let LuaSemanticDeclId::LuaDecl(decl_id) =
        semantic_model.find_decl(expr.syntax().clone().into(), SemanticDeclLevel::default())?
    else {
        return None;
    };
    let db = semantic_model.get_db();
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    let tree = db.get_vfs().get_syntax_tree(&decl_id.file_id)?;
    let root = tree.get_red_root();
    let node = initializer.get_expr_syntax_id().to_node_from_root(&root)?;
    let call_expr = LuaCallExpr::cast(node)?;
    Some((call_expr, initializer.get_ret_idx()))
}

fn expr_has_prior_correlated_return_guard(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
    discriminant_decl_id: LuaDeclId,
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

    let scan_scopes = preceding_sibling_scan_scopes(&containing_stat);
    for (scope_index, (parent, stat_start)) in scan_scopes.iter().enumerate() {
        for sibling in parent.children() {
            if sibling.text_range().start() >= *stat_start {
                break;
            }

            let kind: LuaSyntaxKind = sibling.kind().into();
            if kind != LuaSyntaxKind::IfStat {
                continue;
            }

            let Some(if_stat) = LuaIfStat::cast(sibling) else {
                continue;
            };
            if !if_body_exits(&if_stat) {
                continue;
            }
            let Some(condition) = if_stat.get_condition_expr() else {
                continue;
            };
            let Some(discriminant_expr) =
                negative_guard_operand_for_decl(semantic_model, &condition, discriminant_decl_id)
            else {
                continue;
            };

            if !guard_continuing_clauses_reassign_guarded_expr(
                semantic_model,
                &discriminant_expr,
                &if_stat,
            ) && !guard_continuing_clauses_reassign_guarded_expr(semantic_model, expr, &if_stat)
                && !guarded_expr_reassigned_between(
                    semantic_model,
                    &discriminant_expr,
                    parent,
                    if_stat.syntax().text_range(),
                    *stat_start,
                )
                && !guarded_expr_reassigned_between(
                    semantic_model,
                    expr,
                    parent,
                    if_stat.syntax().text_range(),
                    *stat_start,
                )
                && !descendant_path_reassigns_guarded_expr(
                    semantic_model,
                    &discriminant_expr,
                    &scan_scopes,
                    scope_index,
                )
                && !descendant_path_reassigns_guarded_expr(
                    semantic_model,
                    expr,
                    &scan_scopes,
                    scope_index,
                )
            {
                return true;
            }
        }
    }

    false
}

fn if_body_exits(if_stat: &LuaIfStat) -> bool {
    if_stat.get_block().is_some_and(|block| {
        block.syntax().children().any(|child| {
            LuaSyntaxKind::from(child.kind()) == LuaSyntaxKind::ReturnStat
                || LuaCallExprStat::cast(child)
                    .and_then(|stat| stat.get_call_expr())
                    .is_some_and(|call_expr| call_expr.is_error())
        })
    })
}

fn negative_guard_operand_for_decl(
    semantic_model: &SemanticModel,
    condition: &LuaExpr,
    decl_id: LuaDeclId,
) -> Option<LuaExpr> {
    match condition {
        LuaExpr::UnaryExpr(unary_expr) => {
            if !unary_expr
                .get_op_token()
                .is_some_and(|token| token.get_op() == UnaryOperator::OpNot)
            {
                return None;
            }
            let inner_expr = unary_expr.get_expr()?;
            if !is_stable_guard_expr(&inner_expr) {
                return None;
            }
            let LuaSemanticDeclId::LuaDecl(inner_decl_id) = semantic_model.find_decl(
                inner_expr.syntax().clone().into(),
                SemanticDeclLevel::default(),
            )?
            else {
                return None;
            };
            (inner_decl_id == decl_id).then_some(inner_expr)
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let op = binary_expr.get_op_token().map(|token| token.get_op())?;
            if op != BinaryOperator::OpOr {
                return None;
            }
            let (left, right) = binary_expr.get_exprs()?;
            negative_guard_operand_for_decl(semantic_model, &left, decl_id)
                .or_else(|| negative_guard_operand_for_decl(semantic_model, &right, decl_id))
        }
        LuaExpr::ParenExpr(paren_expr) => {
            negative_guard_operand_for_decl(semantic_model, &paren_expr.get_expr()?, decl_id)
        }
        _ => None,
    }
}

/// Walks prior sibling `if` statements to find an early-return guard for `expr`.
/// The `condition_matches` closure determines whether the if-condition matches
/// the guarded expression (e.g. via a TypeGuard call or a general negation check).
fn is_expr_guarded_by_prior_early_return<F>(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
    condition_matches: F,
) -> bool
where
    F: Fn(&LuaExpr, &LuaExpr) -> bool,
{
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

    let scan_scopes = preceding_sibling_scan_scopes(&containing_stat);
    for (scope_index, (parent, stat_start)) in scan_scopes.iter().enumerate() {
        for sibling in parent.children() {
            if sibling.text_range().start() >= *stat_start {
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
            if condition_matches(&condition, expr)
                && !guard_continuing_clauses_reassign_guarded_expr(semantic_model, expr, &if_stat)
                && !guarded_expr_reassigned_between(
                    semantic_model,
                    expr,
                    parent,
                    if_stat.syntax().text_range(),
                    *stat_start,
                )
                && !descendant_path_reassigns_guarded_expr(
                    semantic_model,
                    expr,
                    &scan_scopes,
                    scope_index,
                )
            {
                return true;
            }
        }
    }

    false
}

fn preceding_sibling_scan_scopes(
    containing_stat: &LuaSyntaxNode,
) -> Vec<(LuaSyntaxNode, rowan::TextSize)> {
    let mut scopes = Vec::new();
    let mut current = containing_stat.clone();

    while let Some(parent) = current.parent() {
        if LuaSyntaxKind::from(parent.kind()) == LuaSyntaxKind::Block {
            scopes.push((parent.clone(), current.text_range().start()));
        }
        current = parent;
    }

    scopes
}

fn descendant_path_reassigns_guarded_expr(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    scan_scopes: &[(LuaSyntaxNode, rowan::TextSize)],
    guard_scope_index: usize,
) -> bool {
    scan_scopes
        .iter()
        .take(guard_scope_index)
        .any(|(parent, stat_start)| {
            guarded_expr_reassigned_before_stat(semantic_model, guarded_expr, parent, *stat_start)
        })
}

fn guarded_expr_reassigned_before_stat(
    semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    parent: &LuaSyntaxNode,
    stat_start: rowan::TextSize,
) -> bool {
    for sibling in parent.children() {
        if sibling.text_range().start() >= stat_start {
            break;
        }

        if let Some(assign_stat) = LuaAssignStat::cast(sibling.clone())
            && assign_stat_reassigns_guarded_expr(semantic_model, &assign_stat, guarded_expr)
        {
            return true;
        }

        if let Some(local_stat) = LuaLocalStat::cast(sibling.clone())
            && local_stat_shadows_guarded_expr(semantic_model, &local_stat, guarded_expr)
        {
            return true;
        }

        if let Some(local_func_stat) = LuaLocalFuncStat::cast(sibling.clone())
            && local_func_stat_shadows_guarded_expr(semantic_model, &local_func_stat, guarded_expr)
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

        // Local declarations shadow the guarded name and invalidate the guard.
        // `local x = nil` after `if not x then return end` means the guard
        // proved the old binding, not the new local.
        // Only direct siblings are checked — locals inside nested blocks
        // (do...end, if...end) have their own scope and don't shadow the
        // outer scope at the guarded access site.
        if let Some(local_stat) = LuaLocalStat::cast(sibling.clone())
            && local_stat_shadows_guarded_expr(semantic_model, &local_stat, guarded_expr)
        {
            return true;
        }
        if let Some(local_func_stat) = LuaLocalFuncStat::cast(sibling.clone())
            && local_func_stat_shadows_guarded_expr(semantic_model, &local_func_stat, guarded_expr)
        {
            return true;
        }

        // Assignments inside nested blocks can still reassign the guarded
        // expression (e.g. `if cond then ent = nil end`), so scan descendants
        // for assignments — but NOT for local declarations.
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

/// Check if a `local` declaration shadows the guarded expression's root name.
/// This invalidates the guard because the guard proved the old binding.
fn local_stat_shadows_guarded_expr(
    semantic_model: &SemanticModel,
    local_stat: &LuaLocalStat,
    guarded_expr: &LuaExpr,
) -> bool {
    // Extract the root name from the guarded expression.
    // For `x`, root is `x`. For `t.field`, root is `t`. For `self.x`, root is `self`.
    let Some(root_text) = root_name_text(guarded_expr) else {
        return false;
    };

    // Check if any local name in the declaration matches the root.
    local_stat.get_local_name_list().any(|local_name| {
        local_name.get_name_token().is_some_and(|token| {
            let name = token.get_name_text();
            name == root_text
                || guarded_expr_key_references_name(semantic_model, guarded_expr, &name)
        })
    })
}

fn expr_has_invalidated_prior_nil_early_return(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> bool {
    if !expr_contains_dynamic_key(expr) {
        return false;
    }
    expr_has_prior_early_return_matching(semantic_model, expr, |condition, guarded| {
        condition_is_negative_expr_guard(condition, guarded)
    })
}

fn expr_contains_dynamic_key(expr: &LuaExpr) -> bool {
    let mut current = expr.clone();
    while let LuaExpr::IndexExpr(index_expr) = current {
        if matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_))) {
            return true;
        }
        let Some(prefix) = index_expr.get_prefix_expr() else {
            return false;
        };
        current = prefix;
    }
    false
}

fn expr_has_prior_early_return_matching<F>(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
    condition_matches: F,
) -> bool
where
    F: Fn(&LuaExpr, &LuaExpr) -> bool,
{
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

    let scan_scopes = preceding_sibling_scan_scopes(&containing_stat);
    for (scope_index, (parent, stat_start)) in scan_scopes.iter().enumerate() {
        for sibling in parent.children() {
            if sibling.text_range().start() >= *stat_start {
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
            if condition_matches(&condition, expr)
                && (guard_continuing_clauses_reassign_guarded_expr(semantic_model, expr, &if_stat)
                    || guarded_expr_reassigned_between(
                        semantic_model,
                        expr,
                        parent,
                        if_stat.syntax().text_range(),
                        *stat_start,
                    )
                    || descendant_path_reassigns_guarded_expr(
                        semantic_model,
                        expr,
                        &scan_scopes,
                        scope_index,
                    ))
            {
                return true;
            }
        }
    }

    false
}

fn local_func_stat_shadows_guarded_expr(
    semantic_model: &SemanticModel,
    local_func_stat: &LuaLocalFuncStat,
    guarded_expr: &LuaExpr,
) -> bool {
    let Some(local_name) = local_func_stat.get_local_name() else {
        return false;
    };
    local_name.get_name_token().is_some_and(|token| {
        let name = token.get_name_text();
        root_name_text(guarded_expr).is_some_and(|root_text| root_text == name)
            || guarded_expr_key_references_name(semantic_model, guarded_expr, &name)
    })
}

/// Extract the root name text from an expression.
/// For `x` → `x`, for `t.field` → `t`, for `self.x` → `self`,
/// for `t[i]` → `t`.
fn root_name_text(expr: &LuaExpr) -> Option<String> {
    let mut current = expr.clone();
    loop {
        match &current {
            LuaExpr::NameExpr(name_expr) => {
                return Some(name_expr.get_name_token()?.get_name_text().to_string());
            }
            LuaExpr::IndexExpr(index_expr) => {
                current = index_expr.get_prefix_expr()?;
            }
            _ => return None,
        }
    }
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
    if assigned_index_root_references_guarded_name(semantic_model, assigned_expr, guarded_expr) {
        return false;
    }

    if exprs_reference_same_var(semantic_model, assigned_expr, guarded_expr) {
        return true;
    }

    // Text-based fallback for dynamic-key index expressions where structural
    // matching fails. If the assigned expression text-matches the guarded
    // expression (or any of its prefixes), the guard is invalidated.
    if expr_text_matches(assigned_expr, guarded_expr) {
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
        if expr_text_matches(assigned_expr, &prefix) {
            return true;
        }
        // If the index key variable is reassigned, the guard is invalidated:
        // `if not t[i] then return end; i = i + 1; t[i].field` — the new key
        // was not checked by the guard.
        if index_expr_key_reassigned(semantic_model, assigned_expr, &index_expr) {
            return true;
        }
        current = prefix;
    }

    false
}

fn assigned_index_root_references_guarded_name(
    semantic_model: &SemanticModel,
    assigned_expr: &LuaExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    if !matches!(guarded_expr, LuaExpr::NameExpr(_)) {
        return false;
    }

    let LuaExpr::IndexExpr(index_expr) = assigned_expr else {
        return false;
    };
    let Some(mut root) = index_expr.get_prefix_expr() else {
        return false;
    };

    while let LuaExpr::IndexExpr(prefix_index) = root {
        let Some(prefix) = prefix_index.get_prefix_expr() else {
            return false;
        };
        root = prefix;
    }

    exprs_reference_same_var(semantic_model, &root, guarded_expr)
}

/// Check if the assigned expression is referenced inside the index key.
/// For `t[i]`, if `i` is reassigned, the guard on `t[i]` is invalidated.
/// For `t[i + 1]`, reassigning `i` also invalidates because `i` is
/// referenced inside the compound key expression.
fn index_expr_key_reassigned(
    semantic_model: &SemanticModel,
    assigned_expr: &LuaExpr,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(key) = index_expr.get_index_key() else {
        return false;
    };
    let LuaIndexKey::Expr(key_expr) = key else {
        // Literal keys (Name/String/Integer) can't be invalidated by reassignment
        return false;
    };
    // First check exact match (fast path).
    if exprs_reference_same_var(semantic_model, assigned_expr, &key_expr) {
        return true;
    };
    // Then check if the assigned variable is referenced anywhere inside the
    // key expression (e.g. `i + 1` references `i`). Walk all NameExpr
    // descendants and compare via semantic var/ref identity.
    assigned_var_referenced_in_expr(semantic_model, assigned_expr, &key_expr)
}

fn guarded_expr_key_references_name(
    _semantic_model: &SemanticModel,
    guarded_expr: &LuaExpr,
    local_name: &str,
) -> bool {
    let mut current = guarded_expr.clone();
    while let LuaExpr::IndexExpr(index_expr) = current {
        if index_expr_key_references_name(&index_expr, local_name) {
            return true;
        }
        let Some(prefix) = index_expr.get_prefix_expr() else {
            return false;
        };
        current = prefix;
    }
    false
}

fn index_expr_key_references_name(index_expr: &LuaIndexExpr, local_name: &str) -> bool {
    let Some(LuaIndexKey::Expr(key_expr)) = index_expr.get_index_key() else {
        return false;
    };
    key_expr.syntax().descendants().any(|node| {
        LuaNameExpr::cast(node).is_some_and(|name_expr| {
            name_expr
                .get_name_text()
                .is_some_and(|name| name == local_name)
        })
    })
}

/// Check if the variable referenced by `assigned_expr` appears anywhere inside `expr`.
/// This catches cases like `i = i + 1` invalidating `t[i + 1]` where the
/// key expression `i + 1` contains a reference to `i`.
///
/// The descendant walk includes both `NameExpr` and `IndexExpr` nodes.
/// The `IndexExpr` branch is defensive hardening of the semantic path:
/// when the assigned expression resolves to a stable ref id (e.g. an
/// `IndexRef` for `self.Key`), indexed references inside the key expression
/// are compared semantically. No current test scenario reaches this branch
/// with a differing outcome (reverting to `NameExpr`-only leaves the full
/// suite green), but the widening only ever *adds* invalidations, which is
/// the soundness-safe direction.
///
/// Uses semantic var/ref identity for the comparison, with text fallback only
/// when the assigned expression doesn't resolve to a var ref.
fn assigned_var_referenced_in_expr(
    semantic_model: &SemanticModel,
    assigned_expr: &LuaExpr,
    expr: &LuaExpr,
) -> bool {
    // Try to resolve the assigned expression to a var ref id.
    let mut cache = semantic_model.get_cache().borrow_mut();
    let assigned_ref_id =
        get_var_expr_var_ref_id(semantic_model.get_db(), &mut cache, assigned_expr.clone());
    drop(cache);

    // Extract root name text as a fallback for non-resolvable expressions.
    let assigned_expr_text = normalized_expr_text(assigned_expr);
    let assigned_name_text = root_name_text(assigned_expr);

    // Walk all descendant expressions that can reference a variable:
    // both NameExpr (local/global refs) and IndexExpr (field/table refs).
    // Comparing only NameExpr would miss `self.Key` inside `self.Objects[self.Key + 1]`.
    expr.syntax()
        .descendants()
        .filter_map(LuaExpr::cast)
        .filter(|descendant_expr| {
            matches!(
                descendant_expr,
                LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_)
            )
        })
        .any(|descendant_expr| {
            if normalized_expr_text(&descendant_expr)
                .as_deref()
                .is_some_and(|text| {
                    assigned_expr_text
                        .as_deref()
                        .is_some_and(|assigned| assigned == text)
                })
            {
                return true;
            }
            if let Some(ref assigned_ref_id) = assigned_ref_id {
                // Semantic comparison: resolve the descendant expr and compare ref ids.
                let mut cache = semantic_model.get_cache().borrow_mut();
                let descendant_ref_id = get_var_expr_var_ref_id(
                    semantic_model.get_db(),
                    &mut cache,
                    descendant_expr.clone(),
                );
                drop(cache);
                descendant_ref_id.is_some_and(|id| &id == assigned_ref_id)
            } else {
                // Text fallback: only when semantic resolution fails.
                let descendant_text = root_name_text(&descendant_expr);
                assigned_name_text
                    .as_deref()
                    .is_some_and(|text| descendant_text.as_deref().is_some_and(|dt| dt == text))
            }
        })
}

fn if_body_has_return(if_stat: &LuaIfStat) -> bool {
    if_stat.get_block().is_some_and(|block| {
        block
            .syntax()
            .children()
            .any(|child| LuaSyntaxKind::from(child.kind()) == LuaSyntaxKind::ReturnStat)
    })
}

fn condition_is_negative_type_guard(
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
                    is_string_registry_exists_call_guarding_reference_expr(
                        semantic_model,
                        &call_expr,
                        guarded_expr,
                    ) || is_type_guard_call_guarding_expr(semantic_model, &call_expr, guarded_expr)
                        || is_self_guard_call_guarding_expr(
                            semantic_model,
                            &call_expr,
                            guarded_expr,
                        )
                }
                LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
                    condition_is_negative_type_guard(semantic_model, &expr, guarded_expr)
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
            condition_is_negative_type_guard(semantic_model, &left, guarded_expr)
                || condition_is_negative_type_guard(semantic_model, &right, guarded_expr)
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
            condition_is_negative_type_guard(semantic_model, &expr, guarded_expr)
        }),
        _ => false,
    }
}

fn condition_is_negative_null_excluding_type_guard(
    semantic_model: &SemanticModel,
    condition: &LuaExpr,
    guarded_expr: &LuaExpr,
    guarded_type: &LuaType,
) -> bool {
    match condition {
        LuaExpr::UnaryExpr(unary_expr) => {
            let Some(op) = unary_expr.get_op_token() else {
                return false;
            };
            if op.get_op() != UnaryOperator::OpNot {
                return false;
            }
            let Some(inner_expr) = unary_expr.get_expr() else {
                return false;
            };
            match inner_expr {
                LuaExpr::CallExpr(call_expr) => is_null_excluding_type_guard_call_guarding_expr(
                    semantic_model,
                    &call_expr,
                    guarded_expr,
                    guarded_type,
                ),
                LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
                    condition_is_negative_null_excluding_type_guard(
                        semantic_model,
                        &expr,
                        guarded_expr,
                        guarded_type,
                    )
                }),
                _ => false,
            }
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let Some(op) = binary_expr.get_op_token().map(|op| op.get_op()) else {
                return false;
            };
            if op != BinaryOperator::OpOr {
                return false;
            }
            let Some((left, right)) = binary_expr.get_exprs() else {
                return false;
            };
            condition_is_negative_null_excluding_type_guard(
                semantic_model,
                &left,
                guarded_expr,
                guarded_type,
            ) || condition_is_negative_null_excluding_type_guard(
                semantic_model,
                &right,
                guarded_expr,
                guarded_type,
            )
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
            condition_is_negative_null_excluding_type_guard(
                semantic_model,
                &expr,
                guarded_expr,
                guarded_type,
            )
        }),
        _ => false,
    }
}

fn condition_is_positive_null_excluding_type_guard(
    semantic_model: &SemanticModel,
    condition: &LuaExpr,
    guarded_expr: &LuaExpr,
    guarded_type: &LuaType,
) -> bool {
    match condition {
        LuaExpr::CallExpr(call_expr) => is_null_excluding_type_guard_call_guarding_expr(
            semantic_model,
            call_expr,
            guarded_expr,
            guarded_type,
        ),
        LuaExpr::BinaryExpr(binary_expr) => {
            let Some(op) = binary_expr.get_op_token().map(|op| op.get_op()) else {
                return false;
            };
            if op != BinaryOperator::OpAnd {
                return false;
            }
            let Some((left, right)) = binary_expr.get_exprs() else {
                return false;
            };
            condition_is_positive_null_excluding_type_guard(
                semantic_model,
                &left,
                guarded_expr,
                guarded_type,
            ) || condition_is_positive_null_excluding_type_guard(
                semantic_model,
                &right,
                guarded_expr,
                guarded_type,
            )
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr.get_expr().is_some_and(|expr| {
            condition_is_positive_null_excluding_type_guard(
                semantic_model,
                &expr,
                guarded_expr,
                guarded_type,
            )
        }),
        _ => false,
    }
}

/// Check if `condition` is a negated expression (`not <expr>`) where `<expr>`
/// textually matches `guarded_expr`. This is the general form of the nil
/// guard: `if not self.Objects[i] then return end` should suppress nil
/// diagnostics on the subsequent `self.Objects[i]` access.
///
/// Text-based matching is used instead of var_ref_id because dynamic-key
/// index expressions like `t[k]` (where `k` is not a compile-time constant)
/// do not produce stable var_ref_ids, so the structural matching path in
/// `exprs_reference_same_var` fails for them.
fn condition_is_negative_expr_guard(condition: &LuaExpr, guarded_expr: &LuaExpr) -> bool {
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
            // Only stable expressions are safe to guard: a CallExpr
            // (e.g. `maybeEnt()`) may return a different value on each
            // invocation, so `not maybeEnt()` proves nothing about the
            // next `maybeEnt()` call. NameExpr and IndexExpr reference a
            // fixed variable/field and are stable across reads. A negated
            // conjunction (`not (a and b)`) with an early return proves each
            // stable operand truthy on the continuing path.
            truthy_conjunction_operand_matches(&inner_expr, guarded_expr)
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
            condition_is_negative_expr_guard(&left, guarded_expr)
                || condition_is_negative_expr_guard(&right, guarded_expr)
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|expr| condition_is_negative_expr_guard(&expr, guarded_expr)),
        _ => false,
    }
}

fn negated_conjunction_guards_expr(binary_expr: &LuaBinaryExpr, guarded_expr: &LuaExpr) -> bool {
    let Some(op) = binary_expr.get_op_token().map(|token| token.get_op()) else {
        return false;
    };
    if op != BinaryOperator::OpAnd {
        return false;
    }

    let Some((left, right)) = binary_expr.get_exprs() else {
        return false;
    };

    truthy_conjunction_operand_matches(&left, guarded_expr)
        || truthy_conjunction_operand_matches(&right, guarded_expr)
}

fn truthy_conjunction_operand_matches(expr: &LuaExpr, guarded_expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|expr| truthy_conjunction_operand_matches(&expr, guarded_expr)),
        LuaExpr::BinaryExpr(binary_expr) => {
            negated_conjunction_guards_expr(binary_expr, guarded_expr)
        }
        LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_) => expr_text_matches(expr, guarded_expr),
        _ => false,
    }
}

/// Text-based comparison of two expressions, normalizing `:` to `.` for
/// method-vs-field equivalence.
///
/// `a` is the condition's inner expression (the negated expr).
/// `b` is the guarded expression (the nullable receiver/prefix).
///
/// Also matches when `b` is a prefix of `a`: `not self.Objects[i]` guards
/// `self.Objects` because accessing `self.Objects[i]` would error if
/// `self.Objects` were nil. The reverse direction is NOT sound: checking
/// `not self.Objects` does not prove `self.Objects[i]` is non-nil, since
/// the indexed value may be absent even when the table exists.
fn expr_text_matches(a: &LuaExpr, b: &LuaExpr) -> bool {
    let Some(a_text) = normalized_expr_text(a) else {
        return false;
    };
    let Some(b_text) = normalized_expr_text(b) else {
        return false;
    };
    if a_text == b_text {
        return true;
    }
    // Prefix match: condition (`a`) extends guarded (`b`) with an index access.
    // `self.Objects` (guarded) is a prefix of `self.Objects[i]` (condition),
    // meaning the condition accessed the guarded table — so the table must
    // have been non-nil for the condition to have evaluated.
    a_text.len() > b_text.len()
        && a_text.starts_with(&b_text)
        && (a_text[b_text.len()..].starts_with('[') || a_text[b_text.len()..].starts_with('.'))
}

fn normalized_expr_text(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .and_then(|inner| normalized_expr_text(&inner)),
        _ => Some(expr.syntax().text().to_string().replacen(':', ".", 1)),
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

fn is_non_nullable_receiver_self_type_guard(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    receiver: &LuaExpr,
) -> bool {
    if !call_expr.is_colon_call()
        || !is_type_guard_call_guarding_expr(semantic_model, call_expr, receiver)
    {
        return false;
    }

    semantic_model
        .infer_expr(receiver.clone())
        .is_ok_and(|receiver_type| !receiver_type.is_nullable())
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
    if is_isvalid_call_guarding_stable_expr(semantic_model, guard_call, receiver) {
        return true;
    }

    if !call_returns_non_nullable_type_guard(semantic_model, guard_call) {
        return false;
    };

    if guard_call.is_colon_call()
        && let Some(LuaExpr::IndexExpr(index_expr)) = guard_call.get_prefix_expr()
        && let Some(self_expr) = index_expr.get_prefix_expr()
    {
        if !is_stable_guard_expr(&self_expr) || !is_stable_guard_expr(receiver) {
            return false;
        }
        return exprs_reference_same_var_or_text(semantic_model, &self_expr, receiver);
    }

    let Some(first_arg) = guard_call
        .get_args_list()
        .and_then(|args| args.get_args().next())
    else {
        return false;
    };

    is_stable_guard_expr(&first_arg)
        && is_stable_guard_expr(receiver)
        && exprs_reference_same_var_or_text(semantic_model, &first_arg, receiver)
}

fn is_string_registry_exists_call_guarding_reference_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    let Some((guard_domain, guard_key)) =
        static_string_call_arg_role(semantic_model, guard_call, &[GMOD_ROLE_EXISTS])
    else {
        return false;
    };

    let LuaExpr::CallExpr(reference_call) = guarded_expr else {
        return false;
    };
    let Some((reference_domain, reference_key)) =
        static_string_call_arg_role(semantic_model, reference_call, &[GMOD_ROLE_REFERENCE])
    else {
        return false;
    };

    guard_domain == reference_domain && guard_key == reference_key
}

fn static_string_call_arg_role(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    roles: &[&str],
) -> Option<(String, String)> {
    // Batch 1 intentionally supports non-method registry functions only: raw
    // call argument indices match signature parameter indices for these calls.
    if call_expr.is_colon_call() {
        return None;
    }

    let prefix_expr = call_expr.get_prefix_expr()?;
    let callee_type = semantic_model.infer_expr(prefix_expr).ok()?;
    let db = semantic_model.get_db();
    let args = call_expr.get_args_list()?;
    for (arg_idx, _) in args.get_args().enumerate() {
        let Some(key) = crate::ast_util::literal_string_arg_value(call_expr, arg_idx) else {
            continue;
        };
        for domain in GMOD_CALL_ARG_DOMAINS {
            if let Some(role) =
                find_best_call_arg_role_from_type(db, &callee_type, arg_idx, domain, roles)
            {
                return Some((role.domain.to_string(), key));
            }
        }
    }

    None
}

fn is_isvalid_call_guarding_stable_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    let Some(signature_id) = call_signature_id(semantic_model, guard_call) else {
        return false;
    };
    if !is_isvalid_call(guard_call)
        || !call_signature_is_valid_guard(semantic_model, guard_call, signature_id)
            && !call_returns_non_nullable_type_guard(semantic_model, guard_call)
        || !is_stable_guard_expr(guarded_expr)
    {
        return false;
    }

    let Some(first_arg) = guard_call
        .get_args_list()
        .and_then(|args| args.get_args().next())
    else {
        return false;
    };

    is_stable_guard_expr(&first_arg)
        && exprs_reference_same_var_or_text(semantic_model, &first_arg, guarded_expr)
}

fn call_signature_is_valid_guard(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> bool {
    let call_realm = semantic_model
        .get_db()
        .get_gmod_infer_index()
        .get_realm_at_offset(&semantic_model.get_file_id(), call_expr.get_position());
    signature_is_valid_guard_or_base_runtime_isvalid_in_realm(
        semantic_model.get_db(),
        signature_id,
        call_realm,
    )
}

fn call_signature_has_valid_guard_metadata(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> bool {
    let call_realm = semantic_model
        .get_db()
        .get_gmod_infer_index()
        .get_realm_at_offset(&semantic_model.get_file_id(), call_expr.get_position());
    signature_is_valid_guard_in_realm(semantic_model.get_db(), signature_id, call_realm)
}

fn is_self_guard_call_guarding_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    if !guard_call.is_colon_call() {
        return false;
    }

    let Some(LuaExpr::IndexExpr(index_expr)) = guard_call.get_prefix_expr() else {
        return false;
    };
    let Some(self_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    if !is_stable_guard_expr(&self_expr) || !is_stable_guard_expr(guarded_expr) {
        return false;
    }
    if !exprs_reference_same_var_or_text(semantic_model, &self_expr, guarded_expr) {
        return false;
    }

    let Some(member_key) = index_expr
        .get_index_key()
        .and_then(|key| semantic_model.get_member_key(&key))
    else {
        return false;
    };

    if let Ok(receiver_type) = semantic_model.infer_expr(self_expr) {
        let non_nil_receiver_type = remove_false_or_nil(receiver_type);
        if semantic_model
            .infer_member_type(&non_nil_receiver_type, &member_key)
            .is_ok_and(|typ| type_returns_non_nullable_type_guard(semantic_model, &typ))
        {
            return true;
        }
    }

    indexed_member_key_returns_non_nullable_type_guard(semantic_model, &member_key)
}

fn is_null_excluding_type_guard_call_guarding_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    guarded_expr: &LuaExpr,
    guarded_type: &LuaType,
) -> bool {
    if is_self_guard_call_guarding_expr(semantic_model, guard_call, guarded_expr) {
        return true;
    }

    if !is_type_guard_call_guarding_expr(semantic_model, guard_call, guarded_expr) {
        return false;
    }

    let db = semantic_model.get_db();
    if is_isvalid_call(guard_call) && contains_gmod_null_type(db, guarded_type) {
        // Global `IsValid` is the canonical GLua NULL-excluding validity guard.
        // Generated annotations currently model it as `TypeGuard<Entity>` without
        // a scoped `return_cast ent -NULL`; keep this local fallback constrained by
        // the TypeGuard/same-expression checks above until that metadata exists.
        return true;
    }

    let Some((signature_id, signature_cast)) = call_signature_cast(semantic_model, guard_call)
    else {
        return false;
    };
    if !signature_cast_targets_guarded_expr(
        semantic_model,
        guard_call,
        signature_id,
        signature_cast,
        guarded_expr,
    ) {
        return false;
    }

    let Some(syntax_tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return false;
    };
    let signature_root = syntax_tree.get_chunk_node();
    let Some(cast_op_type) = signature_cast.cast.to_node(&signature_root) else {
        return false;
    };
    let Ok(casted_type) = cast_type(
        db,
        signature_id.get_file_id(),
        cast_op_type,
        guarded_type.clone(),
        InferConditionFlow::TrueCondition,
    ) else {
        return false;
    };

    !contains_gmod_null_type(db, &casted_type)
}

fn is_isvalid_call(guard_call: &LuaCallExpr) -> bool {
    let Some(prefix) = guard_call.get_prefix_expr() else {
        return false;
    };

    normalized_expr_text(&prefix).is_some_and(|text| text == "IsValid" || text == "_G.IsValid")
}

fn call_signature_cast<'a>(
    semantic_model: &'a SemanticModel,
    guard_call: &LuaCallExpr,
) -> Option<(LuaSignatureId, &'a LuaSignatureCast)> {
    let signature_id = call_signature_id(semantic_model, guard_call)?;
    let signature_cast = semantic_model
        .get_db()
        .get_flow_index()
        .get_signature_cast(&signature_id)?;
    Some((signature_id, signature_cast))
}

fn call_signature_id(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
) -> Option<LuaSignatureId> {
    let prefix = guard_call.get_prefix_expr()?;
    if let Ok(LuaType::Signature(signature_id)) = semantic_model.infer_expr(prefix.clone()) {
        return Some(signature_id);
    }

    let semantic_decl =
        semantic_model.find_decl(prefix.syntax().clone().into(), SemanticDeclLevel::default())?;
    signature_id_from_semantic_decl(semantic_model, semantic_decl)
}

fn signature_id_from_semantic_decl(
    semantic_model: &SemanticModel,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    let db = semantic_model.get_db();
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return Some(signature_id);
    }

    match &semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&(*decl_id).into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
        }
        LuaSemanticDeclId::Member(member_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&(*member_id).into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
        }
        LuaSemanticDeclId::Signature(signature_id) => return Some(*signature_id),
        LuaSemanticDeclId::TypeDecl(_) => return None,
    }

    let file_id = match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => decl_id.file_id,
        LuaSemanticDeclId::Member(member_id) => member_id.file_id,
        LuaSemanticDeclId::Signature(signature_id) => return Some(signature_id),
        LuaSemanticDeclId::TypeDecl(_) => return None,
    };

    let LuaExpr::ClosureExpr(closure) = semantic_decl_value_expr(semantic_model, semantic_decl)?
    else {
        return None;
    };
    Some(LuaSignatureId::from_closure(file_id, &closure))
}

fn semantic_decl_value_expr(
    semantic_model: &SemanticModel,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaExpr> {
    let db = semantic_model.get_db();
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            let value_syntax_id = decl.get_value_syntax_id()?;
            let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
            LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)
        }
        LuaSemanticDeclId::Member(member_id) => get_member_value_expr(db, member_id),
        LuaSemanticDeclId::Signature(_) | LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn signature_cast_targets_guarded_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    signature_id: LuaSignatureId,
    signature_cast: &LuaSignatureCast,
    guarded_expr: &LuaExpr,
) -> bool {
    if signature_cast.name == "self" {
        let Some(LuaExpr::IndexExpr(index_expr)) = guard_call.get_prefix_expr() else {
            return false;
        };
        let Some(self_expr) = index_expr.get_prefix_expr() else {
            return false;
        };
        return exprs_reference_same_var_or_text(semantic_model, &self_expr, guarded_expr);
    }

    let Some(arg_list) = guard_call.get_args_list() else {
        return false;
    };
    let Some(signature) = semantic_model
        .get_db()
        .get_signature_index()
        .get(&signature_id)
    else {
        return false;
    };
    let Some(mut param_idx) = signature.find_param_idx(signature_cast.name.as_str()) else {
        return false;
    };

    match (guard_call.is_colon_call(), signature.is_colon_define) {
        (true, false) => {
            if param_idx == 0 {
                return false;
            }

            param_idx -= 1;
        }
        (false, true) => {
            param_idx += 1;
        }
        _ => {}
    }

    arg_list
        .get_args()
        .nth(param_idx)
        .is_some_and(|arg_expr| exprs_reference_same_var(semantic_model, &arg_expr, guarded_expr))
}

fn call_returns_non_nullable_type_guard(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
) -> bool {
    let Some(prefix) = guard_call.get_prefix_expr() else {
        return false;
    };

    if call_signature_id(semantic_model, guard_call).is_some_and(|signature_id| {
        call_signature_has_valid_guard_metadata(semantic_model, guard_call, signature_id)
    }) {
        return true;
    }

    if semantic_model
        .infer_expr(prefix.clone())
        .is_ok_and(|typ| type_returns_non_nullable_type_guard(semantic_model, &typ))
    {
        return true;
    }

    let LuaExpr::IndexExpr(index_expr) = prefix else {
        return false;
    };
    let Some(receiver) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Some(member_key) = index_expr
        .get_index_key()
        .and_then(|key| semantic_model.get_member_key(&key))
    else {
        return false;
    };
    let Ok(receiver_type) = semantic_model.infer_expr(receiver) else {
        return false;
    };
    let non_nil_receiver_type = remove_false_or_nil(receiver_type);

    semantic_model
        .infer_member_type(&non_nil_receiver_type, &member_key)
        .is_ok_and(|typ| type_returns_non_nullable_type_guard(semantic_model, &typ))
        || indexed_member_key_returns_non_nullable_type_guard(semantic_model, &member_key)
}

fn indexed_member_key_returns_non_nullable_type_guard(
    semantic_model: &SemanticModel,
    member_key: &crate::LuaMemberKey,
) -> bool {
    let db = semantic_model.get_db();
    db.get_member_index()
        .get_current_members_for_key(member_key)
        .into_iter()
        .filter_map(|member| {
            db.get_type_index()
                .get_type_cache(&member.get_id().into())
                .map(|type_cache| type_cache.as_type().clone())
        })
        .any(|typ| type_returns_non_nullable_type_guard(semantic_model, &typ))
}

fn type_returns_non_nullable_type_guard(semantic_model: &SemanticModel, typ: &LuaType) -> bool {
    use crate::{GMOD_ATTR_SELF_GUARD, find_signature_attribute_use};

    match typ {
        LuaType::DocFunction(func) => return_type_is_non_nullable_type_guard(func.get_ret()),
        LuaType::Signature(signature_id) => {
            let db = semantic_model.get_db();
            // Check TypeGuard<T> return type first.
            if db
                .get_signature_index()
                .get(signature_id)
                .is_some_and(|sig| return_type_is_non_nullable_type_guard(&sig.get_return_type()))
            {
                return true;
            }
            // A standalone `self_guard` attribute marks the receiver (colon-call self)
            // as a valid guard, so treat the method as a type guard for nil-check purposes.
            if find_signature_attribute_use(db, *signature_id, GMOD_ATTR_SELF_GUARD).is_some() {
                return true;
            }
            // A `return_cast self` annotation explicitly casts the receiver after the call.
            if let Some(cast) = db.get_flow_index().get_signature_cast(signature_id) {
                if cast.name == "self" {
                    return true;
                }
            }
            false
        }
        LuaType::Union(union_type) => union_type
            .types()
            .any(|typ| type_returns_non_nullable_type_guard(semantic_model, typ)),
        LuaType::Intersection(intersection_type) => intersection_type
            .get_types()
            .iter()
            .any(|typ| type_returns_non_nullable_type_guard(semantic_model, typ)),
        _ => false,
    }
}

fn return_type_is_non_nullable_type_guard(return_type: &LuaType) -> bool {
    match return_type {
        LuaType::TypeGuard(inner) => !inner.is_nullable(),
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
            if is_nil_comparison_guarded_by_type_guard(
                semantic_model,
                &binary_expr,
                non_nil_side,
                op,
            ) || is_nil_sentinel_comparison_before_type_guard_elseif(
                semantic_model,
                &binary_expr,
                non_nil_side,
                op,
            ) {
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

        if left_type.is_nullable()
            && !is_expr_guarded_by_prior_nil_early_return(semantic_model, &left)
            && !is_expr_guarded_by_correlated_multi_return(semantic_model, &left)
        {
            context.add_diagnostic(
                DiagnosticCode::NeedCheckNil,
                left.get_range(),
                format!("{name} value may be nil", name = left.syntax().text()).to_string(),
                None,
            );
        }

        let right_type = semantic_model.infer_expr(right.clone()).ok()?;
        if right_type.is_nullable()
            && !is_expr_guarded_by_prior_nil_early_return(semantic_model, &right)
            && !is_expr_guarded_by_correlated_multi_return(semantic_model, &right)
        {
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
                if is_short_circuit_type_guard_condition_guard(semantic_model, op, &left, &right) {
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

fn is_short_circuit_type_guard_condition_guard(
    semantic_model: &SemanticModel,
    op: BinaryOperator,
    left: &LuaExpr,
    right: &LuaExpr,
) -> bool {
    match op {
        BinaryOperator::OpAnd => {
            expr_matches_type_guard_condition_guard(semantic_model, left, right, false)
                || expr_matches_type_guard_condition_guard(semantic_model, right, left, false)
        }
        BinaryOperator::OpOr => {
            expr_matches_type_guard_condition_guard(semantic_model, left, right, true)
                || expr_matches_type_guard_condition_guard(semantic_model, right, left, true)
        }
        _ => false,
    }
}

fn expr_matches_type_guard_condition_guard(
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

    is_type_guard_call_guarding_expr(semantic_model, &guard_call, &truthy_expr)
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

fn is_nil_comparison_guarded_by_type_guard(
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

    is_type_guard_call_guarding_expr(semantic_model, &guard_call, non_nil_side)
}

fn is_nil_sentinel_comparison_before_type_guard_elseif(
    semantic_model: &SemanticModel,
    comparison_expr: &LuaBinaryExpr,
    non_nil_side: &LuaExpr,
    comparison_op: BinaryOperator,
) -> bool {
    if comparison_op != BinaryOperator::OpEq {
        return false;
    }

    let Some(if_stat) = comparison_expr
        .syntax()
        .ancestors()
        .find_map(LuaIfStat::cast)
    else {
        return false;
    };
    if !if_body_has_return(&if_stat) {
        return false;
    }
    let comparison_range = comparison_expr.syntax().text_range();
    if !if_stat
        .get_condition_expr()
        .is_some_and(|condition| range_contains(condition.syntax().text_range(), comparison_range))
    {
        return false;
    }

    if_stat.get_else_if_clause_list().any(|elseif_clause| {
        let Some(condition) = elseif_clause.get_condition_expr() else {
            return false;
        };
        let Some(guard_call) = unwrap_paren_call(condition) else {
            return false;
        };

        is_type_guard_call_guarding_expr(semantic_model, &guard_call, non_nil_side)
            || type_guard_call_textually_guards_expr(semantic_model, &guard_call, non_nil_side)
    })
}

fn unwrap_paren_call(expr: LuaExpr) -> Option<LuaCallExpr> {
    match expr {
        LuaExpr::CallExpr(call_expr) => Some(call_expr),
        LuaExpr::ParenExpr(paren_expr) => unwrap_paren_call(paren_expr.get_expr()?),
        _ => None,
    }
}

fn type_guard_call_textually_guards_expr(
    semantic_model: &SemanticModel,
    guard_call: &LuaCallExpr,
    guarded_expr: &LuaExpr,
) -> bool {
    let Some(first_arg) = guard_call
        .get_args_list()
        .and_then(|args| args.get_args().next())
    else {
        return false;
    };

    if !exprs_reference_same_var_or_text(semantic_model, &first_arg, guarded_expr) {
        return false;
    }

    // Prefer semantic TypeGuard resolution. The canonical `IsValid` fallback exists
    // only for this returning-sentinel/elseif shape because combined workspace
    // loading can fail to resolve the global signature at shipped-code call sites.
    call_returns_non_nullable_type_guard(semantic_model, guard_call) || is_isvalid_call(guard_call)
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

fn exprs_reference_same_var_or_text(
    semantic_model: &SemanticModel,
    left: &LuaExpr,
    right: &LuaExpr,
) -> bool {
    exprs_reference_same_var(semantic_model, left, right) || expr_text_matches(left, right)
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

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::*;
    use crate::VirtualWorkspace;

    fn expr_by_text(code: &str, text: &str) -> LuaExpr {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file("test.lua", code);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        semantic_model
            .get_root()
            .descendants::<LuaExpr>()
            .find(|expr| normalized_expr_text(expr).as_deref() == Some(text))
            .unwrap_or_else(|| panic!("expected expression `{text}` in `{code}`"))
    }

    #[gtest]
    fn recursive_stable_guard_expr_rejects_calls_in_prefix_or_key() {
        assert_that!(
            is_stable_guard_expr(&expr_by_text("return t[key]", "t[key]")),
            eq(true)
        );
        assert_that!(
            is_stable_guard_expr(&expr_by_text("return t[key + 1]", "t[key + 1]")),
            eq(true)
        );
        assert_that!(
            is_stable_guard_expr(&expr_by_text("return getTable()[key]", "getTable()[key]")),
            eq(false)
        );
        assert_that!(
            is_stable_guard_expr(&expr_by_text("return t[getKey()]", "t[getKey()]")),
            eq(false)
        );
    }
}
