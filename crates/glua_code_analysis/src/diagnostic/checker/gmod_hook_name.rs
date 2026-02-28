use crate::{DiagnosticCode, GmodHookNameIssue, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct GmodHookNameChecker;

impl Checker for GmodHookNameChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::GmodInvalidHookName];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let Some(hook_metadata) = semantic_model
            .get_db()
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id)
        else {
            return;
        };

        for hook_site in &hook_metadata.sites {
            let Some(name_issue) = hook_site.name_issue else {
                continue;
            };
            let Some(name_range) = hook_site.name_range else {
                continue;
            };

            let message = match name_issue {
                GmodHookNameIssue::Empty => t!("Hook name should not be empty.").to_string(),
                GmodHookNameIssue::NonStringLiteral => {
                    t!("Hook name should be a string literal when static.").to_string()
                }
            };

            context.add_diagnostic(
                DiagnosticCode::GmodInvalidHookName,
                name_range,
                message,
                None,
            );
        }
    }
}
