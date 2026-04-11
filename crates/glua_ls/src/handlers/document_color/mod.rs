mod build_color;

use build_color::{build_colors, convert_color_to_hex};
use glua_code_analysis::SemanticModel;
use glua_parser::{LuaAstNode, LuaCallExpr};
use lsp_types::{
    ClientCapabilities, Color, ColorInformation, ColorPresentation, ColorPresentationParams,
    ColorProviderCapability, DocumentColorParams, ServerCapabilities, TextEdit,
};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

use super::RegisterCapabilities;

/// Converts a color picker result back to a GMod `Color(r, g, b[, a])` constructor string.
/// Preserves the original 3-arg vs 4-arg form based on the source text.
fn convert_color_to_gmod(color: Color, original_text: &str) -> String {
    let r = (color.red * 255.0).round() as u8;
    let g = (color.green * 255.0).round() as u8;
    let b = (color.blue * 255.0).round() as u8;
    let comma_count = original_text.chars().filter(|&c| c == ',').count();
    let had_alpha = comma_count >= 3;
    let a = (color.alpha * 255.0).round() as u8;

    if had_alpha || a != 255 {
        format!("Color({r}, {g}, {b}, {a})")
    } else {
        format!("Color({r}, {g}, {b})")
    }
}

fn convert_color_to_tuple(color: Color, original_text: &str, arity: Option<usize>) -> String {
    let r = (color.red * 255.0).round() as u8;
    let g = (color.green * 255.0).round() as u8;
    let b = (color.blue * 255.0).round() as u8;
    let comma_count = original_text.chars().filter(|&c| c == ',').count();
    let had_alpha = comma_count >= 3;
    let a = (color.alpha * 255.0).round() as u8;

    let use_alpha = match arity {
        Some(4) => had_alpha || a != 255,
        Some(3) => had_alpha,
        _ => had_alpha,
    };

    if use_alpha {
        format!("{r}, {g}, {b}, {a}")
    } else {
        format!("{r}, {g}, {b}")
    }
}

pub async fn on_document_color(
    context: ServerContextSnapshot,
    params: DocumentColorParams,
    cancel_token: CancellationToken,
) -> Vec<ColorInformation> {
    let uri = params.text_document.uri;
    let Some(analysis) = context.read_analysis(&cancel_token).await else {
        return vec![];
    };
    let file_id = if let Some(file_id) = analysis.get_file_id(&uri) {
        file_id
    } else {
        return vec![];
    };

    let semantic_model =
        if let Some(semantic_model) = analysis.compilation.get_semantic_model(file_id) {
            semantic_model
        } else {
            return vec![];
        };

    if !semantic_model.get_emmyrc().document_color.enable {
        return vec![];
    }

    let document = semantic_model.get_document();
    let root = semantic_model.get_root();
    build_colors(root.syntax().clone(), &document, Some(&semantic_model))
}

fn is_gmod_color_call(text: &str) -> bool {
    text.starts_with("Color") && text[5..].trim_start().starts_with('(')
}

fn get_color_tuple_arity(semantic_model: &SemanticModel, range: rowan::TextRange) -> Option<usize> {
    let root = semantic_model.get_root();

    // Find the token at the start of the range, and traverse ancestors to find the matching CallExpr.
    let token = root
        .syntax()
        .token_at_offset(range.start())
        .right_biased()?;
    let mut current = token.parent();

    while let Some(node) = current {
        if let Some(call_expr) = LuaCallExpr::cast(node.clone()) {
            if let Some(arity) = check_call_for_exact_tuple_range(semantic_model, call_expr, range)
            {
                return Some(arity);
            }
        }
        current = node.parent();
    }

    None
}

fn check_call_for_exact_tuple_range(
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
    expected_range: rowan::TextRange,
) -> Option<usize> {
    let args_list = call_expr.get_args_list()?;
    let args: Vec<_> = args_list.get_args().collect();
    if args.len() < 3 {
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

    let first_arg = &args[start_idx];
    let last_arg = &args[start_idx + supplied_len - 1];

    let text_range = rowan::TextRange::new(
        first_arg.syntax().text_range().start(),
        last_arg.syntax().text_range().end(),
    );

    if text_range == expected_range {
        Some(tuple_len)
    } else {
        None
    }
}

pub async fn on_document_color_presentation(
    context: ServerContextSnapshot,
    params: ColorPresentationParams,
    cancel_token: CancellationToken,
) -> Vec<ColorPresentation> {
    let uri = params.text_document.uri;
    let Some(analysis) = context.read_analysis(&cancel_token).await else {
        return vec![];
    };
    let file_id = if let Some(file_id) = analysis.get_file_id(&uri) {
        file_id
    } else {
        return vec![];
    };

    let semantic_model =
        if let Some(semantic_model) = analysis.compilation.get_semantic_model(file_id) {
            semantic_model
        } else {
            return vec![];
        };
    let document = semantic_model.get_document();

    let range = if let Some(range) = document.to_rowan_range(params.range) {
        range
    } else {
        return vec![];
    };
    let color = params.color;
    let text = document.get_text_slice(range);
    let color_text = if is_gmod_color_call(text) {
        convert_color_to_gmod(color, text)
    } else if text.contains(',') {
        let arity = get_color_tuple_arity(&semantic_model, range);
        convert_color_to_tuple(color, text, arity)
    } else {
        convert_color_to_hex(color, text.len())
    };
    let color_presentations = vec![ColorPresentation {
        label: text.to_string(),
        text_edit: Some(TextEdit {
            range: params.range,
            new_text: color_text,
        }),
        additional_text_edits: None,
    }];

    color_presentations
}

pub struct DocumentColorCapabilities;

impl RegisterCapabilities for DocumentColorCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.color_provider = Some(ColorProviderCapability::Simple(true));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        convert_color_to_gmod, convert_color_to_tuple, get_color_tuple_arity, is_gmod_color_call,
    };
    use glua_parser::LuaAstNode;
    use lsp_types::Color;

    fn get_semantic_tuple_arity(text: &str) -> Option<usize> {
        let mut ws = glua_code_analysis::VirtualWorkspace::new();
        let file_id = ws.def(text);
        let semantic_model = ws.analysis.compilation.get_semantic_model(file_id).unwrap();

        let doc = semantic_model.get_document();
        let root = semantic_model.get_root();

        let colors = crate::handlers::document_color::build_color::build_colors(
            root.syntax().clone(),
            &doc,
            Some(&semantic_model),
        );
        if colors.is_empty() {
            return None;
        }

        // Use the first generated color range, skipping normal Color(...) calls
        // to specifically test semantic tuple logic if there are multiple.
        let range = doc.to_rowan_range(colors.last().unwrap().range).unwrap();
        get_color_tuple_arity(&semantic_model, range)
    }

    #[test]
    fn test_get_color_tuple_arity() {
        let arity_4 = get_semantic_tuple_arity(
            r#"
            ---@class Surface
            local surface = {}
            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a? number
            function surface.SetDrawColor(r, g, b, a) end
            
            surface.SetDrawColor(255, 0, 0)
        "#,
        );
        assert_eq!(arity_4, Some(4));

        let arity_3 = get_semantic_tuple_arity(
            r#"
            ---@class Surface
            local surface = {}
            ---@param r number
            ---@param g number
            ---@param b number
            function surface.SetDrawColor(r, g, b) end
            
            surface.SetDrawColor(255, 0, 0)
        "#,
        );
        assert_eq!(arity_3, Some(3));
    }

    #[test]
    fn test_is_gmod_color_call() {
        assert!(is_gmod_color_call("Color(255, 0, 0)"));
        assert!(is_gmod_color_call("Color(255, 0, 0, 255)"));
        assert!(is_gmod_color_call("Color (255, 0, 0)"));
        assert!(is_gmod_color_call("Color\n(255, 0, 0)"));
        assert!(!is_gmod_color_call("Color32(255, 0, 0)"));
        assert!(!is_gmod_color_call("Color"));
        assert!(!is_gmod_color_call("255, 0, 0"));
    }

    #[test]
    fn test_convert_color_to_gmod() {
        let color_rgb = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        assert_eq!(
            convert_color_to_gmod(color_rgb, "Color(255, 0, 0)"),
            "Color(255, 0, 0)"
        );

        let color_rgba = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.5,
        };
        assert_eq!(
            convert_color_to_gmod(color_rgba, "Color(255, 0, 0)"),
            "Color(255, 0, 0, 128)"
        );

        // Alpha upgrade with whitespace in constructor
        assert_eq!(
            convert_color_to_gmod(color_rgba, "Color (255, 0, 0)"),
            "Color(255, 0, 0, 128)"
        );

        let color_rgba_original = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        assert_eq!(
            convert_color_to_gmod(color_rgba_original, "Color(255, 0, 0, 255)"),
            "Color(255, 0, 0, 255)"
        );
    }

    #[test]
    fn test_convert_color_to_tuple() {
        let color_rgb = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        assert_eq!(
            convert_color_to_tuple(color_rgb, "255, 0, 0", None),
            "255, 0, 0"
        );

        let color_rgba = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.5,
        };
        assert_eq!(
            convert_color_to_tuple(color_rgba, "255, 0, 0", Some(4)),
            "255, 0, 0, 128"
        );

        // Preserve 3-arg if arity is 3, even if alpha != 1.0
        assert_eq!(
            convert_color_to_tuple(color_rgba, "255, 0, 0", Some(3)),
            "255, 0, 0"
        );

        // Fallback to original text arity (comma count) if arity is None
        assert_eq!(
            convert_color_to_tuple(color_rgba, "255, 0, 0", None),
            "255, 0, 0"
        );

        let color_rgba_original = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        assert_eq!(
            convert_color_to_tuple(color_rgba_original, "255, 0, 0, 255", None),
            "255, 0, 0, 255"
        );
    }
}
