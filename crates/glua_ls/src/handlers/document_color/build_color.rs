use glua_code_analysis::{LuaDocument, SemanticModel};
use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaExpr, LuaLiteralToken, LuaSyntaxNode, LuaSyntaxToken, LuaTokenKind,
    NumberResult,
};
use lsp_types::{Color, ColorInformation};
use rowan::{TextRange, TextSize};

pub fn build_colors(
    root: LuaSyntaxNode,
    document: &LuaDocument,
    semantic_model: Option<&SemanticModel>,
) -> Vec<ColorInformation> {
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

    // Scan for GMod Color(r, g, b[, a]) constructor calls or tuple arguments.
    for call_expr in root.descendants().filter_map(LuaCallExpr::cast) {
        if try_build_gmod_color_call(call_expr.clone(), document, &mut result).is_some() {
            continue;
        }

        if let Some(semantic_model) = semantic_model {
            try_build_semantic_color_tuple(call_expr, document, semantic_model, &mut result);
        }
    }

    result
}

fn try_build_semantic_color_tuple(
    call_expr: LuaCallExpr,
    document: &LuaDocument,
    semantic_model: &SemanticModel,
    result: &mut Vec<ColorInformation>,
) -> Option<()> {
    let args_list = call_expr.get_args_list()?;
    let args: Vec<_> = args_list.get_args().collect();
    if args.len() < 3 {
        return None;
    }

    // Cheap pre-check: require at least 3 numeric literal args before the expensive inference call.
    let numeric_literal_count = args.iter().filter(|arg| {
        matches!(arg, LuaExpr::LiteralExpr(lit) if matches!(lit.get_literal(), Some(LuaLiteralToken::Number(_))))
    }).count();
    if numeric_literal_count < 3 {
        return None;
    }

    let func = semantic_model.infer_call_expr_func(call_expr.clone(), Some(args.len()))?;
    let params = func.get_params();

    let mut effective_params = Vec::new();
    for (name, _) in params {
        effective_params.push(name.clone());
    }

    match (func.is_colon_define(), call_expr.is_colon_call()) {
        (true, false) => {
            effective_params.insert(0, "self".to_string());
        }
        (false, true) => {
            if !effective_params.is_empty() {
                effective_params.remove(0);
            }
        }
        _ => {}
    }

    let mut tuple_start = None;
    let mut tuple_len = 0;

    for i in 0..effective_params.len() {
        let name = effective_params[i].to_lowercase();
        if name == "r" || name == "red" {
            if i + 2 < effective_params.len() {
                let g = effective_params[i + 1].to_lowercase();
                let b = effective_params[i + 2].to_lowercase();
                if (g == "g" || g == "green") && (b == "b" || b == "blue") {
                    tuple_start = Some(i);
                    tuple_len = 3;
                    if i + 3 < effective_params.len() {
                        let a = effective_params[i + 3].to_lowercase();
                        if a == "a" || a == "alpha" {
                            tuple_len = 4;
                        }
                    }
                    break;
                }
            }
        }
    }

    let start_idx = tuple_start?;
    if args.len() < start_idx + 3 {
        return None;
    }

    let supplied_len = (args.len() - start_idx).min(tuple_len);
    if supplied_len < 3 {
        return None;
    }

    let mut components = [0.0f32; 4];
    components[3] = 1.0;

    for i in 0..supplied_len {
        let arg = &args[start_idx + i];
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

    let first_arg = &args[start_idx];
    let last_arg = &args[start_idx + supplied_len - 1];

    let text_range = TextRange::new(
        first_arg.syntax().text_range().start(),
        last_arg.syntax().text_range().end(),
    );
    let range = document.to_lsp_range(text_range)?;

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

    // Use the range of the arguments only (not the whole call expr) so the swatch
    // appears inside the brackets, consistent with other color-tuple detections.
    let first_arg = args.first()?;
    let last_arg = args.last()?;
    let args_range = TextRange::new(
        first_arg.syntax().text_range().start(),
        last_arg.syntax().text_range().end(),
    );
    let range = document.to_lsp_range(args_range)?;
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

            let is_end_boundary = if j == len {
                true
            } else {
                !bytes[j].is_ascii_alphanumeric()
            };
            if !is_end_boundary {
                i = j;
                continue;
            }

            if j - i == 6 || j - i == 8 {
                let color_text = &text[i..j];
                if let Some(color) = parse_hex_color(color_text) {
                    let source_text_range = token.text_range();
                    let start = if i > 0 && bytes[i - 1] == b'#' {
                        i - 1
                    } else {
                        i
                    };
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use glua_code_analysis::{FileId, VirtualWorkspace};
    use glua_parser::{LineIndex, LuaAstNode, LuaParser, ParserConfig};
    use googletest::prelude::*;

    use super::build_colors;

    fn collect_colors(text: &str) -> Vec<lsp_types::ColorInformation> {
        let tree = LuaParser::parse(text, ParserConfig::default());
        let line_index = LineIndex::parse(text);
        let path = PathBuf::from("test.lua");
        let document =
            glua_code_analysis::LuaDocument::new(FileId::new(0), &path, text, &line_index);
        build_colors(tree.get_red_root(), &document, None)
    }

    fn collect_colors_semantic(text: &str) -> Vec<lsp_types::ColorInformation> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(text);
        let semantic_model = ws.analysis.compilation.get_semantic_model(file_id).unwrap();
        let document = semantic_model.get_document();
        let root = semantic_model.get_root();
        build_colors(root.syntax().clone(), &document, Some(&semantic_model))
    }

    #[gtest]
    fn does_not_treat_hook_name_prefix_as_hex_color() -> Result<()> {
        let colors = collect_colors(r#"hook.Add("AddDeathNotice", "id", function() end)"#);

        verify_that!(colors, is_empty())?;
        Ok(())
    }

    #[gtest]
    fn still_detects_real_hex_colors_inside_strings() -> Result<()> {
        let colors = collect_colors(r##"print("#FF00AA")"##);

        verify_that!(colors.len(), eq(1))?;
        Ok(())
    }

    #[gtest]
    fn detects_semantic_rgb_tuple() -> Result<()> {
        let colors = collect_colors_semantic(
            r#"
            ---@class Surface
            local surface = {}
            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a? number
            function surface.SetDrawColor(r, g, b, a) end
            
            surface.SetDrawColor(255, 0, 0)
            surface.SetDrawColor(255, 0, 0, 255)
            "#,
        );

        verify_that!(colors.len(), eq(2))?;
        verify_that!(colors[0].color.alpha, eq(1.0))?;
        verify_that!(colors[1].color.alpha, eq(1.0))?;
        Ok(())
    }

    #[gtest]
    fn does_not_duplicate_color_swatches() -> Result<()> {
        let colors = collect_colors_semantic(
            r#"
            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a? number
            function Color(r, g, b, a) end
            
            local c = Color(255, 0, 0)
            "#,
        );

        verify_that!(colors.len(), eq(1))?;
        Ok(())
    }

    #[gtest]
    fn ignores_non_color_tuples() -> Result<()> {
        let colors = collect_colors_semantic(
            r#"
            ---@param x number
            ---@param y number
            ---@param z number
            function SetPos(x, y, z) end
            
            SetPos(255, 0, 0)
            "#,
        );

        verify_that!(colors, is_empty())?;
        Ok(())
    }
}
