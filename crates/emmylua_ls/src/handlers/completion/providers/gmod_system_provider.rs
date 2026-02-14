use std::collections::HashMap;

use emmylua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaLiteralExpr, LuaStringToken, PathTrait,
};
use lsp_types::{CompletionItem, CompletionTextEdit, TextEdit};

use crate::handlers::completion::completion_builder::CompletionBuilder;

use super::get_text_edit_range_in_string;

pub fn add_completion(builder: &mut CompletionBuilder) -> Option<()> {
    if builder.is_cancelled() {
        return None;
    }
    if !builder.semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let string_token = LuaStringToken::cast(builder.trigger_token.clone())?;
    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let call_expr = literal_expr
        .get_parent::<LuaCallArgList>()?
        .get_parent::<LuaCallExpr>()?;

    let text_edit_range = get_text_edit_range_in_string(builder, string_token)?;
    let added = if is_net_message_string_context(&call_expr, literal_expr.clone()) {
        add_net_message_completion_items(builder, Some(text_edit_range))
    } else if is_hook_name_string_context(builder, &call_expr, literal_expr) {
        add_hook_completion_items(builder, Some(text_edit_range))
    } else {
        false
    };
    if added {
        builder.stop_here();
    }

    Some(())
}

fn add_net_message_completion_items(
    builder: &mut CompletionBuilder,
    text_edit_range: Option<lsp_types::Range>,
) -> bool {
    let before_count = builder.get_completion_items_mut().len();
    let infer_index = builder.semantic_model.get_db().get_gmod_infer_index();
    let mut net_name_stats: HashMap<String, (usize, usize)> = HashMap::new();
    for (_, metadata) in infer_index.iter_system_file_metadata() {
        for net_registration in &metadata.net_add_string_calls {
            if let Some(name) = normalize_name(net_registration.name.as_deref()) {
                net_name_stats.entry(name.to_string()).or_default().0 += 1;
            }
        }
        for net_receive in &metadata.net_receive_calls {
            if let Some(name) = normalize_name(net_receive.message_name.as_deref()) {
                net_name_stats.entry(name.to_string()).or_default().1 += 1;
            }
        }
    }

    let mut names = net_name_stats.into_iter().collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, (registration_count, receiver_count)) in names {
        let text_edit = text_edit_range.map(|range| {
            CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: name.clone(),
            })
        });
        let _ = builder.add_completion_item(CompletionItem {
            label: name,
            kind: Some(lsp_types::CompletionItemKind::CONSTANT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(format!(
                    "({registration_count} registrations, {receiver_count} receivers)"
                )),
                description: None,
            }),
            detail: Some("GMod net message".to_string()),
            text_edit,
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn add_hook_completion_items(
    builder: &mut CompletionBuilder,
    text_edit_range: Option<lsp_types::Range>,
) -> bool {
    #[derive(Default)]
    struct HookStats {
        add_count: usize,
        method_count: usize,
        emit_count: usize,
        callback_params: Option<Vec<String>>,
    }

    let before_count = builder.get_completion_items_mut().len();
    let infer_index = builder.semantic_model.get_db().get_gmod_infer_index();
    let mut hook_stats: HashMap<String, HookStats> = HashMap::new();

    for (_, metadata) in infer_index.iter_hook_file_metadata() {
        for hook_site in &metadata.sites {
            let Some(name) = normalize_name(hook_site.hook_name.as_deref()) else {
                continue;
            };

            let stats = hook_stats.entry(name.to_string()).or_default();
            match hook_site.kind {
                emmylua_code_analysis::GmodHookKind::Add => stats.add_count += 1,
                emmylua_code_analysis::GmodHookKind::GamemodeMethod => stats.method_count += 1,
                emmylua_code_analysis::GmodHookKind::Emit => stats.emit_count += 1,
            }

            if stats.callback_params.is_none() && !hook_site.callback_params.is_empty() {
                stats.callback_params = Some(hook_site.callback_params.clone());
            }
        }
    }

    let mut names = hook_stats.into_iter().collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, stats) in names {
        let text_edit = text_edit_range.map(|range| {
            CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: name.clone(),
            })
        });
        let args_detail = stats
            .callback_params
            .as_ref()
            .filter(|params| !params.is_empty())
            .map(|params| format!(" args: {}", params.join(", ")))
            .unwrap_or_default();
        let _ = builder.add_completion_item(CompletionItem {
            label: name,
            kind: Some(lsp_types::CompletionItemKind::CONSTANT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(format!(
                    "({} add, {} methods, {} emits){}",
                    stats.add_count, stats.method_count, stats.emit_count, args_detail
                )),
                description: None,
            }),
            detail: Some("GMod hook".to_string()),
            text_edit,
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn is_net_message_string_context(call_expr: &LuaCallExpr, literal_expr: LuaLiteralExpr) -> bool {
    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };
    if !matches_call_path(&call_path, "util.AddNetworkString")
        && !matches_call_path(&call_path, "net.Start")
        && !matches_call_path(&call_path, "net.Receive")
    {
        return false;
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };
    let arg_idx = args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position());

    arg_idx == Some(0)
}

fn is_hook_name_string_context(
    builder: &CompletionBuilder,
    call_expr: &LuaCallExpr,
    literal_expr: LuaLiteralExpr,
) -> bool {
    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };
    let is_builtin = matches_call_path(&call_path, "hook.Add")
        || matches_call_path(&call_path, "hook.Run")
        || matches_call_path(&call_path, "hook.Call");
    let is_custom_emitter = builder
        .semantic_model
        .get_emmyrc()
        .gmod
        .hook_mappings
        .emitter_to_hook
        .iter()
        .any(|(emitter_path, mapped_hook)| {
            mapped_hook == "*" && matches_call_path(&call_path, emitter_path)
        });
    if !is_builtin && !is_custom_emitter {
        return false;
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };
    let arg_idx = args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position());
    arg_idx == Some(0)
}

fn matches_call_path(path: &str, target: &str) -> bool {
    path == target || path.ends_with(&format!(".{target}")) || path.ends_with(&format!(":{target}"))
}

fn normalize_name(name: Option<&str>) -> Option<&str> {
    let name = name?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
