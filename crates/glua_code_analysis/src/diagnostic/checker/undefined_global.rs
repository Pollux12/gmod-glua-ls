use std::collections::{HashMap, HashSet};

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBlock, LuaCallExpr, LuaClosureExpr, LuaExpr,
    LuaIfStat, LuaIndexKey, LuaLiteralToken, LuaLocalStat, LuaNameExpr, LuaStat, UnaryOperator,
};
use rowan::{TextRange, TextSize};

use crate::{
    DiagnosticCode, GlobalId, LuaDeclId, LuaMemberKey, LuaMemberOwner, LuaSignatureId,
    SemanticModel, semantic::unwrap_paren_to_name_expr,
};

use super::{Checker, DiagnosticContext};

pub struct UndefinedGlobalChecker;

impl Checker for UndefinedGlobalChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::UndefinedGlobal,
        DiagnosticCode::UndefinedGlobalArgument,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let mut use_range_set = HashSet::new();
        let guarded_range_set = calc_guarded_name_expr_ranges(semantic_model);
        let safe_read_ranges = calc_safe_read_name_expr_ranges(&root);
        let direct_call_arg_name_ranges = calc_direct_call_arg_name_expr_ranges(&root);
        calc_name_expr_ref(semantic_model, &mut use_range_set);
        for name_expr in root.descendants::<LuaNameExpr>() {
            check_name_expr(
                context,
                semantic_model,
                &mut use_range_set,
                &guarded_range_set,
                &safe_read_ranges,
                &direct_call_arg_name_ranges,
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

    guarded_ranges.extend(calc_continuation_guarded_name_expr_ranges(root));

    guarded_ranges
}

#[derive(Debug, Clone, Copy)]
struct ContinuationGuardRule {
    block_range: TextRange,
    guard_start: TextSize,
}

fn calc_continuation_guarded_name_expr_ranges(root: &glua_parser::LuaChunk) -> HashSet<TextRange> {
    let mut guarded_ranges = HashSet::new();
    let mut guard_rules_by_name = HashMap::<String, Vec<ContinuationGuardRule>>::new();

    for block in root.descendants::<LuaBlock>() {
        let block_range = block.get_range();
        for stat in block.get_stats() {
            let LuaStat::IfStat(if_stat) = stat else {
                continue;
            };

            let Some(guarded_name) = continuation_guard_name(&if_stat) else {
                continue;
            };

            guard_rules_by_name
                .entry(guarded_name)
                .or_default()
                .push(ContinuationGuardRule {
                    block_range,
                    guard_start: if_stat.get_range().end(),
                });
        }
    }

    if guard_rules_by_name.is_empty() {
        return guarded_ranges;
    }

    for name_expr in root.descendants::<LuaNameExpr>() {
        let expr_range = name_expr.get_range();
        let Some(name_text) = name_expr.get_name_text() else {
            continue;
        };

        let Some(guard_rules) = guard_rules_by_name.get(name_text.as_str()) else {
            continue;
        };

        if guard_rules.iter().any(|rule| {
            expr_range.start() >= rule.guard_start
                && expr_range.start() >= rule.block_range.start()
                && expr_range.end() <= rule.block_range.end()
        }) {
            guarded_ranges.insert(expr_range);
        }
    }

    guarded_ranges
}

fn continuation_guard_name(if_stat: &LuaIfStat) -> Option<String> {
    let block = if_stat.get_block()?;
    if !is_return_only_block(&block) {
        return None;
    }

    extract_continuation_guarded_name(&if_stat.get_condition_expr()?)
}

fn is_return_only_block(block: &LuaBlock) -> bool {
    let mut has_return_stat = false;
    for stat in block.get_stats() {
        match stat {
            LuaStat::EmptyStat(_) => {}
            LuaStat::ReturnStat(_) => {
                if has_return_stat {
                    return false;
                }
                has_return_stat = true;
            }
            _ => return false,
        }
    }

    has_return_stat
}

fn extract_continuation_guarded_name(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => {
            extract_continuation_guarded_name(&paren_expr.get_expr()?)
        }
        LuaExpr::UnaryExpr(unary_expr) => {
            let is_not = unary_expr
                .get_op_token()
                .is_some_and(|op| op.get_op() == UnaryOperator::OpNot);
            if !is_not {
                return None;
            }

            extract_truthy_guarded_name(&unary_expr.get_expr()?)
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let is_eq = binary_expr
                .get_op_token()
                .is_some_and(|op| op.get_op() == BinaryOperator::OpEq);
            if !is_eq {
                return None;
            }

            let (left_expr, right_expr) = binary_expr.get_exprs()?;
            name_compared_with_nil(&left_expr, &right_expr)
                .and_then(|name_expr| name_expr.get_name_text().map(|text| text.to_string()))
        }
        _ => None,
    }
}

fn extract_truthy_guarded_name(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => extract_truthy_guarded_name(&paren_expr.get_expr()?),
        LuaExpr::NameExpr(name_expr) => name_expr.get_name_text().map(|text| text.to_string()),
        LuaExpr::CallExpr(call_expr) => guarded_call_target_name(call_expr)
            .and_then(|name_expr| name_expr.get_name_text().map(|text| text.to_string())),
        _ => None,
    }
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
                BinaryOperator::OpEq => {
                    if let Some(name_expr) = name_compared_with_nil(&left_expr, &right_expr) {
                        condition_guard_ranges.insert(name_expr.get_range());
                    }
                    HashSet::new()
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
        LuaExpr::IndexExpr(index_expr) => {
            // For index expressions like `ctp.Disable`, extract the base name (`ctp`)
            // If we're checking `if ctp.Disable then`, it implies `ctp` exists
            let mut names = HashSet::new();
            if let Some(prefix_expr) = index_expr.get_prefix_expr()
                && let Some(name_expr) = unwrap_paren_to_name_expr(&prefix_expr)
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
        return unwrap_paren_to_name_expr(right_expr);
    }

    if is_nil_literal(right_expr) {
        return unwrap_paren_to_name_expr(left_expr);
    }

    None
}

fn is_nil_literal(expr: &LuaExpr) -> bool {
    let LuaExpr::LiteralExpr(literal_expr) = expr else {
        return false;
    };

    matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Nil(_)))
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
            unwrap_paren_to_name_expr(&first_arg)
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

            unwrap_paren_to_name_expr(&index_expr.get_prefix_expr()?)
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
    if not_count.is_multiple_of(2) {
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

fn calc_safe_read_name_expr_ranges(root: &glua_parser::LuaChunk) -> HashSet<TextRange> {
    let mut ranges = HashSet::new();

    for assign_stat in root.descendants::<LuaAssignStat>() {
        let (_, exprs) = assign_stat.get_var_and_expr_list();
        for expr in exprs {
            collect_safe_name_exprs_from_value(&expr, &mut ranges);
        }
    }

    for local_stat in root.descendants::<LuaLocalStat>() {
        for expr in local_stat.get_value_exprs() {
            collect_safe_name_exprs_from_value(&expr, &mut ranges);
        }
    }

    ranges
}

fn collect_safe_name_exprs_from_value(expr: &LuaExpr, ranges: &mut HashSet<TextRange>) {
    match expr {
        LuaExpr::NameExpr(name_expr) => {
            ranges.insert(name_expr.get_range());
        }
        LuaExpr::ParenExpr(paren_expr) => {
            if let Some(inner) = paren_expr.get_expr() {
                collect_safe_name_exprs_from_value(&inner, ranges);
            }
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let is_or = binary_expr
                .get_op_token()
                .is_some_and(|op| op.get_op() == BinaryOperator::OpOr);
            if is_or {
                if let Some((left, right)) = binary_expr.get_exprs() {
                    collect_safe_name_exprs_from_value(&left, ranges);
                    collect_safe_name_exprs_from_value(&right, ranges);
                }
            }
        }
        _ => {}
    }
}

fn calc_direct_call_arg_name_expr_ranges(root: &glua_parser::LuaChunk) -> HashSet<TextRange> {
    let mut ranges = HashSet::new();

    for call_expr in root.descendants::<LuaCallExpr>() {
        let Some(args_list) = call_expr.get_args_list() else {
            continue;
        };

        for arg_expr in args_list.get_args() {
            if let Some(name_expr) = extract_direct_name_expr(&arg_expr) {
                ranges.insert(name_expr.get_range());
            }
        }
    }

    ranges
}

fn extract_direct_name_expr(expr: &LuaExpr) -> Option<LuaNameExpr> {
    match expr {
        LuaExpr::NameExpr(name_expr) => Some(name_expr.clone()),
        LuaExpr::ParenExpr(paren_expr) => extract_direct_name_expr(&paren_expr.get_expr()?),
        _ => None,
    }
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
    safe_read_ranges: &HashSet<TextRange>,
    direct_call_arg_name_ranges: &HashSet<TextRange>,
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

    if is_legacy_module_local_name_visible(semantic_model, &name_expr, &name_text) {
        return Some(());
    }

    if is_legacy_module_without_seeall_after_activation(semantic_model, &name_expr) {
        context.add_diagnostic(
            undefined_global_diagnostic_code(name_range, direct_call_arg_name_ranges),
            name_range,
            t!("undefined global variable: %{name}", name = name_text).to_string(),
            None,
        );
        return Some(());
    }

    // Check if name exists as a global
    let module_index = db.get_module_index();
    let is_valid_global = if let Some(current_workspace_id) =
        module_index.get_workspace_id(semantic_model.get_file_id())
    {
        db.get_global_index().is_exist_global_decl_in_workspace(
            &name_text,
            module_index,
            current_workspace_id,
        )
    } else {
        db.get_global_index().is_exist_global_decl(&name_text)
    };

    if is_valid_global {
        // Name exists as global - skip diagnostic
        return Some(());
    }

    if name_text == "self" && check_self_name(semantic_model, name_expr.clone()).is_some() {
        return Some(());
    }

    if db.get_emmyrc().gmod.enabled
        && db
            .get_gmod_infer_index()
            .get_scoped_class_info(&semantic_model.get_file_id())
            .is_some_and(|info| info.global_name == name_text.as_str())
    {
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

    let legacy_module_env = semantic_model
        .get_db()
        .get_module_index()
        .get_legacy_module_env_at(semantic_model.get_file_id(), name_expr.get_position());
    let in_legacy_module = legacy_module_env.is_some();
    let in_seeall_module = legacy_module_env.is_some_and(|env| env.seeall);

    // In legacy modules with seeall, the type inference may resolve names through
    // the _G.__index chain and return a non-unknown type even for truly undefined
    // globals. Only trust the narrowing check outside legacy modules.
    if !in_legacy_module && is_narrowed_unresolved_global_valid(semantic_model, &name_expr) {
        return Some(());
    }

    if !in_legacy_module && safe_read_ranges.contains(&name_range) {
        return Some(());
    }

    // Self-shadowing defensive-import pattern: `local foo = foo` (and the
    // colon-call equivalent on indexed targets). This is the canonical Lua
    // idiom for capturing an optional/conditionally-loaded global into a
    // module-local. Inside a seeall module the RHS read goes through the
    // `_G.__index` fallback, so flagging it as undefined is noise. Outside
    // legacy modules it's already covered by `safe_read_ranges` above; this
    // arm only adds the in-seeall case so we don't regress typo detection
    // for unrelated `local x = unknown` patterns.
    if in_seeall_module && is_self_shadowing_local_assignment(&name_expr, &name_text) {
        return Some(());
    }

    context.add_diagnostic(
        undefined_global_diagnostic_code(name_range, direct_call_arg_name_ranges),
        name_range,
        t!("undefined global variable: %{name}", name = name_text).to_string(),
        None,
    );

    Some(())
}

fn undefined_global_diagnostic_code(
    name_range: TextRange,
    direct_call_arg_name_ranges: &HashSet<TextRange>,
) -> DiagnosticCode {
    if direct_call_arg_name_ranges.contains(&name_range) {
        DiagnosticCode::UndefinedGlobalArgument
    } else {
        DiagnosticCode::UndefinedGlobal
    }
}

fn is_legacy_module_local_name_visible(
    semantic_model: &SemanticModel,
    name_expr: &LuaNameExpr,
    name: &str,
) -> bool {
    let db = semantic_model.get_db();
    let file_id = semantic_model.get_file_id();
    let Some(module_env) = db
        .get_module_index()
        .get_legacy_module_env_at(file_id, name_expr.get_position())
    else {
        return false;
    };

    if matches!(name, "_M" | "_NAME" | "_PACKAGE") {
        return true;
    }

    // The module's own name is bound as a global by `module(name, ...)` at runtime
    // (and chain segments like `foo` in `module("foo.bar", ...)` get tables created
    // in `_G` as well). We don't synthesize global decls for these, so treat them
    // as visible here. Cross-file references resolve through the legacy module
    // namespace check earlier in the pipeline.
    if is_legacy_module_chain_segment(&module_env.module_path, name) {
        return true;
    }

    let decl_visible = db
        .get_decl_index()
        .get_decl_tree(&file_id)
        .is_some_and(|tree| {
            tree.find_local_decl(name, name_expr.get_position())
                .filter(|decl| {
                    decl.is_module_scoped()
                        && decl.get_module_path() == Some(module_env.module_path.as_str())
                })
                .or_else(|| {
                    tree.find_module_scoped_decl_anywhere(
                        name,
                        &module_env.module_path,
                        module_env.activation_position,
                    )
                })
                .is_some()
        });
    if decl_visible {
        return true;
    }

    let owner = LuaMemberOwner::GlobalPath(GlobalId::new(&module_env.module_path));
    let key = LuaMemberKey::Name(name.into());
    let Some(member_item) = db.get_member_index().get_member_item(&owner, &key) else {
        return false;
    };
    let visible_ids =
        member_item.visible_member_ids_with_realm_at_offset(db, &file_id, name_expr.get_position());
    visible_ids.into_iter().any(|member_id| {
        let decl_id = LuaDeclId::new(member_id.file_id, member_id.get_position());
        db.get_decl_index().get_decl(&decl_id).is_some()
    })
}

fn is_legacy_module_without_seeall_after_activation(
    semantic_model: &SemanticModel,
    name_expr: &LuaNameExpr,
) -> bool {
    let db = semantic_model.get_db();
    let file_id = semantic_model.get_file_id();
    let Some(module_env) = db
        .get_module_index()
        .get_legacy_module_env_at(file_id, name_expr.get_position())
    else {
        return false;
    };

    !module_env.seeall
        && !matches!(
            name_expr.get_name_text().as_deref(),
            Some("_M" | "_NAME" | "_PACKAGE")
        )
        && name_expr
            .get_name_text()
            .as_deref()
            .is_none_or(|name| !is_legacy_module_chain_segment(&module_env.module_path, name))
}

/// Returns true if `name` is the full module path or any leading dotted-chain segment
/// of `module_path`. For `module("foo.bar.baz", ...)` the chain segments are
/// "foo", "foo.bar", "foo.bar.baz" — all are bound as globals at runtime.
fn is_legacy_module_chain_segment(module_path: &str, name: &str) -> bool {
    if module_path == name {
        return true;
    }
    module_path
        .strip_prefix(name)
        .is_some_and(|rest| rest.starts_with('.'))
}

/// Detects the canonical defensive-import idiom `local foo = foo`, where the
/// RHS is the bare-name reference being checked and the matching LHS local
/// has the same identifier text. Used to suppress undefined-global noise for
/// optional-import patterns inside seeall legacy modules without weakening
/// generic typo detection (`local _ = unknown_typo`).
fn is_self_shadowing_local_assignment(name_expr: &LuaNameExpr, name_text: &str) -> bool {
    let Some(local_stat) = name_expr.get_parent::<LuaLocalStat>() else {
        return false;
    };
    let value_exprs: Vec<LuaExpr> = local_stat.get_value_exprs().collect();
    let Some(value_index) = value_exprs.iter().position(|expr| {
        unwrap_paren_to_name_expr(expr)
            .map(|n| n.syntax() == name_expr.syntax())
            .unwrap_or(false)
    }) else {
        return false;
    };
    let local_names: Vec<_> = local_stat.get_local_name_list().collect();
    let Some(local_name) = local_names.get(value_index) else {
        return false;
    };
    local_name
        .get_name_token()
        .map(|t| t.get_name_text() == name_text)
        .unwrap_or(false)
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

fn is_narrowed_unresolved_global_valid(
    semantic_model: &SemanticModel,
    name_expr: &LuaNameExpr,
) -> bool {
    let Ok(inferred_type) = semantic_model.infer_expr(LuaExpr::NameExpr(name_expr.clone())) else {
        return false;
    };

    !inferred_type.is_unknown() && !inferred_type.is_never() && !inferred_type.is_always_falsy()
}
