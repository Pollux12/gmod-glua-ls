use std::collections::HashMap;

use emmylua_code_analysis::{
    FileId, GmodHookSiteMetadata, GmodRealm, NetSendFlow, NetSendKind, SemanticModel,
};
use emmylua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaComment, LuaCommentOwner, LuaDocTag,
    LuaDocTagRealm, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaLiteralExpr,
    LuaLocalFuncStat, LuaStringToken, PathTrait,
};
use lsp_types::{CompletionItem, CompletionTextEdit, TextEdit};
use rowan::TextSize;

use crate::handlers::completion::completion_builder::CompletionBuilder;
use crate::handlers::completion::completion_data::CompletionData;
use crate::handlers::hover::resolve_hook_property_owner;

use super::get_text_edit_range_in_string;

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

    if let Some(string_token) = LuaStringToken::cast(builder.trigger_token.clone())
        && let Some(literal_expr) = string_token.get_parent::<LuaLiteralExpr>()
        && let Some(call_expr) = literal_expr
            .get_parent::<LuaCallArgList>()
            .and_then(|args| args.get_parent::<LuaCallExpr>())
        && let Some(text_edit_range) = get_text_edit_range_in_string(builder, string_token)
    {
        let string_added = if is_net_message_string_context(&call_expr, literal_expr.clone()) {
            add_net_message_completion_items(builder, Some(text_edit_range))
        } else if is_hook_name_string_context(builder, &call_expr, literal_expr) {
            add_hook_completion_items(builder, Some(text_edit_range))
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
                .map(|read_kind| (entry.kind, read_kind))
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
            if let Some((_, expected_kind)) = remaining_expected_reads.first() {
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

    for (index, (write_kind, read_kind)) in remaining_expected_reads.into_iter().enumerate() {
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

        let _ = builder.add_completion_item(CompletionItem {
            label: read_kind.to_fn_name().to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some(detail),
            sort_text: Some(format!("000_gmod_net_read_{index:03}")),
            insert_text: Some(read_kind.to_fn_name().to_string()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range.clone(),
                new_text: read_kind.to_fn_name().to_string(),
            })),
            ..Default::default()
        });
    }

    true
}

fn choose_preferred_send_flow<'a>(
    send_flows: Vec<(FileId, &'a NetSendFlow)>,
    infer_index: &emmylua_code_analysis::GmodInferIndex,
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
        NetSendKind::Send | NetSendKind::Broadcast => Some(GmodRealm::Client),
        NetSendKind::SendToServer => Some(GmodRealm::Server),
    }
}

fn opposite_realm(realm: GmodRealm) -> Option<GmodRealm> {
    match realm {
        GmodRealm::Client => Some(GmodRealm::Server),
        GmodRealm::Server => Some(GmodRealm::Client),
        GmodRealm::Shared | GmodRealm::Unknown => None,
    }
}

fn realm_label(realm: GmodRealm) -> &'static str {
    match realm {
        GmodRealm::Client => "client",
        GmodRealm::Server => "server",
        GmodRealm::Shared => "shared",
        GmodRealm::Unknown => "unknown",
    }
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
        callback_params: Option<(u8, Vec<String>)>,
    }

    let before_count = builder.get_completion_items_mut().len();
    let infer_index = builder.semantic_model.get_db().get_gmod_infer_index();
    let mut hook_stats: HashMap<String, HookStats> = HashMap::new();
    let call_realm = infer_index.get_realm_at_offset(
        &builder.semantic_model.get_file_id(),
        builder.position_offset,
    );
    let should_filter_realm = matches!(call_realm, GmodRealm::Client | GmodRealm::Server);

    for (file_id, metadata) in infer_index.iter_hook_file_metadata() {
        for hook_site in &metadata.sites {
            if should_filter_realm {
                let hook_realm =
                    resolve_hook_site_realm(&builder.semantic_model, file_id, hook_site);
                if !is_realm_compatible(call_realm, hook_realm) {
                    continue;
                }
            }

            let Some(name) = normalize_name(hook_site.hook_name.as_deref()) else {
                continue;
            };

            let stats = hook_stats.entry(name.to_string()).or_default();
            match hook_site.kind {
                emmylua_code_analysis::GmodHookKind::Add => stats.add_count += 1,
                emmylua_code_analysis::GmodHookKind::GamemodeMethod => stats.method_count += 1,
                emmylua_code_analysis::GmodHookKind::Emit => stats.emit_count += 1,
            }

            let callback_priority = match hook_site.kind {
                emmylua_code_analysis::GmodHookKind::GamemodeMethod => 2,
                emmylua_code_analysis::GmodHookKind::Add => 1,
                emmylua_code_analysis::GmodHookKind::Emit => 0,
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
            .filter(|(_, params)| !params.is_empty())
            .map(|(_, params)| format!(" args: {}", params.join(", ")))
            .unwrap_or_default();
        let data = resolve_hook_property_owner(
            &builder.semantic_model,
            file_id,
            builder.position_offset,
            &name,
        )
        .and_then(|id| CompletionData::from_property_owner_id(builder, id, None));
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
            data,
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

fn is_realm_compatible(call_realm: GmodRealm, item_realm: GmodRealm) -> bool {
    !matches!(
        (call_realm, item_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
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
