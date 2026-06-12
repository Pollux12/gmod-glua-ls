use std::collections::HashMap;

use glua_code_analysis::{
    FileId, GmodHookSiteMetadata, GmodRealm, LuaType, NetSendFlow, NetSendKind, SemanticModel,
    find_call_arg_role_from_type,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaComment, LuaCommentOwner, LuaDocTag,
    LuaDocTagRealm, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaLiteralExpr,
    LuaLocalFuncStat, LuaStringToken,
};
use lsp_types::{
    Command, CompletionItem, CompletionTextEdit, InsertTextFormat, InsertTextMode, TextEdit,
};
use rowan::TextSize;

use crate::handlers::completion::add_completions::CompletionTriggerStatus;
use crate::handlers::completion::completion_builder::CompletionBuilder;
use crate::handlers::completion::completion_data::CompletionData;
use crate::handlers::gmod_string_context::{
    find_call_arg_roles, find_string_call_arg_role, is_hook_name_string_context,
    is_net_message_string_context,
};
use crate::handlers::hover::resolve_hook_property_owner;

use super::get_text_edit_range_in_string;

const TRIGGER_SUGGEST_COMMAND: &str = "editor.action.triggerSuggest";

#[derive(Default, Clone)]
struct HookStats {
    add_count: usize,
    method_count: usize,
    emit_count: usize,
    callback_params: Option<(u8, Vec<String>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagedStringCallKind {
    NetReceive,
    HookAdd,
    HookEmit { include_gamemode_arg: bool },
}

pub fn add_completion(builder: &mut CompletionBuilder) -> Option<()> {
    if builder.is_cancelled() {
        return None;
    }
    if !builder.semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let mut added = false;
    if builder
        .semantic_model
        .get_emmyrc()
        .gmod
        .network
        .completion
        .smart_read_suggestions
    {
        added |= add_net_read_completion_items(builder);
    }

    if let Some(string_token) = completion_string_token(builder)
        && let Some(literal_expr) = string_token.get_parent::<LuaLiteralExpr>()
        && let Some(call_expr) = literal_expr
            .get_parent::<LuaCallArgList>()
            .and_then(|args| args.get_parent::<LuaCallExpr>())
        && let Some(arg_index) = literal_arg_index(&call_expr, &literal_expr)
        && let Some(text_edit_range) = get_text_edit_range_in_string(builder, string_token)
    {
        let string_added =
            if is_net_message_string_context(&builder.semantic_model, &call_expr, arg_index) {
                match staged_string_call_kind(&builder.semantic_model, &call_expr, arg_index) {
                    Some(StagedStringCallKind::NetReceive)
                        if staged_call_snippets_enabled(builder) =>
                    {
                        add_staged_net_receive_completion_items(builder, &call_expr)
                    }
                    _ => add_net_message_completion_items(builder, Some(text_edit_range)),
                }
            } else if is_hook_name_string_context(&builder.semantic_model, &call_expr, arg_index) {
                match staged_string_call_kind(&builder.semantic_model, &call_expr, arg_index) {
                    Some(StagedStringCallKind::HookAdd)
                        if staged_call_snippets_enabled(builder) =>
                    {
                        add_staged_hook_add_completion_items(builder, &call_expr)
                    }
                    Some(StagedStringCallKind::HookEmit {
                        include_gamemode_arg,
                    }) if staged_call_snippets_enabled(builder) => {
                        add_staged_hook_emit_completion_items(
                            builder,
                            &call_expr,
                            include_gamemode_arg,
                        )
                    }
                    _ => add_hook_completion_items(builder, Some(text_edit_range)),
                }
            } else {
                false
            };
        if string_added {
            builder.stop_here();
        }

        return Some(());
    }

    if added { Some(()) } else { None }
}

pub fn apply_staged_call_snippet(
    builder: &CompletionBuilder,
    label: &str,
    status: CompletionTriggerStatus,
    typ: &LuaType,
    completion_item: &mut CompletionItem,
) -> Option<()> {
    if status != CompletionTriggerStatus::Dot || !staged_call_snippets_enabled(builder) {
        return None;
    }

    let kind = staged_string_call_kind_from_type(builder.semantic_model.get_db(), typ)?;

    completion_item.insert_text = Some(match kind {
        StagedStringCallKind::HookEmit {
            include_gamemode_arg: true,
        } => format!(r#"{}("${{1}}", ${{2:GAMEMODE}})"#, label),
        _ => format!(r#"{}("${{1}}")"#, label),
    });
    completion_item.insert_text_format = Some(InsertTextFormat::SNIPPET);
    completion_item.sort_text = Some(format!("000_gmod_staged_call_{}", label.to_lowercase()));
    completion_item.command = Some(trigger_suggest_command());
    Some(())
}

fn add_net_read_completion_items(builder: &mut CompletionBuilder) -> bool {
    let Some(trigger_parent) = builder.trigger_token.parent() else {
        return false;
    };
    let Some(index_expr) = LuaIndexExpr::cast(trigger_parent) else {
        return false;
    };
    let Some(index_token) = index_expr.get_index_token() else {
        return false;
    };
    if !index_token.is_dot() {
        return false;
    }

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let LuaExpr::NameExpr(prefix_name_expr) = prefix_expr else {
        return false;
    };
    if prefix_name_expr.get_name_text().as_deref() != Some("net") {
        return false;
    }

    let typed_member = match index_expr.get_index_key() {
        Some(LuaIndexKey::Name(name_token)) => name_token.get_name_text().to_string(),
        None => String::new(),
        _ => return false,
    };
    if !typed_member.is_empty() && !typed_member.starts_with('R') && !typed_member.starts_with('r')
    {
        return false;
    }

    let Some(replace_range) = builder
        .semantic_model
        .get_document()
        .to_lsp_range(index_expr.get_range())
    else {
        return false;
    };

    let db = builder.semantic_model.get_db();
    let infer_index = db.get_gmod_infer_index();
    let network_index = db.get_gmod_network_index();
    let file_id = builder.semantic_model.get_file_id();
    let Some(system_metadata) = infer_index.get_system_file_metadata(&file_id) else {
        return false;
    };

    let Some(receive_site) = system_metadata.net_receive_calls.iter().find(|site| {
        site.callback
            .callback_range
            .is_some_and(|range| range.contains(builder.position_offset))
    }) else {
        return false;
    };

    let Some(message_name) = normalize_name(receive_site.message_name.as_deref()) else {
        return false;
    };

    let receive_call_range = receive_site.syntax_id.get_range();
    let Some(file_network_data) = network_index.get_file_data(file_id) else {
        return false;
    };
    let Some(receive_flow) = file_network_data.receive_flows.iter().find(|flow| {
        flow.receive_range == receive_call_range && flow.message_name.as_str() == message_name
    }) else {
        return false;
    };

    let consumed_reads = receive_flow
        .reads
        .iter()
        .filter(|entry| entry.range.end() <= builder.position_offset)
        .count();
    let current_read = receive_flow
        .reads
        .iter()
        .find(|entry| entry.range.contains(builder.position_offset))
        .map(|entry| entry.kind);

    let receive_realm = infer_index.get_realm_at_offset(&file_id, builder.position_offset);
    let Some((send_file_id, send_flow)) = choose_preferred_send_flow(
        network_index.get_send_flows_for_message(message_name),
        infer_index,
        receive_realm,
    ) else {
        return false;
    };

    let sender_realm =
        infer_index.get_realm_at_offset(&send_file_id, send_flow.start_range.start());
    let remaining_expected_reads: Vec<_> = send_flow
        .writes
        .iter()
        .skip(consumed_reads)
        .filter_map(|entry| {
            entry
                .kind
                .to_read_counterpart()
                .map(|read_kind| (entry.kind, read_kind, entry.bits))
        })
        .collect();
    if remaining_expected_reads.is_empty() {
        return false;
    }

    let mismatch_marker = if builder
        .semantic_model
        .get_emmyrc()
        .gmod
        .network
        .completion
        .mismatch_hints
    {
        if let Some(actual_kind) = current_read {
            if let Some((_, expected_kind, _)) = remaining_expected_reads.first() {
                if actual_kind != *expected_kind {
                    Some(format!(
                        " [hint: current read is {}, expected {}]",
                        actual_kind.to_fn_name(),
                        expected_kind.to_fn_name()
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    for (index, (write_kind, read_kind, write_bits)) in
        remaining_expected_reads.into_iter().enumerate()
    {
        let mut detail = format!(
            "Expected read (matches {} in {} send)",
            write_kind.to_fn_name(),
            realm_label(sender_realm)
        );
        if index == 0
            && let Some(marker) = &mismatch_marker
        {
            detail.push_str(marker);
        }

        // For ReadUInt/ReadInt, fill in the bit-width literal automatically
        // when the matching writer used a literal we can read. When unknown,
        // leave a snippet placeholder so the user knows they must specify it.
        let needs_bits = matches!(
            read_kind,
            glua_code_analysis::NetOpKind::ReadUInt | glua_code_analysis::NetOpKind::ReadInt
        );
        let (insert_text, insert_text_format) = if needs_bits {
            match write_bits {
                Some(bits) => (
                    format!("{}({bits})", read_kind.to_fn_name()),
                    Some(InsertTextFormat::PLAIN_TEXT),
                ),
                None => (
                    format!("{}(${{1:bits}})", read_kind.to_fn_name()),
                    Some(InsertTextFormat::SNIPPET),
                ),
            }
        } else {
            (
                read_kind.to_fn_name().to_string(),
                Some(InsertTextFormat::PLAIN_TEXT),
            )
        };
        let kind = if needs_bits && write_bits.is_none() {
            lsp_types::CompletionItemKind::SNIPPET
        } else {
            lsp_types::CompletionItemKind::FUNCTION
        };

        let _ = builder.add_completion_item(CompletionItem {
            label: read_kind.to_fn_name().to_string(),
            kind: Some(kind),
            detail: Some(detail),
            sort_text: Some(format!("000_gmod_net_read_{index:03}")),
            insert_text: Some(insert_text.clone()),
            insert_text_format,
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range.clone(),
                new_text: insert_text,
            })),
            ..Default::default()
        });
    }

    true
}

fn choose_preferred_send_flow<'a>(
    send_flows: Vec<(FileId, &'a NetSendFlow)>,
    infer_index: &glua_code_analysis::GmodInferIndex,
    receive_realm: GmodRealm,
) -> Option<(FileId, &'a NetSendFlow)> {
    send_flows
        .into_iter()
        .filter(|(_, flow)| !flow.writes.is_empty())
        .max_by_key(|(send_file_id, flow)| {
            let sender_realm =
                infer_index.get_realm_at_offset(send_file_id, flow.start_range.start());
            let realm_score = if matches!(receive_realm, GmodRealm::Client | GmodRealm::Server) {
                let mut score = 0;
                if expected_receiver_realm(flow.send_kind) == Some(receive_realm) {
                    score += 4;
                }
                if opposite_realm(receive_realm).is_some_and(|realm| realm == sender_realm) {
                    score += 2;
                }
                if matches!(sender_realm, GmodRealm::Client | GmodRealm::Server) {
                    score += 1;
                }
                score
            } else if matches!(sender_realm, GmodRealm::Client | GmodRealm::Server) {
                1
            } else {
                0
            };

            (realm_score, flow.writes.len())
        })
}

fn expected_receiver_realm(send_kind: NetSendKind) -> Option<GmodRealm> {
    match send_kind {
        NetSendKind::Send
        | NetSendKind::Broadcast
        | NetSendKind::Omit
        | NetSendKind::PAS
        | NetSendKind::PVS => Some(GmodRealm::Client),
        NetSendKind::SendToServer => Some(GmodRealm::Server),
    }
}

fn opposite_realm(realm: GmodRealm) -> Option<GmodRealm> {
    match realm {
        GmodRealm::Client => Some(GmodRealm::Server),
        GmodRealm::Server => Some(GmodRealm::Client),
        GmodRealm::Shared | GmodRealm::Menu | GmodRealm::Unknown => None,
    }
}

fn realm_label(realm: GmodRealm) -> &'static str {
    match realm {
        GmodRealm::Client => "client",
        GmodRealm::Server => "server",
        GmodRealm::Shared => "shared",
        GmodRealm::Menu => "menu",
        GmodRealm::Unknown => "unknown",
    }
}

fn add_net_message_completion_items(
    builder: &mut CompletionBuilder,
    text_edit_range: Option<lsp_types::Range>,
) -> bool {
    let before_count = builder.get_completion_items_mut().len();
    for (name, (registration_count, receiver_count)) in collect_net_message_stats(builder) {
        let filter_text = name.clone();
        let sort_text = format!("010_gmod_net_message_{}", completion_sort_key(&name));
        let text_edit = text_edit_range.map(|range| {
            CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: name.clone(),
            })
        });
        let _ = builder.add_completion_item(CompletionItem {
            label: name,
            kind: Some(lsp_types::CompletionItemKind::EVENT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(net_message_label_detail(registration_count, receiver_count)),
                description: Some("GMod net message".to_string()),
            }),
            detail: Some("GMod net message".to_string()),
            filter_text: Some(filter_text),
            sort_text: Some(sort_text),
            text_edit,
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn add_staged_net_receive_completion_items(
    builder: &mut CompletionBuilder,
    call_expr: &LuaCallExpr,
) -> bool {
    let before_count = builder.get_completion_items_mut().len();
    let Some(string_token) = completion_string_token(builder) else {
        return false;
    };
    let Some(replace_range) = staged_call_edit_range(builder, &string_token, call_expr) else {
        return false;
    };

    let call_realm = builder
        .semantic_model
        .get_db()
        .get_gmod_infer_index()
        .get_realm_at_offset(
            &builder.semantic_model.get_file_id(),
            builder.position_offset,
        );

    for (name, (registration_count, receiver_count)) in collect_net_message_stats(builder) {
        let snippet = build_net_receive_snippet(&name, call_realm);
        let sort_text = format!("000_gmod_net_receive_{}", completion_sort_key(&name));
        let _ = builder.add_completion_item(CompletionItem {
            label: name.clone(),
            kind: Some(lsp_types::CompletionItemKind::EVENT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(net_message_label_detail(registration_count, receiver_count)),
                description: Some("GMod net message".to_string()),
            }),
            detail: Some("GMod net message".to_string()),
            filter_text: Some(name.clone()),
            sort_text: Some(sort_text),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            insert_text_mode: Some(InsertTextMode::ADJUST_INDENTATION),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range.clone(),
                new_text: snippet,
            })),
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn add_hook_completion_items(
    builder: &mut CompletionBuilder,
    text_edit_range: Option<lsp_types::Range>,
) -> bool {
    let before_count = builder.get_completion_items_mut().len();
    for (name, stats, data) in collect_hook_completion_entries(builder) {
        let filter_text = name.clone();
        let sort_text = format!("010_gmod_hook_name_{}", completion_sort_key(&name));
        let text_edit = text_edit_range.map(|range| {
            CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: name.clone(),
            })
        });
        let _ = builder.add_completion_item(CompletionItem {
            label: name,
            kind: Some(lsp_types::CompletionItemKind::EVENT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(hook_label_detail(&stats)),
                description: Some("GMod hook".to_string()),
            }),
            detail: Some("GMod hook".to_string()),
            data,
            filter_text: Some(filter_text),
            sort_text: Some(sort_text),
            text_edit,
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn add_staged_hook_add_completion_items(
    builder: &mut CompletionBuilder,
    call_expr: &LuaCallExpr,
) -> bool {
    let before_count = builder.get_completion_items_mut().len();
    let Some(string_token) = completion_string_token(builder) else {
        return false;
    };
    let Some(replace_range) = staged_call_edit_range(builder, &string_token, call_expr) else {
        return false;
    };

    for (name, stats, data) in collect_hook_completion_entries(builder) {
        let callback_params = stats
            .callback_params
            .as_ref()
            .map_or(&[][..], |(_, params)| params.as_slice());
        let sort_text = format!("000_gmod_hook_add_{}", completion_sort_key(&name));
        let _ = builder.add_completion_item(CompletionItem {
            label: name.clone(),
            kind: Some(lsp_types::CompletionItemKind::EVENT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(hook_label_detail(&stats)),
                description: Some("GMod hook".to_string()),
            }),
            detail: Some("GMod hook".to_string()),
            data,
            filter_text: Some(name.clone()),
            sort_text: Some(sort_text),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            insert_text_mode: Some(InsertTextMode::ADJUST_INDENTATION),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range.clone(),
                new_text: build_hook_add_snippet(&name, callback_params),
            })),
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn add_staged_hook_emit_completion_items(
    builder: &mut CompletionBuilder,
    call_expr: &LuaCallExpr,
    include_gamemode_arg: bool,
) -> bool {
    let before_count = builder.get_completion_items_mut().len();
    let Some(string_token) = completion_string_token(builder) else {
        return false;
    };
    let Some(replace_range) = staged_call_edit_range(builder, &string_token, call_expr) else {
        return false;
    };

    for (name, stats, data) in collect_hook_completion_entries(builder) {
        let callback_params = stats
            .callback_params
            .as_ref()
            .map_or(&[][..], |(_, params)| params.as_slice());
        let sort_text = format!("000_gmod_hook_emit_{}", completion_sort_key(&name));
        let _ = builder.add_completion_item(CompletionItem {
            label: name.clone(),
            kind: Some(lsp_types::CompletionItemKind::EVENT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(hook_label_detail(&stats)),
                description: Some("GMod hook".to_string()),
            }),
            detail: Some("GMod hook".to_string()),
            data,
            filter_text: Some(name.clone()),
            sort_text: Some(sort_text),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            insert_text_mode: Some(InsertTextMode::ADJUST_INDENTATION),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range.clone(),
                new_text: build_hook_emit_snippet(&name, callback_params, include_gamemode_arg),
            })),
            ..Default::default()
        });
    }

    builder.get_completion_items_mut().len() > before_count
}

fn net_message_label_detail(registration_count: usize, receiver_count: usize) -> String {
    format!(
        "({}, {})",
        count_label(registration_count, "registration", "registrations"),
        count_label(receiver_count, "receiver", "receivers")
    )
}

fn hook_label_detail(stats: &HookStats) -> String {
    let mut source_parts = Vec::with_capacity(3);
    if stats.add_count > 0 {
        source_parts.push(count_label(stats.add_count, "hook.Add", "hook.Add"));
    }
    if stats.method_count > 0 {
        source_parts.push(count_label(stats.method_count, "method", "methods"));
    }
    if stats.emit_count > 0 {
        source_parts.push(count_label(stats.emit_count, "emit", "emits"));
    }

    let source_detail = if source_parts.is_empty() {
        "0 sources".to_string()
    } else {
        source_parts.join(", ")
    };

    if let Some((_, params)) = &stats.callback_params
        && !params.is_empty()
    {
        return format!("({source_detail}; args: {})", params.join(", "));
    }

    format!("({source_detail})")
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let label = if count == 1 { singular } else { plural };
    format!("{count} {label}")
}

fn collect_net_message_stats(builder: &CompletionBuilder) -> Vec<(String, (usize, usize))> {
    let infer_index = builder.semantic_model.get_db().get_gmod_infer_index();
    let mut net_name_stats: HashMap<String, (usize, usize)> = HashMap::new();
    for (_, metadata) in infer_index.iter_system_file_metadata() {
        if builder.is_cancelled() {
            return Vec::new();
        }
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
    names
}

fn collect_hook_completion_entries(
    builder: &CompletionBuilder,
) -> Vec<(String, HookStats, Option<serde_json::Value>)> {
    let infer_index = builder.semantic_model.get_db().get_gmod_infer_index();
    let mut hook_stats: HashMap<String, HookStats> = HashMap::new();
    let call_mask = infer_index.get_state_mask_at_offset(
        &builder.semantic_model.get_file_id(),
        builder.position_offset,
    );
    let should_filter_realm = !call_mask.is_empty();

    for (file_id, metadata) in infer_index.iter_hook_file_metadata() {
        if builder.is_cancelled() {
            return Vec::new();
        }
        for hook_site in &metadata.sites {
            if should_filter_realm {
                let hook_realm =
                    resolve_hook_site_realm(&builder.semantic_model, file_id, hook_site);
                if !call_mask.is_compatible_with(hook_realm.state_mask()) {
                    continue;
                }
            }

            let Some(name) = normalize_name(hook_site.hook_name.as_deref()) else {
                continue;
            };

            let stats = hook_stats.entry(name.to_string()).or_default();
            match hook_site.kind {
                glua_code_analysis::GmodHookKind::Add => stats.add_count += 1,
                glua_code_analysis::GmodHookKind::GamemodeMethod => stats.method_count += 1,
                glua_code_analysis::GmodHookKind::Emit => stats.emit_count += 1,
            }

            let callback_priority = match hook_site.kind {
                glua_code_analysis::GmodHookKind::GamemodeMethod => 2,
                glua_code_analysis::GmodHookKind::Add => 1,
                glua_code_analysis::GmodHookKind::Emit => 0,
            };
            if !hook_site.callback_params.is_empty()
                && stats
                    .callback_params
                    .as_ref()
                    .is_none_or(|(priority, params)| {
                        callback_priority > *priority
                            || (callback_priority == *priority
                                && hook_site.callback_params.len() > params.len())
                    })
            {
                stats.callback_params =
                    Some((callback_priority, hook_site.callback_params.clone()));
            }
        }
    }

    let mut names = hook_stats.into_iter().collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let file_id = builder.semantic_model.get_file_id();
    names
        .into_iter()
        .map(|(name, stats)| {
            let data = resolve_hook_property_owner(
                &builder.semantic_model,
                file_id,
                builder.position_offset,
                &name,
            )
            .and_then(|id| CompletionData::from_property_owner_id(builder, id, None));
            (name, stats, data)
        })
        .collect()
}

fn literal_arg_index(call_expr: &LuaCallExpr, literal_expr: &LuaLiteralExpr) -> Option<usize> {
    let args_list = call_expr.get_args_list()?;
    args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position())
}

fn staged_string_call_kind(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> Option<StagedStringCallKind> {
    if find_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.net_message",
        &["receive"],
    )
    .is_some()
    {
        return Some(StagedStringCallKind::NetReceive);
    }

    let hook_role = find_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.hook",
        &["add", "emit"],
    )?;
    match hook_role.role.as_str() {
        "add" => Some(StagedStringCallKind::HookAdd),
        "emit" => Some(StagedStringCallKind::HookEmit {
            include_gamemode_arg: call_has_hook_gamemode_table_role(semantic_model, call_expr),
        }),
        _ => None,
    }
}

fn staged_string_call_kind_from_type(
    db: &glua_code_analysis::DbIndex,
    typ: &LuaType,
) -> Option<StagedStringCallKind> {
    if find_call_arg_role_from_type(db, typ, 0, "gmod.net_message", &["receive"]).is_some() {
        return Some(StagedStringCallKind::NetReceive);
    }

    let hook_role = find_call_arg_role_from_type(db, typ, 0, "gmod.hook", &["add", "emit"])?;
    match hook_role.role.as_str() {
        "add" => Some(StagedStringCallKind::HookAdd),
        "emit" => Some(StagedStringCallKind::HookEmit {
            include_gamemode_arg: find_call_arg_role_from_type(
                db,
                typ,
                1,
                "gmod.hook",
                &["gamemode_table"],
            )
            .is_some(),
        }),
        _ => None,
    }
}

fn call_has_hook_gamemode_table_role(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> bool {
    if let Some(args_list) = call_expr.get_args_list() {
        let arg_count = args_list.get_args().count();
        if find_call_arg_roles(
            semantic_model,
            call_expr,
            arg_count,
            "gmod.hook",
            &["gamemode_table"],
        )
        .into_iter()
        .any(|(_, role)| role.role == "gamemode_table")
        {
            return true;
        }
    }

    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };
    let Some(callable_type) = semantic_model.infer_expr(prefix_expr).ok() else {
        return false;
    };
    find_call_arg_role_from_type(
        semantic_model.get_db(),
        &callable_type,
        if call_expr.is_colon_call() { 2 } else { 1 },
        "gmod.hook",
        &["gamemode_table"],
    )
    .is_some()
}

fn completion_string_token(builder: &CompletionBuilder) -> Option<LuaStringToken> {
    LuaStringToken::cast(builder.trigger_token.clone())
        .or_else(|| {
            builder
                .trigger_token
                .prev_token()
                .and_then(LuaStringToken::cast)
        })
        .or_else(|| {
            builder
                .trigger_token
                .next_token()
                .and_then(LuaStringToken::cast)
        })
        .or_else(|| {
            builder.trigger_token.parent_ancestors().find_map(|node| {
                let literal_expr = LuaLiteralExpr::cast(node)?;
                literal_expr.token::<LuaStringToken>()
            })
        })
}

fn staged_call_edit_range(
    builder: &CompletionBuilder,
    string_token: &LuaStringToken,
    call_expr: &LuaCallExpr,
) -> Option<lsp_types::Range> {
    let text = string_token.get_text();
    let range = string_token.get_range();
    if text.is_empty() {
        return None;
    }

    let mut start_offset = u32::from(range.start());
    if text.starts_with('"') || text.starts_with('\'') {
        start_offset += 1;
    }

    let start_range = rowan::TextRange::new(start_offset.into(), start_offset.into());
    let start = builder
        .semantic_model
        .get_document()
        .to_lsp_range(start_range)?
        .start;
    let end = builder
        .semantic_model
        .get_document()
        .to_lsp_range(call_expr.get_range())?
        .end;

    Some(lsp_types::Range { start, end })
}

fn staged_call_snippets_enabled(builder: &CompletionBuilder) -> bool {
    builder
        .semantic_model
        .get_emmyrc()
        .completion
        .staged_call_snippets
}

fn trigger_suggest_command() -> Command {
    Command {
        title: "Suggest".to_string(),
        command: TRIGGER_SUGGEST_COMMAND.to_string(),
        arguments: None,
    }
}

fn build_hook_add_snippet(hook_name: &str, callback_params: &[String]) -> String {
    let callback_signature = if callback_params.is_empty() {
        "function()".to_string()
    } else {
        let params = callback_params
            .iter()
            .map(|param| escape_snippet_text(param))
            .collect::<Vec<_>>()
            .join(", ");
        format!("function({params})")
    };

    format!(
        "{}\", \"${{1:identifier}}\", {}\n\t$0\nend)",
        escape_snippet_text(hook_name),
        callback_signature
    )
}

fn build_hook_emit_snippet(
    hook_name: &str,
    callback_params: &[String],
    include_gamemode_arg: bool,
) -> String {
    let mut args = Vec::with_capacity(callback_params.len() + usize::from(include_gamemode_arg));
    let mut placeholder_index = 1;
    if include_gamemode_arg {
        args.push(format!("${{{placeholder_index}:GAMEMODE}}"));
        placeholder_index += 1;
    }

    for param in callback_params {
        args.push(format!(
            "${{{}:{}}}",
            placeholder_index,
            escape_snippet_text(param)
        ));
        placeholder_index += 1;
    }

    if args.is_empty() {
        format!("{}\")", escape_snippet_text(hook_name))
    } else {
        format!("{}\", {})", escape_snippet_text(hook_name), args.join(", "))
    }
}

fn build_net_receive_snippet(message_name: &str, call_realm: GmodRealm) -> String {
    let callback_signature = if call_realm == GmodRealm::Client {
        "function(len)"
    } else {
        "function(len, ply)"
    };

    format!(
        "{}\", {}\n\t$0\nend)",
        escape_snippet_text(message_name),
        callback_signature
    )
}

fn completion_sort_key(name: &str) -> String {
    name.to_lowercase()
}

fn escape_snippet_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('}', "\\}")
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

fn resolve_hook_site_realm(
    semantic_model: &SemanticModel,
    file_id: &FileId,
    hook_site: &GmodHookSiteMetadata,
) -> GmodRealm {
    let offset = hook_site.syntax_id.get_range().start();
    if let Some(annotation_realm) =
        resolve_decl_annotation_realm_at_offset(semantic_model, file_id, offset)
    {
        return annotation_realm;
    }

    semantic_model
        .get_db()
        .get_gmod_infer_index()
        .get_realm_at_offset(file_id, offset)
}

fn resolve_decl_annotation_realm_at_offset(
    semantic_model: &SemanticModel,
    file_id: &FileId,
    offset: TextSize,
) -> Option<GmodRealm> {
    let tree = semantic_model.get_db().get_vfs().get_syntax_tree(file_id)?;
    for func_stat in tree.get_chunk_node().descendants::<LuaFuncStat>() {
        if func_stat.get_range().contains(offset)
            && let Some(comment) = func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            return Some(realm);
        }
    }

    for local_func_stat in tree.get_chunk_node().descendants::<LuaLocalFuncStat>() {
        if local_func_stat.get_range().contains(offset)
            && let Some(comment) = local_func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            return Some(realm);
        }
    }

    None
}

fn realm_from_doc_comment(comment: &LuaComment) -> Option<GmodRealm> {
    for tag in comment.get_doc_tags() {
        if let LuaDocTag::Realm(realm_tag) = tag
            && let Some(realm) = realm_from_doc_tag(&realm_tag)
        {
            return Some(realm);
        }
    }

    None
}

fn realm_from_doc_tag(tag: &LuaDocTagRealm) -> Option<GmodRealm> {
    let name = tag.get_name_token()?;
    match name.get_name_text() {
        "client" => Some(GmodRealm::Client),
        "server" => Some(GmodRealm::Server),
        "shared" => Some(GmodRealm::Shared),
        _ => None,
    }
}
