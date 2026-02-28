mod did_change_workspace_folders;
mod did_rename_files;

pub use did_change_workspace_folders::on_did_change_workspace_folders;
pub use did_rename_files::on_did_rename_files_handler;
use lsp_types::{
    ClientCapabilities, FileOperationFilter, FileOperationPattern, FileOperationPatternOptions,
    FileOperationRegistrationOptions, OneOf, ServerCapabilities,
    WorkspaceFileOperationsServerCapabilities, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};

use crate::handlers::RegisterCapabilities;

pub struct WorkspaceCapabilities;

impl RegisterCapabilities for WorkspaceCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.workspace = Some(WorkspaceServerCapabilities {
            file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                did_rename: Some(FileOperationRegistrationOptions {
                    filters: vec![FileOperationFilter {
                        scheme: Some(String::from("file")),
                        pattern: FileOperationPattern {
                            glob: "**/*".to_string(),
                            matches: None,
                            options: Some(FileOperationPatternOptions {
                                ignore_case: Some(true),
                            }),
                        },
                    }],
                }),
                ..Default::default()
            }),
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
        });
    }
}
