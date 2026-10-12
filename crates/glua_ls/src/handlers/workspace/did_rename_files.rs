use std::collections::HashMap;

use glua_code_analysis::{FileId, file_path_to_uri, read_file_with_encoding, uri_to_file_path};
use lsp_types::{
    ApplyWorkspaceEditParams, MessageActionItem, MessageType, RenameFilesParams,
    ShowMessageRequestParams, TextEdit,
};
use tokio_util::sync::CancellationToken;

use super::will_rename_files::{file_renames, load_path_edits, workspace_edit};
use crate::{context::ServerContextSnapshot, handlers::text_document::apply_file_system_changes};

/// Moves the renamed files in the server's state. A client that asks
/// `workspace/willRenameFiles` rewrites the load paths naming them from its
/// answer; one that does not but applies edits is offered the same edit here.
pub async fn on_did_rename_files_handler(
    context: ServerContextSnapshot,
    params: RenameFilesParams,
) -> Option<()> {
    let renames = file_renames(&params.files);
    let features = context.lsp_features();
    let offers_edit = !features.supports_will_rename_files() && features.supports_apply_edit();
    let (encoding, moved, edits) = {
        let analysis = context.analysis().read().await;
        // The load paths are found where the files are indexed now, before
        // they move.
        let edits = if offers_edit {
            load_path_edits(analysis.compilation.get_db(), &renames)
        } else {
            Vec::new()
        };
        let vfs = analysis.compilation.get_db().get_vfs();
        let moved = renames
            .moved_files(vfs)
            .into_iter()
            .filter_map(|(file_id, new_path)| {
                Some((vfs.get_uri(&file_id)?, file_path_to_uri(&new_path)?))
            })
            .collect::<Vec<_>>();
        (
            analysis.get_emmyrc().workspace.encoding.clone(),
            moved,
            edits,
        )
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
    if !edits.is_empty() {
        // The user's answer can take a while; later file changes do not wait
        // for it.
        tokio::spawn(offer_load_path_edits(context, edits));
    }
    Some(())
}

/// Asks the user whether to rewrite the load paths naming the renamed files,
/// and applies `edits` if so. A moved file keeps its id, so each edit goes to
/// where its file is now.
async fn offer_load_path_edits(context: ServerContextSnapshot, edits: Vec<(FileId, TextEdit)>) {
    let update = MessageActionItem {
        title: "Update".to_string(),
        properties: HashMap::new(),
    };
    let answer = context
        .client()
        .show_message_request(
            ShowMessageRequestParams {
                typ: MessageType::INFO,
                message: "Update the paths that load the renamed files?".to_string(),
                actions: Some(vec![update.clone()]),
            },
            CancellationToken::new(),
        )
        .await;
    if answer.is_none_or(|answer| answer.title != update.title) {
        return;
    }
    let Some(edit) = workspace_edit(
        context
            .analysis()
            .read()
            .await
            .compilation
            .get_db()
            .get_vfs(),
        edits,
    ) else {
        return;
    };
    context
        .client()
        .apply_edit(
            ApplyWorkspaceEditParams { label: None, edit },
            CancellationToken::new(),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use glua_code_analysis::{FileId, file_path_to_uri};
    use lsp_server::{Connection, Message};
    use std::collections::HashMap;

    use lsp_types::{
        ApplyWorkspaceEditParams, ClientCapabilities, DidChangeWatchedFilesParams, FileChangeType,
        FileEvent, FileRename, Position, PublishDiagnosticsParams, Range, RenameFilesParams,
        TextEdit, Uri, WorkspaceClientCapabilities, WorkspaceFileOperationsClientCapabilities,
    };
    use tokio_util::sync::CancellationToken;

    use crate::{
        context::{ServerContext, ServerContextSnapshot, diagnostic_store::Priority},
        handlers::text_document::on_did_change_watched_files,
    };

    const LOADER: &str = "include(\"sh_old.lua\")\n";
    const RENAMED: &str = "SHARED_VALUE = 1\n";
    /// Text with a syntax error, so it always has a diagnostic.
    const DIAGNOSED: &str = "SHARED_VALUE = 1\nlocal broken =\n";

    struct RenamedWorkspace {
        _root: crate::util::TempDir,
        snapshot: ServerContextSnapshot,
        client: Connection,
        old_uri: Uri,
        new_uri: Uri,
        loader_uri: Uri,
        old_id: FileId,
        _context: ServerContext,
    }

    /// A workspace where `lua/autorun/sh_old.lua`, which another file
    /// includes, was renamed on disk to `sh_new.lua`, which holds `text`.
    async fn workspace_to_rename(name: &str, text: &str) -> RenamedWorkspace {
        workspace_to_rename_for(name, text, ClientCapabilities::default()).await
    }

    /// [`workspace_to_rename`] served to a client with `capabilities`.
    async fn workspace_to_rename_for(
        name: &str,
        text: &str,
        capabilities: ClientCapabilities,
    ) -> RenamedWorkspace {
        let root = crate::util::TempDir::new(&format!("gmod_glua_ls_did_rename_{name}"));
        let autorun = root.join("lua/autorun");
        std::fs::create_dir_all(&autorun).expect("workspace folder is created");
        std::fs::write(autorun.join("sh_new.lua"), text).expect("renamed file is written");
        let old_uri = file_path_to_uri(&autorun.join("sh_old.lua")).expect("old URI");
        let new_uri = file_path_to_uri(&autorun.join("sh_new.lua")).expect("new URI");
        let loader_uri = file_path_to_uri(&autorun.join("sh_loader.lua")).expect("loader URI");

        let (connection, client) = Connection::memory();
        let context = ServerContext::new(connection, capabilities);
        let snapshot = context.snapshot();
        let old_id = {
            let mut analysis = snapshot.analysis().write().await;
            analysis.add_main_workspace(root.to_path_buf());
            analysis.update_file_by_uri(&loader_uri, Some(LOADER.to_string()));
            analysis
                .update_file_by_uri(&old_uri, Some(text.to_string()))
                .expect("the renamed file is added")
                .0
        };
        RenamedWorkspace {
            _root: root,
            snapshot,
            client,
            old_uri,
            new_uri,
            loader_uri,
            old_id,
            _context: context,
        }
    }

    /// A client that applies edits, and asks `willRenameFiles` when `asks`.
    fn applying_client(asks_will_rename: bool) -> ClientCapabilities {
        ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                apply_edit: Some(true),
                file_operations: Some(WorkspaceFileOperationsClientCapabilities {
                    did_rename: Some(true),
                    will_rename: Some(asks_will_rename),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The next request the server sends the client within `wait`.
    fn next_request(client: &Connection, wait: Duration) -> Option<lsp_server::Request> {
        let deadline = std::time::Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match client.receiver.recv_timeout(left) {
                Ok(Message::Request(request)) => return Some(request),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Hands the rename of `workspace` to `didRenameFiles`.
    async fn rename(workspace: &RenamedWorkspace) {
        super::on_did_rename_files_handler(
            workspace.snapshot.clone(),
            RenameFilesParams {
                files: vec![FileRename {
                    old_uri: workspace.old_uri.to_string(),
                    new_uri: workspace.new_uri.to_string(),
                }],
            },
        )
        .await;
    }

    /// [`workspace_to_rename`] with `didRenameFiles` handled.
    async fn renamed_workspace(name: &str) -> RenamedWorkspace {
        let workspace = workspace_to_rename(name, RENAMED).await;
        rename(&workspace).await;
        workspace
    }

    async fn file_id(snapshot: &ServerContextSnapshot, uri: &Uri) -> Option<FileId> {
        snapshot.analysis().read().await.get_file_id(uri)
    }

    /// The diagnostics published to the client so far, in order.
    fn published(client: &Connection) -> Vec<PublishDiagnosticsParams> {
        client
            .receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "textDocument/publishDiagnostics" =>
                {
                    serde_json::from_value(notification.params).ok()
                }
                _ => None,
            })
            .collect()
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

    /// A renamed file is the same file at its new path.
    #[test]
    fn a_handled_rename_keeps_the_file_id() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = renamed_workspace("keeps_id").await;

            let old = file_id(&workspace.snapshot, &workspace.old_uri).await;
            let new = file_id(&workspace.snapshot, &workspace.new_uri).await;

            assert_eq!((old, new), (None, Some(workspace.old_id)));
        });
    }

    /// A move the watcher alone reports, as a deletion and a creation of the
    /// same text, keeps the file's id too.
    #[test]
    fn a_watched_move_keeps_the_file_id() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = workspace_to_rename("watched_move", RENAMED).await;

            watch(
                &workspace.snapshot,
                vec![
                    (workspace.old_uri.clone(), FileChangeType::DELETED),
                    (workspace.new_uri.clone(), FileChangeType::CREATED),
                ],
            )
            .await;

            let old = file_id(&workspace.snapshot, &workspace.old_uri).await;
            let new = file_id(&workspace.snapshot, &workspace.new_uri).await;
            assert_eq!((old, new), (None, Some(workspace.old_id)));
            assert_eq!(
                content(&workspace.snapshot, &workspace.new_uri).await,
                Some(RENAMED.to_string())
            );
        });
    }

    /// What the client showed at a renamed file's old URI is cleared, and its
    /// diagnostics go out at the new URI.
    #[test]
    fn a_handled_rename_moves_the_diagnostics_to_the_new_uri() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace = workspace_to_rename("diagnostics", DIAGNOSED).await;
            let store = workspace.snapshot.file_diagnostic().store();
            let cancel = CancellationToken::new();
            let before = store
                .get(workspace.old_id, Priority::Edited, &cancel)
                .await
                .expect("diagnostics before the rename");
            assert!(!before.diagnostics.is_empty());

            rename(&workspace).await;
            let after = tokio::time::timeout(
                Duration::from_secs(60),
                store.get(workspace.old_id, Priority::Edited, &cancel),
            )
            .await
            .expect("the moved file is diagnosed")
            .expect("diagnostics after the rename");

            assert_eq!(after.uri, workspace.new_uri);
            let count = before.diagnostics.len();
            let published = published(&workspace.client)
                .into_iter()
                .filter(|params| [&workspace.old_uri, &workspace.new_uri].contains(&&params.uri))
                .map(|params| (params.uri, params.diagnostics.len()))
                .collect::<Vec<_>>();
            assert_eq!(
                published,
                vec![
                    (workspace.old_uri.clone(), count),
                    (workspace.old_uri.clone(), 0),
                    (workspace.new_uri.clone(), count),
                ]
            );
        });
    }

    /// A client that applies edits but never asks `willRenameFiles` is offered
    /// the load path edit after the rename, at the loading file.
    #[test]
    fn a_client_without_will_rename_is_offered_the_load_path_edit() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace =
                workspace_to_rename_for("offered", RENAMED, applying_client(false)).await;
            rename(&workspace).await;

            let prompt = next_request(&workspace.client, Duration::from_secs(10))
                .expect("the user is asked");
            assert_eq!(prompt.method, "window/showMessageRequest");
            workspace
                .snapshot
                .client()
                .on_response(lsp_server::Response::new_ok(
                    prompt.id,
                    serde_json::json!({ "title": "Update" }),
                ))
                .await;
            let apply = next_request(&workspace.client, Duration::from_secs(10))
                .expect("the edit is applied");
            assert_eq!(apply.method, "workspace/applyEdit");

            let params: ApplyWorkspaceEditParams =
                serde_json::from_value(apply.params).expect("applyEdit params");
            #[allow(clippy::mutable_key_type)]
            let expected = HashMap::from([(
                workspace.loader_uri.clone(),
                vec![TextEdit {
                    range: Range::new(Position::new(0, 9), Position::new(0, 19)),
                    new_text: "sh_new.lua".to_string(),
                }],
            )]);
            assert_eq!(params.edit.changes, Some(expected));
        });
    }

    /// A client that asks `willRenameFiles` already has the edit, so
    /// `didRenameFiles` does not offer it again.
    #[test]
    fn a_client_with_will_rename_is_not_offered_the_edit_again() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace =
                workspace_to_rename_for("asked_before", RENAMED, applying_client(true)).await;
            rename(&workspace).await;

            let requests =
                std::iter::from_fn(|| next_request(&workspace.client, Duration::from_secs(2)))
                    .map(|request| request.method)
                    .filter(|method| {
                        method == "window/showMessageRequest" || method == "workspace/applyEdit"
                    })
                    .collect::<Vec<_>>();
            assert!(requests.is_empty(), "{requests:?}");
        });
    }

    /// A client that cannot apply an edit is neither asked nor sent one.
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
