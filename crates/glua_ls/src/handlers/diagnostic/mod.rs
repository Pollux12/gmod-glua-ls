mod document_diagnostic;
mod result_id;
mod workspace_diagnostic;

use super::RegisterCapabilities;
pub use document_diagnostic::on_pull_document_diagnostic;
use lsp_types::{
    ClientCapabilities, DiagnosticOptions, DiagnosticServerCapabilities, ServerCapabilities,
};
use result_id::diagnostic_result_id;
pub use workspace_diagnostic::on_pull_workspace_diagnostic;

pub struct DiagnosticCapabilities;

impl RegisterCapabilities for DiagnosticCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.diagnostic_provider =
            Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
                identifier: Some("GLuaLS".to_string()),
                // Editing one file changes the diagnostics of others through
                // the shared index, which is what this flag declares.
                inter_file_dependencies: true,
                workspace_diagnostics: true,
                ..Default::default()
            }))
    }
}
