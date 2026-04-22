use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaBlock, LuaCallExpr, LuaChunk,
    LuaClosureExpr, LuaComment, LuaCommentOwner, LuaDocDescriptionOwner, LuaDocTag,
    LuaDocTagFileparam, LuaDocTagRealm, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexKey,
    LuaLiteralToken, LuaLocalFuncStat, LuaLocalStat, LuaStat, LuaSyntaxNode, LuaVarExpr, PathTrait,
};

use crate::{
    EmmyrcGmodRealm, FileId, GmodClassCallLiteral, GmodScriptedClassCallKind,
    GmodScriptedClassCallMetadata, GmodScriptedClassFileMetadata, LuaDecl, LuaDeclExtra, LuaDeclId,
    LuaDeclLocation, LuaDeclTypeKind, LuaFunctionType, LuaMember, LuaMemberFeature, LuaMemberId,
    LuaMemberKey, LuaType, LuaTypeCache, LuaTypeDecl, LuaTypeDeclId, LuaTypeFlag,
    compilation::analyzer::{AnalysisPipeline, AnalyzeContext, common::add_member},
    db_index::{
        AsyncState, DbIndex, GmodCallbackSiteMetadata, GmodConVarKind, GmodConVarSiteMetadata,
        GmodConcommandSiteMetadata, GmodHookKind, GmodHookNameIssue, GmodHookSiteMetadata,
        GmodNamedSiteMetadata, GmodNetReceiveSiteMetadata, GmodRealm, GmodRealmFileMetadata,
        GmodRealmRange, GmodScopedClassInfo, GmodTimerKind, GmodTimerSiteMetadata,
        LuaDependencyKind, LuaMemberOwner, NetOpEntry, NetOpKind, NetReceiveFlow, NetSendFlow,
        NetSendKind,
    },
    profile::Profile,
};
use rowan::{TextRange, TextSize};

/// Pre-scanned keyword flags for fast gmod_pre skip decisions.
/// Each flag indicates the file source contains the corresponding keyword pattern.
#[derive(Default)]
struct GmodKeywords {
    /// "hook" — hook.Add/Run/Remove call sites, hook method sites
    has_hook: bool,
    /// "net." — net.Start/Receive/Send flows, system call metadata
    has_net: bool,
    /// timer/concommand/ConVar/AddNetworkString — system call metadata
    has_system_call: bool,
    /// "GM:" or "GAMEMODE:" — GM/GAMEMODE method sites
    has_gm_func: bool,
    /// "CLIENT" or "SERVER" — branch realm ranges (if CLIENT/if SERVER)
    has_realm_branch: bool,
    /// "@realm" — file-level realm annotation
    has_realm_anno: bool,
}

impl GmodKeywords {
    /// Whether the LuaCallExpr walk in collect_hook_metadata is needed
    fn needs_call_walk(&self) -> bool {
        self.has_hook || self.has_net || self.has_system_call
    }

    /// Whether the LuaFuncStat walk in collect_hook_metadata is needed
    fn needs_func_walk(&self) -> bool {
        self.has_gm_func || self.has_realm_anno
    }

    /// Whether any hook/net metadata collection is needed at all
    fn needs_hook_metadata(&self) -> bool {
        self.needs_call_walk() || self.needs_func_walk()
    }
}

fn scan_gmod_keywords(content: &str, hook_method_prefixes: &[String]) -> GmodKeywords {
    let has_gm_func = content.contains("GM:")
        || content.contains("GAMEMODE:")
        || hook_method_prefixes
            .iter()
            .any(|p| content.contains(&format!("{p}:")));
    GmodKeywords {
        has_hook: content.contains("hook"),
        has_net: content.contains("net."),
        has_system_call: content.contains("timer.")
            || content.contains("concommand")
            || content.contains("ConVar")
            || content.contains("AddNetworkString"),
        has_gm_func,
        has_realm_branch: content.contains("CLIENT") || content.contains("SERVER"),
        has_realm_anno: content.contains("@realm"),
    }
}

/// Pre-analysis phase: runs BEFORE lua_analyze.
/// Collects purely syntactic metadata (hooks, network, realm, scripted class
/// type declarations) so that lua_analyze has correct realm keys and scripted
/// class types available from the start. This avoids the previous architecture
/// where flow analysis used `GmodRealm::Unknown` during lua_analyze and had to
/// recompute everything in the unresolve phase with the correct realm.
pub struct GmodPreAnalysisPipeline;

impl AnalysisPipeline for GmodPreAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        if !db.get_emmyrc().gmod.enabled {
            return;
        }

        let _p = Profile::cond_new("gmod pre-analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let file_ids: Vec<FileId> = tree_list.iter().map(|x| x.file_id).collect();
        let do_profile = tree_list.len() > 100;

        // Pre-compute scripted class scope for all files (compile globs once)
        let scripted_scope_files = context.get_or_compute_scripted_scope_files(db).clone();

        let t0 = std::time::Instant::now();
        let mut branch_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>> = HashMap::new();
        let mut annotation_realms: HashMap<FileId, GmodRealm> = HashMap::new();
        let mut t_hook = std::time::Duration::ZERO;
        let mut t_netflow = std::time::Duration::ZERO;
        let mut t_scoped = std::time::Duration::ZERO;
        let mut t_realm = std::time::Duration::ZERO;
        for in_filed_tree in &tree_list {
            let is_in_scope = scripted_scope_files.contains(&in_filed_tree.file_id);

            // Pre-scan file source for gmod-relevant keywords to skip unnecessary AST walks
            let method_prefixes = &db.get_emmyrc().gmod.hook_mappings.method_prefixes;
            let keywords = db
                .get_vfs()
                .get_file_content(&in_filed_tree.file_id)
                .map(|c| scan_gmod_keywords(c, method_prefixes))
                .unwrap_or_default();

            let s = std::time::Instant::now();
            let (gm_method_realms, receive_flows) = if keywords.needs_hook_metadata() {
                collect_hook_metadata(db, in_filed_tree.file_id, in_filed_tree.value.clone())
            } else {
                (Vec::new(), Vec::new())
            };
            t_hook += s.elapsed();

            let s = std::time::Instant::now();
            if keywords.has_net || !receive_flows.is_empty() {
                collect_network_flow_metadata(
                    db,
                    in_filed_tree.file_id,
                    in_filed_tree.value.clone(),
                    receive_flows,
                );
            }
            t_netflow += s.elapsed();

            if !gm_method_realms.is_empty() {
                db.get_gmod_infer_index_mut()
                    .set_gm_method_realm_annotations(in_filed_tree.file_id, gm_method_realms);
            }
            if is_in_scope {
                let s = std::time::Instant::now();
                // Use cached scoped class info from decl phase, or detect if not cached
                let scope_match = db
                    .get_gmod_infer_index()
                    .get_scoped_class_info(&in_filed_tree.file_id)
                    .map(|info| GmodScopedClassMatch {
                        class_name: info.class_name.clone(),
                        global_name: info.global_name.clone(),
                        class_name_prefix: info.class_name_prefix.clone(),
                    })
                    .or_else(|| {
                        let m = detect_scoped_class_from_path(db, in_filed_tree.file_id)?;
                        db.get_gmod_infer_index_mut().set_scoped_class_info(
                            in_filed_tree.file_id,
                            GmodScopedClassInfo {
                                class_name: m.class_name.clone(),
                                global_name: m.global_name.clone(),
                                class_name_prefix: m.class_name_prefix.clone(),
                            },
                        );
                        Some(m)
                    });
                if let Some(scope_match) = scope_match {
                    ensure_scoped_class_type_decl(
                        db,
                        in_filed_tree.file_id,
                        &scope_match.class_name,
                        &scope_match.global_name,
                        in_filed_tree.value.syntax().text_range(),
                    );

                    collect_scripted_scope_type_bindings_with(
                        db,
                        in_filed_tree.file_id,
                        &scope_match,
                    );
                    synthesize_scoped_base_assignments_with(
                        db,
                        in_filed_tree.file_id,
                        in_filed_tree.value.clone(),
                        &scope_match,
                    );
                }
                t_scoped += s.elapsed();
            }
            let s = std::time::Instant::now();
            if keywords.has_realm_branch {
                let ranges = collect_branch_realm_ranges(&in_filed_tree.value);
                if !ranges.is_empty() {
                    branch_realm_ranges.insert(in_filed_tree.file_id, ranges);
                }
            }
            if keywords.has_realm_anno {
                if let Some(realm) = collect_realm_annotation(&in_filed_tree.value) {
                    annotation_realms.insert(in_filed_tree.file_id, realm);
                }
            }
            t_realm += s.elapsed();

            // Pre-index @fileparam annotations (O(1) lookup during resolve vs O(file_size) AST walk)
            // @fileparam is extremely rare; only scan if file content contains it
            if db
                .get_vfs()
                .get_file_content(&in_filed_tree.file_id)
                .is_some_and(|c| c.contains("@fileparam"))
            {
                let file_params = collect_file_params(&in_filed_tree.value);
                if !file_params.is_empty() {
                    db.get_gmod_infer_index_mut()
                        .set_file_params(in_filed_tree.file_id, file_params);
                }
            }
        }
        if do_profile {
            log::info!(
                "gmod pre: per-file metadata cost {:?} (hook={:?}, netflow={:?}, scoped={:?}, realm={:?})",
                t0.elapsed(),
                t_hook,
                t_netflow,
                t_scoped,
                t_realm
            );
        }

        // Network var wrappers are purely syntactic (AST pattern matching)
        let t1 = std::time::Instant::now();
        let tree_map: HashMap<FileId, LuaChunk> = tree_list
            .iter()
            .map(|x| (x.file_id, x.value.clone()))
            .collect();
        synthesize_network_var_wrappers(db, &scripted_scope_files, &tree_map);
        if do_profile {
            log::info!("gmod pre: network_var_wrappers cost {:?}", t1.elapsed());
        }

        let t2 = std::time::Instant::now();
        rebuild_realm_metadata(db, branch_realm_ranges, annotation_realms, &file_ids);
        if do_profile {
            log::info!("gmod pre: rebuild_realm_metadata cost {:?}", t2.elapsed());
        }
    }
}

/// Post-analysis phase: runs AFTER lua_analyze.
/// Synthesizes members that depend on metadata collected during lua_analyze
/// (gmod_class_metadata_index: AccessorFunc, NetworkVar, VGUI register calls).
pub struct GmodPostAnalysisPipeline;

impl AnalysisPipeline for GmodPostAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        if !db.get_emmyrc().gmod.enabled {
            return;
        }

        let _p = Profile::cond_new("gmod post-analyze", context.tree_list.len() > 1);
        let file_ids: Vec<FileId> = context.tree_list.iter().map(|x| x.file_id).collect();
        let do_profile = context.tree_list.len() > 100;

        let scripted_scope_files = context.get_or_compute_scripted_scope_files(db).clone();

        let t0 = std::time::Instant::now();
        synthesize_scripted_class_members(db, &scripted_scope_files, &file_ids);
        if do_profile {
            log::info!("gmod post: scripted_class_members cost {:?}", t0.elapsed());
        }

        let t1 = std::time::Instant::now();
        synthesize_vgui_registrations(db, &file_ids);
        if do_profile {
            log::info!("gmod post: vgui_registrations cost {:?}", t1.elapsed());
        }
    }
}

fn collect_hook_metadata(
    db: &mut DbIndex,
    file_id: FileId,
    root: LuaChunk,
) -> (Vec<(String, GmodRealm)>, Vec<NetReceiveFlow>) {
    let mut gm_method_realms = Vec::new();
    let mut receive_flows = Vec::new();

    for call_expr in root.descendants::<LuaCallExpr>() {
        if let Some(site) = collect_hook_call_site(db, call_expr.clone()) {
            db.get_gmod_infer_index_mut().add_hook_site(file_id, site);
        }

        if let Some(receive_flow) = collect_net_receive_flow(&call_expr) {
            receive_flows.push(receive_flow);
        }

        collect_system_call_metadata(db, file_id, call_expr);
    }

    for func_stat in root.descendants::<LuaFuncStat>() {
        if let Some(site) = collect_hook_method_site(db, func_stat.clone()) {
            db.get_gmod_infer_index_mut().add_hook_site(file_id, site);
        }

        if let Some((method_name, realm)) = collect_gm_method_realm_annotation(&func_stat)
            && !gm_method_realms
                .iter()
                .any(|(existing_name, existing_realm)| {
                    existing_name == &method_name && *existing_realm == realm
                })
        {
            gm_method_realms.push((method_name, realm));
        }
    }

    receive_flows.sort_by_key(|flow| flow.receive_range.start());
    (gm_method_realms, receive_flows)
}

fn collect_network_flow_metadata(
    db: &mut DbIndex,
    file_id: FileId,
    root: LuaChunk,
    receive_flows: Vec<NetReceiveFlow>,
) {
    let mut send_flows = collect_net_send_flows(&root);
    send_flows.extend(collect_wrapped_net_send_flows(&root));
    send_flows.sort_by_key(|flow| flow.start_range.start());

    let data = crate::db_index::FileNetworkData {
        send_flows,
        receive_flows,
    };
    db.get_gmod_network_index_mut().add_file_data(file_id, data);
}

fn collect_gm_method_realm_annotation(func_stat: &LuaFuncStat) -> Option<(String, GmodRealm)> {
    let LuaVarExpr::IndexExpr(function_name_expr) = func_stat.get_func_name()? else {
        return None;
    };
    let LuaExpr::NameExpr(function_prefix_name) = function_name_expr.get_prefix_expr()? else {
        return None;
    };
    let function_prefix_text = function_prefix_name.get_name_text()?;
    if !matches!(function_prefix_text.as_str(), "GM" | "GAMEMODE") {
        return None;
    }
    let LuaIndexKey::Name(function_method_name) = function_name_expr.get_index_key()? else {
        return None;
    };
    let comment = func_stat.get_left_comment()?;
    let realm = realm_from_doc_comment(&comment)?;
    let method_name = function_method_name.get_name_text().to_string();
    Some((method_name, realm))
}

fn collect_net_send_flows(root: &LuaChunk) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();

    for block in root.descendants::<LuaBlock>() {
        let stats: Vec<LuaStat> = block.get_stats().collect();
        for (index, stat) in stats.iter().enumerate() {
            let Some(call_expr) = call_expr_from_stat(stat) else {
                continue;
            };

            let Some(method_name) = get_exact_net_method_name(&call_expr) else {
                continue;
            };

            if method_name != "Start" {
                continue;
            }

            let Some(message_name) = extract_static_string_arg_value(&call_expr, 0) else {
                continue;
            };

            let mut writes = Vec::new();
            let mut send = None;

            for next_stat in stats.iter().skip(index + 1) {
                if let Some(next_call_expr) = call_expr_from_stat(next_stat)
                    && let Some(next_method_name) = get_exact_net_method_name(&next_call_expr)
                {
                    if next_method_name == "Start" {
                        break;
                    }

                    if let Some(send_kind) = net_send_kind_from_method_name(&next_method_name) {
                        send = Some((next_call_expr.get_range(), send_kind));
                        break;
                    }
                }

                collect_net_write_ops_from_stat(&block, next_stat, &mut writes);
            }

            let Some((send_range, send_kind)) = send else {
                continue;
            };

            writes.sort_by_key(|entry| entry.range.start());
            flows.push(NetSendFlow {
                message_name,
                start_range: call_expr.get_range(),
                writes,
                send_range,
                send_kind,
                is_wrapped: false,
            });
        }
    }

    flows.sort_by_key(|flow| flow.start_range.start());
    flows
}

fn collect_wrapped_net_send_flows(root: &LuaChunk) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();

    for func_stat in root.descendants::<LuaFuncStat>() {
        if let Some(block) = func_stat
            .get_closure()
            .and_then(|closure_expr| closure_expr.get_block())
        {
            collect_wrapped_net_send_flows_in_function_block(&block, &mut flows);
        }
    }

    for local_func_stat in root.descendants::<LuaLocalFuncStat>() {
        if let Some(block) = local_func_stat
            .get_closure()
            .and_then(|closure_expr| closure_expr.get_block())
        {
            collect_wrapped_net_send_flows_in_function_block(&block, &mut flows);
        }
    }

    for local_stat in root.descendants::<LuaLocalStat>() {
        for value_expr in local_stat.get_value_exprs() {
            if let LuaExpr::ClosureExpr(closure_expr) = value_expr
                && let Some(block) = closure_expr.get_block()
            {
                collect_wrapped_net_send_flows_in_function_block(&block, &mut flows);
            }
        }
    }

    for assign_stat in root.descendants::<LuaAssignStat>() {
        let (_, value_exprs) = assign_stat.get_var_and_expr_list();
        for value_expr in value_exprs {
            if let LuaExpr::ClosureExpr(closure_expr) = value_expr
                && let Some(block) = closure_expr.get_block()
            {
                collect_wrapped_net_send_flows_in_function_block(&block, &mut flows);
            }
        }
    }

    flows.sort_by_key(|flow| flow.start_range.start());
    flows
}

fn collect_wrapped_net_send_flows_in_function_block(
    function_block: &LuaBlock,
    flows: &mut Vec<NetSendFlow>,
) {
    for block in function_block
        .syntax()
        .descendants()
        .filter_map(LuaBlock::cast)
    {
        if block.syntax() != function_block.syntax()
            && is_block_in_nested_closure(function_block, &block)
        {
            continue;
        }

        let stats: Vec<LuaStat> = block.get_stats().collect();
        for (index, stat) in stats.iter().enumerate() {
            let Some(call_expr) = call_expr_from_stat(stat) else {
                continue;
            };

            let Some(method_name) = get_exact_net_method_name(&call_expr) else {
                continue;
            };

            if method_name != "Start" {
                continue;
            }

            let Some(message_name) = extract_static_string_arg_value(&call_expr, 0) else {
                continue;
            };

            let mut writes = Vec::new();
            let mut send = None;

            for next_stat in stats.iter().skip(index + 1) {
                if let Some(next_call_expr) = call_expr_from_stat(next_stat)
                    && let Some(next_method_name) = get_exact_net_method_name(&next_call_expr)
                {
                    if next_method_name == "Start" {
                        break;
                    }

                    if let Some(send_kind) = net_send_kind_from_method_name(&next_method_name) {
                        send = Some((next_call_expr.get_range(), send_kind));
                        break;
                    }
                }

                collect_net_write_ops_from_stat(&block, next_stat, &mut writes);
            }

            writes.sort_by_key(|entry| entry.range.start());

            if let Some((send_range, send_kind)) = send {
                flows.push(NetSendFlow {
                    message_name,
                    start_range: call_expr.get_range(),
                    writes,
                    send_range,
                    send_kind,
                    is_wrapped: true,
                });
                continue;
            }

            // Wrapped helper flows can start a net message in one function and send at call-site.
            // Keep a conservative stub so counterpart diagnostics can still resolve by message name.
            flows.push(NetSendFlow {
                message_name,
                start_range: call_expr.get_range(),
                writes: Vec::new(),
                send_range: call_expr.get_range(),
                send_kind: NetSendKind::Broadcast,
                is_wrapped: true,
            });
        }
    }
}

fn is_block_in_nested_closure(function_block: &LuaBlock, candidate_block: &LuaBlock) -> bool {
    candidate_block
        .syntax()
        .ancestors()
        .take_while(|node| node != function_block.syntax())
        .any(|node| LuaClosureExpr::can_cast(node.kind().into()))
}

fn collect_net_receive_flow(call_expr: &LuaCallExpr) -> Option<NetReceiveFlow> {
    let method_name = get_exact_net_method_name(call_expr)?;
    if method_name != "Receive" {
        return None;
    }

    let message_name = extract_static_string_arg_value(call_expr, 0)?;

    let mut reads = Vec::new();
    if let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(1))
        && let LuaExpr::ClosureExpr(closure_expr) = callback_expr
        && let Some(callback_block) = closure_expr.get_block()
    {
        collect_net_read_ops_from_block(callback_block, &mut reads);
    }

    reads.sort_by_key(|entry| entry.range.start());
    Some(NetReceiveFlow {
        message_name,
        receive_range: call_expr.get_range(),
        reads,
    })
}

fn collect_net_read_ops_from_block(block: LuaBlock, reads: &mut Vec<NetOpEntry>) {
    for call_expr in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
        if is_call_expr_in_nested_closure(&block, &call_expr) {
            continue;
        }

        let Some(method_name) = get_exact_net_method_name(&call_expr) else {
            continue;
        };

        if let Some(op_kind) = NetOpKind::from_fn_name(method_name.as_str())
            && op_kind.is_read()
        {
            reads.push(NetOpEntry {
                kind: op_kind,
                range: call_expr.get_range(),
            });
        }
    }
}

fn collect_net_write_ops_from_stat(block: &LuaBlock, stat: &LuaStat, writes: &mut Vec<NetOpEntry>) {
    for call_expr in stat.syntax().descendants().filter_map(LuaCallExpr::cast) {
        if is_call_expr_in_nested_closure(block, &call_expr) {
            continue;
        }

        let Some(method_name) = get_exact_net_method_name(&call_expr) else {
            continue;
        };

        if let Some(op_kind) = NetOpKind::from_fn_name(method_name.as_str())
            && op_kind.is_write()
        {
            writes.push(NetOpEntry {
                kind: op_kind,
                range: call_expr.get_range(),
            });
        }
    }
}

fn is_call_expr_in_nested_closure(block: &LuaBlock, call_expr: &LuaCallExpr) -> bool {
    call_expr
        .syntax()
        .ancestors()
        .take_while(|node| node != block.syntax())
        .any(|node| LuaClosureExpr::can_cast(node.kind().into()))
}

fn call_expr_from_stat(stat: &LuaStat) -> Option<LuaCallExpr> {
    let LuaStat::CallExprStat(call_expr_stat) = stat else {
        return None;
    };

    call_expr_stat.get_call_expr()
}

fn get_exact_net_method_name(call_expr: &LuaCallExpr) -> Option<String> {
    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };

    let LuaExpr::NameExpr(prefix_name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };

    if prefix_name_expr.get_name_text()? != "net" {
        return None;
    }

    let LuaIndexKey::Name(method_name_token) = index_expr.get_index_key()? else {
        return None;
    };

    Some(method_name_token.get_name_text().to_string())
}

fn extract_static_string_arg_value(call_expr: &LuaCallExpr, arg_idx: usize) -> Option<String> {
    let arg_expr = call_expr.get_args_list()?.get_args().nth(arg_idx)?;
    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return None;
    };

    let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
        return None;
    };

    Some(string_token.get_value())
}

fn net_send_kind_from_method_name(method_name: &str) -> Option<NetSendKind> {
    match method_name {
        "Send" => Some(NetSendKind::Send),
        "Broadcast" => Some(NetSendKind::Broadcast),
        "SendToServer" => Some(NetSendKind::SendToServer),
        "SendOmit" => Some(NetSendKind::Omit),
        "SendPAS" => Some(NetSendKind::PAS),
        "SendPVS" => Some(NetSendKind::PVS),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GmodScopedClassMatch {
    pub global_name: String,
    pub class_name: String,
    /// The scope's `classNamePrefix` (if any). Used to derive the stripped
    /// short name for parent-alias synthesis (e.g. `gamemode_sandbox` →
    /// `sandbox` → `Sandbox`).
    pub class_name_prefix: Option<String>,
}

const GMOD_ENT_BASE_TO_ENT: &[&str] = &[
    "base_gmodentity",
    "base_brush",
    "base_anim",
    "base_ai",
    "base_nextbot",
    "base_point",
    "base_filter",
];

fn collect_scripted_scope_type_bindings_with(
    db: &mut DbIndex,
    file_id: FileId,
    scope_match: &GmodScopedClassMatch,
) {
    let mut decls = Vec::new();
    {
        let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) else {
            return;
        };

        for decl in decl_tree.get_decls().values() {
            if decl.get_name() != scope_match.global_name {
                continue;
            }

            if decl.is_local() || decl.is_global() {
                decls.push((decl.get_id(), decl.get_range()));
            }
        }
    }

    if decls.is_empty() {
        return;
    }

    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        &scope_match.global_name,
        decls[0].1,
    );

    for (decl_id, _) in decls {
        let previous_decl_type = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|type_cache| type_cache.as_type().clone());

        db.get_type_index_mut().force_bind_type(
            decl_id.into(),
            LuaTypeCache::InferType(LuaType::Def(class_decl_id.clone())),
        );

        if let Some(LuaType::TableConst(table_range)) = previous_decl_type {
            let table_member_owner = LuaMemberOwner::Element(table_range);
            let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
            let table_member_ids = db
                .get_member_index()
                .get_members(&table_member_owner)
                .map(|members| {
                    members
                        .iter()
                        .map(|member| member.get_id())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for member_id in table_member_ids {
                add_member(db, class_member_owner.clone(), member_id);
            }
        }
    }
}

fn ensure_scoped_class_type_decl(
    db: &mut DbIndex,
    file_id: FileId,
    class_name: &str,
    global_name: &str,
    range: rowan::TextRange,
) -> LuaTypeDeclId {
    let class_decl_id = LuaTypeDeclId::global(class_name);
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                range,
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
    } else if db
        .get_type_index()
        .get_type_decl(&class_decl_id)
        .is_some_and(|decl| {
            !decl
                .get_locations()
                .iter()
                .any(|loc| loc.file_id == file_id)
        })
    {
        db.get_type_index_mut().add_type_decl_location(
            file_id,
            &class_decl_id,
            LuaDeclLocation {
                file_id,
                range,
                flag: LuaTypeFlag::None.into(),
            },
        );
    }

    for super_type in scoped_class_super_types(global_name) {
        db.get_type_index_mut().add_super_type_if_missing(
            class_decl_id.clone(),
            file_id,
            super_type,
        );
    }
    class_decl_id
}

fn scoped_class_super_types(global_name: &str) -> Vec<LuaType> {
    let mut super_types = vec![LuaType::Ref(LuaTypeDeclId::global(global_name))];
    match global_name {
        "TOOL" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Tool"))),
        "SWEP" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Weapon"))),
        "ENT" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        "PLUGIN" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("GM"))),
        _ => {}
    }

    super_types
}

pub(crate) fn ensure_scoped_class_type_decl_for_file(
    db: &mut DbIndex,
    file_id: FileId,
    range: rowan::TextRange,
) -> Option<LuaTypeDeclId> {
    // Use cached info if available, otherwise detect from path
    let (class_name, global_name) =
        if let Some(info) = db.get_gmod_infer_index().get_scoped_class_info(&file_id) {
            (info.class_name.clone(), info.global_name.clone())
        } else {
            let scope_match = detect_scoped_class_from_path(db, file_id)?;
            (scope_match.class_name, scope_match.global_name)
        };
    Some(ensure_scoped_class_type_decl(
        db,
        file_id,
        &class_name,
        &global_name,
        range,
    ))
}

/// Synthesize typed members from AccessorFunc, NetworkVar, and DEFINE_BASECLASS
/// calls for all files that have scripted class metadata.
fn synthesize_scripted_class_members(
    db: &mut DbIndex,
    scripted_scope_files: &HashSet<FileId>,
    file_ids: &[FileId],
) {
    for file_id in file_ids.iter().copied() {
        // Use cached scoped class info (computed during gmod_pre phase)
        let scope_match = if scripted_scope_files.contains(&file_id) {
            db.get_gmod_infer_index()
                .get_scoped_class_info(&file_id)
                .cloned()
        } else {
            None
        };

        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            if let Some((effective_base_name, is_derive)) = resolve_effective_inheritance_base(
                &metadata,
                scope_match.class_name_prefix.as_deref(),
            ) {
                synthesize_inheritance_base(
                    db,
                    file_id,
                    &class_decl_id,
                    &effective_base_name,
                    is_derive,
                    scope_match.class_name_prefix.as_deref(),
                );
            }
            if let Some(effective_call) =
                metadata.define_baseclass_calls.iter().rev().find(|call| {
                    matches!(
                        call.literal_args.first(),
                        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty()
                    )
                })
            {
                synthesize_define_baseclass_parent_alias(
                    db,
                    file_id,
                    &class_decl_id,
                    scope_match.class_name_prefix.as_deref(),
                    effective_call,
                );
            }
        }

        // AccessorFunc: synthesize Get/Set/field members
        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            for call in &metadata.accessor_func_calls {
                synthesize_accessor_func(db, file_id, &class_decl_id, call);
            }
        }

        // NetworkVar: synthesize Get/Set members
        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            for call in &metadata.network_var_calls {
                synthesize_network_var(db, file_id, &class_decl_id, call);
            }
        }

        // NetworkVarElement: synthesize Get/Set members (always number type)
        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            for call in &metadata.network_var_element_calls {
                synthesize_network_var_element(db, file_id, &class_decl_id, call);
            }
        }
    }
}

/// Synthesize vgui.Register / derma.DefineControl class types.
fn synthesize_vgui_registrations(db: &mut DbIndex, file_ids: &[FileId]) {
    let mut original_decl_table_types: HashMap<LuaDeclId, Option<LuaType>> = HashMap::new();
    // Track (file_id, table_var_name, panel_name) for AccessorFunc synthesis
    let mut vgui_table_vars: Vec<(FileId, String, String)> = Vec::new();

    for file_id in file_ids.iter().copied() {
        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        for call in &metadata.vgui_register_calls {
            // Extract table var name and panel name before synthesizing
            if let Some(Some(GmodClassCallLiteral::String(panel_name))) = call.literal_args.first()
            {
                if let Some(Some(GmodClassCallLiteral::NameRef(table_var))) =
                    call.literal_args.get(1)
                {
                    vgui_table_vars.push((file_id, table_var.clone(), panel_name.clone()));
                }
            }
            synthesize_vgui_register(db, file_id, call, &mut original_decl_table_types);
        }

        for call in &metadata.derma_define_control_calls {
            if let Some(Some(GmodClassCallLiteral::String(panel_name))) = call.literal_args.first()
            {
                if let Some(Some(GmodClassCallLiteral::NameRef(table_var))) =
                    call.literal_args.get(2)
                {
                    vgui_table_vars.push((file_id, table_var.clone(), panel_name.clone()));
                }
            }
            synthesize_derma_define_control(db, file_id, call, &mut original_decl_table_types);
        }
    }

    // Synthesize AccessorFunc members for VGUI-registered classes
    for (file_id, table_var_name, panel_name) in &vgui_table_vars {
        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        log::debug!(
            "VGUI AccessorFunc: file {:?} has {} accessor_func_calls for table_var={} panel={}",
            file_id,
            metadata.accessor_func_calls.len(),
            table_var_name,
            panel_name
        );
        let class_decl_id = LuaTypeDeclId::global(panel_name);
        for call in &metadata.accessor_func_calls {
            // Check if the AccessorFunc's first arg matches this table variable
            if let Some(Some(GmodClassCallLiteral::NameRef(target_name))) =
                call.literal_args.first()
            {
                if target_name == table_var_name {
                    synthesize_accessor_func(db, *file_id, &class_decl_id, call);
                }
            }
        }
    }
}

fn synthesize_scoped_base_assignments_with(
    db: &mut DbIndex,
    file_id: FileId,
    root: LuaChunk,
    scope_match: &GmodScopedClassMatch,
) {
    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        &scope_match.global_name,
        root.syntax().text_range(),
    );
    let expected_base_path = format!("{}.Base", scope_match.global_name);

    for assign_stat in root.descendants::<LuaAssignStat>() {
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.into_iter().enumerate() {
            let Some(value_expr) = exprs.get(idx) else {
                continue;
            };

            let Some(access_path) = var.get_access_path() else {
                continue;
            };
            if !access_path.eq_ignore_ascii_case(&expected_base_path) {
                continue;
            }

            let Some(base_name) = extract_scoped_base_name(value_expr) else {
                continue;
            };

            let mapped_base_name = remap_scoped_base_name(&scope_match, &base_name);
            let super_type = LuaType::Ref(LuaTypeDeclId::global(&mapped_base_name));
            if super_type == LuaType::Ref(class_decl_id.clone()) {
                continue;
            }

            let has_super = db
                .get_type_index()
                .get_super_types_iter(&class_decl_id)
                .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
                .unwrap_or(false);
            if !has_super {
                db.get_type_index_mut()
                    .add_super_type(class_decl_id.clone(), file_id, super_type);
            }
        }
    }
}

fn extract_scoped_base_name(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal() {
            Some(LuaLiteralToken::String(string_token)) => {
                let value = string_token.get_value();
                (!value.trim().is_empty()).then_some(value)
            }
            _ => None,
        },
        LuaExpr::NameExpr(name_expr) => {
            let value = name_expr.get_name_text()?;
            (!value.trim().is_empty()).then_some(value)
        }
        LuaExpr::IndexExpr(index_expr) => {
            let value = index_expr.get_access_path()?;
            (!value.trim().is_empty()).then_some(value)
        }
        _ => None,
    }
}

fn remap_scoped_base_name(scope_match: &GmodScopedClassMatch, base_name: &str) -> String {
    if scope_match.global_name == "ENT"
        && GMOD_ENT_BASE_TO_ENT
            .iter()
            .any(|name| name.eq_ignore_ascii_case(base_name))
    {
        return scope_match.global_name.to_string();
    }

    base_name.to_string()
}

/// A wrapper function that internally calls NetworkVar or NetworkVarElement.
/// For example:
/// ```lua
/// function ENT:SetupNW(type, name)
///     self:NetworkVar(type, 0, name)
/// end
/// ```
#[derive(Debug, Clone)]
struct NetworkVarWrapper {
    /// The method name of the wrapper (e.g. "SetupNW")
    method_name: String,
    /// Fixed type name if the type arg is a string literal in the wrapper body
    fixed_type: Option<String>,
    /// Index of the wrapper parameter that provides the type arg (if not fixed)
    type_param_index: Option<usize>,
    /// Index of the wrapper parameter that provides the name arg
    name_param_index: Option<usize>,
    /// Fixed name if the name arg is a string literal in the wrapper body
    fixed_name: Option<String>,
    /// Whether this wraps NetworkVarElement (always number return type)
    is_element: bool,
    /// Whether the wrapper is a local function (`local function Foo(...)`)
    is_local: bool,
}

/// Detect wrapper functions that internally call NetworkVar/NetworkVarElement
/// and synthesize Get/Set members from their call sites.
fn synthesize_network_var_wrappers(
    db: &mut DbIndex,
    scripted_scope_files: &HashSet<FileId>,
    tree_map: &HashMap<FileId, LuaChunk>,
) {
    // Sort by FileId for deterministic iteration order
    let mut sorted_entries: Vec<_> = tree_map.iter().collect();
    sorted_entries.sort_by_key(|(fid, _)| fid.id);
    for (file_id, root) in sorted_entries {
        if !scripted_scope_files.contains(file_id) {
            continue;
        }

        // Use cached scoped class info (computed earlier in gmod_pre per-file loop)
        let Some(scope_match) = db
            .get_gmod_infer_index()
            .get_scoped_class_info(file_id)
            .cloned()
        else {
            continue;
        };

        let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);

        // Step 1: Collect wrapper definitions from method definitions
        let wrappers = collect_network_var_wrappers(root, &scope_match.global_name);
        if wrappers.is_empty() {
            continue;
        }

        // Step 2: Find calls to these wrappers and synthesize members
        for call_expr in root.descendants::<LuaCallExpr>() {
            let (method_name, is_local_call) = match call_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(index_expr)) => {
                    let Some(LuaIndexKey::Name(name_token)) = index_expr.get_index_key() else {
                        continue;
                    };
                    (name_token.get_name_text().to_string(), false)
                }
                Some(LuaExpr::NameExpr(name_expr)) => {
                    let Some(name_text) = name_expr.get_name_text() else {
                        continue;
                    };
                    (name_text.to_string(), true)
                }
                _ => continue,
            };

            let Some(wrapper) = wrappers
                .iter()
                .find(|w| w.method_name == method_name && w.is_local == is_local_call)
            else {
                continue;
            };

            synthesize_from_wrapper_call(db, *file_id, &class_decl_id, wrapper, &call_expr);
        }
    }
}

/// Scan function definitions in a file for methods that internally call
/// NetworkVar or NetworkVarElement, and map their parameters.
fn collect_network_var_wrappers(root: &LuaChunk, global_name: &str) -> Vec<NetworkVarWrapper> {
    let mut wrappers = Vec::new();

    for func_stat in root.descendants::<LuaFuncStat>() {
        // Only methods on the entity global: function ENT:MethodName(...)
        let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name() else {
            continue;
        };

        // Check the prefix is the entity global name
        let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
            continue;
        };
        let Some(prefix_text) = prefix_name.get_name_text() else {
            continue;
        };
        if prefix_text != global_name {
            continue;
        }

        let Some(LuaIndexKey::Name(method_name_token)) = index_expr.get_index_key() else {
            continue;
        };
        let method_name = method_name_token.get_name_text().to_string();

        // Skip known call kinds that are already handled directly
        if GmodScriptedClassCallKind::from_call_name(&method_name).is_some() {
            continue;
        }

        let Some(closure) = func_stat.get_closure() else {
            continue;
        };

        // Collect parameter names for mapping
        let param_names: Vec<String> = get_closure_param_names(&closure);

        // Walk the closure body looking for NetworkVar/NetworkVarElement calls
        if let Some(wrapper) = find_networkvar_in_closure(&closure, &method_name, &param_names) {
            wrappers.push(wrapper);
        }
    }

    for local_func_stat in root.descendants::<LuaLocalFuncStat>() {
        let Some(local_name) = local_func_stat.get_local_name() else {
            continue;
        };
        let Some(name_token) = local_name.get_name_token() else {
            continue;
        };
        let method_name = name_token.get_name_text().to_string();

        if GmodScriptedClassCallKind::from_call_name(&method_name).is_some() {
            continue;
        }

        let Some(closure) = local_func_stat.get_closure() else {
            continue;
        };

        let param_names: Vec<String> = get_closure_param_names(&closure);

        if let Some(mut wrapper) = find_networkvar_in_closure(&closure, &method_name, &param_names)
        {
            wrapper.is_local = true;
            wrappers.push(wrapper);
        }
    }

    wrappers
}

/// Get the parameter names of a closure (excluding `self` for methods).
fn get_closure_param_names(closure: &LuaClosureExpr) -> Vec<String> {
    let Some(params_list) = closure.get_params_list() else {
        return Vec::new();
    };

    params_list
        .get_params()
        .filter_map(|param| {
            if param.is_dots() {
                return None;
            }
            Some(param.get_name_token()?.get_name_text().to_string())
        })
        .collect()
}

/// Look inside a closure body for NetworkVar/NetworkVarElement calls and map
/// their arguments back to the closure's parameter list.
fn find_networkvar_in_closure(
    closure: &LuaClosureExpr,
    wrapper_method_name: &str,
    param_names: &[String],
) -> Option<NetworkVarWrapper> {
    let block = closure.get_block()?;

    for call_expr in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
        let Some(LuaExpr::IndexExpr(inner_index)) = call_expr.get_prefix_expr() else {
            continue;
        };

        let Some(LuaIndexKey::Name(inner_name_token)) = inner_index.get_index_key() else {
            continue;
        };

        let inner_method = inner_name_token.get_name_text();
        let is_element = match inner_method {
            "NetworkVar" => false,
            "NetworkVarElement" => true,
            _ => continue,
        };

        // Found a NetworkVar/NetworkVarElement call inside the wrapper.
        // Collect the arguments and map them.
        let Some(args_list) = call_expr.get_args_list() else {
            continue;
        };
        let inner_args: Vec<LuaExpr> = args_list.get_args().collect();

        // Determine the type argument (first arg to NetworkVar)
        let (fixed_type, type_param_index) =
            resolve_wrapper_arg_mapping(&inner_args, 0, param_names);

        // Determine the name argument — find the last string-like argument
        // For 3-arg NetworkVar: name is at index 2
        // For 2-arg NetworkVar: name is at index 1
        // For 4-arg NetworkVarElement: name is at index 3
        // Try from the end to find the name position
        let name_indices: &[usize] = if is_element { &[3, 2, 1] } else { &[2, 1] };

        let mut fixed_name = None;
        let mut name_param_index = None;

        for &idx in name_indices {
            if idx >= inner_args.len() {
                continue;
            }
            let (fixed, param_idx) = resolve_wrapper_arg_mapping(&inner_args, idx, param_names);
            if fixed.is_some() || param_idx.is_some() {
                fixed_name = fixed;
                name_param_index = param_idx;
                break;
            }
        }

        // Must have either a fixed name or a parameter mapping for the name
        if fixed_name.is_none() && name_param_index.is_none() {
            continue;
        }

        return Some(NetworkVarWrapper {
            method_name: wrapper_method_name.to_string(),
            fixed_type,
            type_param_index,
            name_param_index,
            fixed_name,
            is_element,
            is_local: false,
        });
    }

    None
}

/// Given a call argument expression and the wrapper's parameter names,
/// determine if the argument is a fixed string literal or a reference to
/// one of the wrapper's parameters.
fn resolve_wrapper_arg_mapping(
    inner_args: &[LuaExpr],
    arg_index: usize,
    param_names: &[String],
) -> (Option<String>, Option<usize>) {
    let Some(arg) = inner_args.get(arg_index) else {
        return (None, None);
    };

    match arg {
        LuaExpr::LiteralExpr(literal) => {
            if let Some(LuaLiteralToken::String(s)) = literal.get_literal() {
                let value = s.get_value();
                if !value.is_empty() {
                    return (Some(value), None);
                }
            }
            (None, None)
        }
        LuaExpr::NameExpr(name_expr) => {
            if let Some(name) = name_expr.get_name_text() {
                if let Some(idx) = param_names.iter().position(|p| p == &name) {
                    return (None, Some(idx));
                }
            }
            (None, None)
        }
        _ => (None, None),
    }
}

/// Given a call to a known wrapper method and the wrapper's parameter mapping,
/// resolve the concrete type and name from the call arguments and synthesize
/// Get/Set members.
fn synthesize_from_wrapper_call(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    wrapper: &NetworkVarWrapper,
    call_expr: &LuaCallExpr,
) {
    let Some(args_list) = call_expr.get_args_list() else {
        return;
    };
    let call_args: Vec<LuaExpr> = args_list.get_args().collect();

    // Resolve the type name
    let type_name = if let Some(ref fixed) = wrapper.fixed_type {
        fixed.clone()
    } else if let Some(idx) = wrapper.type_param_index {
        match call_args.get(idx) {
            Some(LuaExpr::LiteralExpr(lit)) => {
                if let Some(LuaLiteralToken::String(s)) = lit.get_literal() {
                    s.get_value()
                } else {
                    return;
                }
            }
            _ => return,
        }
    } else {
        return;
    };

    // Resolve the property name
    let (prop_name, prop_name_expr) = if let Some(ref fixed) = wrapper.fixed_name {
        (fixed.clone(), None)
    } else if let Some(idx) = wrapper.name_param_index {
        match call_args.get(idx) {
            Some(LuaExpr::LiteralExpr(lit)) => {
                if let Some(LuaLiteralToken::String(s)) = lit.get_literal() {
                    let value = s.get_value();
                    if value.is_empty() {
                        return;
                    }
                    (value, Some(call_args[idx].clone()))
                } else {
                    return;
                }
            }
            _ => return,
        }
    } else {
        return;
    };

    let value_type = if wrapper.is_element {
        LuaType::Number
    } else {
        resolve_networkvar_type(&type_name)
    };
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Use the name expression's syntax id for the getter if available,
    // otherwise use the call expression's syntax id.
    let getter_syntax_id = prop_name_expr
        .as_ref()
        .map(|e| e.get_syntax_id())
        .unwrap_or_else(|| call_expr.get_syntax_id());

    let getter_name = format!("Get{prop_name}");
    let getter_func =
        LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
    let member_id = LuaMemberId::new(getter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(getter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
    );

    // Setter
    let setter_syntax_id = call_expr.get_syntax_id();
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_inheritance_base(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    base_name: &str,
    is_derive: bool,
    class_name_prefix: Option<&str>,
) {
    if base_name.is_empty() {
        return;
    }

    let effective_base_name = if is_derive {
        let Some(prefix) = class_name_prefix else {
            return;
        };
        if base_name.starts_with(prefix) {
            base_name.to_string()
        } else {
            format!("{prefix}{base_name}")
        }
    } else {
        base_name.to_string()
    };

    materialize_scoped_gamemode_base(db, file_id, class_name_prefix, &effective_base_name);

    let super_type = LuaType::Ref(LuaTypeDeclId::global(&effective_base_name));
    db.get_type_index_mut()
        .add_super_type_if_missing(class_decl_id.clone(), file_id, super_type);
}

fn materialize_scoped_gamemode_base(
    db: &mut DbIndex,
    file_id: FileId,
    class_name_prefix: Option<&str>,
    base_name: &str,
) {
    let Some("gamemode_") = class_name_prefix else {
        return;
    };

    let Some(stripped) = base_name.strip_prefix("gamemode_") else {
        return;
    };
    if stripped.is_empty() {
        return;
    }

    let range = rowan::TextRange::default();
    ensure_scoped_class_type_decl(db, file_id, base_name, "GM", range);
}

fn resolve_effective_inheritance_call(
    metadata: &GmodScriptedClassFileMetadata,
) -> Option<&GmodScriptedClassCallMetadata> {
    metadata
        .derive_gamemode_calls
        .iter()
        .rev()
        .find(|call| valid_inheritance_literal(call))
        .or_else(|| {
            metadata
                .define_baseclass_calls
                .iter()
                .rev()
                .find(|call| valid_inheritance_literal(call))
        })
}

fn valid_inheritance_literal(call: &GmodScriptedClassCallMetadata) -> bool {
    matches!(
        call.literal_args.first(),
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty()
    )
}

fn resolve_effective_inheritance_base(
    metadata: &GmodScriptedClassFileMetadata,
    class_name_prefix: Option<&str>,
) -> Option<(String, bool)> {
    let call = resolve_effective_inheritance_call(metadata)?;
    let base_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) => name.as_str(),
        _ => return None,
    };

    if metadata
        .derive_gamemode_calls
        .iter()
        .any(|candidate| std::ptr::eq(candidate, call))
    {
        let prefix = class_name_prefix?;
        return Some((
            if base_name.starts_with(prefix) {
                base_name.to_string()
            } else {
                format!("{prefix}{base_name}")
            },
            true,
        ));
    }

    Some((base_name.to_string(), false))
}

/// Synthesize a parent-name alias member on a derived scripted class.
///
/// In Garry's Mod, derived gamemodes can access their inherited base via a
/// field named after the parent's short (prefix-stripped) folder name. For
/// example, a DarkRP gamemode inheriting from Sandbox uses `self.Sandbox` to
/// reach the base gamemode table. The runtime exposes this field, but the
/// analyzer would otherwise have no type for it, which breaks hover, goto,
/// and completion on `self.<ShortParentName>.<member>`.
///
/// Rules (mirroring the oracle-approved design):
/// - Only applies when the scope declares a non-empty `classNamePrefix`.
/// - The parent class name must start with that prefix, and the remainder
///   must be non-empty (otherwise we skip silently to avoid bogus aliases
///   on malformed or cross-scope base names).
/// - If the derived class already has a member with the alias name (for
///   example, because the user wrote `GM.Sandbox = BaseClass` themselves),
///   the explicit field wins and we do not synthesize a duplicate.
fn synthesize_define_baseclass_parent_alias(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    class_name_prefix: Option<&str>,
    call: &GmodScriptedClassCallMetadata,
) {
    let prefix = match class_name_prefix {
        Some(p) if !p.is_empty() => p,
        _ => return,
    };

    let base_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name.as_str(),
        _ => return,
    };

    // Parent must belong to the same prefix-scoped class family, otherwise
    // the short-name convention does not apply.
    let Some(stripped) = base_name.strip_prefix(prefix) else {
        return;
    };
    if stripped.is_empty() {
        return;
    }

    let alias_name = capitalize_ascii_first(stripped);
    if alias_name.is_empty() {
        return;
    }

    let owner = LuaMemberOwner::Type(class_decl_id.clone());
    let member_key = LuaMemberKey::Name(alias_name.as_str().into());

    // If the user already defined this field (e.g. `GM.Sandbox = BaseClass`),
    // let their definition win — don't shadow it with a synthetic decl.
    if db
        .get_member_index()
        .get_member_item(&owner, &member_key)
        .is_some()
    {
        return;
    }

    // Prefer the base-name string argument's syntax id for provenance; fall
    // back to the call itself so hover/goto still lands somewhere useful.
    let syntax_id = call
        .args
        .first()
        .map(|a| a.syntax_id)
        .unwrap_or(call.syntax_id);

    let member_id = LuaMemberId::new(syntax_id, file_id);
    let member = LuaMember::new(member_id, member_key, LuaMemberFeature::FileFieldDecl, None);
    db.get_member_index_mut().add_member(owner, member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::Ref(LuaTypeDeclId::global(base_name))),
    );
}

/// Uppercase the first ASCII letter of `s`, leaving the rest untouched.
/// Non-ASCII leading bytes are preserved as-is (GMod class names are ASCII
/// in practice, so this keeps the implementation simple and allocation-light).
fn capitalize_ascii_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => {
            let mut out = String::with_capacity(s.len());
            for c in first.to_uppercase() {
                out.push(c);
            }
            out.extend(chars);
            out
        }
        None => String::new(),
    }
}

fn synthesize_accessor_func(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // AccessorFunc(target, "m_VarKey", "Name", forceType)
    // args[0] = target (ENT etc) - non-literal name ref
    // args[1] = backing field name (string)
    // args[2] = accessor name (string)
    // args[3] = force type (FORCE_STRING, number, bool, etc)

    let accessor_name = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    if accessor_name.is_empty() {
        return;
    }

    let var_key = match call.literal_args.get(1) {
        Some(Some(GmodClassCallLiteral::String(name))) => Some(name.clone()),
        _ => None,
    };

    let force_type = call.literal_args.get(3).and_then(|arg| arg.as_ref());
    let value_type = resolve_accessor_force_type(force_type);
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize the backing field if var_key is present
    if let Some(ref var_key_name) = var_key {
        if let Some(field_syntax_id) = call.args.get(1).map(|a| a.syntax_id) {
            let member_id = LuaMemberId::new(field_syntax_id, file_id);
            let member = LuaMember::new(
                member_id,
                LuaMemberKey::Name(var_key_name.as_str().into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            );
            db.get_member_index_mut().add_member(owner.clone(), member);
            db.get_type_index_mut()
                .bind_type(member_id.into(), LuaTypeCache::DocType(value_type.clone()));
        }
    }

    // Synthesize the getter: GetName(self: Class): valueType
    if let Some(getter_syntax_id) = call.args.get(2).map(|a| a.syntax_id) {
        let getter_name = format!("Get{accessor_name}");
        let getter_func =
            LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut().add_member(owner.clone(), member);
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
        );
    }

    // Synthesize the setter: SetName(self: Class, value: valueType)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{accessor_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_network_var(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // ENT:NetworkVar("Type", slot, "Name") — 3-arg form
    // ENT:NetworkVar("Type", "Name")       — 2-arg form (slot omitted)
    // args[0] = type name (string)
    // args[1] = slot (integer) OR name (string, if 2-arg form)
    // args[2] = name (string, if 3-arg form)

    let type_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    // Try index 2 first (3-arg form), then index 1 (2-arg form)
    let (prop_name, prop_name_arg_idx) = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
            (name.clone(), 2usize)
        }
        _ => match call.literal_args.get(1) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), 1usize)
            }
            _ => return,
        },
    };

    let value_type = resolve_networkvar_type(&type_name);
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize getter: GetPropName(self: Class): valueType
    if let Some(getter_syntax_id) = call.args.get(prop_name_arg_idx).map(|a| a.syntax_id) {
        let getter_name = format!("Get{prop_name}");
        let getter_func =
            LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut().add_member(owner.clone(), member);
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
        );
    }

    // Synthesize setter: SetPropName(self: Class, value: valueType)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_network_var_element(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // ENT:NetworkVarElement("Type", slot, element, "Name") — 4-arg form
    // ENT:NetworkVarElement("Type", slot, "Name")          — 3-arg form
    // ENT:NetworkVarElement("Type", "Name")                — 2-arg form
    // The value type is always `number` for element access.
    // args[0] = type name (string) — used only for validation, not for type
    // args[1] = slot or name
    // args[2] = element or name
    // args[3] = name (if 4-arg form)

    // Type name must be present (first arg is always the type)
    if call.literal_args.first().and_then(|a| a.as_ref()).is_none() {
        return;
    }

    // Find the property name: try index 3, then 2, then 1
    let (prop_name, prop_name_arg_idx) = match call.literal_args.get(3) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
            (name.clone(), 3usize)
        }
        _ => match call.literal_args.get(2) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), 2usize)
            }
            _ => match call.literal_args.get(1) {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    (name.clone(), 1usize)
                }
                _ => return,
            },
        },
    };

    // NetworkVarElement always produces number accessors
    let value_type = LuaType::Number;
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize getter: GetPropName(self: Class): number
    if let Some(getter_syntax_id) = call.args.get(prop_name_arg_idx).map(|a| a.syntax_id) {
        let getter_name = format!("Get{prop_name}");
        let getter_func =
            LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut().add_member(owner.clone(), member);
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
        );
    }

    // Synthesize setter: SetPropName(self: Class, value: number)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_vgui_register(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    original_decl_table_types: &mut HashMap<LuaDeclId, Option<LuaType>>,
) {
    // vgui.Register("PanelName", TABLE, "BasePanel")
    // args[0] = panel name (string)
    // args[1] = table variable (name ref)
    // args[2] = base panel name (string)
    let panel_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name.clone(),
        _ => return,
    };

    let table_var_name = match call.literal_args.get(1) {
        Some(Some(GmodClassCallLiteral::NameRef(name))) => Some(name.clone()),
        _ => None,
    };

    let base_panel = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    synthesize_panel_class(
        db,
        file_id,
        &panel_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        call,
        original_decl_table_types,
    );
}

fn synthesize_derma_define_control(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    original_decl_table_types: &mut HashMap<LuaDeclId, Option<LuaType>>,
) {
    // derma.DefineControl("ControlName", "description", TABLE, "BasePanel")
    // args[0] = control name (string)
    // args[1] = description (string, ignored)
    // args[2] = table variable (name ref)
    // args[3] = base panel name (string)
    let control_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name.clone(),
        _ => return,
    };

    let table_var_name = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::NameRef(name))) => Some(name.clone()),
        _ => None,
    };

    let base_panel = match call.literal_args.get(3) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    synthesize_panel_class(
        db,
        file_id,
        &control_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        call,
        original_decl_table_types,
    );

    // Register the control name as a global variable with the panel type
    register_global_panel(db, file_id, &control_name, call);
}

/// Register a panel name as a global variable with the panel class type.
fn register_global_panel(
    db: &mut DbIndex,
    file_id: FileId,
    panel_name: &str,
    call: &GmodScriptedClassCallMetadata,
) {
    use glua_parser::LuaSyntaxKind;

    let class_decl_id = LuaTypeDeclId::global(panel_name);
    let call_range = call.syntax_id.get_range();

    // Create a global declaration for the panel name
    let global_decl = LuaDecl::new(
        panel_name,
        file_id,
        call_range,
        LuaDeclExtra::Global {
            kind: LuaSyntaxKind::NameExpr.into(),
        },
        None,
    );

    let decl_id = global_decl.get_id();

    // Add the declaration to the declaration tree
    if let Some(decl_tree) = db.get_decl_index_mut().get_decl_tree_mut(&file_id) {
        decl_tree.add_decl(global_decl);
    }

    // Register the global in the global index
    db.get_global_index_mut()
        .add_global_decl(panel_name, decl_id);

    // Bind the panel class type to the global declaration
    db.get_type_index_mut().force_bind_type(
        decl_id.into(),
        LuaTypeCache::InferType(LuaType::Def(class_decl_id)),
    );
}

fn find_table_type_for_register(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    register_position: TextSize,
) -> Option<LuaType> {
    let latest_write_decl_id =
        find_latest_decl_write_before_position(db, file_id, decl_id, register_position)
            .map(|position| LuaDeclId::new(file_id, position));

    if let Some(write_decl_id) = latest_write_decl_id
        && let Some(type_cache) = db.get_type_index().get_type_cache(&write_decl_id.into())
    {
        return Some(type_cache.as_type().clone());
    }

    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .map(|type_cache| type_cache.as_type().clone())
}

fn find_latest_decl_write_before_position(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    position: TextSize,
) -> Option<TextSize> {
    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .and_then(|decl_references| {
            decl_references
                .cells
                .iter()
                .filter(|cell| cell.is_write && cell.range.start() < position)
                .max_by_key(|cell| cell.range.start())
                .map(|cell| cell.range.start())
        })
}

fn synthesize_panel_class(
    db: &mut DbIndex,
    file_id: FileId,
    panel_name: &str,
    table_var_name: Option<&str>,
    base_panel: Option<&str>,
    call: &GmodScriptedClassCallMetadata,
    original_decl_table_types: &mut HashMap<LuaDeclId, Option<LuaType>>,
) {
    let class_decl_id = LuaTypeDeclId::global(panel_name);

    // Create the class type declaration if it doesn't exist
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                call.syntax_id.get_range(),
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
    }

    // Set super type from base panel
    if let Some(base_name) = base_panel {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(base_name));
        let has_super = db
            .get_type_index()
            .get_super_types_iter(&class_decl_id)
            .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
            .unwrap_or(false);
        if !has_super {
            db.get_type_index_mut()
                .add_super_type(class_decl_id.clone(), file_id, super_type);
        }
    }

    // Bind the table variable to the panel class
    if let Some(var_name) = table_var_name {
        let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) else {
            return;
        };

        let register_position = call.syntax_id.get_range().start();
        let selected_decl_id = decl_tree
            .find_local_decl(var_name, register_position)
            .map(|decl| decl.get_id());

        let Some(decl_id) = selected_decl_id else {
            return;
        };

        let previous_decl_type =
            find_table_type_for_register(db, file_id, decl_id, register_position);
        let decl_table_type = original_decl_table_types
            .entry(decl_id)
            .or_insert_with(|| {
                db.get_type_index()
                    .get_type_cache(&decl_id.into())
                    .map(|type_cache| type_cache.as_type().clone())
            })
            .clone();
        let latest_write_position =
            find_latest_decl_write_before_position(db, file_id, decl_id, register_position);

        db.get_type_index_mut().force_bind_type(
            decl_id.into(),
            LuaTypeCache::InferType(LuaType::Def(class_decl_id.clone())),
        );

        // Transfer table members to the class
        let mut table_ranges = Vec::new();
        if let Some(LuaType::TableConst(table_range)) = previous_decl_type {
            table_ranges.push(table_range);
        }
        if let Some(LuaType::TableConst(table_range)) = decl_table_type
            && !table_ranges.iter().any(|existing| existing == &table_range)
        {
            table_ranges.push(table_range);
        }

        if !table_ranges.is_empty() {
            let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
            let mut table_member_ids = HashSet::new();

            for table_range in table_ranges {
                let table_member_owner = LuaMemberOwner::Element(table_range);
                if let Some(members) = db.get_member_index().get_members(&table_member_owner) {
                    for member in members {
                        let member_position = member.get_id().get_position();
                        if member_position < register_position
                            && latest_write_position
                                .map(|write_position| member_position >= write_position)
                                .unwrap_or(true)
                        {
                            table_member_ids.insert(member.get_id());
                        }
                    }
                }
            }

            for member_id in table_member_ids {
                add_member(db, class_member_owner.clone(), member_id);
            }
        }
    }
}

/// Resolve AccessorFunc force type argument to a LuaType.
fn resolve_accessor_force_type(force_arg: Option<&GmodClassCallLiteral>) -> LuaType {
    match force_arg {
        Some(GmodClassCallLiteral::NameRef(name)) => match name.as_str() {
            "FORCE_STRING" => LuaType::String,
            "FORCE_NUMBER" => LuaType::Number,
            "FORCE_BOOL" => LuaType::Boolean,
            "FORCE_ANGLE" => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            "FORCE_COLOR" => LuaType::Ref(LuaTypeDeclId::global("Color")),
            "FORCE_VECTOR" => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Integer(n)) => match *n {
            1 => LuaType::String,
            2 => LuaType::Number,
            3 => LuaType::Boolean,
            4 => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            5 => LuaType::Ref(LuaTypeDeclId::global("Color")),
            6 => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Unsigned(n)) => match *n {
            1 => LuaType::String,
            2 => LuaType::Number,
            3 => LuaType::Boolean,
            4 => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            5 => LuaType::Ref(LuaTypeDeclId::global("Color")),
            6 => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Boolean(true)) => LuaType::Boolean,
        _ => LuaType::Any,
    }
}

/// Resolve NetworkVar type name to a LuaType.
fn resolve_networkvar_type(type_name: &str) -> LuaType {
    match type_name {
        "String" => LuaType::String,
        "Bool" => LuaType::Boolean,
        "Float" | "Double" => LuaType::Number,
        "Int" | "UInt" => LuaType::Integer,
        "Vector" => LuaType::Ref(LuaTypeDeclId::global("Vector")),
        "Angle" => LuaType::Ref(LuaTypeDeclId::global("Angle")),
        "Entity" => LuaType::Ref(LuaTypeDeclId::global("Entity")),
        "Color" => LuaType::Ref(LuaTypeDeclId::global("Color")),
        _ => {
            log::warn!(
                "Unknown NetworkVar type '{}', defaulting to Any. Valid types are: \
                String, Bool, Float, Double, Int, UInt, Vector, Angle, Entity, Color",
                type_name
            );
            LuaType::Any
        }
    }
}

fn detect_scoped_class_from_path(db: &DbIndex, file_id: FileId) -> Option<GmodScopedClassMatch> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    db.get_emmyrc()
        .gmod
        .scripted_class_scopes
        .detect_class_for_path(file_path)
        .map(|scope_match| GmodScopedClassMatch {
            global_name: scope_match.definition.class_global,
            class_name: scope_match.class_name,
            class_name_prefix: scope_match.definition.class_name_prefix,
        })
}

/// Returns the gmod scripted-class name that the given file belongs to, if any.
/// For example, a file at `lua/entities/base_glide_car/sv_braking.lua` returns
/// `Some("base_glide_car")`.
/// Uses cached scoped class info when available (populated during gmod_pre phase),
/// falling back to path detection.
pub fn get_gmod_class_name_for_file(db: &DbIndex, file_id: FileId) -> Option<String> {
    if let Some(info) = db.get_gmod_infer_index().get_scoped_class_info(&file_id) {
        return Some(info.class_name.clone());
    }
    detect_scoped_class_from_path(db, file_id).map(|m| m.class_name)
}

/// Returns the scripted class info `(class_name, global_name)` for a file, if it belongs to a
/// GMod scripted class scope.  `global_name` is the well-known table name used in the file
/// (e.g. `"ENT"`, `"SWEP"`, `"TOOL"`, `"EFFECT"`, `"PLUGIN"`).
/// Uses cached scoped class info when available, falling back to path detection.
pub fn get_scripted_class_info_for_file(db: &DbIndex, file_id: FileId) -> Option<(String, String)> {
    get_scripted_class_info_with_prefix(db, file_id).map(|(c, g, _)| (c, g))
}

/// Like [`get_scripted_class_info_for_file`] but also returns the scope's
/// `class_name_prefix`, so callers can correctly strip it to recover the
/// folder short-name (used for parent-alias synthesis on inherited classes).
pub(crate) fn get_scripted_class_info_with_prefix(
    db: &DbIndex,
    file_id: FileId,
) -> Option<(String, String, Option<String>)> {
    if let Some(info) = db.get_gmod_infer_index().get_scoped_class_info(&file_id) {
        return Some((
            info.class_name.clone(),
            info.global_name.clone(),
            info.class_name_prefix.clone(),
        ));
    }
    detect_scoped_class_from_path(db, file_id)
        .map(|m| (m.class_name, m.global_name, m.class_name_prefix))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GmodSystemCallKind {
    AddNetworkString,
    NetStart,
    NetReceive,
    ConcommandAdd,
    CreateConVar,
    CreateClientConVar,
    TimerCreate,
    TimerSimple,
}

fn collect_system_call_metadata(
    db: &mut DbIndex,
    file_id: FileId,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;
    let kind = classify_system_call_path(&call_path)?;

    match kind {
        GmodSystemCallKind::AddNetworkString => {
            let (name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            db.get_gmod_infer_index_mut().add_net_message_registration(
                file_id,
                GmodNamedSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    name,
                    name_range,
                },
            );
        }
        GmodSystemCallKind::NetStart => {
            let (name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            db.get_gmod_infer_index_mut().add_net_start_site(
                file_id,
                GmodNamedSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    name,
                    name_range,
                },
            );
        }
        GmodSystemCallKind::NetReceive => {
            let (message_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            let callback = extract_callback_arg(call_expr.clone(), 1);
            db.get_gmod_infer_index_mut().add_net_receive_site(
                file_id,
                GmodNetReceiveSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    message_name,
                    name_range,
                    callback,
                },
            );
        }
        GmodSystemCallKind::ConcommandAdd => {
            let (command_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            let callback = extract_callback_arg(call_expr.clone(), 1);
            db.get_gmod_infer_index_mut().add_concommand_site(
                file_id,
                GmodConcommandSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    command_name,
                    name_range,
                    callback,
                },
            );
        }
        GmodSystemCallKind::CreateConVar | GmodSystemCallKind::CreateClientConVar => {
            let (convar_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            db.get_gmod_infer_index_mut().add_convar_site(
                file_id,
                GmodConVarSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    kind: if kind == GmodSystemCallKind::CreateClientConVar {
                        GmodConVarKind::Client
                    } else {
                        GmodConVarKind::Server
                    },
                    convar_name,
                    name_range,
                },
            );
        }
        GmodSystemCallKind::TimerCreate => {
            let (timer_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            let callback = extract_callback_arg(call_expr.clone(), 3);
            db.get_gmod_infer_index_mut().add_timer_site(
                file_id,
                GmodTimerSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    kind: GmodTimerKind::Create,
                    timer_name,
                    name_range,
                    callback,
                },
            );
        }
        GmodSystemCallKind::TimerSimple => {
            let callback = extract_callback_arg(call_expr.clone(), 1);
            db.get_gmod_infer_index_mut().add_timer_site(
                file_id,
                GmodTimerSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    kind: GmodTimerKind::Simple,
                    timer_name: None,
                    name_range: None,
                    callback,
                },
            );
        }
    }

    Some(())
}

fn classify_system_call_path(path: &str) -> Option<GmodSystemCallKind> {
    if matches_call_path(path, "util.AddNetworkString") {
        return Some(GmodSystemCallKind::AddNetworkString);
    }
    if matches_call_path(path, "net.Start") {
        return Some(GmodSystemCallKind::NetStart);
    }
    if matches_call_path(path, "net.Receive") {
        return Some(GmodSystemCallKind::NetReceive);
    }
    if matches_call_path(path, "concommand.Add") {
        return Some(GmodSystemCallKind::ConcommandAdd);
    }
    if matches_call_path(path, "CreateClientConVar") {
        return Some(GmodSystemCallKind::CreateClientConVar);
    }
    if matches_call_path(path, "CreateConVar") {
        return Some(GmodSystemCallKind::CreateConVar);
    }
    if matches_call_path(path, "timer.Create") {
        return Some(GmodSystemCallKind::TimerCreate);
    }
    if matches_call_path(path, "timer.Simple") {
        return Some(GmodSystemCallKind::TimerSimple);
    }
    None
}

fn matches_call_path(path: &str, target: &str) -> bool {
    path == target || path.ends_with(&format!(".{target}")) || path.ends_with(&format!(":{target}"))
}

fn extract_static_string_arg(
    call_expr: LuaCallExpr,
    arg_idx: usize,
) -> (Option<String>, Option<rowan::TextRange>) {
    let Some(arg_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))
    else {
        return (None, None);
    };

    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return (None, None);
    };

    match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(string_token)) => (
            Some(string_token.get_value()),
            Some(string_token.get_range()),
        ),
        Some(_) => (None, Some(literal_expr.get_range())),
        None => (None, Some(literal_expr.get_range())),
    }
}

fn extract_callback_arg(call_expr: LuaCallExpr, arg_idx: usize) -> GmodCallbackSiteMetadata {
    let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))
    else {
        return GmodCallbackSiteMetadata::default();
    };

    GmodCallbackSiteMetadata {
        syntax_id: Some(callback_expr.get_syntax_id()),
        callback_range: Some(callback_expr.get_range()),
    }
}

fn collect_hook_call_site(db: &DbIndex, call_expr: LuaCallExpr) -> Option<GmodHookSiteMetadata> {
    let call_path = call_expr.get_access_path()?;
    let mapped_hook = mapped_hook_for_emitter_call(db, &call_path, call_expr.clone());
    let kind = mapped_hook
        .as_ref()
        .map(|_| GmodHookKind::Emit)
        .or_else(|| classify_hook_call_path(&call_path))?;
    let (hook_name, name_range, name_issue) = mapped_hook.unwrap_or_else(|| {
        extract_static_hook_name(
            call_expr
                .get_args_list()
                .and_then(|args| args.get_args().next()),
        )
    });

    Some(GmodHookSiteMetadata {
        syntax_id: call_expr.get_syntax_id(),
        kind,
        hook_name,
        name_range,
        name_issue,
        callback_params: if kind == GmodHookKind::Add {
            extract_hook_callback_params_from_call(&call_expr)
        } else {
            Vec::new()
        },
    })
}

fn classify_hook_call_path(path: &str) -> Option<GmodHookKind> {
    if matches_call_path(path, "hook.Add") {
        return Some(GmodHookKind::Add);
    }

    if matches_call_path(path, "hook.Run") || matches_call_path(path, "hook.Call") {
        return Some(GmodHookKind::Emit);
    }

    None
}

fn mapped_hook_for_emitter_call(
    db: &DbIndex,
    call_path: &str,
    call_expr: LuaCallExpr,
) -> Option<(
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
)> {
    for (emitter_path, mapped_hook) in &db.get_emmyrc().gmod.hook_mappings.emitter_to_hook {
        if !matches_call_path(call_path, emitter_path) {
            continue;
        }

        if mapped_hook == "*" {
            return Some(extract_static_hook_name(
                call_expr
                    .get_args_list()
                    .and_then(|args| args.get_args().next()),
            ));
        }

        let trimmed = mapped_hook.trim();
        return Some(if trimmed.is_empty() {
            (None, None, Some(GmodHookNameIssue::Empty))
        } else {
            (Some(trimmed.to_string()), None, None)
        });
    }

    None
}

fn collect_hook_method_site(db: &DbIndex, func_stat: LuaFuncStat) -> Option<GmodHookSiteMetadata> {
    let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
        return None;
    };
    let is_colon = index_expr.get_index_token()?.is_colon();

    let LuaExpr::NameExpr(prefix_name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };

    let prefix_name = prefix_name_expr.get_name_text()?;
    let separator = if is_colon { ":" } else { "." };

    let (method_name, name_range) = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name_token) => (
            Some(name_token.get_name_text().to_string()),
            Some(name_token.get_range()),
        ),
        LuaIndexKey::String(string_token) => (
            Some(string_token.get_value()),
            Some(string_token.get_range()),
        ),
        _ => (None, None),
    };

    let mapped_method_hook = method_mapped_hook_name(
        db,
        &prefix_name,
        separator,
        method_name.as_deref().unwrap_or_default(),
    );
    let annotation = hook_annotation_from_doc(&func_stat);
    let trimmed_method_name = method_name
        .as_ref()
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    let (hook_name, mut name_issue) = if let Some((hook_name, name_issue)) = mapped_method_hook {
        (hook_name, name_issue)
    } else if let Some(annotation_hook) = annotation
        && (is_builtin_method_hook_prefix(&prefix_name)
            || is_configured_method_hook_prefix(db, &prefix_name))
    {
        let hook_name = annotation_hook.hook_name.or_else(|| {
            (!trimmed_method_name.is_empty()).then_some(trimmed_method_name.to_string())
        });
        let name_issue = if hook_name.is_none() {
            Some(GmodHookNameIssue::Empty)
        } else {
            annotation_hook.name_issue
        };
        (hook_name, name_issue)
    } else {
        if !is_colon
            || (!is_builtin_method_hook_prefix(&prefix_name)
                && !is_configured_method_hook_prefix(db, &prefix_name))
        {
            return None;
        }
        let name_issue = trimmed_method_name
            .is_empty()
            .then_some(GmodHookNameIssue::Empty);
        let hook_name = (!trimmed_method_name.is_empty()).then_some(trimmed_method_name);
        (hook_name, name_issue)
    };

    let hook_name = normalize_gamemode_hook_name(hook_name);
    if hook_name.is_none() && name_issue.is_none() {
        name_issue = Some(GmodHookNameIssue::Empty);
    }

    Some(GmodHookSiteMetadata {
        syntax_id: index_expr.get_syntax_id(),
        kind: GmodHookKind::GamemodeMethod,
        hook_name,
        name_range,
        name_issue,
        callback_params: extract_hook_callback_params_from_method(&func_stat),
    })
}

fn extract_hook_callback_params_from_call(call_expr: &LuaCallExpr) -> Vec<String> {
    let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(2))
    else {
        return Vec::new();
    };

    let LuaExpr::ClosureExpr(closure_expr) = callback_expr else {
        return Vec::new();
    };

    extract_param_names_from_closure(closure_expr)
}

fn extract_hook_callback_params_from_method(func_stat: &LuaFuncStat) -> Vec<String> {
    let Some(closure_expr) = func_stat.get_closure() else {
        return Vec::new();
    };

    extract_param_names_from_closure(closure_expr)
}

fn extract_param_names_from_closure(closure_expr: glua_parser::LuaClosureExpr) -> Vec<String> {
    let Some(params_list) = closure_expr.get_params_list() else {
        return Vec::new();
    };

    params_list
        .get_params()
        .filter_map(|param| {
            if param.is_dots() {
                Some("...".to_string())
            } else {
                Some(param.get_name_token()?.get_name_text().to_string())
            }
        })
        .collect()
}

fn is_builtin_method_hook_prefix(prefix_name: &str) -> bool {
    matches!(prefix_name, "GM" | "GAMEMODE" | "PLUGIN" | "SANDBOX")
}

fn is_configured_method_hook_prefix(db: &DbIndex, prefix_name: &str) -> bool {
    db.get_emmyrc()
        .gmod
        .hook_mappings
        .method_prefixes
        .iter()
        .any(|configured_prefix| {
            configured_prefix
                .trim()
                .trim_end_matches([':', '.'])
                .eq_ignore_ascii_case(prefix_name)
        })
}

#[derive(Debug, Clone)]
struct HookAnnotationMetadata {
    hook_name: Option<String>,
    name_issue: Option<GmodHookNameIssue>,
}

fn hook_annotation_from_doc(func_stat: &LuaFuncStat) -> Option<HookAnnotationMetadata> {
    let comment = func_stat.get_left_comment()?;
    for tag in comment.get_doc_tags() {
        let LuaDocTag::Other(other_tag) = tag else {
            continue;
        };
        let tag_name = other_tag.get_tag_name()?;
        if !tag_name
            .trim_start_matches('@')
            .eq_ignore_ascii_case("hook")
        {
            continue;
        }

        let annotation_value = other_tag
            .get_description()
            .map(|description| description.get_description_text())
            .unwrap_or_default();
        let normalized = annotation_value.trim();
        let hook_name = (!normalized.is_empty()).then_some(normalized.to_string());

        return Some(HookAnnotationMetadata {
            hook_name,
            name_issue: None,
        });
    }

    None
}

fn method_mapped_hook_name(
    db: &DbIndex,
    prefix_name: &str,
    separator: &str,
    method_name: &str,
) -> Option<(Option<String>, Option<GmodHookNameIssue>)> {
    let mappings = &db.get_emmyrc().gmod.hook_mappings.method_to_hook;
    let method_name = method_name.trim();
    let mut candidates = vec![format!("{prefix_name}{separator}{method_name}")];
    if separator == ":" {
        candidates.push(format!("{prefix_name}.{method_name}"));
    } else {
        candidates.push(format!("{prefix_name}:{method_name}"));
    }

    for candidate in candidates {
        let Some(mapped_hook) = mappings.get(&candidate) else {
            continue;
        };

        if mapped_hook == "*" {
            return Some((
                (!method_name.is_empty()).then_some(method_name.to_string()),
                method_name.is_empty().then_some(GmodHookNameIssue::Empty),
            ));
        }

        let trimmed = mapped_hook.trim();
        return Some(if trimmed.is_empty() {
            (None, Some(GmodHookNameIssue::Empty))
        } else {
            (Some(trimmed.to_string()), None)
        });
    }

    None
}

fn normalize_gamemode_hook_name(hook_name: Option<String>) -> Option<String> {
    let hook_name = hook_name?;
    let trimmed = hook_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = strip_builtin_method_hook_prefix(trimmed)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed);

    Some(normalized.to_string())
}

fn strip_builtin_method_hook_prefix(name: &str) -> Option<&str> {
    for separator in [':', '.'] {
        let Some((prefix, remainder)) = name.split_once(separator) else {
            continue;
        };

        if is_builtin_method_hook_prefix(prefix.trim()) {
            return Some(remainder);
        }
    }

    None
}

fn extract_static_hook_name(
    first_arg: Option<LuaExpr>,
) -> (
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
) {
    let Some(first_arg) = first_arg else {
        return (None, None, None);
    };

    let LuaExpr::LiteralExpr(literal_expr) = first_arg else {
        return (None, None, None);
    };

    match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(string_token)) => {
            let hook_name = string_token.get_value();
            let issue = hook_name
                .trim()
                .is_empty()
                .then_some(GmodHookNameIssue::Empty);
            (Some(hook_name), Some(string_token.get_range()), issue)
        }
        Some(_) => (
            None,
            Some(literal_expr.get_range()),
            Some(GmodHookNameIssue::NonStringLiteral),
        ),
        None => (
            None,
            Some(literal_expr.get_range()),
            Some(GmodHookNameIssue::NonStringLiteral),
        ),
    }
}

/// Detect `if CLIENT then`/`if SERVER then` blocks and return realm-narrowed ranges.
fn collect_branch_realm_ranges(root: &LuaChunk) -> Vec<GmodRealmRange> {
    let mut ranges = Vec::new();
    for if_stat in root.descendants::<LuaIfStat>() {
        collect_if_realm_ranges(&if_stat, &mut ranges);
    }
    ranges.sort_by_key(|range| (range.range.len(), range.range.start()));
    ranges
}

/// Collect the first `---@realm client|server|shared` annotation from a file.
fn collect_realm_annotation(root: &LuaChunk) -> Option<GmodRealm> {
    for comment in root.descendants::<LuaComment>() {
        let is_file_level = matches!(comment.get_owner(), None | Some(LuaAst::LuaChunk(_)));
        if !is_file_level {
            continue;
        }

        if let Some(realm) = realm_from_doc_comment(&comment) {
            return Some(realm);
        }
    }

    None
}

pub(crate) fn realm_from_doc_comment(comment: &LuaComment) -> Option<GmodRealm> {
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

/// Extract realm narrowing from a single if-statement, handling if/elseif/else clauses.
/// Also handles early-return guards like `if not CLIENT then return end` which narrows
/// the realm of code after the if-statement to the complementary realm.
fn collect_if_realm_ranges(if_stat: &LuaIfStat, ranges: &mut Vec<GmodRealmRange>) {
    let condition_realm = if_stat
        .get_condition_expr()
        .as_ref()
        .and_then(realm_from_condition);

    if let Some(realm) = condition_realm {
        if let Some(block) = if_stat.get_block() {
            let range = block.syntax().text_range();
            ranges.push(GmodRealmRange { range, realm });
        } else {
            // Empty block (e.g., comment-only if-body): still record the realm
            // so that realm-awareness checks (like AddCSLuaFile CLIENT detection) work.
            // Use a zero-width range at the start of the if-statement as a marker.
            let pos = if_stat.syntax().text_range().start();
            ranges.push(GmodRealmRange {
                range: TextRange::new(pos, pos),
                realm,
            });
        }

        // Identify the complementary realm for else block
        let complement = match realm {
            GmodRealm::Client => Some(GmodRealm::Server),
            GmodRealm::Server => Some(GmodRealm::Client),
            _ => None,
        };

        // Handle elseif/else clauses
        let mut has_elseif = false;
        for clause in if_stat.get_all_clause() {
            match &clause {
                glua_parser::LuaIfClauseStat::ElseIf(elseif) => {
                    has_elseif = true;
                    if let Some(elseif_realm) = elseif
                        .get_condition_expr()
                        .as_ref()
                        .and_then(realm_from_condition)
                    {
                        if let Some(block) = elseif.get_block() {
                            ranges.push(GmodRealmRange {
                                range: block.syntax().text_range(),
                                realm: elseif_realm,
                            });
                        }
                    }
                }
                glua_parser::LuaIfClauseStat::Else(else_clause) => {
                    // Only assign complement realm if there's no elseif
                    // (with elseif, else block realm is ambiguous)
                    if !has_elseif {
                        if let Some(complement_realm) = complement {
                            if let Some(block) = else_clause.get_block() {
                                ranges.push(GmodRealmRange {
                                    range: block.syntax().text_range(),
                                    realm: complement_realm,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Check for early-return guard: `if not REALM then return end` or `if REALM then return end`
        // This should narrow the code AFTER the if-statement to the complement realm
        if let Some(block) = if_stat.get_block() {
            if is_early_return_block(&block) {
                if let Some(parent_block) = find_parent_block(if_stat.syntax()) {
                    let if_end = if_stat.syntax().text_range().end();
                    let block_end = parent_block.syntax().text_range().end();
                    let after_range = TextRange::new(if_end, block_end);

                    let after_realm = if let Some(expr) = if_stat.get_condition_expr() {
                        if is_not_condition(&expr) {
                            // `if not CLIENT then return end` → code after is Client
                            get_original_realm_from_complement(realm)
                        } else {
                            // `if CLIENT then return end` → code after is Server
                            complement
                        }
                    } else {
                        None
                    };

                    if let Some(after_realm) = after_realm {
                        ranges.push(GmodRealmRange {
                            range: after_range,
                            realm: after_realm,
                        });
                    }
                }
            }
        }
    }
}

/// Check if a condition expression is a "not" unary expression
fn is_not_condition(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => {
            // Handle `(not CLIENT)` - check inside parentheses
            if let Some(inner) = paren_expr.get_expr() {
                return is_not_condition(&inner);
            }
            false
        }
        LuaExpr::UnaryExpr(unary_expr) => {
            let op = unary_expr.get_op_token();
            if let Some(op) = op {
                let op_kind = op.get_op();
                return op_kind == glua_parser::UnaryOperator::OpNot;
            }
            false
        }
        _ => false,
    }
}

/// Given a complement realm (e.g., Server from `not CLIENT`), get the original realm (Client)
fn get_original_realm_from_complement(complement: GmodRealm) -> Option<GmodRealm> {
    match complement {
        GmodRealm::Client => Some(GmodRealm::Server),
        GmodRealm::Server => Some(GmodRealm::Client),
        _ => None,
    }
}

/// Check if a block contains only a return statement (early-return guard pattern)
fn is_early_return_block(block: &LuaBlock) -> bool {
    let mut stats = block.get_stats().peekable();

    // Check if there's exactly one statement
    let first_stat = stats.next();
    if first_stat.is_none() || stats.peek().is_some() {
        return false;
    }

    // Check if that statement is a return statement
    matches!(first_stat, Some(LuaStat::ReturnStat(_)))
}

/// Find the parent block containing a syntax node
fn find_parent_block(node: &LuaSyntaxNode) -> Option<LuaBlock> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if let Some(block) = LuaBlock::cast(parent.clone()) {
            return Some(block);
        }
        current = parent.parent();
    }
    None
}

/// Match condition expressions to realms.
/// Handles: `CLIENT`, `SERVER`, `not CLIENT`, `not SERVER`, `(CLIENT)`, `(SERVER)`
fn realm_from_condition(expr: &LuaExpr) -> Option<GmodRealm> {
    match expr {
        // Handle parentheses: extract inner expression and recurse
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .as_ref()
            .and_then(realm_from_condition),
        LuaExpr::NameExpr(name_expr) => match name_expr.get_name_text()?.as_str() {
            "CLIENT" => Some(GmodRealm::Client),
            "SERVER" => Some(GmodRealm::Server),
            _ => None,
        },
        LuaExpr::UnaryExpr(unary_expr) => {
            let op = unary_expr.get_op_token()?;
            let op_kind = op.get_op();
            if op_kind == glua_parser::UnaryOperator::OpNot {
                let inner = unary_expr.get_expr()?;
                let inner_realm = realm_from_condition(&inner)?;
                match inner_realm {
                    GmodRealm::Client => Some(GmodRealm::Server),
                    GmodRealm::Server => Some(GmodRealm::Client),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn rebuild_realm_metadata(
    db: &mut DbIndex,
    branch_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>>,
    annotation_realms: HashMap<FileId, GmodRealm>,
    analyzed_file_ids: &[FileId],
) {
    let file_ids = db.get_vfs().get_all_local_file_ids();
    let meta_file_ids: HashSet<FileId> = {
        let module_index = db.get_module_index();
        file_ids
            .iter()
            .copied()
            .filter(|file_id| module_index.is_meta_file(file_id))
            .collect()
    };
    let library_file_ids: HashSet<FileId> = {
        let module_index = db.get_module_index();
        file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                module_index
                    .get_workspace_id(*file_id)
                    .map(|ws_id| module_index.is_library_workspace_id(ws_id))
                    .unwrap_or(false)
            })
            .collect()
    };
    let default_realm = gmod_config_default_realm(db);
    let detect_filename = db
        .get_emmyrc()
        .gmod
        .detect_realm_from_filename
        .unwrap_or(true);
    let detect_calls = db.get_emmyrc().gmod.detect_realm_from_calls.unwrap_or(true);

    let analyzed_file_ids: HashSet<FileId> = analyzed_file_ids.iter().copied().collect();
    let previous_realm_metadata: HashMap<FileId, GmodRealmFileMetadata> = file_ids
        .iter()
        .filter_map(|file_id| {
            db.get_gmod_infer_index()
                .get_realm_file_metadata(file_id)
                .cloned()
                .map(|metadata| (*file_id, metadata))
        })
        .collect();

    let resolve_branch_ranges = |file_id: &FileId| {
        if let Some(ranges) = branch_realm_ranges.get(file_id) {
            return ranges.clone();
        }
        if analyzed_file_ids.contains(file_id) {
            return Vec::new();
        }
        previous_realm_metadata
            .get(file_id)
            .map(|metadata| metadata.branch_realm_ranges.clone())
            .unwrap_or_default()
    };

    let resolve_annotation_realm = |file_id: &FileId| {
        if let Some(realm) = annotation_realms.get(file_id) {
            return Some(*realm);
        }
        if analyzed_file_ids.contains(file_id) {
            return None;
        }
        previous_realm_metadata
            .get(file_id)
            .and_then(|metadata| metadata.annotation_realm)
    };

    if !detect_filename && !detect_calls {
        let realm_metadata = file_ids
            .into_iter()
            .map(|file_id| {
                let ranges = if meta_file_ids.contains(&file_id) {
                    Vec::new()
                } else {
                    resolve_branch_ranges(&file_id)
                };
                let annotation_realm = resolve_annotation_realm(&file_id);
                let is_meta_file = meta_file_ids.contains(&file_id);
                let is_library_file = library_file_ids.contains(&file_id);
                let realm = if is_meta_file || is_library_file {
                    annotation_realm.unwrap_or(GmodRealm::Shared)
                } else {
                    annotation_realm.unwrap_or(default_realm)
                };
                (
                    file_id,
                    GmodRealmFileMetadata {
                        inferred_realm: realm,
                        annotation_realm,
                        branch_realm_ranges: ranges,
                        ..Default::default()
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        db.get_gmod_infer_index_mut()
            .set_all_realm_file_metadata(realm_metadata);
        return;
    }

    let mut filename_hints: HashMap<FileId, Option<GmodRealm>> = HashMap::new();
    let mut dependency_hints: HashMap<FileId, HashSet<GmodRealm>> = HashMap::new();
    let mut include_edges = Vec::new();

    for file_id in &file_ids {
        let hint = if detect_filename {
            infer_realm_from_filename(db, *file_id)
        } else {
            None
        };
        filename_hints.insert(*file_id, hint);
    }

    if detect_calls {
        let dependency_index = db.get_file_dependencies_index();
        for source_file_id in &file_ids {
            let Some(dependencies) = dependency_index.get_required_files(source_file_id) else {
                continue;
            };

            for dependency_file_id in dependencies {
                let Some(kinds) =
                    dependency_index.get_dependency_kinds(source_file_id, dependency_file_id)
                else {
                    continue;
                };
                if kinds.contains(&LuaDependencyKind::AddCSLuaFile)
                    || kinds.contains(&LuaDependencyKind::IncludeCS)
                {
                    if source_file_id == dependency_file_id {
                        // Self-ref AddCSLuaFile() (no args): file sends itself to client,
                        // meaning it runs on both server (caller) and client → Shared.
                        dependency_hints
                            .entry(*source_file_id)
                            .or_default()
                            .insert(GmodRealm::Shared);
                    } else {
                        // AddCSLuaFile/IncludeCS marks the TARGET as Client
                        // (it's being sent to the client for download/execution).
                        // We do NOT add a Server hint to the source file — although AddCSLuaFile is
                        // server-only, shared files commonly call it inside `if SERVER then` blocks,
                        // so hinting the source as Server would cause false positives.
                        dependency_hints
                            .entry(*dependency_file_id)
                            .or_default()
                            .insert(GmodRealm::Client);
                    }
                }
                if kinds.contains(&LuaDependencyKind::Require) {
                    dependency_hints
                        .entry(*dependency_file_id)
                        .or_default()
                        .insert(GmodRealm::Shared);
                }
                if kinds.contains(&LuaDependencyKind::Include)
                    || kinds.contains(&LuaDependencyKind::IncludeCS)
                {
                    // NOTE: Include edges are file-level, not branch-scoped.
                    // An include() inside `if CLIENT then` still creates a file-level edge.
                    // This is a deliberate simplification; branch-scoped tracking would
                    // require storing call-site offsets in dependency edges (major arch change).
                    include_edges.push((*source_file_id, *dependency_file_id));
                }
            }
        }
    }

    let mut inferred_realms: HashMap<FileId, GmodRealm> = file_ids
        .iter()
        .map(|file_id| {
            (
                *file_id,
                if meta_file_ids.contains(file_id) {
                    GmodRealm::Unknown
                } else {
                    infer_realm(
                        filename_hints.get(file_id).copied().flatten(),
                        dependency_hints.get(file_id),
                        default_realm,
                    )
                },
            )
        })
        .collect();

    if detect_calls && !include_edges.is_empty() {
        for _ in 0..20 {
            let mut next_dependency_hints = dependency_hints.clone();
            for (source_file_id, dependency_file_id) in &include_edges {
                // Collect evidence for the source file: filename hint + dependency hints
                let mut source_evidence: HashSet<GmodRealm> = HashSet::new();
                if let Some(Some(fh)) = filename_hints.get(source_file_id) {
                    source_evidence.insert(*fh);
                }
                if let Some(hints) = next_dependency_hints.get(source_file_id) {
                    source_evidence.extend(hints.iter().copied());
                }

                // Forward propagation only: source → dependency.
                // We do NOT propagate backward (dependency → source) because a server-only
                // file can legitimately include a shared file without becoming shared itself.
                for hint in &source_evidence {
                    next_dependency_hints
                        .entry(*dependency_file_id)
                        .or_default()
                        .insert(*hint);
                }
            }

            let next_inferred_realms: HashMap<FileId, GmodRealm> = file_ids
                .iter()
                .map(|file_id| {
                    (
                        *file_id,
                        if meta_file_ids.contains(file_id) {
                            GmodRealm::Unknown
                        } else {
                            infer_realm(
                                filename_hints.get(file_id).copied().flatten(),
                                next_dependency_hints.get(file_id),
                                default_realm,
                            )
                        },
                    )
                })
                .collect();

            dependency_hints = next_dependency_hints;
            if next_inferred_realms == inferred_realms {
                break;
            }

            inferred_realms = next_inferred_realms;
        }
    }

    let mut realm_metadata = HashMap::new();
    for file_id in file_ids {
        let ranges = if meta_file_ids.contains(&file_id) {
            Vec::new()
        } else {
            resolve_branch_ranges(&file_id)
        };

        let annotation_realm = resolve_annotation_realm(&file_id);
        let is_meta_file = meta_file_ids.contains(&file_id);
        let hints = if is_meta_file {
            Vec::new()
        } else {
            let mut hints = dependency_hints
                .remove(&file_id)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            hints.sort_by_key(|realm| realm_sort_key(*realm));
            hints
        };

        let is_library_file = library_file_ids.contains(&file_id);

        let final_realm = if is_meta_file {
            // Meta files default to Shared unless explicitly annotated otherwise
            annotation_realm.unwrap_or(GmodRealm::Shared)
        } else if is_library_file {
            // Library files (annotations) default to Shared since they define cross-realm APIs
            annotation_realm.unwrap_or(GmodRealm::Shared)
        } else {
            annotation_realm.unwrap_or_else(|| {
                inferred_realms
                    .get(&file_id)
                    .copied()
                    .unwrap_or(default_realm)
            })
        };

        realm_metadata.insert(
            file_id,
            GmodRealmFileMetadata {
                inferred_realm: final_realm,
                filename_hint: if is_meta_file {
                    None
                } else {
                    filename_hints.get(&file_id).copied().flatten()
                },
                dependency_hints: hints,
                annotation_realm,
                branch_realm_ranges: ranges,
            },
        );
    }

    db.get_gmod_infer_index_mut()
        .set_all_realm_file_metadata(realm_metadata);
}

fn infer_realm(
    filename_hint: Option<GmodRealm>,
    dependency_hints: Option<&HashSet<GmodRealm>>,
    default_realm: GmodRealm,
) -> GmodRealm {
    if let Some(filename_hint) = filename_hint
        && filename_hint != GmodRealm::Unknown
    {
        return filename_hint;
    }

    let mut hints = HashSet::new();

    if let Some(dependency_hints) = dependency_hints {
        hints.extend(
            dependency_hints
                .iter()
                .copied()
                .filter(|realm| *realm != GmodRealm::Unknown),
        );
    }

    if hints.is_empty() {
        return default_realm;
    }

    if hints.len() == 1 {
        return *hints.iter().next().expect("len checked");
    }

    // Any combination containing Shared, or both Client+Server, resolves to Shared
    if hints.contains(&GmodRealm::Shared)
        || (hints.contains(&GmodRealm::Client) && hints.contains(&GmodRealm::Server))
    {
        return GmodRealm::Shared;
    }

    // Fallback for unexpected combinations
    default_realm
}

fn gmod_config_default_realm(db: &DbIndex) -> GmodRealm {
    match db.get_emmyrc().gmod.default_realm {
        EmmyrcGmodRealm::Client => GmodRealm::Client,
        EmmyrcGmodRealm::Server => GmodRealm::Server,
        EmmyrcGmodRealm::Shared => GmodRealm::Shared,
        EmmyrcGmodRealm::Menu => GmodRealm::Unknown,
    }
}

fn infer_realm_from_filename(db: &DbIndex, file_id: FileId) -> Option<GmodRealm> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    let file_name = file_path
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();

    // 1. Check filename prefixes FIRST (highest confidence)
    if file_name.starts_with("cl_") {
        return Some(GmodRealm::Client);
    }
    if file_name.starts_with("sv_") {
        return Some(GmodRealm::Server);
    }
    if file_name.starts_with("sh_") {
        return Some(GmodRealm::Shared);
    }

    // 2. Check parent directory names for realm hints SECOND
    // Prefer the path segment after the last `/lua/` anchor to avoid false realm hints
    // from unrelated parent directory names (e.g. a user home directory named "server").
    // If there is no `/lua/` anchor, still allow inference for known GMod workspace layouts
    // such as addon-root (`lua/...`) and gamemode-root (`gamemode/...`, `entities/...`).
    let path_str = file_path.to_string_lossy().to_ascii_lowercase();
    let path_str = path_str.replace('\\', "/");
    let components = file_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();

    // Try to find /lua/ anchor first
    let search_str = if let Some(idx) = path_str.rfind("/lua/") {
        &path_str[idx..]
    } else {
        // Fall back to full-path detection only for known GMod-like trees.
        let is_gmod_tree = components.iter().any(|segment| {
            matches!(
                segment.as_str(),
                "addons"
                    | "gamemodes"
                    | "lua"
                    | "gamemode"
                    | "entities"
                    | "weapons"
                    | "effects"
                    | "postprocess"
                    | "vgui"
                    | "matproxy"
                    | "skins"
                    | "autorun"
                    | "includes"
            )
        });
        if !is_gmod_tree {
            return None;
        }
        &path_str
    };

    if search_str.contains("/client/") || search_str.contains("/cl/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/server/") || search_str.contains("/sv/") {
        return Some(GmodRealm::Server);
    }
    if search_str.contains("/shared/") || search_str.contains("/sh/") {
        return Some(GmodRealm::Shared);
    }

    // 3. Check GMod special directory patterns (engine-defined realm behavior per GMod loading order)
    // These MUST come before the init.lua/shared.lua filename checks because e.g.
    // effects/init.lua should be Shared (effects load on both realms), not Server.
    if search_str.contains("/effects/") {
        return Some(GmodRealm::Shared);
    }
    if search_str.contains("/vgui/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/postprocess/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/matproxy/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/skins/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/autorun/") {
        // Note: autorun/server/ and autorun/client/ are already caught above
        // by the /server/ and /client/ directory checks.
        return Some(GmodRealm::Shared);
    }
    if search_str.contains("/includes/") {
        return Some(GmodRealm::Shared);
    }
    if search_str.contains("/stools/") {
        return Some(GmodRealm::Shared);
    }

    // 4. Check specific filenames LAST (lowest confidence)
    if file_name == "cl_init.lua" {
        return Some(GmodRealm::Client);
    }
    if file_name == "init.lua" {
        return Some(GmodRealm::Server);
    }
    if file_name == "shared.lua" {
        return Some(GmodRealm::Shared);
    }

    None
}

fn realm_sort_key(realm: GmodRealm) -> u8 {
    match realm {
        GmodRealm::Client => 0,
        GmodRealm::Server => 1,
        GmodRealm::Shared => 2,
        GmodRealm::Unknown => 3,
    }
}

/// Collect @fileparam annotations from a chunk, returning (name_lowercase, type_text) pairs.
fn collect_file_params(chunk: &LuaChunk) -> Vec<(String, String)> {
    let mut params = Vec::new();
    for descendant in chunk.syntax().descendants() {
        if LuaDocTagFileparam::can_cast(descendant.kind().into()) {
            if let Some(fileparam) = LuaDocTagFileparam::cast(descendant) {
                if let Some(name_token) = fileparam.get_name_token() {
                    if let Some(typ) = fileparam.get_type() {
                        let name = name_token.get_name_text().to_ascii_lowercase();
                        let type_text = typ.syntax().text().to_string();
                        params.push((name, type_text));
                    }
                }
            }
        }
    }
    params
}
