use std::{cell::RefCell, collections::HashMap};

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaClosureExpr, LuaExpr, LuaGotoStat, LuaLiteralToken, LuaLocalName,
    LuaLocalStat, LuaNameExpr, LuaStat,
};

use rowan::TextRange;

use crate::{DiagnosticCode, LuaDeclId, LuaDefinitionId, LuaType, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct UnbalancedAssignmentsChecker;

/// Whether a `goto` exists in a declaring function, keyed by its range (`None` for the chunk).
type GotoScopes = RefCell<HashMap<Option<TextRange>, bool>>;

impl Checker for UnbalancedAssignmentsChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::UnbalancedAssignments];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        let goto_scopes = GotoScopes::default();
        for stat in buckets.statements.iter().cloned() {
            match stat {
                LuaStat::LocalStat(local) => {
                    check_local_stat(context, semantic_model, &local, &goto_scopes);
                }
                LuaStat::AssignStat(assign) => {
                    check_assign_stat(context, semantic_model, &assign);
                }
                _ => {}
            }
        }
    }
}

fn check_assign_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    assign: &LuaAssignStat,
) -> Option<()> {
    let (vars, value_exprs) = assign.get_var_and_expr_list();
    check_unbalanced_assignment(context, semantic_model, &vars, &value_exprs, |_| false)
}

fn check_local_stat(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    local: &LuaLocalStat,
    goto_scopes: &GotoScopes,
) -> Option<()> {
    let vars = local.get_local_name_list().collect::<Vec<_>>();
    let value_exprs = local.get_value_exprs().collect::<Vec<_>>();
    check_unbalanced_assignment(context, semantic_model, &vars, &value_exprs, |var| {
        padded_nil_is_never_read(semantic_model, goto_scopes, var)
    })
}

fn check_unbalanced_assignment<V: LuaAstNode>(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    vars: &[V],
    value_exprs: &[LuaExpr],
    is_padding_unobserved: impl Fn(&V) -> bool,
) -> Option<()> {
    let last_value_expr = match value_exprs.last() {
        Some(expr) => expr,
        None => return Some(()),
    };

    if is_nil_literal(last_value_expr) {
        return Some(());
    }

    if check_last_expr(semantic_model, last_value_expr).unwrap_or(false) {
        return Some(());
    }

    let value_types = semantic_model.infer_expr_list_types(value_exprs, Some(vars.len()));
    if let Some(last_type) = value_types.last()
        && check_last(&last_type.0)
    {
        return Some(());
    }

    let value_len = value_types.len();

    if vars.len() > value_len {
        for var in vars[value_len..]
            .iter()
            .filter(|var| !is_padding_unobserved(var))
        {
            context.add_diagnostic(
                DiagnosticCode::UnbalancedAssignments,
                var.get_range(),
                "The value is assigned as `nil` because the number of values is not enough."
                    .to_string(),
                None,
            );
        }
    }

    Some(())
}

fn is_nil_literal(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::LiteralExpr(literal) => {
            matches!(literal.get_literal(), Some(LuaLiteralToken::Nil(_)))
        }
        LuaExpr::ParenExpr(paren) => paren.get_expr().is_some_and(|inner| is_nil_literal(&inner)),
        _ => false,
    }
}

/// Whether the local is written after its declaration and no read can be
/// reached by the declaration itself, so the padded `nil` is never observed.
/// A `goto` in the declaring function leaves the flow graph without jump edges, so it proves nothing.
fn padded_nil_is_never_read(
    semantic_model: &SemanticModel,
    goto_scopes: &GotoScopes,
    local_name: &LuaLocalName,
) -> bool {
    let db = semantic_model.get_db();
    let file_id = semantic_model.get_file_id();
    let decl_id = LuaDeclId::new(file_id, local_name.get_position());
    let Some(references) = db.get_reference_index().get_decl_references(&file_id, &decl_id) else {
        return false;
    };
    if !references.is_written_after_declaration(decl_id.position) {
        return false;
    }
    if declaring_scope_has_goto(semantic_model, goto_scopes, local_name) {
        return false;
    }
    let Some(flow_tree) = db.get_flow_index().get_flow_tree(&file_id) else {
        return false;
    };

    let root = semantic_model.get_root().syntax();
    references
        .cells
        .iter()
        .filter(|cell| !cell.is_write)
        .all(|cell| {
            root.covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .filter(|name| name.get_range() == cell.range)
                .and_then(|name| flow_tree.get_flow_id(name.get_syntax_id()))
                .is_some_and(|flow| {
                    !flow_tree
                        .reaching_definitions(decl_id, flow)
                        .contains(&LuaDefinitionId::Declaration(decl_id))
                })
        })
}

fn declaring_scope_has_goto(
    semantic_model: &SemanticModel,
    goto_scopes: &GotoScopes,
    local_name: &LuaLocalName,
) -> bool {
    let closure = local_name.ancestors::<LuaClosureExpr>().next();
    let key = closure.as_ref().map(|closure| closure.get_range());
    *goto_scopes.borrow_mut().entry(key).or_insert_with(|| {
        match &closure {
            Some(closure) => closure.descendants::<LuaGotoStat>().next(),
            None => semantic_model.get_root().descendants::<LuaGotoStat>().next(),
        }
        .is_some()
    })
}

fn check_last(last_type: &LuaType) -> bool {
    match last_type {
        LuaType::Instance(instance) => check_last(instance.get_base()),
        _ => false,
    }
}

#[allow(unused)]
fn check_last_expr(semantic_model: &SemanticModel, last_expr: &LuaExpr) -> Option<bool> {
    match last_expr {
        // TODO: establish a separate rule for signature
        LuaExpr::CallExpr(call_expr) => {
            Some(true)
            // For now only pcall and xpcall may disable the check; perhaps the check should be disabled for all function calls?
            // let decl_id = semantic_model.find_decl(
            //     call_expr.get_prefix_expr()?.syntax().clone().into(),
            //     SemanticDeclLevel::Trace(50),
            // )?;
            // if let LuaSemanticDeclId::LuaDecl(decl_id) = decl_id {
            //     let decl = semantic_model
            //         .get_db()
            //         .get_decl_index()
            //         .get_decl(&decl_id)?;

            //     if semantic_model
            //         .get_db()
            //         .get_module_index()
            //         .is_std(&decl.get_file_id())
            //         && (decl.get_name() == "pcall" || decl.get_name() == "xpcall")
            //     {
            //         return Some(true);
            //     }
            // }
            // None
        }
        _ => None,
    }
}
