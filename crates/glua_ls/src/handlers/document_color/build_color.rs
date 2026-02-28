use glua_code_analysis::LuaDocument;
use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaExpr, LuaLiteralToken, LuaSyntaxNode, LuaSyntaxToken, LuaTokenKind,
    NumberResult,
};
use lsp_types::{Color, ColorInformation};
use rowan::{TextRange, TextSize};

pub fn build_colors(root: LuaSyntaxNode, document: &LuaDocument) -> Vec<ColorInformation> {
    let mut result = vec![];

    // Scan for hex colors embedded in string literals.
    let string_tokens = root
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| {
            it.kind() == LuaTokenKind::TkString.into()
                || it.kind() == LuaTokenKind::TkLongString.into()
        });

    for token in string_tokens {
        try_build_color_information(token, document, &mut result);
    }

    // Scan for GMod Color(r, g, b[, a]) constructor calls.
    for call_expr in root.descendants().filter_map(LuaCallExpr::cast) {
        try_build_gmod_color_call(call_expr, document, &mut result);
    }

    result
}

/// Detects `Color(r, g, b)` or `Color(r, g, b, a)` calls where every argument is a
/// numeric integer literal in the 0–255 range and registers a color swatch for them.
fn try_build_gmod_color_call(
    call_expr: LuaCallExpr,
    document: &LuaDocument,
    result: &mut Vec<ColorInformation>,
) -> Option<()> {
    // Prefix must be a bare name expression "Color".
    let prefix = call_expr.get_prefix_expr()?;
    let LuaExpr::NameExpr(name_expr) = &prefix else {
        return None;
    };
    let name_token = name_expr.get_name_token()?;
    if name_token.get_name_text() != "Color" {
        return None;
    }

    let args_list = call_expr.get_args_list()?;
    let args: Vec<_> = args_list.get_args().collect();

    if args.len() < 3 || args.len() > 4 {
        return None;
    }

    let mut components = [0.0f32; 4];
    components[3] = 1.0; // default alpha = 255

    for (i, arg) in args.iter().enumerate() {
        let LuaExpr::LiteralExpr(lit_expr) = arg else {
            return None;
        };
        let LuaLiteralToken::Number(num_token) = lit_expr.get_literal()? else {
            return None;
        };
        let value: f64 = match num_token.get_number_value() {
            NumberResult::Int(n) => n as f64,
            NumberResult::Uint(n) => n as f64,
            NumberResult::Float(n) => n,
        };
        if !(0.0..=255.0).contains(&value) {
            return None;
        }
        components[i] = (value / 255.0) as f32;
    }

    let range = document.to_lsp_range(call_expr.syntax().text_range())?;
    result.push(ColorInformation {
        range,
        color: Color {
            red: components[0],
            green: components[1],
            blue: components[2],
            alpha: components[3],
        },
    });

    Some(())
}

fn try_build_color_information(
    token: LuaSyntaxToken,
    document: &LuaDocument,
    result: &mut Vec<ColorInformation>,
) -> Option<()> {
    let text = token.text();
    let bytes = text.as_bytes();
    let len = bytes.len();

    let mut i = 0;
    while i + 6 <= len {
        if bytes[i].is_ascii_hexdigit() {
            let is_start_boundary = if i == 0 {
                true
            } else {
                !bytes[i - 1].is_ascii_alphanumeric()
            };
            if !is_start_boundary {
                i += 1;
                continue;
            }

            let mut j = i + 1;
            while j < len && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }

            if j - i == 6 || j - i == 8 {
                let color_text = &text[i..j];
                if let Some(color) = parse_hex_color(color_text) {
                    let source_text_range = token.text_range();
                    let start = if bytes[i - 1] == b'#' { i - 1 } else { i };
                    let text_range = TextRange::new(
                        source_text_range.start() + TextSize::new(start as u32),
                        source_text_range.start() + TextSize::new(j as u32),
                    );
                    let lsp_range = document.to_lsp_range(text_range)?;

                    result.push(ColorInformation {
                        range: lsp_range,
                        color,
                    });
                }
            }

            i = j;
        } else {
            i += 1;
        }
    }

    Some(())
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    match hex.len() {
        6 => {
            // RGB格式
            let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f32 / 255.0;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f32 / 255.0;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f32 / 255.0;
            Some(Color {
                red: r,
                green: g,
                blue: b,
                alpha: 1.0,
            })
        }
        8 => {
            // RGBA格式
            let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f32 / 255.0;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f32 / 255.0;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f32 / 255.0;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()? as f32 / 255.0;
            Some(Color {
                red: r,
                green: g,
                blue: b,
                alpha: a,
            })
        }
        _ => None, // 不匹配的长度
    }
}

pub fn convert_color_to_hex(color: Color, len: usize) -> String {
    let r = (color.red * 255.0).round() as u8;
    let g = (color.green * 255.0).round() as u8;
    let b = (color.blue * 255.0).round() as u8;
    match len {
        6 => format!("{:02X}{:02X}{:02X}", r, g, b),
        7 => format!("#{:02X}{:02X}{:02X}", r, g, b),
        8 => {
            let a = (color.alpha * 255.0).round() as u8;
            format!("{:02X}{:02X}{:02X}{:02X}", r, g, b, a)
        }
        9 => {
            let a = (color.alpha * 255.0).round() as u8;
            format!("#{:02X}{:02X}{:02X}{:02X}", r, g, b, a)
        }
        _ => "".to_string(),
    }
}
