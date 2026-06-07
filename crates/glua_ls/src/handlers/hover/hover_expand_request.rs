use lsp_types::request::Request;
use lsp_types::{Position, TextDocumentIdentifier};
use serde::{Deserialize, Serialize};

use glua_code_analysis::{DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT, LuaType, SemanticModel};
use glua_parser::LuaAstNode;
use rowan::TokenAtOffset;

// ── LSP request type ────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum GluaHoverExpandRequest {}

impl Request for GluaHoverExpandRequest {
    type Params = HoverExpandParams;
    type Result = Option<HoverExpandResponse>;
    const METHOD: &'static str = "gluals/hoverExpand";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverExpandParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    /// Verbosity level. 0 = default compact member count, higher = more members.
    pub level: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverExpandResponse {
    /// Full hover markdown content.
    pub content: lsp_types::HoverContents,
    /// The range in the document this hover applies to.
    pub range: Option<lsp_types::Range>,
    /// Maximum verbosity level available for this symbol.
    pub max_level: u32,
}

// ── Verbosity level helpers ─────────────────────────────────────────────────

/// Maps a verbosity level to the max number of class members to display.
pub fn level_to_display_count(level: u32) -> usize {
    match level {
        0 => DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT, // default — compact, fits in hover popup
        1 => 12,                                  // more detail
        2 => 24,                                  // verbose
        3 => 50,                                  // very verbose
        4 => 100,                                 // extremely verbose
        _ => usize::MAX,                          // level 5+ - show everything
    }
}

/// Computes the maximum verbosity level for a given total member count.
///
/// Returns 0 when all members fit at the default display count (no + button).
pub fn compute_max_level(total: usize) -> u32 {
    (0..)
        .find(|level| level_to_display_count(*level) >= total)
        .unwrap_or(0)
}

fn compute_max_level_for_type(semantic_model: &SemanticModel, typ: &LuaType) -> u32 {
    let total = match typ {
        LuaType::Ref(_) | LuaType::TableConst(_) | LuaType::MergedTable(_) => semantic_model
            .get_member_infos(typ)
            .map(|members| members.len())
            .unwrap_or(0),
        LuaType::Object(object) => object.get_fields().len(),
        _ => 0,
    };
    compute_max_level(total)
}

/// Computes the maximum verbosity level for the type at the given position.
///
/// Returns non-zero for rendered table-like hovers that can show more members.
pub fn compute_max_level_at_position(semantic_model: &SemanticModel, position: Position) -> u32 {
    let document = semantic_model.get_document();
    let Some(offset) = document.get_offset(position.line as usize, position.character as usize)
    else {
        return 0;
    };
    let root = semantic_model.get_root();
    let token = match root.syntax().token_at_offset(offset) {
        TokenAtOffset::Single(t) => t,
        TokenAtOffset::Between(l, _) => l,
        TokenAtOffset::None => return 0,
    };

    let semantic_info = semantic_model.get_semantic_info(token.into());
    let typ = match semantic_info {
        Some(info) => info.typ,
        None => return 0,
    };

    compute_max_level_for_type(semantic_model, &typ)
}

#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::check;

    use glua_code_analysis::{RenderLevel, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::HoverContents;
    use rowan::TextSize;

    use super::{compute_max_level, compute_max_level_at_position, level_to_display_count};

    #[gtest]
    fn max_level_display_count_covers_large_member_count() {
        let total = 501;
        let max_level = compute_max_level(total);

        assert_that!(level_to_display_count(max_level), ge(total));
    }

    #[gtest]
    fn default_level_display_count_is_compact() {
        assert_that!(level_to_display_count(0), eq(6));
        assert_that!(compute_max_level(6), eq(0));
        assert_that!(compute_max_level(7), eq(1));
    }

    #[gtest]
    fn table_const_field_hover_reports_expandable_max_level() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let content = r#"
            local ENT = {}
            ENT.SuspensionPoseParameters = {
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
            }
        "#;
        let file_id = ws.def_file("lua/entities/fl_audi_r8.lua", content);
        let semantic_model = check!(ws.analysis.compilation.get_semantic_model(file_id));
        let document = semantic_model.get_document();
        let name_offset = check!(content.find("SuspensionPoseParameters"));
        let position = check!(document.to_lsp_position(TextSize::new(name_offset as u32)));

        assert_that!(
            compute_max_level_at_position(&semantic_model, position),
            eq(2)
        );
        Ok(())
    }

    #[gtest]
    fn default_table_const_field_hover_renders_six_rows() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let content = r#"
            local ENT = {}
            ENT.SuspensionPoseParameters = {
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
            }
        "#;
        let file_id = ws.def_file("lua/entities/fl_audi_r8.lua", content);
        let semantic_model = check!(ws.analysis.compilation.get_semantic_model(file_id));
        let document = semantic_model.get_document();
        let name_offset = check!(content.find("SuspensionPoseParameters"));
        let position = check!(document.to_lsp_position(TextSize::new(name_offset as u32)));

        let hover = check!(crate::handlers::hover::hover(
            &ws.analysis,
            file_id,
            position,
            None,
        ));
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        verify_that!(markup.value.as_str(), contains_substring("[6]"))?;
        verify_that!(markup.value.as_str(), not(contains_substring("[7]")))?;
        verify_that!(markup.value.as_str(), contains_substring("    ..."))?;
        Ok(())
    }

    #[gtest]
    fn expanded_table_const_field_hover_renders_more_rows() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let content = r#"
            local ENT = {}
            ENT.SuspensionPoseParameters = {
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
                { parameter = "vehicle_wheel_fr_height", wheel = 2 },
                { parameter = "vehicle_wheel_rl_height", wheel = 3 },
                { parameter = "vehicle_wheel_rr_height", wheel = 4 },
                { parameter = "vehicle_wheel_fl_height", wheel = 1 },
            }
        "#;
        let file_id = ws.def_file("lua/entities/fl_audi_r8.lua", content);
        let semantic_model = check!(ws.analysis.compilation.get_semantic_model(file_id));
        let document = semantic_model.get_document();
        let name_offset = check!(content.find("SuspensionPoseParameters"));
        let position = check!(document.to_lsp_position(TextSize::new(name_offset as u32)));

        let hover = check!(crate::handlers::hover::hover(
            &ws.analysis,
            file_id,
            position,
            Some(RenderLevel::DetailedCount(24)),
        ));
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        verify_that!(markup.value.as_str(), contains_substring("[13]"))?;
        verify_that!(markup.value.as_str(), not(contains_substring("    ...")))?;
        Ok(())
    }
}
