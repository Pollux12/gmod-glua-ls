use std::collections::HashSet;

use glua_parser::{
    BinaryOperator, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaIfStat, LuaIndexKey,
    LuaLiteralToken, LuaNameExpr, UnaryOperator,
};
use rowan::TextRange;

use crate::{DiagnosticCode, LuaSignatureId, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct UndefinedGlobalChecker;

impl Checker for UndefinedGlobalChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::UndefinedGlobal];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let mut use_range_set = HashSet::new();
        let guarded_range_set = calc_guarded_name_expr_ranges(semantic_model);
        calc_name_expr_ref(semantic_model, &mut use_range_set);
        for name_expr in root.descendants::<LuaNameExpr>() {
            check_name_expr(
                context,
                semantic_model,
                &mut use_range_set,
                &guarded_range_set,
                name_expr,
            );
        }
    }
}

const VALIDITY_HELPER_NAMES: &[&str] = &[
    "IsValid",
    "isfunction",
    "isstring",
    "isnumber",
    "isbool",
    "istable",
    "isentity",
    "isvector",
    "isangle",
    "ismatrix",
    "ispanel",
    "IsColor",
    "IsEntity",
];

fn calc_guarded_name_expr_ranges(semantic_model: &SemanticModel) -> HashSet<TextRange> {
    let mut guarded_ranges = HashSet::new();
    let root = semantic_model.get_root();

    for if_stat in root.descendants::<LuaIfStat>() {
        if let (Some(condition), Some(block)) = (if_stat.get_condition_expr(), if_stat.get_block())
        {
            collect_clause_guarded_name_ranges(&condition, &block, &mut guarded_ranges);
        }

        for else_if_clause in if_stat.get_else_if_clause_list() {
            if let (Some(condition), Some(block)) = (
                else_if_clause.get_condition_expr(),
                else_if_clause.get_block(),
            ) {
                collect_clause_guarded_name_ranges(&condition, &block, &mut guarded_ranges);
            }
        }
    }

    guarded_ranges
}

fn collect_clause_guarded_name_ranges(
    condition: &LuaExpr,
    block: &glua_parser::LuaBlock,
    guarded_ranges: &mut HashSet<TextRange>,
) {
    let mut condition_guard_ranges = HashSet::new();
    let truthy_names = collect_truthy_guarded_names(condition, &mut condition_guard_ranges);
    guarded_ranges.extend(condition_guard_ranges);

    if truthy_names.is_empty() {
        return;
    }

    for name_expr in block.descendants::<LuaNameExpr>() {
        let Some(name_text) = name_expr.get_name_text() else {
            continue;
        };

        if truthy_names.contains(name_text.as_str()) {
            guarded_ranges.insert(name_expr.get_range());
        }
    }
}

fn collect_truthy_guarded_names(
    expr: &LuaExpr,
    condition_guard_ranges: &mut HashSet<TextRange>,
) -> HashSet<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => {
            let mut names = HashSet::new();
            if let Some(name_text) = name_expr.get_name_text() {
                condition_guard_ranges.insert(name_expr.get_range());
                names.insert(name_text.to_string());
            }
            names
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .map(|inner| collect_truthy_guarded_names(&inner, condition_guard_ranges))
            .unwrap_or_default(),
        LuaExpr::UnaryExpr(unary_expr) => {
            let Some(inner_expr) = unary_expr.get_expr() else {
                return HashSet::new();
            };

            let is_not = unary_expr
                .get_op_token()
                .is_some_and(|op| op.get_op() == UnaryOperator::OpNot);
            if is_not {
                return collect_truthy_guarded_names_with_not_chain(expr, condition_guard_ranges);
            }

            collect_truthy_guarded_names(&inner_expr, condition_guard_ranges)
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let Some((left_expr, right_expr)) = binary_expr.get_exprs() else {
                return HashSet::new();
            };

            let op = binary_expr
                .get_op_token()
                .map(|op| op.get_op())
                .unwrap_or(BinaryOperator::OpNop);

            match op {
                BinaryOperator::OpAnd => {
                    let mut names =
                        collect_truthy_guarded_names(&left_expr, condition_guard_ranges);
                    names.extend(collect_truthy_guarded_names(
                        &right_expr,
                        condition_guard_ranges,
                    ));
                    names
                }
                BinaryOperator::OpOr => {
                    let _ = collect_truthy_guarded_names(&left_expr, condition_guard_ranges);
                    let _ = collect_truthy_guarded_names(&right_expr, condition_guard_ranges);
                    HashSet::new()
                }
                BinaryOperator::OpNe => {
                    let mut names = HashSet::new();
                    if let Some(name_expr) = name_compared_with_nil(&left_expr, &right_expr)
                        && let Some(name_text) = name_expr.get_name_text()
                    {
                        condition_guard_ranges.insert(name_expr.get_range());
                        names.insert(name_text.to_string());
                    }
                    names
                }
                _ => HashSet::new(),
            }
        }
        LuaExpr::CallExpr(call_expr) => {
            let mut names = HashSet::new();
            if let Some(name_expr) = guarded_call_target_name(call_expr)
                && let Some(name_text) = name_expr.get_name_text()
            {
                condition_guard_ranges.insert(name_expr.get_range());
                names.insert(name_text.to_string());
            }
            names
        }
        _ => HashSet::new(),
    }
}

fn name_compared_with_nil(left_expr: &LuaExpr, right_expr: &LuaExpr) -> Option<LuaNameExpr> {
    if is_nil_literal(left_expr) {
        return extract_name_expr(right_expr);
    }

    if is_nil_literal(right_expr) {
        return extract_name_expr(left_expr);
    }

    None
}

fn is_nil_literal(expr: &LuaExpr) -> bool {
    let LuaExpr::LiteralExpr(literal_expr) = expr else {
        return false;
    };

    matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Nil(_)))
}

fn extract_name_expr(expr: &LuaExpr) -> Option<LuaNameExpr> {
    match expr {
        LuaExpr::NameExpr(name_expr) => Some(name_expr.clone()),
        LuaExpr::ParenExpr(paren_expr) => extract_name_expr(&paren_expr.get_expr()?),
        _ => None,
    }
}

fn guarded_call_target_name(call_expr: &LuaCallExpr) -> Option<LuaNameExpr> {
    let prefix_expr = call_expr.get_prefix_expr()?;

    match prefix_expr {
        LuaExpr::NameExpr(name_expr) => {
            let helper_name = name_expr.get_name_text()?;
            if !is_validity_helper_name(&helper_name) {
                return None;
            }

            let first_arg = call_expr.get_args_list()?.get_args().next()?;
            extract_name_expr(&first_arg)
        }
        LuaExpr::IndexExpr(index_expr) => {
            if !call_expr.is_colon_call() {
                return None;
            }

            let is_isvalid_call = matches!(
                index_expr.get_index_key(),
                Some(LuaIndexKey::Name(name_token)) if name_token.get_name_text() == "IsValid"
            );
            if !is_isvalid_call {
                return None;
            }

            extract_name_expr(&index_expr.get_prefix_expr()?)
        }
        _ => None,
    }
}

fn collect_truthy_guarded_names_with_not_chain(
    expr: &LuaExpr,
    condition_guard_ranges: &mut HashSet<TextRange>,
) -> HashSet<String> {
    let mut current_expr = expr.clone();
    let mut not_count = 0usize;

    loop {
        match &current_expr {
            LuaExpr::ParenExpr(paren_expr) => {
                let Some(inner_expr) = paren_expr.get_expr() else {
                    return HashSet::new();
                };
                current_expr = inner_expr;
            }
            LuaExpr::UnaryExpr(unary_expr) => {
                let is_not = unary_expr
                    .get_op_token()
                    .is_some_and(|op| op.get_op() == UnaryOperator::OpNot);
                if !is_not {
                    break;
                }

                not_count += 1;
                let Some(inner_expr) = unary_expr.get_expr() else {
                    return HashSet::new();
                };
                current_expr = inner_expr;
            }
            _ => break,
        }
    }

    let names = collect_truthy_guarded_names(&current_expr, condition_guard_ranges);
    if not_count % 2 == 0 {
        names
    } else {
        HashSet::new()
    }
}

fn is_validity_helper_name(helper_name: &str) -> bool {
    VALIDITY_HELPER_NAMES.contains(&helper_name) || looks_like_validity_helper(helper_name)
}

fn looks_like_validity_helper(name: &str) -> bool {
    starts_with_boolean_helper_prefix(name, "is") || starts_with_boolean_helper_prefix(name, "has")
}

fn starts_with_boolean_helper_prefix(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };

    let Some(first_char) = rest.chars().next() else {
        return false;
    };

    first_char == '_' || first_char.is_ascii_uppercase()
}

fn calc_name_expr_ref(
    semantic_model: &SemanticModel,
    use_range_set: &mut HashSet<TextRange>,
) -> Option<()> {
    let file_id = semantic_model.get_file_id();
    let db = semantic_model.get_db();
    let refs_index = db.get_reference_index().get_local_reference(&file_id)?;
    for decl_refs in refs_index.get_decl_references_map().values() {
        for decl_ref in &decl_refs.cells {
            use_range_set.insert(decl_ref.range);
        }
    }

    None
}

fn check_name_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    use_range_set: &mut HashSet<TextRange>,
    guarded_range_set: &HashSet<TextRange>,
    name_expr: LuaNameExpr,
) -> Option<()> {
    let name_range = name_expr.get_range();
    if use_range_set.contains(&name_range) || guarded_range_set.contains(&name_range) {
        return Some(());
    }

    let name_text = name_expr.get_name_text()?;
    if name_text == "_" {
        return Some(());
    }

    let db = semantic_model.get_db();
    let module_index = db.get_module_index();
    if let Some(current_workspace_id) = module_index.get_workspace_id(semantic_model.get_file_id())
    {
        if db.get_global_index().is_exist_global_decl_in_workspace(
            &name_text,
            module_index,
            current_workspace_id,
        ) {
            return Some(());
        }
    } else if db.get_global_index().is_exist_global_decl(&name_text) {
        return Some(());
    }

    if context
        .config
        .global_disable_set
        .contains(name_text.as_str())
    {
        return Some(());
    }

    if context
        .config
        .global_disable_glob
        .iter()
        .any(|re| re.is_match(&name_text))
    {
        return Some(());
    }

    if name_text == "self" && check_self_name(semantic_model, name_expr).is_some() {
        return Some(());
    }

    if name_text == "BaseClass"
        && semantic_model
            .get_db()
            .get_gmod_class_metadata_index()
            .get_define_baseclass_name(&semantic_model.get_file_id())
            .is_some()
    {
        return Some(());
    }

    context.add_diagnostic(
        DiagnosticCode::UndefinedGlobal,
        name_range,
        t!("undefined global variable: %{name}", name = name_text).to_string(),
        None,
    );

    Some(())
}

fn check_self_name(semantic_model: &SemanticModel, name_expr: LuaNameExpr) -> Option<()> {
    let closure_expr = name_expr.ancestors::<LuaClosureExpr>();
    for closure_expr in closure_expr {
        let signature_id =
            LuaSignatureId::from_closure(semantic_model.get_file_id(), &closure_expr);
        let signature = semantic_model
            .get_db()
            .get_signature_index()
            .get(&signature_id)?;
        if signature.is_method(semantic_model, None) {
            return Some(());
        }
    }
    None
}
