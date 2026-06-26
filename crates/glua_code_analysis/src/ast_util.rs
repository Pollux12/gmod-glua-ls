use glua_parser::{LuaCallExpr, LuaExpr, LuaLiteralToken};

pub(crate) fn literal_string_arg_value(call_expr: &LuaCallExpr, arg_idx: usize) -> Option<String> {
    let arg_expr = call_expr.get_args_list()?.get_args().nth(arg_idx)?;
    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return None;
    };

    let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
        return None;
    };

    Some(string_token.get_value())
}
