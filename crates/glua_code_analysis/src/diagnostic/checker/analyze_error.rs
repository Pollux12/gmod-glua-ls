use crate::{DiagnosticCode, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct AnalyzeErrorChecker;

impl Checker for AnalyzeErrorChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::TypeNotFound,
        DiagnosticCode::AnnotationUsageError,
    ];

    fn check(context: &mut DiagnosticContext, _: &SemanticModel) {
        let file_id = context.get_file_id();
        let errors = {
            let db = context.get_db();
            let Some(diagnostics) = db.get_diagnostic_index().get_diagnostics(&file_id) else {
                return;
            };
            diagnostics.to_vec()
        };
        for error in errors {
            context.add_diagnostic(error.kind, error.range, error.message, None);
        }
    }
}
