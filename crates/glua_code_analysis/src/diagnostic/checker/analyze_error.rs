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
            // `TypeNotFound` is a verdict over the whole index baked while the
            // file was analysed. Files are not all analysed in one batch — the
            // initial load can be split and a ripple can re-analyse a consumer
            // while the provider's decl is removed — so the name may resolve
            // by the time diagnostics are collected. Re-check the recorded
            // reference and drop the entry if the decl now exists.
            let stale = error.kind == DiagnosticCode::TypeNotFound && {
                let db = context.get_db();
                db.get_reference_index()
                    .get_type_references_at(file_id, error.range)
                    .iter()
                    .any(|decl_id| {
                        db.get_type_index()
                            .find_type_decl(file_id, decl_id.get_name())
                            .is_some()
                    })
            };
            if stale {
                continue;
            }
            context.add_diagnostic(error.kind, error.range, error.message, None);
        }
    }
}
