use glua_codestyle::check_code_style;
use rowan::TextRange;

use crate::{DiagnosticCode, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct CodeStyleCheckChecker;

impl Checker for CodeStyleCheckChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::CodeStyleCheck];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let document = semantic_model.get_document();
        let file_path = document.get_file_path();
        let text = document.get_text();
        let non_standard_symbol = !semantic_model
            .get_emmyrc()
            .runtime
            .nonstandard_symbol
            .is_empty();
        let result = check_code_style(
            file_path.to_string_lossy().as_ref(),
            text,
            non_standard_symbol,
        );
        for diagnostic in result {
            let start = document.get_offset_by_char_col(
                diagnostic.start_line as usize,
                diagnostic.start_col as usize,
            );
            let end = document
                .get_offset_by_char_col(diagnostic.end_line as usize, diagnostic.end_col as usize);
            let text_range = TextRange::new(start, end);
            context.add_diagnostic(
                DiagnosticCode::CodeStyleCheck,
                text_range,
                diagnostic.message,
                None,
            );
        }
    }
}
