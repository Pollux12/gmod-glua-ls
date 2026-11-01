mod did_change_workspace_folders;
mod did_rename_files;
mod will_rename_files;

pub use did_change_workspace_folders::on_did_change_workspace_folders;
pub use did_rename_files::on_did_rename_files_handler;
use lsp_types::{
    ClientCapabilities, FileOperationFilter, FileOperationPattern, FileOperationPatternKind,
    FileOperationPatternOptions, FileOperationRegistrationOptions, OneOf, ServerCapabilities,
    WorkspaceFileOperationsServerCapabilities, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};
pub use will_rename_files::on_will_rename_files_handler;

use crate::handlers::RegisterCapabilities;

pub struct WorkspaceCapabilities;

impl RegisterCapabilities for WorkspaceCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.workspace = Some(WorkspaceServerCapabilities {
            file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                did_rename: Some(FileOperationRegistrationOptions {
                    filters: vec![file_operation_filter("**/*", None)],
                }),
                will_rename: Some(FileOperationRegistrationOptions {
                    filters: vec![
                        file_operation_filter("**/*.lua", Some(FileOperationPatternKind::File)),
                        file_operation_filter("**/*", Some(FileOperationPatternKind::Folder)),
                    ],
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

fn file_operation_filter(
    glob: &str,
    matches: Option<FileOperationPatternKind>,
) -> FileOperationFilter {
    FileOperationFilter {
        scheme: Some(String::from("file")),
        pattern: FileOperationPattern {
            glob: glob.to_string(),
            matches,
            options: Some(FileOperationPatternOptions {
                ignore_case: Some(true),
            }),
        },
    }
}
