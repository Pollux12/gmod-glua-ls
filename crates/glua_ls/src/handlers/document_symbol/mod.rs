use std::collections::HashSet;

mod builder;
mod comment;
mod expr;
mod stats;

use builder::{DocumentSymbolBuilder, LuaSymbol};
use expr::{build_closure_expr_symbol, build_table_symbol};
use glua_code_analysis::{EmmyrcGmodOutlineVerbosity, SemanticModel};
use glua_parser::{
    LuaAstNode, LuaBlock, LuaCallExpr, LuaChunk, LuaClosureExpr, LuaComment, LuaExpr, LuaFuncStat,
    LuaSingleArgExpr, LuaStat, LuaSyntaxId, LuaSyntaxNode, LuaVarExpr, PathTrait,
};
use lsp_types::{
    ClientCapabilities, DocumentSymbol, DocumentSymbolOptions, DocumentSymbolParams,
    DocumentSymbolResponse, OneOf, ServerCapabilities, SymbolKind,
};
use stats::{
    IfSymbolContext, build_assign_stat_symbol, build_do_stat_symbol, build_for_range_stat_symbol,
    build_for_stat_symbol, build_func_stat_symbol, build_if_stat_symbol,
    build_local_func_stat_symbol, build_local_stat_symbol,
};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

use super::RegisterCapabilities;
use comment::build_doc_region_symbol;

pub async fn on_document_symbol(
    context: ServerContextSnapshot,
    params: DocumentSymbolParams,
    cancel_token: CancellationToken,
) -> Option<DocumentSymbolResponse> {
    if cancel_token.is_cancelled() {
        return None;
    }
    let uri = params.text_document.uri;
    let analysis = context.read_analysis(&cancel_token).await?;
    if cancel_token.is_cancelled() {
        return None;
    }
    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    let document_symbol_root = build_document_symbol(&semantic_model, &cancel_token)?;
    // remove root file symbol
    let children = document_symbol_root.children?;
    let response = DocumentSymbolResponse::Nested(children);
    Some(response)
}

fn build_document_symbol(
    semantic_model: &SemanticModel,
    cancel_token: &CancellationToken,
) -> Option<DocumentSymbol> {
    let document = semantic_model.get_document();
    let root = semantic_model.get_root();
    let file_id = semantic_model.get_file_id();
    let decl_tree = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl_tree(&file_id)?;
    let db = semantic_model.get_db();

    let mut builder = DocumentSymbolBuilder::new(db, decl_tree, &document);
    let symbol = LuaSymbol::new("".into(), None, SymbolKind::FILE, root.get_range());
    let root_id = builder.add_node_symbol(root.syntax().clone(), symbol, None);
    build_child_document_symbols(&mut builder, root, root_id, cancel_token);

    Some(builder.build(root))
}

fn build_child_document_symbols(
    builder: &mut DocumentSymbolBuilder,
    root: &LuaChunk,
    root_id: LuaSyntaxId,
    cancel_token: &CancellationToken,
) -> Option<()> {
    // Pre-create the scripted entity class symbol (e.g. "my_entity (Entity)") before processing
    // any statements so that method routing in resolve_func_parent_id can find it.
    if let Some(block) = root.syntax().children().find_map(LuaBlock::cast) {
        builder.maybe_ensure_scripted_class_symbol(
            block.syntax().clone(),
            root_id,
            root.get_range(),
        );
    }
    process_chunk(builder, root, root_id, cancel_token)
}

fn process_chunk(
    builder: &mut DocumentSymbolBuilder,
    chunk: &LuaChunk,
    parent_id: LuaSyntaxId,
    cancel_token: &CancellationToken,
) -> Option<()> {
    for node in chunk.syntax().children() {
        if cancel_token.is_cancelled() {
            return None;
        }
        match node {
            comment if LuaComment::can_cast(comment.kind().into()) => {
                let comment = LuaComment::cast(comment.clone())?;
                process_comment(builder, &comment, parent_id);
            }
            block if LuaBlock::can_cast(block.kind().into()) => {
                let block = LuaBlock::cast(block.clone())?;
                process_block(builder, block, parent_id, cancel_token);
            }
            _ => {}
        }
    }

    Some(())
}

fn process_comment(
    builder: &mut DocumentSymbolBuilder,
    comment: &LuaComment,
    parent_id: LuaSyntaxId,
) {
    build_doc_region_symbol(builder, comment.clone(), parent_id);
}

fn process_block(
    builder: &mut DocumentSymbolBuilder,
    block: LuaBlock,
    parent_id: LuaSyntaxId,
    cancel_token: &CancellationToken,
) -> Option<()> {
    for child in block.syntax().children() {
        if cancel_token.is_cancelled() {
            return None;
        }
        match child {
            comment if LuaComment::can_cast(comment.kind().into()) => {
                let comment = LuaComment::cast(comment.clone())?;
                process_comment(builder, &comment, parent_id);
            }
            stat if LuaStat::can_cast(stat.kind().into()) => {
                let stat = LuaStat::cast(stat.clone())?;
                process_stat(builder, stat, parent_id, cancel_token)?;
            }
            _ => {}
        }
    }

    Some(())
}

fn process_stat(
    builder: &mut DocumentSymbolBuilder,
    stat: LuaStat,
    parent_id: LuaSyntaxId,
    cancel_token: &CancellationToken,
) -> Option<()> {
    let verbose = builder.get_verbosity() == EmmyrcGmodOutlineVerbosity::Verbose;

    match stat {
        LuaStat::LocalStat(local_stat) => {
            let bindings = build_local_stat_symbol(builder, local_stat.clone(), parent_id)?;
            let mut processed_value_expr_ids = HashSet::new();
            for binding in bindings {
                if let Some(expr) = binding.value_expr {
                    processed_value_expr_ids.insert(expr.get_syntax_id());
                    process_expr(builder, expr, binding.symbol_id, true, cancel_token)?;
                }
            }

            for expr in local_stat.get_value_exprs() {
                if processed_value_expr_ids.contains(&expr.get_syntax_id()) {
                    continue;
                }
                process_expr(builder, expr, parent_id, false, cancel_token)?;
            }
        }
        LuaStat::AssignStat(assign_stat) => {
            let bindings = build_assign_stat_symbol(builder, assign_stat.clone(), parent_id)?;
            let mut processed_value_expr_ids = HashSet::new();
            for binding in bindings {
                if let Some(expr) = binding.value_expr {
                    processed_value_expr_ids.insert(expr.get_syntax_id());
                    process_expr(builder, expr, binding.symbol_id, true, cancel_token)?;
                }
            }

            let (_, value_exprs) = assign_stat.get_var_and_expr_list();
            for expr in value_exprs {
                if processed_value_expr_ids.contains(&expr.get_syntax_id()) {
                    continue;
                }
                process_expr(builder, expr, parent_id, false, cancel_token)?;
            }
        }
        LuaStat::FuncStat(func_stat) => {
            let func_parent_id = resolve_func_parent_id(builder, &func_stat, parent_id);
            let func_id = build_func_stat_symbol(builder, func_stat.clone(), func_parent_id)?;
            if let Some(closure) = func_stat.get_closure() {
                let scope_parent =
                    build_closure_expr_symbol(builder, closure.clone(), func_id, false)?;
                if let Some(block) = closure.get_block() {
                    process_block(builder, block, scope_parent, cancel_token)?;
                }
            }
        }
        LuaStat::LocalFuncStat(local_func) => {
            let func_id = build_local_func_stat_symbol(builder, local_func.clone(), parent_id)?;
            if let Some(closure) = local_func.get_closure() {
                let scope_parent =
                    build_closure_expr_symbol(builder, closure.clone(), func_id, false)?;
                if let Some(block) = closure.get_block() {
                    process_block(builder, block, scope_parent, cancel_token)?;
                }
            }
        }
        LuaStat::ForStat(for_stat) => {
            if verbose {
                let for_id = build_for_stat_symbol(builder, for_stat.clone(), parent_id)?;
                process_exprs(builder, for_stat.syntax(), for_id, cancel_token)?;
                if let Some(block) = for_stat.get_block() {
                    process_block(builder, block, for_id, cancel_token)?;
                }
            } else {
                // Promote children directly to parent — no `for` clutter.
                process_exprs(builder, for_stat.syntax(), parent_id, cancel_token)?;
                if let Some(block) = for_stat.get_block() {
                    process_block(builder, block, parent_id, cancel_token)?;
                }
            }
        }
        LuaStat::ForRangeStat(for_range_stat) => {
            if verbose {
                let for_range_id =
                    build_for_range_stat_symbol(builder, for_range_stat.clone(), parent_id)?;
                process_exprs(builder, for_range_stat.syntax(), for_range_id, cancel_token)?;
                if let Some(block) = for_range_stat.get_block() {
                    process_block(builder, block, for_range_id, cancel_token)?;
                }
            } else {
                process_exprs(builder, for_range_stat.syntax(), parent_id, cancel_token)?;
                if let Some(block) = for_range_stat.get_block() {
                    process_block(builder, block, parent_id, cancel_token)?;
                }
            }
        }
        LuaStat::IfStat(if_stat) => {
            if verbose {
                let ctx = build_if_stat_symbol(builder, if_stat.clone(), parent_id)?;
                if let Some(condition) = if_stat.get_condition_expr() {
                    process_expr(builder, condition, ctx.if_id, false, cancel_token)?;
                }
                if let Some(block) = if_stat.get_block() {
                    process_block(builder, block, ctx.if_id, cancel_token)?;
                }
                process_if_clauses(builder, ctx, cancel_token)?;
            } else {
                // Promote all if/elseif/else children to parent — no `if` clutter.
                if let Some(condition) = if_stat.get_condition_expr() {
                    process_expr(builder, condition, parent_id, false, cancel_token)?;
                }
                if let Some(block) = if_stat.get_block() {
                    process_block(builder, block, parent_id, cancel_token)?;
                }
                for clause in if_stat.get_all_clause() {
                    use glua_parser::LuaIfClauseStat;
                    let (condition, block) = match &clause {
                        LuaIfClauseStat::ElseIf(c) => (c.get_condition_expr(), c.get_block()),
                        LuaIfClauseStat::Else(c) => (None, c.get_block()),
                    };
                    if let Some(condition) = condition {
                        process_expr(builder, condition, parent_id, false, cancel_token)?;
                    }
                    if let Some(block) = block {
                        process_block(builder, block, parent_id, cancel_token)?;
                    }
                }
            }
        }
        LuaStat::WhileStat(while_stat) => {
            if let Some(condition) = while_stat.get_condition_expr() {
                process_expr(builder, condition, parent_id, false, cancel_token)?;
            }
            if let Some(block) = while_stat.get_block() {
                process_block(builder, block, parent_id, cancel_token)?;
            }
        }
        LuaStat::RepeatStat(repeat_stat) => {
            if let Some(block) = repeat_stat.get_block() {
                process_block(builder, block, parent_id, cancel_token)?;
            }
            if let Some(condition) = repeat_stat.get_condition_expr() {
                process_expr(builder, condition, parent_id, false, cancel_token)?;
            }
        }
        LuaStat::DoStat(do_stat) => {
            if verbose {
                let do_id = build_do_stat_symbol(builder, do_stat.clone(), parent_id)?;
                if let Some(block) = do_stat.get_block() {
                    process_block(builder, block, do_id, cancel_token)?;
                }
            } else if let Some(block) = do_stat.get_block() {
                process_block(builder, block, parent_id, cancel_token)?;
            }
        }
        LuaStat::CallExprStat(call_stat) => {
            // Check whether this is a named GMod call (hook.Add, net.Receive, …).
            if builder.is_gmod_enabled() {
                if let Some(call_expr) = call_stat.syntax().children().find_map(LuaCallExpr::cast) {
                    let call_syntax_id = call_expr.get_syntax_id();
                    if let Some(entry) = builder.get_gmod_call_entry(&call_syntax_id) {
                        let label = entry.label.clone();
                        let kind = entry.kind;
                        let cb_arg_idx = entry.callback_arg_index;
                        let symbol = LuaSymbol::new(label, None, kind, call_stat.get_range());
                        let call_symbol_id = builder.add_node_symbol(
                            call_stat.syntax().clone(),
                            symbol,
                            Some(parent_id),
                        );
                        // Traverse the callback closure body so that nested functions appear
                        // as children of the hook/net/timer symbol.
                        if let Some(arg_idx) = cb_arg_idx {
                            if let Some(closure) = get_call_arg_closure(&call_expr, arg_idx) {
                                let scope_parent = build_closure_expr_symbol(
                                    builder,
                                    closure.clone(),
                                    call_symbol_id,
                                    true,
                                )?;
                                if let Some(block) = closure.get_block() {
                                    process_block(builder, block, scope_parent, cancel_token)?;
                                }
                            }
                        }
                        return Some(());
                    }
                }
            }
            // Fall through: no named GMod symbol — scan for interesting exprs inside.
            process_exprs(builder, call_stat.syntax(), parent_id, cancel_token)?;
        }
        LuaStat::ReturnStat(return_stat) => {
            process_exprs(builder, return_stat.syntax(), parent_id, cancel_token)?;
        }
        // GMod: dead path — Lua 5.5 `global` statement disabled
        LuaStat::GlobalStat(global_stat) => {
            process_exprs(builder, global_stat.syntax(), parent_id, cancel_token)?;
        }
        LuaStat::GotoStat(_)
        | LuaStat::BreakStat(_)
        | LuaStat::LabelStat(_)
        | LuaStat::EmptyStat(_) => {}
    }

    Some(())
}

/// Extract the closure expression at a specific argument index from a call expression.
fn get_call_arg_closure(call_expr: &LuaCallExpr, arg_index: usize) -> Option<LuaClosureExpr> {
    let args = call_expr.get_args_list()?;
    let arg = args.get_args().nth(arg_index)?;
    match arg {
        LuaExpr::ClosureExpr(closure) => Some(closure),
        _ => None,
    }
}

fn check_and_build_net_op_symbol(
    builder: &mut DocumentSymbolBuilder,
    call_expr: &LuaCallExpr,
    parent_id: LuaSyntaxId,
) -> bool {
    if !builder.is_gmod_enabled() {
        return false;
    }

    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };

    let Some(op_name) = call_path.strip_prefix("net.") else {
        return false;
    };

    let is_net_op = op_name == "Broadcast"
        || op_name == "Start"
        || op_name.starts_with("Read")
        || op_name.starts_with("Write")
        || op_name.starts_with("Send");

    if !is_net_op {
        return false;
    }

    let call_id = call_expr.get_syntax_id();
    if builder.contains_symbol(&call_id) {
        return false;
    }

    let text = call_expr.syntax().text().to_string();
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = if normalized.chars().count() > 60 {
        let mut truncated: String = normalized.chars().take(56).collect();
        truncated.push_str("...");
        truncated
    } else {
        normalized
    };

    let symbol = LuaSymbol::new(name, None, SymbolKind::EVENT, call_expr.get_range());
    builder.add_node_symbol(call_expr.syntax().clone(), symbol, Some(parent_id));
    true
}

fn resolve_func_parent_id(
    builder: &mut DocumentSymbolBuilder,
    func_stat: &LuaFuncStat,
    default_parent_id: LuaSyntaxId,
) -> LuaSyntaxId {
    let Some(func_name) = func_stat.get_func_name() else {
        return default_parent_id;
    };

    let LuaVarExpr::IndexExpr(index_expr) = func_name else {
        return default_parent_id;
    };

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return default_parent_id;
    };

    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return default_parent_id;
    };

    let Some(prefix_name) = name_expr.get_name_text() else {
        return default_parent_id;
    };

    // Check VGUI / scripted-entity decl-based grouping first.
    if let Some(decl_id) =
        builder.resolve_local_decl_id(&prefix_name, func_stat.get_range().start())
    {
        if builder.get_vgui_panel_name(&decl_id).is_some() {
            if let Some(sym_id) = builder.get_decl_symbol_id(&decl_id) {
                return sym_id;
            }

            if let Some(decl) = builder.get_decl(&decl_id)
                && decl.is_global()
                && builder.scripted_class_global_name() == Some(prefix_name.as_ref())
                && let Some(class_sym_id) = builder.get_scripted_class_symbol_id()
            {
                return class_sym_id;
            }
        }

        // Local shadow exists but is not a class panel/scripted-class declaration.
        // Do not fall back to the file-level scripted class symbol in this case.
        return default_parent_id;
    }

    // Fallback: check whether the prefix matches the scripted class global name (handles files
    // that define methods without an explicit `ENT = {}` assignment).
    if let Some(class_global) = builder.scripted_class_global_name() {
        if prefix_name == class_global {
            if let Some(class_sym_id) = builder.get_scripted_class_symbol_id() {
                return class_sym_id;
            }
        }
    }

    default_parent_id
}

fn process_if_clauses(
    builder: &mut DocumentSymbolBuilder,
    ctx: IfSymbolContext,
    cancel_token: &CancellationToken,
) -> Option<()> {
    for (clause, clause_id) in ctx.clause_symbols {
        if let Some(condition) = clause.get_condition_expr() {
            process_expr(builder, condition, clause_id, false, cancel_token)?;
        }
        if let Some(block) = clause.get_block() {
            process_block(builder, block, clause_id, cancel_token)?;
        }
    }

    Some(())
}

fn process_exprs(
    builder: &mut DocumentSymbolBuilder,
    syntax: &LuaSyntaxNode,
    parent_id: LuaSyntaxId,
    cancel_token: &CancellationToken,
) -> Option<()> {
    for child in syntax.children() {
        match child {
            expr if LuaExpr::can_cast(expr.kind().into()) => {
                let expr = LuaExpr::cast(expr.clone())?;
                process_expr(builder, expr, parent_id, false, cancel_token)?;
            }
            _ => {}
        }
    }
    Some(())
}

fn process_expr(
    builder: &mut DocumentSymbolBuilder,
    expr: LuaExpr,
    parent_id: LuaSyntaxId,
    inline_table_to_parent: bool,
    cancel_token: &CancellationToken,
) -> Option<()> {
    match expr {
        LuaExpr::TableExpr(table) => {
            if !inline_table_to_parent {
                if table.is_object() {
                    for field in table.get_fields() {
                        if let Some(value_expr) = field.get_value_expr() {
                            process_expr(builder, value_expr, parent_id, false, cancel_token)?;
                        }
                    }
                }
                return Some(());
            }
            let table_id =
                build_table_symbol(builder, table.clone(), parent_id, inline_table_to_parent)?;
            for field in table.get_fields() {
                if let Some(value_expr) = field.get_value_expr() {
                    let field_id =
                        LuaSyntaxId::new(field.syntax().kind(), field.syntax().text_range());
                    let next_parent = if builder.contains_symbol(&field_id) {
                        field_id
                    } else {
                        table_id
                    };
                    process_expr(builder, value_expr, next_parent, true, cancel_token)?;
                }
            }
        }
        LuaExpr::ClosureExpr(closure) => {
            if !inline_table_to_parent {
                return Some(());
            }
            let scope_parent =
                build_closure_expr_symbol(builder, closure.clone(), parent_id, false)?;
            if let Some(block) = closure.get_block() {
                process_block(builder, block, scope_parent, cancel_token)?;
            }
        }
        LuaExpr::BinaryExpr(binary) => {
            if let Some((left, right)) = binary.get_exprs() {
                process_expr(
                    builder,
                    left,
                    parent_id,
                    inline_table_to_parent,
                    cancel_token,
                )?;
                process_expr(
                    builder,
                    right,
                    parent_id,
                    inline_table_to_parent,
                    cancel_token,
                )?;
            }
        }
        LuaExpr::UnaryExpr(unary) => {
            if let Some(inner) = unary.get_expr() {
                process_expr(
                    builder,
                    inner,
                    parent_id,
                    inline_table_to_parent,
                    cancel_token,
                )?;
            }
        }
        LuaExpr::ParenExpr(paren) => {
            if let Some(inner) = paren.get_expr() {
                process_expr(
                    builder,
                    inner,
                    parent_id,
                    inline_table_to_parent,
                    cancel_token,
                )?;
            }
        }
        LuaExpr::CallExpr(call) => {
            check_and_build_net_op_symbol(builder, &call, parent_id);
            if let Some(prefix) = call.get_prefix_expr() {
                process_expr(
                    builder,
                    prefix,
                    parent_id,
                    inline_table_to_parent,
                    cancel_token,
                )?;
            }
            if let Some(args) = call.get_args_list() {
                let collected: Vec<_> = args.get_args().collect();
                if collected.is_empty() && args.is_single_arg_no_parens() {
                    if let Some(single) = args.get_single_arg_expr() {
                        if let LuaSingleArgExpr::TableExpr(table) = single {
                            process_expr(
                                builder,
                                LuaExpr::TableExpr(table),
                                parent_id,
                                inline_table_to_parent,
                                cancel_token,
                            )?;
                        }
                    }
                } else {
                    for arg in collected {
                        process_expr(
                            builder,
                            arg,
                            parent_id,
                            inline_table_to_parent,
                            cancel_token,
                        )?;
                    }
                }
            }
        }
        LuaExpr::IndexExpr(index_expr) => {
            if let Some(prefix) = index_expr.get_prefix_expr() {
                process_expr(
                    builder,
                    prefix,
                    parent_id,
                    inline_table_to_parent,
                    cancel_token,
                )?;
            }
        }
        LuaExpr::NameExpr(_) | LuaExpr::LiteralExpr(_) => {}
    }

    Some(())
}

pub struct DocumentSymbolCapabilities;

impl RegisterCapabilities for DocumentSymbolCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.document_symbol_provider = Some(OneOf::Right(DocumentSymbolOptions {
            label: Some("GLuaLS".into()),
            work_done_progress_options: Default::default(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use glua_code_analysis::{Emmyrc, EmmyrcGmodOutlineVerbosity, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::{DocumentSymbol, SymbolKind};
    use tokio_util::sync::CancellationToken;

    use super::build_document_symbol;

    fn find_top_level_symbol<'a>(
        symbols: &'a [DocumentSymbol],
        name: &str,
    ) -> Option<&'a DocumentSymbol> {
        symbols.iter().find(|symbol| symbol.name == name)
    }

    fn top_level_names(symbols: &[DocumentSymbol]) -> Vec<String> {
        symbols.iter().map(|symbol| symbol.name.clone()).collect()
    }

    fn symbol_contains_child<'a>(
        parent: &'a DocumentSymbol,
        child_name: &str,
    ) -> Option<&'a DocumentSymbol> {
        parent
            .children
            .as_ref()?
            .iter()
            .find(|child| child.name == child_name)
    }

    fn range_contains(parent: &DocumentSymbol, child: &DocumentSymbol) -> bool {
        parent.range.start <= child.range.start && parent.range.end >= child.range.end
    }

    fn any_symbol_matches<F>(symbols: &[DocumentSymbol], predicate: &F) -> bool
    where
        F: Fn(&DocumentSymbol) -> bool,
    {
        for symbol in symbols {
            if predicate(symbol) {
                return true;
            }
            if let Some(children) = symbol.children.as_ref()
                && any_symbol_matches(children, predicate)
            {
                return true;
            }
        }

        false
    }

    #[gtest]
    fn vgui_panel_symbols_are_class_named_and_methods_are_nested() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/vgui/my_panel.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
            end

            function PANEL:Paint(w, h)
            end

            vgui.Register("MyPanel", PANEL, "DPanel")
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let panel_symbol = find_top_level_symbol(top_level_symbols, "MyPanel (VGUI)").or_fail()?;
        verify_that!(panel_symbol.kind, eq(SymbolKind::CLASS))?;

        let panel_children = panel_symbol.children.as_ref().or_fail()?;
        let child_names = panel_children
            .iter()
            .map(|child| child.name.clone())
            .collect::<Vec<_>>();

        let init_symbol = symbol_contains_child(panel_symbol, "PANEL:Init").or_fail()?;
        let paint_symbol = symbol_contains_child(panel_symbol, "PANEL:Paint").or_fail()?;

        verify_that!(child_names.contains(&"PANEL:Init".to_string()), eq(true))?;
        verify_that!(child_names.contains(&"PANEL:Paint".to_string()), eq(true))?;
        verify_that!(range_contains(panel_symbol, init_symbol), eq(true))?;
        verify_that!(range_contains(panel_symbol, paint_symbol), eq(true))?;
        verify_that!(panel_symbol.selection_range.start.line, eq(1))?;
        verify_that!(panel_symbol.selection_range.end.line, eq(1))?;

        verify_that!(
            top_level_symbols
                .iter()
                .any(|symbol| symbol.name == "PANEL:Init"),
            eq(false)
        )?;
        verify_that!(
            top_level_symbols
                .iter()
                .any(|symbol| symbol.name == "PANEL:Paint"),
            eq(false)
        )?;
        verify_that!(
            top_level_symbols
                .iter()
                .any(|symbol| symbol.name == "PANEL"),
            eq(false)
        )
    }

    #[gtest]
    fn vgui_panel_assignment_symbols_keep_selection_range_and_expand_container_range() -> Result<()>
    {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/vgui/global_panel.lua",
            r#"
            PANEL = {}

            function PANEL:Init()
            end

            vgui.Register("GlobalPanel", PANEL, "DPanel")
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let panel_symbol =
            find_top_level_symbol(top_level_symbols, "GlobalPanel (VGUI)").or_fail()?;
        let init_symbol = symbol_contains_child(panel_symbol, "PANEL:Init").or_fail()?;

        verify_that!(panel_symbol.kind, eq(SymbolKind::CLASS))?;
        verify_that!(range_contains(panel_symbol, init_symbol), eq(true))?;
        verify_that!(panel_symbol.selection_range.start.line, eq(1))?;
        verify_that!(panel_symbol.selection_range.end.line, eq(1))
    }

    #[gtest]
    fn normal_verbosity_hides_primitive_locals() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.outline.verbosity = EmmyrcGmodOutlineVerbosity::Normal;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/outline_normal.lua",
            r#"
            local primitive = 1
            local fn = function() end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;
        let names = top_level_names(top_level_symbols);

        verify_that!(names.contains(&"primitive".to_string()), eq(false))?;
        verify_that!(names.contains(&"fn".to_string()), eq(true))
    }

    #[gtest]
    fn hidden_local_value_exprs_are_still_processed() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.outline.verbosity = EmmyrcGmodOutlineVerbosity::Normal;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/hidden_local_value_exprs.lua",
            r#"
            local hidden = net.ReadUInt(8)
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let symbol = find_top_level_symbol(top_level_symbols, "net.ReadUInt(8)").or_fail()?;
        verify_that!(symbol.kind, eq(SymbolKind::EVENT))
    }

    #[gtest]
    fn hidden_assign_value_exprs_are_still_processed() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.outline.verbosity = EmmyrcGmodOutlineVerbosity::Normal;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/hidden_assign_value_exprs.lua",
            r#"
            local hidden = 1
            hidden = net.ReadUInt(8)
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let symbol = find_top_level_symbol(top_level_symbols, "net.ReadUInt(8)").or_fail()?;
        verify_that!(symbol.kind, eq(SymbolKind::EVENT))
    }

    #[gtest]
    fn verbose_verbosity_keeps_primitive_locals() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.outline.verbosity = EmmyrcGmodOutlineVerbosity::Verbose;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/outline_verbose.lua",
            r#"
            local primitive = 1
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;
        let names = top_level_names(top_level_symbols);

        verify_that!(names.contains(&"primitive".to_string()), eq(true))
    }

    #[gtest]
    fn hook_add_has_named_outline_symbol() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/hook_symbol.lua",
            r#"
            hook.Add("Think", "MyHook", function()
                local x = 1
            end)
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let hook_symbol = find_top_level_symbol(top_level_symbols, "hook: Think").or_fail()?;
        verify_that!(hook_symbol.kind, eq(SymbolKind::EVENT))
    }

    #[gtest]
    fn hook_symbol_range_contains_nested_children() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.outline.verbosity = EmmyrcGmodOutlineVerbosity::Verbose;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/hook_symbol_range.lua",
            r#"
            hook.Add("Think", "MyHook", function()
                local nested = function()
                end
            end)
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let hook_symbol = find_top_level_symbol(top_level_symbols, "hook: Think").or_fail()?;
        let nested_symbol = symbol_contains_child(hook_symbol, "nested").or_fail()?;

        verify_that!(range_contains(hook_symbol, nested_symbol), eq(true))
    }

    #[gtest]
    fn net_operations_are_reported_as_event_symbols() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/net_ops.lua",
            r#"
            net.Start("MyMessage")
            net.WriteUInt(16, 8)
            net.Broadcast()
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        for expected in [
            "net.Start(\"MyMessage\")",
            "net.WriteUInt(16, 8)",
            "net.Broadcast()",
        ] {
            let symbol = find_top_level_symbol(top_level_symbols, expected).or_fail()?;
            verify_that!(symbol.kind, eq(SymbolKind::EVENT))?;
        }

        Ok(())
    }

    #[gtest]
    fn net_operation_symbol_name_is_normalized_and_truncated() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/net_op_label_format.lua",
            r#"
            net.WriteString(
                "ThisIsAnExtremelyLongMessageNameThatShouldForceOutlineLabelTruncationBecauseItKeepsGoing"
            )
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let symbol = top_level_symbols
            .iter()
            .find(|symbol| symbol.name.starts_with("net.WriteString"))
            .or_fail()?;

        verify_that!(symbol.kind, eq(SymbolKind::EVENT))?;
        verify_that!(symbol.name.contains('\n'), eq(false))?;
        verify_that!(symbol.name.contains('\t'), eq(false))?;
        verify_that!(symbol.name.ends_with("..."), eq(true))?;
        verify_that!(symbol.name.len() <= 59, eq(true))
    }

    #[gtest]
    fn net_receive_callback_params_are_inlined_to_call_symbol() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/server/net_receive_inlined.lua",
            r#"
            net.Receive("MyMessage", function(len, ply)
                local captured = len
            end)
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let receive_symbol =
            find_top_level_symbol(top_level_symbols, "net.Receive: MyMessage").or_fail()?;
        let children = receive_symbol.children.as_ref().or_fail()?;
        let child_names = top_level_names(children);

        verify_that!(child_names.contains(&"len".to_string()), eq(true))?;
        verify_that!(child_names.contains(&"ply".to_string()), eq(true))?;
        verify_that!(child_names.contains(&"closure".to_string()), eq(false))
    }

    #[gtest]
    fn scripted_entity_methods_group_without_explicit_decl() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/entities/my_entity/init.lua",
            r#"
            function ENT:Think()
            end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let class_symbol =
            find_top_level_symbol(top_level_symbols, "my_entity (Entity)").or_fail()?;
        let children = class_symbol.children.as_ref().or_fail()?;
        let child_names = top_level_names(children);
        verify_that!(child_names.contains(&"ENT:Think".to_string()), eq(true))
    }

    #[gtest]
    fn scripted_entity_explicit_decl_does_not_duplicate_class_symbol() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/entities/my_entity/init.lua",
            r#"
            ENT = {}

            function ENT:Think()
            end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.unwrap_or_default();

        let class_symbols = top_level_symbols
            .iter()
            .filter(|symbol| symbol.name == "my_entity (Entity)")
            .collect::<Vec<_>>();
        verify_that!(class_symbols.len(), eq(1))?;

        let bad_label_exists = top_level_symbols
            .iter()
            .any(|symbol| symbol.name == "my_entity (Entity) (VGUI)");
        verify_that!(bad_label_exists, eq(false))?;

        let children = class_symbols[0].children.as_ref().or_fail()?;
        let child_names = top_level_names(children);
        verify_that!(child_names.contains(&"ENT:Think".to_string()), eq(true))
    }

    #[gtest]
    fn local_function_symbol_keeps_function_kind() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/function_symbol.lua",
            r#"
            local function foo(a, b)
            end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let func_symbol = find_top_level_symbol(top_level_symbols, "foo").or_fail()?;
        verify_that!(func_symbol.kind, eq(SymbolKind::FUNCTION))?;
        verify_that!(func_symbol.detail.as_ref().is_some(), eq(true))
    }

    #[gtest]
    fn gmod_disabled_uses_legacy_verbose_outline_behavior() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/non_gmod_verbose.lua",
            r#"
            local primitive = 1
            if true then
                local nested = 2
            end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;
        let names = top_level_names(top_level_symbols);

        verify_that!(names.contains(&"primitive".to_string()), eq(true))?;
        verify_that!(names.contains(&"if".to_string()), eq(true))
    }

    #[gtest]
    fn shadowed_local_scripted_global_is_not_promoted_to_class() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/entities/my_entity/init.lua",
            r#"
            function ENT:Think()
                local ENT = {}
            end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.as_ref().or_fail()?;

        let class_symbol =
            find_top_level_symbol(top_level_symbols, "my_entity (Entity)").or_fail()?;
        let children = class_symbol.children.as_ref().or_fail()?;
        let has_nested_class_symbol = any_symbol_matches(children, &|symbol| {
            symbol.kind == SymbolKind::CLASS && symbol.name == "my_entity (Entity)"
        });
        verify_that!(has_nested_class_symbol, eq(false))
    }

    #[gtest]
    fn minimal_verbosity_does_not_leak_fields_from_hidden_tables() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.outline.verbosity = EmmyrcGmodOutlineVerbosity::Minimal;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/test/lua/autorun/minimal_hidden_table.lua",
            r#"
            local hidden = {
                BuildPanel = function() end,
            }
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .or_fail()?;
        let root = build_document_symbol(&semantic_model, &CancellationToken::new()).or_fail()?;
        let top_level_symbols = root.children.unwrap_or_default();

        let has_hidden = top_level_symbols
            .iter()
            .any(|symbol| symbol.name == "hidden");
        let has_field = top_level_symbols
            .iter()
            .any(|symbol| symbol.name == "BuildPanel");
        verify_that!(has_hidden, eq(false))?;
        verify_that!(has_field, eq(false))
    }
}
