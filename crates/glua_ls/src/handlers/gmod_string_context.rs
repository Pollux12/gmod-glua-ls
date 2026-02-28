use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaLiteralExpr, LuaStringToken,
    PathTrait,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetMessageCallKind {
    Start,
    Receive,
}

#[derive(Clone, Debug)]
pub(crate) struct StringCallContext {
    pub(crate) call_path: String,
    pub(crate) arg_index: usize,
    pub(crate) name: String,
}

pub(crate) fn extract_string_call_context(string_token: &LuaStringToken) -> Option<StringCallContext> {
    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let call_arg_list = literal_expr.get_parent::<LuaCallArgList>()?;
    let arg_index = call_arg_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position())?;
    let call_expr = call_arg_list.get_parent::<LuaCallExpr>()?;

    Some(StringCallContext {
        call_path: call_expr.get_access_path()?,
        arg_index,
        name: normalize_string_name(string_token.get_value())?,
    })
}

pub(crate) fn is_vgui_panel_string_context(call_path: &str, arg_index: usize) -> bool {
    if matches_call_path(call_path, "vgui.Create") {
        return arg_index == 0;
    }

    if matches_call_path(call_path, "vgui.Register") {
        return arg_index == 0 || arg_index == 2;
    }

    if matches_call_path(call_path, "derma.DefineControl") {
        return arg_index == 0 || arg_index == 3;
    }

    // `:Add` is broadly matched by method name only (no receiver type check).
    // False positives are mitigated by the subsequent VGUI index lookup finding no match.
    matches_call_path(call_path, "Add") && arg_index == 0
}

pub(crate) fn net_message_call_kind(call_path: &str, arg_index: usize) -> Option<NetMessageCallKind> {
    if arg_index != 0 {
        return None;
    }

    if matches_call_path(call_path, "net.Start") {
        return Some(NetMessageCallKind::Start);
    }

    if matches_call_path(call_path, "net.Receive") {
        return Some(NetMessageCallKind::Receive);
    }

    None
}

pub(crate) fn normalize_string_name(name: String) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn matches_call_path(path: &str, target: &str) -> bool {
    if path == target {
        return true;
    }
    if path.len() > target.len() {
        let sep_idx = path.len() - target.len() - 1;
        let sep = path.as_bytes()[sep_idx];
        if (sep == b'.' || sep == b':') && path[sep_idx + 1..] == *target {
            return true;
        }
    }
    false
}
