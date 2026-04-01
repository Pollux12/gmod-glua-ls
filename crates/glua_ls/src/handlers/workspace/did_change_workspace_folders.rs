use std::collections::HashSet;

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

    let client_config = get_client_config(&context, client_id, supports_config_request).await;

    let mut workspace_manager = context.workspace_manager().write().await;
    workspace_manager.client_config = client_config;
    workspace_manager.add_reload_workspace_task(context.workspace_manager_arc());

    Some(())
}
