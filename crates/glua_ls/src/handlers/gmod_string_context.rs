use glua_code_analysis::{
    LuaCallArgRole, LuaDeclId, LuaMemberId, LuaSemanticDeclId, LuaSignatureId, LuaTypeOwner,
    SemanticDeclLevel, SemanticModel, find_call_arg_role_from_type,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaClosureExpr, LuaLiteralExpr,
    LuaStringToken, PathTrait,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetMessageCallKind {
    Start,
    Receive,
}

#[derive(Clone, Debug)]
pub(crate) struct StringCallContext {
    pub(crate) arg_index: usize,
    pub(crate) name: String,
}

pub(crate) fn extract_string_call_context(
    string_token: &LuaStringToken,
) -> Option<StringCallContext> {
    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let call_arg_list = literal_expr.get_parent::<LuaCallArgList>()?;
    let arg_index = call_arg_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position())?;
    call_arg_list.get_parent::<LuaCallExpr>()?;

    Some(StringCallContext {
        arg_index,
        name: normalize_string_name(string_token.get_value())?,
    })
}

pub(crate) fn is_annotated_vgui_panel_string_context(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> bool {
    has_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.vgui_panel",
        &["define", "define_control", "base", "reference"],
    )
}

pub(crate) fn is_annotated_derma_skin_string_context(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> bool {
    has_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.derma_skin",
        &["define", "reference"],
    )
}

pub(crate) fn find_string_call_arg_role(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let adjusted_arg_index = adjusted_arg_index(call_expr, arg_index);
    if let Some(semantic_decl) = semantic_model.find_decl(
        prefix_expr.syntax().clone().into(),
        SemanticDeclLevel::NoTrace,
    ) && let Some(role) = find_call_arg_role_from_semantic_decl(
        semantic_model,
        semantic_decl,
        adjusted_arg_index,
        domain,
        roles,
    ) {
        return Some(role);
    }

    let callable_type = semantic_model.infer_expr(prefix_expr).ok()?;
    find_call_arg_role_from_type(
        semantic_model.get_db(),
        &callable_type,
        adjusted_arg_index,
        domain,
        roles,
    )
}

pub(crate) fn find_call_arg_roles(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_count: usize,
    domain: &str,
    roles: &[&str],
) -> Vec<(usize, LuaCallArgRole)> {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return Vec::new();
    };

    if let Some(semantic_decl) = semantic_model.find_decl(
        prefix_expr.syntax().clone().into(),
        SemanticDeclLevel::NoTrace,
    ) {
        return (0..arg_count)
            .filter_map(|arg_index| {
                find_call_arg_role_from_semantic_decl(
                    semantic_model,
                    semantic_decl.clone(),
                    adjusted_arg_index(call_expr, arg_index),
                    domain,
                    roles,
                )
                .map(|role| (arg_index, role))
            })
            .collect();
    }

    let Some(callable_type) = semantic_model.infer_expr(prefix_expr).ok() else {
        return Vec::new();
    };

    (0..arg_count)
        .filter_map(|arg_index| {
            find_call_arg_role_from_type(
                semantic_model.get_db(),
                &callable_type,
                adjusted_arg_index(call_expr, arg_index),
                domain,
                roles,
            )
            .map(|role| (arg_index, role))
        })
        .collect()
}

pub(crate) fn has_string_call_arg_role(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
    domain: &str,
    roles: &[&str],
) -> bool {
    find_string_call_arg_role(semantic_model, call_expr, arg_index, domain, roles).is_some()
}

fn adjusted_arg_index(call_expr: &LuaCallExpr, arg_index: usize) -> usize {
    if call_expr.is_colon_call() {
        arg_index + 1
    } else {
        arg_index
    }
}

pub(crate) fn annotated_net_message_flow_call_kind(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> Option<NetMessageCallKind> {
    annotated_net_message_call_kind(semantic_model, call_expr, arg_index)
}

pub(crate) fn is_net_message_string_context(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> bool {
    has_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.net_message",
        &["define", "start", "receive", "reference"],
    )
}

pub(crate) fn is_hook_name_string_context(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> bool {
    if has_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.hook",
        &["add", "emit", "remove", "reference"],
    ) {
        return true;
    }

    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };
    arg_index == 0
        && semantic_model
            .get_emmyrc()
            .gmod
            .hook_mappings
            .emitter_to_hook
            .iter()
            .any(|(emitter_path, mapped_hook)| mapped_hook == "*" && call_path == *emitter_path)
}

fn find_call_arg_role_from_semantic_decl(
    semantic_model: &SemanticModel,
    semantic_decl: LuaSemanticDeclId,
    arg_index: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    match semantic_decl {
        LuaSemanticDeclId::Signature(signature_id) => find_call_arg_role_from_signature_id(
            semantic_model,
            signature_id,
            arg_index,
            domain,
            roles,
        ),
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            find_call_arg_role_from_decl_id(semantic_model, decl_id, arg_index, domain, roles)
        }
        LuaSemanticDeclId::Member(member_id) => {
            find_call_arg_role_from_member_id(semantic_model, member_id, arg_index, domain, roles)
        }
        LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn find_call_arg_role_from_signature_id(
    semantic_model: &SemanticModel,
    signature_id: LuaSignatureId,
    arg_index: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    let signature = semantic_model
        .get_db()
        .get_signature_index()
        .get(&signature_id)?;
    let mut best = None;
    signature.visit_call_arg_roles_for_param(arg_index, &mut |role| {
        if role.domain != domain || !roles.iter().any(|candidate| *candidate == role.role) {
            return;
        }

        if best.as_ref().is_none_or(|current: &LuaCallArgRole| {
            role.priority.unwrap_or(0) > current.priority.unwrap_or(0)
        }) {
            best = Some(role.clone());
        }
    });
    best
}

fn find_call_arg_role_from_decl_id(
    semantic_model: &SemanticModel,
    decl_id: LuaDeclId,
    arg_index: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    if let Some(signature_id) = signature_id_from_decl_value(semantic_model, decl_id)
        && let Some(role) = find_call_arg_role_from_signature_id(
            semantic_model,
            signature_id,
            arg_index,
            domain,
            roles,
        )
    {
        return Some(role);
    }

    let typ = semantic_model.get_type(decl_id.into());
    find_call_arg_role_from_type(semantic_model.get_db(), &typ, arg_index, domain, roles)
}

fn find_call_arg_role_from_member_id(
    semantic_model: &SemanticModel,
    member_id: LuaMemberId,
    arg_index: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    let typ = semantic_model.get_type(LuaTypeOwner::Member(member_id));
    find_call_arg_role_from_type(semantic_model.get_db(), &typ, arg_index, domain, roles)
}

fn signature_id_from_decl_value(
    semantic_model: &SemanticModel,
    decl_id: LuaDeclId,
) -> Option<LuaSignatureId> {
    let decl = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = semantic_model
        .get_db()
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let value_node = value_syntax_id.to_node_from_root(&root)?;
    let closure = LuaClosureExpr::cast(value_node)?;
    Some(LuaSignatureId::from_closure(decl_id.file_id, &closure))
}

pub(crate) fn annotated_net_message_call_kind(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    arg_index: usize,
) -> Option<NetMessageCallKind> {
    let role = find_string_call_arg_role(
        semantic_model,
        call_expr,
        arg_index,
        "gmod.net_message",
        &["start", "receive"],
    )?;
    match role.role.as_str() {
        "start" => Some(NetMessageCallKind::Start),
        "receive" => Some(NetMessageCallKind::Receive),
        _ => None,
    }
}

pub(crate) fn normalize_string_name(name: String) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
