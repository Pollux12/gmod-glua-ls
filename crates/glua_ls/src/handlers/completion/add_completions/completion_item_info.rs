use glua_code_analysis::{LuaType, LuaUnionType};
use glua_parser::{LuaExpr, LuaLiteralToken, UnaryOperator};

use crate::handlers::{
    completion::completion_data::CompletionColorInfo,
    document_color::build_color::{
        GmodColor, gmod_color_from_call_expr, gmod_hex_color_from_hex_text,
    },
};

pub(super) fn color_info_from_type(typ: &LuaType) -> Option<CompletionColorInfo> {
    let text = match typ {
        LuaType::StringConst(text) | LuaType::DocStringConst(text) => text.as_str(),
        _ => return None,
    };

    // Avoid reclassifying arbitrary 6-character ids as colors. In completions,
    // hex string constants are treated as colors only when they use color-style syntax.
    if !text.starts_with('#') {
        return None;
    }

    let hex_color = gmod_hex_color_from_hex_text(text)?;
    Some(completion_color_info(hex_color.color, hex_color.has_alpha))
}

pub(super) fn color_info_from_expr(expr: &LuaExpr) -> Option<CompletionColorInfo> {
    match expr {
        LuaExpr::CallExpr(call_expr) => {
            let color = gmod_color_from_call_expr(call_expr)?;
            let alpha = (color.alpha * 255.0).round() as u8;
            Some(completion_color_info(color, alpha != u8::MAX))
        }
        LuaExpr::LiteralExpr(literal_expr) => {
            let LuaLiteralToken::String(token) = literal_expr.get_literal()? else {
                return None;
            };
            let text = token.get_value();
            if !text.starts_with('#') {
                return None;
            }
            let hex_color = gmod_hex_color_from_hex_text(&text)?;
            Some(completion_color_info(hex_color.color, hex_color.has_alpha))
        }
        _ => None,
    }
}

pub(super) fn scalar_literal_detail(typ: &LuaType) -> Option<String> {
    let value = match typ {
        LuaType::BooleanConst(value) | LuaType::DocBooleanConst(value) => value.to_string(),
        LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) => value.to_string(),
        LuaType::FloatConst(value) => value.to_string(),
        LuaType::StringConst(value) | LuaType::DocStringConst(value) => {
            format!("{:?}", value.as_str())
        }
        _ => return None,
    };

    Some(format!(" = {}", truncate_literal_value(&value)))
}

pub(super) fn scalar_literal_description(typ: &LuaType) -> Option<String> {
    match typ {
        LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_) => Some("boolean".to_string()),
        LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => Some("integer".to_string()),
        LuaType::FloatConst(_) => Some("number".to_string()),
        LuaType::StringConst(_) | LuaType::DocStringConst(_) => Some("string".to_string()),
        _ => None,
    }
}

pub(super) fn gmod_constructor_literal_detail(expr: &LuaExpr) -> Option<String> {
    let LuaExpr::CallExpr(call_expr) = expr else {
        return None;
    };

    let prefix = call_expr.get_prefix_expr()?;
    let LuaExpr::NameExpr(name_expr) = &prefix else {
        return None;
    };
    let name_token = name_expr.get_name_token()?;
    let constructor_name = name_token.get_name_text();
    if !is_gmod_literal_constructor_name(&constructor_name) {
        return None;
    }

    let args = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    if args.len() != 3 {
        return None;
    }

    let components = args
        .iter()
        .map(numeric_literal_text)
        .collect::<Option<Vec<_>>>()?;

    Some(format!(
        " = {}({})",
        constructor_name,
        components.join(", ")
    ))
}

pub(super) fn is_gmod_literal_constructor_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            is_gmod_literal_constructor_name(&id.get_simple_name())
        }
        LuaType::Instance(instance) => is_gmod_literal_constructor_type(instance.get_base()),
        LuaType::Union(union) => match union.as_ref() {
            LuaUnionType::Nullable(typ) => is_gmod_literal_constructor_type(typ),
            LuaUnionType::Multi(types) => types.iter().any(is_gmod_literal_constructor_type),
        },
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(is_gmod_literal_constructor_type),
        _ => false,
    }
}

pub(super) fn is_color_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => id.get_simple_name() == "Color",
        LuaType::Instance(instance) => is_color_type(instance.get_base()),
        LuaType::Union(union) => match union.as_ref() {
            LuaUnionType::Nullable(typ) => is_color_type(typ),
            LuaUnionType::Multi(types) => types.iter().any(is_color_type),
        },
        LuaType::Intersection(intersection) => intersection.get_types().iter().any(is_color_type),
        _ => false,
    }
}

fn is_gmod_literal_constructor_name(name: &str) -> bool {
    matches!(name, "Vector" | "Angle")
}

fn numeric_literal_text(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => {
            let LuaLiteralToken::Number(number) = literal_expr.get_literal()? else {
                return None;
            };
            Some(number.get_number_value().to_string())
        }
        LuaExpr::UnaryExpr(unary_expr)
            if unary_expr.get_op_token()?.get_op() == UnaryOperator::OpUnm =>
        {
            let inner = unary_expr.get_expr()?;
            Some(format!("-{}", numeric_literal_text(&inner)?))
        }
        _ => None,
    }
}

fn completion_color_info(color: GmodColor, include_alpha_in_hex: bool) -> CompletionColorInfo {
    let red = (color.red * 255.0).round() as u8;
    let green = (color.green * 255.0).round() as u8;
    let blue = (color.blue * 255.0).round() as u8;
    let alpha = (color.alpha * 255.0).round() as u8;
    let hex = if include_alpha_in_hex {
        format!("#{:02X}{:02X}{:02X}{:02X}", red, green, blue, alpha)
    } else {
        format!("#{:02X}{:02X}{:02X}", red, green, blue)
    };

    CompletionColorInfo {
        red,
        green,
        blue,
        alpha,
        rgb: format!("rgb({}, {}, {})", red, green, blue),
        rgba: format!("rgba({}, {}, {}, {})", red, green, blue, alpha),
        gmod: format!("Color({}, {}, {}, {})", red, green, blue, alpha),
        hex,
    }
}

fn truncate_literal_value(value: &str) -> String {
    const MAX_LITERAL_DETAIL_CHARS: usize = 80;
    if value.chars().count() <= MAX_LITERAL_DETAIL_CHARS {
        return value.to_string();
    }

    let mut truncated = value
        .chars()
        .take(MAX_LITERAL_DETAIL_CHARS.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}
