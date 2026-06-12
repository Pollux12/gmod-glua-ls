use glua_code_analysis::{
    LuaDeclId, LuaMemberId, LuaMemberOwner, LuaType, LuaTypeDeclId, SemanticModel,
    resolve_alias_type,
};
use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaFuncStat,
    LuaLiteralToken, LuaLocalFuncStat, LuaLocalStat, LuaVarExpr, PathTrait,
};
use lsp_types::{CodeLens, Command, Range};
use rowan::NodeOrToken;

use super::{CodeLensData, CodeLensResolveData};
use crate::handlers::gmod_string_context::find_call_arg_roles;

fn is_top_level_stat(syntax: &glua_parser::LuaSyntaxNode) -> bool {
    syntax
        .parent()
        .and_then(|block| block.parent())
        .is_some_and(|grandparent| glua_parser::LuaChunk::can_cast(grandparent.kind().into()))
}

pub fn build_code_lens(semantic_model: &SemanticModel) -> Option<Vec<CodeLens>> {
    let mut result = Vec::new();
    let enable_vgui_code_lens = semantic_model.get_emmyrc().gmod.vgui.code_lens_enabled;
    let enable_net_code_lens = semantic_model.get_emmyrc().gmod.enabled
        && semantic_model.get_emmyrc().gmod.network.enabled
        && semantic_model.get_emmyrc().gmod.network.code_lens_enabled;
    let root = semantic_model.get_root().clone();
    for node in root.descendants::<LuaAst>() {
        match node {
            LuaAst::LuaFuncStat(func_stat) => {
                add_func_stat_code_lens(
                    semantic_model,
                    &mut result,
                    func_stat,
                    enable_vgui_code_lens,
                )?;
            }
            LuaAst::LuaLocalFuncStat(local_func_stat) => {
                add_local_func_stat_code_lens(
                    semantic_model,
                    &mut result,
                    local_func_stat,
                    enable_vgui_code_lens,
                )?;
            }
            LuaAst::LuaLocalStat(local_stat) => {
                if enable_vgui_code_lens {
                    add_local_stat_code_lens(semantic_model, &mut result, local_stat)?;
                }
            }
            LuaAst::LuaAssignStat(assign_stat) => {
                if enable_vgui_code_lens {
                    add_assign_stat_code_lens(semantic_model, &mut result, assign_stat)?;
                }
            }
            LuaAst::LuaCallExpr(call_expr) => {
                if enable_net_code_lens {
                    add_net_call_code_lens(semantic_model, &mut result, call_expr);
                }
            }
            _ => {}
        }
    }

    Some(result)
}

fn add_func_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    func_stat: LuaFuncStat,
    enable_vgui_code_lens: bool,
) -> Option<()> {
    let file_id = semantic_model.get_file_id();
    let func_name = func_stat.get_func_name()?;
    let document = semantic_model.get_document();
    match func_name {
        LuaVarExpr::IndexExpr(index_expr) => {
            let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
            let data = CodeLensResolveData {
                uri: Some(document.get_uri().clone()),
                payload: CodeLensData::Member(member_id),
            };
            let index_name_token = index_expr.get_index_name_token()?;
            let range = document.to_lsp_range(index_name_token.text_range())?;
            result.push(CodeLens {
                range: range.clone(),
                command: None,
                data: Some(serde_json::to_value(data).unwrap()),
            });

            if enable_vgui_code_lens
                && let Some(owner) = semantic_model
                    .get_db()
                    .get_member_index()
                    .get_member_owner(&member_id)
                && let Some(info) = find_gmod_class_from_member_owner(semantic_model, owner)
            {
                push_gmod_class_code_lens(result, range, &info);
            }
        }
        LuaVarExpr::NameExpr(name_expr) => {
            let name_token = name_expr.get_name_token()?;
            let decl_id = semantic_model
                .get_db()
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
                .unwrap_or_else(|| LuaDeclId::new(file_id, name_token.get_position()));
            let data = CodeLensResolveData {
                uri: Some(document.get_uri().clone()),
                payload: CodeLensData::DeclId(decl_id),
            };
            let range = document.to_lsp_range(name_token.get_range())?;
            result.push(CodeLens {
                range: range.clone(),
                command: None,
                data: Some(serde_json::to_value(data).unwrap()),
            });

            if enable_vgui_code_lens
                && let Some(semantic_info) =
                    semantic_model.get_semantic_info(NodeOrToken::Node(name_expr.syntax().clone()))
                && let Some(info) = find_gmod_class_from_type(semantic_model, &semantic_info.typ)
            {
                push_gmod_class_code_lens(result, range, &info);
            }
        }
    }

    Some(())
}

fn add_local_func_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    local_func_stat: LuaLocalFuncStat,
    enable_vgui_code_lens: bool,
) -> Option<()> {
    let file_id = semantic_model.get_file_id();
    let func_name = local_func_stat.get_local_name()?;
    let document = semantic_model.get_document();
    let range = document.to_lsp_range(func_name.get_range())?;
    let name_token = func_name.get_name_token()?;
    let decl_id = LuaDeclId::new(file_id, name_token.get_position());
    let data = CodeLensResolveData {
        uri: Some(document.get_uri().clone()),
        payload: CodeLensData::DeclId(decl_id),
    };
    result.push(CodeLens {
        range: range.clone(),
        command: None,
        data: Some(serde_json::to_value(data).unwrap()),
    });

    if enable_vgui_code_lens
        && let Some(semantic_info) =
            semantic_model.get_semantic_info(NodeOrToken::Node(func_name.syntax().clone()))
        && let Some(info) = find_gmod_class_from_type(semantic_model, &semantic_info.typ)
    {
        push_gmod_class_code_lens(result, range, &info);
    }

    Some(())
}

fn add_local_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    local_stat: LuaLocalStat,
) -> Option<()> {
    // Only show VGUI code lens for top-level local statements
    if !is_top_level_stat(local_stat.syntax()) {
        return Some(());
    }

    let document = semantic_model.get_document();

    for local_name in local_stat.get_local_name_list() {
        let Some(name_token) = local_name.get_name_token() else {
            continue;
        };

        let Some(semantic_info) =
            semantic_model.get_semantic_info(NodeOrToken::Node(local_name.syntax().clone()))
        else {
            continue;
        };
        let Some(info) = find_gmod_class_from_type(semantic_model, &semantic_info.typ) else {
            continue;
        };

        let range = document.to_lsp_range(name_token.get_range())?;
        push_gmod_class_code_lens(result, range, &info);
    }

    Some(())
}

fn add_assign_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    assign_stat: LuaAssignStat,
) -> Option<()> {
    // Only show VGUI code lens for top-level assignments (not inside function bodies)
    if !is_top_level_stat(assign_stat.syntax()) {
        return Some(());
    }

    let document = semantic_model.get_document();
    let (vars, exprs) = assign_stat.get_var_and_expr_list();

    for (i, var) in vars.into_iter().enumerate() {
        let Some(expr) = exprs.get(i) else {
            continue;
        };

        let Ok(expr_type) = semantic_model.infer_expr(expr.clone()) else {
            continue;
        };
        let Some(info) = find_gmod_class_from_type(semantic_model, &expr_type) else {
            continue;
        };

        let range = document.to_lsp_range(var.get_range())?;
        push_gmod_class_code_lens(result, range, &info);
    }

    Some(())
}

struct GmodClassInfo {
    class_name: String,
    base_name: Option<String>,
}

fn find_gmod_class_from_member_owner(
    semantic_model: &SemanticModel,
    owner: &LuaMemberOwner,
) -> Option<GmodClassInfo> {
    match owner {
        LuaMemberOwner::Type(type_id) => find_gmod_class_from_type_id(semantic_model, type_id),
        _ => None,
    }
}

fn find_gmod_class_from_type(
    semantic_model: &SemanticModel,
    typ: &LuaType,
) -> Option<GmodClassInfo> {
    let resolved = resolve_alias_type(semantic_model.get_db(), typ);
    match &resolved.typ {
        LuaType::Def(type_id) | LuaType::Ref(type_id) => {
            find_gmod_class_from_type_id(semantic_model, type_id)
        }
        _ => None,
    }
}

fn find_gmod_class_from_type_id(
    semantic_model: &SemanticModel,
    type_id: &LuaTypeDeclId,
) -> Option<GmodClassInfo> {
    let type_name = type_id.get_simple_name().to_string();

    // Check VGUI panels first
    if let Some(base_name) = semantic_model
        .get_db()
        .get_gmod_class_metadata_index()
        .get_vgui_panel_base(&type_name)
    {
        return Some(GmodClassInfo {
            class_name: type_name,
            base_name,
        });
    }

    // Check scripted entity supers (ENT, SWEP, EFFECT, TOOL, PLUGIN, GM/GAMEMODE)
    let supers = semantic_model
        .get_db()
        .get_type_index()
        .get_super_types(type_id)?;

    for super_type in supers {
        let resolved_super = resolve_alias_type(semantic_model.get_db(), &super_type);
        let super_name = match &resolved_super.typ {
            LuaType::Def(id) | LuaType::Ref(id) => id.get_simple_name(),
            _ => continue,
        };

        match super_name {
            "Entity" | "Weapon" | "CEffect" | "Tool" | "Plugin" | "Gamemode" => {
                return Some(GmodClassInfo {
                    class_name: type_name,
                    base_name: Some(super_name.to_string()),
                });
            }
            _ => continue,
        }
    }

    None
}

fn push_gmod_class_code_lens(result: &mut Vec<CodeLens>, range: Range, info: &GmodClassInfo) {
    let title = match &info.base_name {
        Some(base) => format!("{} : {}", info.class_name, base),
        None => info.class_name.clone(),
    };

    result.push(CodeLens {
        range,
        command: Some(Command {
            title,
            command: String::new(),
            arguments: None,
        }),
        data: None,
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetCodeLensCallKind {
    Define,
    Start,
    Receive,
}

/// Adds CodeLenses above net message call sites that mirror the VGUI/entity
/// lens style: a lazy-resolved "N usage(s)" lens plus a `Name : Kind` label.
/// The annotated message argument decides whether a call defines, starts, or
/// receives a net message. Builtins keep their familiar labels through their
/// annotation metadata; wrappers fall back to the wrapper's call path label.
fn add_net_call_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;
    let (kind, message_arg_idx) = net_code_lens_call_kind(semantic_model, &call_expr)?;
    let string_token = string_arg_at(&call_expr, message_arg_idx)?;
    let raw_name = string_token.get_value();
    let message_name = raw_name.trim();
    if message_name.is_empty() {
        return Some(());
    }
    let kind_label = match kind {
        NetCodeLensCallKind::Define => call_path.clone(),
        NetCodeLensCallKind::Start => {
            resolve_start_kind_label(semantic_model, &call_expr, &call_path, message_arg_idx)
        }
        NetCodeLensCallKind::Receive => {
            resolve_receive_kind_label(semantic_model, &call_expr, &call_path)
        }
    };

    let document = semantic_model.get_document();
    let range = document.to_lsp_range(call_expr.get_range())?;

    let usage_data = CodeLensResolveData {
        uri: Some(document.get_uri().clone()),
        payload: CodeLensData::NetMessage(message_name.to_string()),
    };
    result.push(CodeLens {
        range,
        command: None,
        data: Some(serde_json::to_value(usage_data).unwrap()),
    });

    result.push(CodeLens {
        range,
        command: Some(Command {
            title: format!("{message_name} : {kind_label}"),
            command: String::new(),
            arguments: None,
        }),
        data: None,
    });

    Some(())
}

fn net_code_lens_call_kind(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<(NetCodeLensCallKind, usize)> {
    let args_list = call_expr.get_args_list()?;
    let args: Vec<LuaExpr> = args_list.get_args().collect();
    if !args
        .iter()
        .any(|arg| matches!(arg, LuaExpr::LiteralExpr(literal_expr) if matches!(literal_expr.get_literal(), Some(LuaLiteralToken::String(_)))))
    {
        return None;
    }

    let arg_count = args.len();
    for (arg_idx, role) in find_call_arg_roles(
        semantic_model,
        call_expr,
        arg_count,
        "gmod.net_message",
        &["define", "start", "receive"],
    ) {
        let kind = match role.role.as_str() {
            "define" => NetCodeLensCallKind::Define,
            "start" => NetCodeLensCallKind::Start,
            "receive" => NetCodeLensCallKind::Receive,
            _ => continue,
        };
        return Some((kind, arg_idx));
    }
    None
}

fn string_arg_at(call_expr: &LuaCallExpr, arg_idx: usize) -> Option<glua_parser::LuaStringToken> {
    let args_list = call_expr.get_args_list()?;
    let arg = args_list.get_args().nth(arg_idx)?;
    let LuaExpr::LiteralExpr(literal_expr) = arg else {
        return None;
    };
    let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
        return None;
    };
    Some(string_token)
}

/// Picks the label for a `net.Start` lens. Looks up the indexed send flow
/// originating at this call site and uses the actual transport call name
/// (e.g. `net.SendToServer`) so the lens reflects the realm/recipients
/// instead of the generic `net.Start`. Falls back to `net.Start` when the
/// flow could not be resolved (wrapped helpers, conservative stubs, message
/// not yet indexed, etc).
fn resolve_start_kind_label(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    fallback: &str,
    message_arg_idx: usize,
) -> String {
    let fallback = fallback.to_string();
    let Some(string_token) = string_arg_at(call_expr, message_arg_idx) else {
        return fallback;
    };
    let message_name = string_token.get_value();
    let message_name = message_name.trim();
    if message_name.is_empty() {
        return fallback;
    }

    let file_id = semantic_model.get_file_id();
    let call_range = call_expr.get_range();
    let flows = semantic_model
        .get_db()
        .get_gmod_network_index()
        .get_send_flows_for_message(message_name);

    let matching = flows
        .iter()
        .find(|(flow_file_id, flow)| {
            *flow_file_id == file_id && flow.start_range == call_range && !flow.is_wrapped
        })
        .map(|(_, flow)| (flow.send_kind, flow.send_target.clone()));

    match matching {
        Some((kind, Some(target))) => format!("{}({target})", kind.to_fn_name()),
        Some((kind, None)) => kind.to_fn_name().to_string(),
        None => fallback,
    }
}

/// Picks the label for a `net.Receive` lens. Looks up the indexed receive
/// flow at this call site, then aggregates the distinct send kinds of every
/// realistic counterpart sender (opposite realm, send-kind targets this
/// receiver's realm, read/write patterns line up). The same `net.MessageName`
/// can be used with multiple distinct read/write patterns across the
/// codebase, so naive aggregation by message name alone would mix unrelated
/// flows; pattern-based pairing keeps the label faithful to *this* receive's
/// actual senders. Falls back to `net.Receive` when no candidate matches
/// (e.g. counterpart not yet indexed, opaque callback, ambiguous realm).
fn resolve_receive_kind_label(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    fallback: &str,
) -> String {
    let file_id = semantic_model.get_file_id();
    let call_range = call_expr.get_range();
    let db = semantic_model.get_db();
    let network_index = db.get_gmod_network_index();
    let infer_index = db.get_gmod_infer_index();

    let Some(file_data) = network_index.get_file_data(file_id) else {
        return fallback.to_string();
    };
    let Some(receive_flow) = file_data
        .receive_flows
        .iter()
        .find(|flow| flow.receive_range == call_range)
    else {
        return fallback.to_string();
    };

    let paired = glua_code_analysis::pair_senders_for_receive(
        network_index,
        infer_index,
        file_id,
        receive_flow,
    );

    let mut kinds: Vec<&'static str> = Vec::new();
    for (_, flow) in &paired {
        let name = flow.send_kind.to_fn_name();
        if !kinds.contains(&name) {
            kinds.push(name);
        }
    }

    if kinds.is_empty() {
        fallback.to_string()
    } else {
        kinds.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use glua_code_analysis::VirtualWorkspace;
    use glua_parser::{LuaAssignStat, LuaAst, LuaAstNode, LuaLocalName};
    use googletest::prelude::*;

    use super::build_code_lens;
    use crate::handlers::code_lens::{CodeLensData, CodeLensResolveData};

    #[gtest]
    fn function_code_lens_uses_forward_local_decl_id() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
                local create_initial_simplex4

                function create_initial_simplex4(points, thread_yield)
                    return { points, thread_yield }
                end

                local faces = create_initial_simplex4({}, nil)
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let lenses = build_code_lens(&semantic_model).expect("expected code lenses");
        let data = lenses
            .iter()
            .filter_map(|lens| lens.data.as_ref())
            .find_map(|value| serde_json::from_value::<CodeLensResolveData>(value.clone()).ok())
            .expect("expected function code lens data");
        let local_name = ws.get_node::<LuaLocalName>(file_id);
        let expected_decl = glua_code_analysis::LuaDeclId::new(file_id, local_name.get_position());

        let CodeLensData::DeclId(actual_decl) = data.payload else {
            panic!("expected declaration code lens");
        };
        assert_that!(actual_decl, eq(expected_decl));
    }

    #[gtest]
    fn vgui_reassigned_panel_code_lens_labels_resolve_per_region() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        let mut emmyrc = glua_code_analysis::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.vgui.code_lens_enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
                local PANEL = {}
                function PANEL:Init() end
                vgui.Register("ReFrame", PANEL, "DFrame")

                PANEL = {}
                function PANEL:Paint() end
                vgui.Register("ReButton", PANEL, "DButton")

                PANEL = {}
                function PANEL:Think() end
                vgui.Register("ReTree", PANEL, "DTree")
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let lenses = build_code_lens(&semantic_model).expect("expected code lenses");

        let titles: Vec<String> = lenses
            .iter()
            .filter_map(|l| l.command.as_ref().map(|c| c.title.clone()))
            .collect();

        assert_that!(titles, contains(eq("ReFrame : DFrame")));
        assert_that!(titles, contains(eq("ReButton : DButton")));
        assert_that!(titles, contains(eq("ReTree : DTree")));
    }

    #[gtest]
    fn vgui_reassigned_panel_assignment_code_lens_resolves_per_region() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        let mut emmyrc = glua_code_analysis::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.vgui.code_lens_enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
                local PANEL = {}
                function PANEL:Init() end
                vgui.Register("ReFrame", PANEL, "DFrame")

                PANEL = {}
                function PANEL:Paint() end
                vgui.Register("ReButton", PANEL, "DButton")

                PANEL = {}
                function PANEL:Think() end
                vgui.Register("ReTree", PANEL, "DTree")
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let document = semantic_model.get_document();
        let lenses = build_code_lens(&semantic_model).expect("expected code lenses");

        let assign_ranges: Vec<_> = semantic_model
            .get_root()
            .clone()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaAssignStat(assign_stat) => Some(assign_stat),
                _ => None,
            })
            .map(|assign_stat: LuaAssignStat| {
                let (vars, _) = assign_stat.get_var_and_expr_list();
                document
                    .to_lsp_range(vars[0].get_range())
                    .expect("assignment var range")
            })
            .collect();

        assert_that!(assign_ranges.len(), eq(2usize));

        let assignment_titles: Vec<_> = assign_ranges
            .iter()
            .map(|range| {
                lenses
                    .iter()
                    .find(|lens| lens.range == *range)
                    .and_then(|lens| lens.command.as_ref())
                    .map(|command| command.title.clone())
                    .expect("expected class CodeLens on assignment")
            })
            .collect();

        assert_that!(assignment_titles[0].as_str(), eq("ReButton : DButton"));
        assert_that!(assignment_titles[1].as_str(), eq("ReTree : DTree"));
    }

    #[gtest]
    fn net_code_lens_uses_annotated_message_argument_roles() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = glua_code_analysis::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.network.enabled = true;
        emmyrc.gmod.network.code_lens_enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
                ---@attribute call_arg(domain: string, role: string, priority: integer?)

                ---@param realm string
                ---@[call_arg("gmod.net_message", "define")]
                ---@param name string
                function RegisterScopedNet(realm, name) end

                ---@param realm string
                ---@[call_arg("gmod.net_message", "start")]
                ---@param name string
                function StartScopedNet(realm, name) end

                ---@param realm string
                ---@[call_arg("gmod.net_message", "receive")]
                ---@param name string
                ---@param callback fun()
                function ReceiveScopedNet(realm, name, callback) end

                RegisterScopedNet("shared", "WrappedMessage")
                StartScopedNet("shared", "WrappedMessage")
                ReceiveScopedNet("shared", "WrappedMessage", function() end)
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let lenses = build_code_lens(&semantic_model).expect("expected code lenses");

        let titles: Vec<String> = lenses
            .iter()
            .filter_map(|lens| lens.command.as_ref().map(|command| command.title.clone()))
            .collect();
        assert_that!(titles, contains(eq("WrappedMessage : RegisterScopedNet")));
        assert_that!(titles, contains(eq("WrappedMessage : StartScopedNet")));
        assert_that!(titles, contains(eq("WrappedMessage : ReceiveScopedNet")));

        let net_message_lens_count = lenses
            .iter()
            .filter_map(|lens| lens.data.as_ref())
            .filter_map(|value| serde_json::from_value::<CodeLensResolveData>(value.clone()).ok())
            .filter(|data| matches!(data.payload, CodeLensData::NetMessage(_)))
            .count();
        assert_that!(net_message_lens_count, eq(3usize));
    }
}
