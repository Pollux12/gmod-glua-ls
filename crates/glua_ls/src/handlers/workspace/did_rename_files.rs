use glua_code_analysis::{file_path_to_uri, read_file_with_encoding, uri_to_file_path};
use lsp_types::RenameFilesParams;

use super::will_rename_files::file_renames;
use crate::{context::ServerContextSnapshot, handlers::text_document::apply_file_system_changes};

/// Moves the renamed files in the server's state. The load paths naming them
/// are rewritten by the client from `workspace/willRenameFiles`.
pub async fn on_did_rename_files_handler(
    context: ServerContextSnapshot,
    params: RenameFilesParams,
) -> Option<()> {
    let renames = file_renames(&params.files);
    let (encoding, moved) = {
        let analysis = context.analysis().read().await;
        let vfs = analysis.compilation.get_db().get_vfs();
        let moved = renames
            .moved_files(vfs)
            .into_iter()
            .filter_map(|(file_id, new_path)| {
                Some((vfs.get_uri(&file_id)?, file_path_to_uri(&new_path)?))
            })
            .collect::<Vec<_>>();
        (analysis.get_emmyrc().workspace.encoding.clone(), moved)
    };
    if moved.is_empty() {
        return Some(());
    }
    let moved = {
        let workspace = context.workspace_manager().read().await;
        moved
            .into_iter()
            .map(|(old_uri, new_uri)| {
                let new_uri = workspace.is_workspace_file(&new_uri).then_some(new_uri);
                (old_uri, new_uri)
            })
            .collect::<Vec<_>>()
    };

    let batch = tokio::task::spawn_blocking(move || {
        let mut batch = Vec::with_capacity(moved.len() * 2);
        for (old_uri, new_uri) in moved {
            batch.push((old_uri, None));
            let Some(new_uri) = new_uri else {
                continue;
            };
            if let Some(text) = uri_to_file_path(&new_uri)
                .and_then(|path| read_file_with_encoding(&path, &encoding))
            {
                batch.push((new_uri, Some(text)));
            }
        }
        batch
    })
    .await
    .ok()?;
    let renamed_uris = batch.iter().map(|(uri, _)| uri.clone()).collect::<Vec<_>>();
    apply_file_system_changes(&context, batch).await;
    context.note_renamed_uris(renamed_uris);
    Some(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use glua_code_analysis::file_path_to_uri;
    use lsp_server::{Connection, Message};
    use lsp_types::{
        ClientCapabilities, DidChangeWatchedFilesParams, FileChangeType, FileEvent, FileRename,
        RenameFilesParams, Uri,
    };

    use crate::{
        context::{ServerContext, ServerContextSnapshot},
        handlers::text_document::on_did_change_watched_files,
    };

    const LOADER: &str = "include(\"sh_old.lua\")\n";
    const RENAMED: &str = "SHARED_VALUE = 1\n";

    struct RenamedWorkspace {
        root: PathBuf,
        snapshot: ServerContextSnapshot,
        client: Connection,
        old_uri: Uri,
        new_uri: Uri,
        _context: ServerContext,
    }

    impl Drop for RenamedWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A workspace where `lua/autorun/sh_old.lua`, which another file
    /// includes, was renamed on disk to `sh_new.lua` and `didRenameFiles` was
    /// handled.
    async fn renamed_workspace(name: &str) -> RenamedWorkspace {
        let root = std::env::temp_dir().join(format!(
            "gmod_glua_ls_did_rename_{name}_{}",
            std::process::id()
        ));
        let autorun = root.join("lua/autorun");
        std::fs::create_dir_all(&autorun).expect("workspace folder is created");
        std::fs::write(autorun.join("sh_new.lua"), RENAMED).expect("renamed file is written");
        let old_uri = file_path_to_uri(&autorun.join("sh_old.lua")).expect("old URI");
        let new_uri = file_path_to_uri(&autorun.join("sh_new.lua")).expect("new URI");
        let loader_uri = file_path_to_uri(&autorun.join("sh_loader.lua")).expect("loader URI");

        let (connection, client) = Connection::memory();
        let context = ServerContext::new(connection, ClientCapabilities::default());
        let snapshot = context.snapshot();
        {
            let mut analysis = snapshot.analysis().write().await;
            analysis.add_main_workspace(root.clone());
            analysis.update_file_by_uri(&old_uri, Some(RENAMED.to_string()));
            analysis.update_file_by_uri(&loader_uri, Some(LOADER.to_string()));
        }
        super::on_did_rename_files_handler(
            snapshot.clone(),
            RenameFilesParams {
                files: vec![FileRename {
                    old_uri: old_uri.to_string(),
                    new_uri: new_uri.to_string(),
                }],
            },
        )
        .await;
        RenamedWorkspace {
            root,
            snapshot,
            client,
            old_uri,
            new_uri,
            _context: context,
        }
    }

    async fn content(snapshot: &ServerContextSnapshot, uri: &Uri) -> Option<String> {
        let analysis = snapshot.analysis().read().await;
        let vfs = analysis.compilation.get_db().get_vfs();
        vfs.latest_content(&vfs.get_file_id(uri)?).cloned()
    }

    async fn watch(snapshot: &ServerContextSnapshot, changes: Vec<(Uri, FileChangeType)>) {
        on_did_change_watched_files(
            snapshot.clone(),
            DidChangeWatchedFilesParams {
                changes: changes
                    .into_iter()
                    .map(|(uri, typ)| FileEvent { uri, typ })
                    .collect(),
            },
        )
        .await;
    }

    #[test]
    fn a_handled_rename_moves_the_file_in_the_server() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = renamed_workspace("moves").await;

            let old = content(&workspace.snapshot, &workspace.old_uri).await;
            let new = content(&workspace.snapshot, &workspace.new_uri).await;

            assert_eq!((old, new), (None, Some(RENAMED.to_string())));
        });
    }

    /// The load paths are rewritten by the client from `willRenameFiles`, so
    /// `didRenameFiles` neither prompts nor applies an edit.
    #[test]
    fn a_handled_rename_neither_prompts_nor_edits() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = renamed_workspace("no_prompt").await;

            let requests = workspace
                .client
                .receiver
                .try_iter()
                .filter_map(|message| match message {
                    Message::Request(request) => Some(request.method),
                    _ => None,
                })
                .filter(|method| {
                    method == "window/showMessageRequest" || method == "workspace/applyEdit"
                })
                .collect::<Vec<_>>();

            assert!(requests.is_empty(), "{requests:?}");
        });
    }

    /// The watcher's delete and create events for a rename `didRenameFiles`
    /// already applied change nothing.
    #[test]
    fn the_watcher_events_of_a_handled_rename_are_not_applied_again() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = renamed_workspace("echo").await;
            let revision = |analysis: &glua_code_analysis::EmmyLuaAnalysis| {
                analysis.compilation.get_db().get_vfs().content_revision()
            };
            let before = revision(&*workspace.snapshot.analysis().read().await);

            watch(
                &workspace.snapshot,
                vec![
                    (workspace.old_uri.clone(), FileChangeType::DELETED),
                    (workspace.new_uri.clone(), FileChangeType::CREATED),
                ],
            )
            .await;

            let after = revision(&*workspace.snapshot.analysis().read().await);
            assert_eq!(after, before);
        });
    }

    /// Recognising a rename's watcher events is one-shot: a later deletion of
    /// the renamed file is applied.
    #[test]
    fn a_later_deletion_of_a_renamed_file_is_applied() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = renamed_workspace("later_delete").await;
            watch(
                &workspace.snapshot,
                vec![(workspace.new_uri.clone(), FileChangeType::CREATED)],
            )
            .await;

            watch(
                &workspace.snapshot,
                vec![(workspace.new_uri.clone(), FileChangeType::DELETED)],
            )
            .await;

            assert_eq!(content(&workspace.snapshot, &workspace.new_uri).await, None);
        });
    }
}
