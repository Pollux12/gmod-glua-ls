use std::{collections::HashSet, time::Duration};

use glua_code_analysis::{WorkspaceFolder, uri_to_file_path};
use lsp_types::DidChangeWorkspaceFoldersParams;

use crate::{context::ServerContextSnapshot, handlers::initialized::get_client_config};

pub async fn on_did_change_workspace_folders(
    context: ServerContextSnapshot,
    params: DidChangeWorkspaceFoldersParams,
) -> Option<()> {
    let added_folders: Vec<WorkspaceFolder> = params
        .event
        .added
        .into_iter()
        .filter_map(|folder| {
            uri_to_file_path(&folder.uri).map(|path| WorkspaceFolder::new(path, false))
        })
        .collect();
    let removed_roots: HashSet<_> = params
        .event
        .removed
        .into_iter()
        .filter_map(|folder| uri_to_file_path(&folder.uri))
        .collect();

    if added_folders.is_empty() && removed_roots.is_empty() {
        return Some(());
    }

    log::info!(
        "workspace folders changed, added: {}, removed: {}",
        added_folders.len(),
        removed_roots.len()
    );

    let (client_id, supports_config_request) = {
        let mut workspace_manager = context.workspace_manager().write().await;

        if !removed_roots.is_empty() {
            workspace_manager
                .workspace_folders
                .retain(|workspace| !removed_roots.contains(&workspace.root));
        }

        for added_workspace in added_folders {
            let already_exists = workspace_manager
                .workspace_folders
                .iter()
                .any(|workspace| workspace.root == added_workspace.root);
            if !already_exists {
                workspace_manager.workspace_folders.push(added_workspace);
            }
        }

        (
            workspace_manager.client_config.client_id,
            context.lsp_features().supports_config_request(),
        )
    };

    let mut client_config = get_client_config(&context, client_id, supports_config_request).await;

    let mut workspace_manager = context.workspace_manager().write().await;
    client_config.keep_session_options(&workspace_manager.client_config);
    workspace_manager.client_config = client_config;
    workspace_manager
        .schedule_config_reload(Duration::ZERO, context.workspace_manager_arc())
        .await;

    Some(())
}

#[cfg(test)]
mod tests {
    use lsp_server::Connection;
    use lsp_types::{
        ClientCapabilities, DidChangeWorkspaceFoldersParams, WorkspaceFolder,
        WorkspaceFoldersChangeEvent,
    };

    use crate::context::ServerContext;

    /// Adding a folder reads the client's settings again, which do not carry
    /// what the command line and the initialization options set.
    #[test]
    fn a_folder_change_keeps_the_options_set_at_initialization() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let folder = crate::util::TempDir::new("gmod_glua_ls_folder_change");
        let kept = runtime.block_on(async {
            let (connection, _peer) = Connection::memory();
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            {
                let mut manager = snapshot.workspace_manager().write().await;
                manager.client_config.gmod_annotations_path = Some("annotations".to_string());
            }
            super::on_did_change_workspace_folders(
                snapshot.clone(),
                DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![WorkspaceFolder {
                            uri: glua_code_analysis::file_path_to_uri(&folder.to_path_buf())
                                .expect("folder URI"),
                            name: "added".to_string(),
                        }],
                        removed: Vec::new(),
                    },
                },
            )
            .await;
            let manager = snapshot.workspace_manager().read().await;
            manager.client_config.gmod_annotations_path.clone()
        });
        assert_eq!(kept, Some("annotations".to_string()));
    }
}
