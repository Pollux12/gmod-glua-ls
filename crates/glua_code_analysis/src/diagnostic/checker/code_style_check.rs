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
        let result = check_code_style(file_path.to_string_lossy().as_ref(), text);
        for diagnostic in result {
            let (Some(start), Some(end)) = (
                document.get_offset(
                    diagnostic.start_line as usize,
                    diagnostic.start_col as usize,
                ),
                document.get_offset(diagnostic.end_line as usize, diagnostic.end_col as usize),
            ) else {
                return;
            };
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
