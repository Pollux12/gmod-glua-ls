use glua_code_analysis::{DbIndex, LuaType, RenderLevel, humanize_type};

pub fn render_typ(db: &DbIndex, typ: &LuaType, level: RenderLevel) -> String {
    match typ {
        LuaType::IntegerConst(_) => "integer".to_string(),
        LuaType::FloatConst(_) => "number".to_string(),
        LuaType::StringConst(_) => "string".to_string(),
        LuaType::BooleanConst(_) => "boolean".to_string(),
        _ => humanize_type(db, typ, level),
    }
}

pub fn render_const(typ: &LuaType) -> Option<String> {
    match typ {
        LuaType::IntegerConst(i) | LuaType::DocIntegerConst(i) => Some(i.to_string()),
        LuaType::FloatConst(f) => Some(f.to_string()),
        LuaType::StringConst(s) | LuaType::DocStringConst(s) => {
            Some(format!("{:?}", s.to_string()))
        }
        LuaType::BooleanConst(b) => Some(b.to_string()),
        _ => None,
    }
}
