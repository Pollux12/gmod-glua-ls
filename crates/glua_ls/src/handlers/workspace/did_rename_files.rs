use std::collections::{BTreeMap, HashMap};

use glua_code_analysis::{
    DbIndex, FileId, FileRenames, LuaDocument, Vfs, file_path_to_uri, load_path_rename_edits,
    read_file_with_encoding, uri_to_file_path,
};
use glua_parser::LineIndex;
use lsp_types::{
    ApplyWorkspaceEditParams, DocumentChanges, MessageActionItem, MessageType, OneOf3,
    OptionalVersionedTextDocumentIdentifier, RenameFilesParams, ShowMessageRequestParams,
    TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};
use rowan::TextRange;

use super::will_rename_files::file_renames;
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
            offered_edits(analysis.compilation.get_db(), &renames)
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

/// A load path edit as offered: the text it replaces in its file as indexed
/// before the rename, and where.
struct OfferedEdit {
    file_id: FileId,
    range: TextRange,
    old_text: String,
    new_text: String,
}

fn offered_edits(db: &DbIndex, renames: &FileRenames) -> Vec<OfferedEdit> {
    let vfs = db.get_vfs();
    load_path_rename_edits(db, renames)
        .into_iter()
        .filter_map(|edit| {
            let old_text = vfs
                .get_file_content(&edit.file_id)?
                .get(std::ops::Range::<usize>::from(edit.range))?
                .to_string();
            Some(OfferedEdit {
                file_id: edit.file_id,
                range: edit.range,
                old_text,
                new_text: edit.new_text,
            })
        })
        .collect()
}

/// The `edits` whose text is still where it was offered in the newest text the
/// server holds of each file, addressed to where the file is now. When the
/// client checks versions, each names the editor version that text came from,
/// or none for text read from disk.
fn current_edit(
    supports_document_changes: bool,
    vfs: &Vfs,
    edits: Vec<OfferedEdit>,
) -> Option<WorkspaceEdit> {
    let mut by_file: BTreeMap<FileId, Vec<OfferedEdit>> = BTreeMap::new();
    for edit in edits {
        by_file.entry(edit.file_id).or_default().push(edit);
    }
    let mut by_uri: Vec<(Uri, Option<i32>, Vec<TextEdit>)> = Vec::new();
    for (file_id, edits) in by_file {
        let (Some(text), Some(path), Some(uri)) = (
            vfs.latest_content(&file_id),
            vfs.get_file_path(&file_id),
            vfs.get_uri(&file_id),
        ) else {
            continue;
        };
        let line_index = LineIndex::parse(text);
        let document = LuaDocument::new(file_id, path, text, &line_index);
        let text_edits = edits
            .into_iter()
            .filter(|edit| {
                text.get(std::ops::Range::<usize>::from(edit.range)) == Some(edit.old_text.as_str())
            })
            .filter_map(|edit| {
                Some(TextEdit {
                    range: document.to_lsp_range(edit.range)?,
                    new_text: edit.new_text,
                })
            })
            .collect::<Vec<_>>();
        if !text_edits.is_empty() {
            by_uri.push((uri, vfs.latest_version(&file_id), text_edits));
        }
    }
    if by_uri.is_empty() {
        return None;
    }
    by_uri.sort_by(|(left, ..), (right, ..)| left.as_str().cmp(right.as_str()));
    if !supports_document_changes {
        return Some(WorkspaceEdit {
            changes: Some(
                by_uri
                    .into_iter()
                    .map(|(uri, _, text_edits)| (uri, text_edits))
                    .collect(),
            ),
            ..Default::default()
        });
    }
    let document_edits = by_uri
        .into_iter()
        .map(|(uri, version, text_edits)| TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
            edits: text_edits.into_iter().map(OneOf3::Left).collect(),
        })
        .collect();
    Some(WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(document_edits)),
        ..Default::default()
    })
}

/// Asks the user whether to rewrite the load paths naming the renamed files,
/// and applies `edits` if so. A moved file keeps its id, so each edit goes to
/// where its file is now; an edit whose text changed while the user decided
/// is dropped. Shutting down ends an unanswered request.
async fn offer_load_path_edits(context: ServerContextSnapshot, edits: Vec<OfferedEdit>) {
    let shutdown = context.debounced_analysis().shutdown_token();
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
            shutdown.clone(),
        )
        .await;
    if answer.is_none_or(|answer| answer.title != update.title) {
        return;
    }
    let edit = {
        let analysis = context.analysis().read().await;
        current_edit(
            context.lsp_features().supports_document_changes(),
            analysis.compilation.get_db().get_vfs(),
            edits,
        )
    };
    let Some(edit) = edit else {
        return;
    };
    context
        .client()
        .apply_edit(ApplyWorkspaceEditParams { label: None, edit }, shutdown)
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

    /// Accepts the load path offer `workspace`'s rename made, and returns the
    /// edit the server then applies, if any.
    async fn accept_offer(workspace: &RenamedWorkspace) -> Option<ApplyWorkspaceEditParams> {
        let prompt =
            next_request(&workspace.client, Duration::from_secs(10)).expect("the user is asked");
        assert_eq!(prompt.method, "window/showMessageRequest");
        accept(workspace, prompt).await
    }

    async fn accept(
        workspace: &RenamedWorkspace,
        prompt: lsp_server::Request,
    ) -> Option<ApplyWorkspaceEditParams> {
        workspace
            .snapshot
            .client()
            .on_response(lsp_server::Response::new_ok(
                prompt.id,
                serde_json::json!({ "title": "Update" }),
            ))
            .await;
        let apply = next_request(&workspace.client, Duration::from_secs(2))?;
        assert_eq!(apply.method, "workspace/applyEdit");
        Some(serde_json::from_value(apply.params).expect("applyEdit params"))
    }

    /// An edit to the loading file while the user decides leaves its load path
    /// edit out: it was made for text that is no longer there.
    #[test]
    fn a_load_path_edit_is_dropped_when_its_text_changed_before_acceptance() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace =
                workspace_to_rename_for("changed", RENAMED, applying_client(false)).await;
            rename(&workspace).await;
            let prompt = next_request(&workspace.client, Duration::from_secs(10))
                .expect("the user is asked");

            workspace
                .snapshot
                .analysis()
                .write()
                .await
                .update_file_by_uri(
                    &workspace.loader_uri,
                    Some(
                        "print(\"sh_old.lua was here\")
"
                        .to_string(),
                    ),
                );

            assert!(accept(&workspace, prompt).await.is_none());
        });
    }

    /// Newer text the server holds but has not indexed yet, as a closed
    /// document's text read back from disk, is what an offered edit is
    /// checked against.
    #[test]
    fn a_load_path_edit_is_checked_against_the_newest_text_held() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace =
                workspace_to_rename_for("staged", RENAMED, applying_client(false)).await;
            rename(&workspace).await;
            let prompt = next_request(&workspace.client, Duration::from_secs(10))
                .expect("the user is asked");

            workspace
                .snapshot
                .analysis()
                .write()
                .await
                .stage_file_system_changes(vec![(
                    workspace.loader_uri.clone(),
                    Some("print(1)\n".to_string()),
                )]);

            assert!(accept(&workspace, prompt).await.is_none());
        });
    }

    /// A client that checks document versions gets the edit for the editor
    /// version of the text it was checked against.
    #[test]
    fn a_load_path_edit_names_the_document_version_it_was_made_for() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let mut capabilities = applying_client(false);
            capabilities
                .workspace
                .as_mut()
                .expect("workspace capabilities")
                .workspace_edit = Some(lsp_types::WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                ..Default::default()
            });
            let workspace = workspace_to_rename_for("versioned", RENAMED, capabilities).await;
            {
                let mut analysis = workspace.snapshot.analysis().write().await;
                let loader = analysis.get_file_id(&workspace.loader_uri).expect("loader");
                analysis
                    .compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&loader, Some(7));
            }
            rename(&workspace).await;

            let params = accept_offer(&workspace).await.expect("the edit is applied");

            assert_eq!(
                params.edit.document_changes,
                Some(lsp_types::DocumentChanges::Edits(vec![
                    lsp_types::TextDocumentEdit {
                        text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                            uri: workspace.loader_uri.clone(),
                            version: Some(7),
                        },
                        edits: vec![lsp_types::OneOf3::Left(TextEdit {
                            range: Range::new(Position::new(0, 9), Position::new(0, 19)),
                            new_text: "sh_new.lua".to_string(),
                        })],
                    }
                ]))
            );
        });
    }

    /// A load path edit the user never answers does not hold the connection
    /// open past shutdown.
    #[test]
    fn an_unanswered_load_path_offer_ends_at_shutdown() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let workspace =
                workspace_to_rename_for("unanswered", RENAMED, applying_client(false)).await;
            rename(&workspace).await;
            let prompt = next_request(&workspace.client, Duration::from_secs(10))
                .expect("the user is asked");
            assert_eq!(prompt.method, "window/showMessageRequest");

            let RenamedWorkspace {
                snapshot,
                client,
                _context: context,
                _root: root,
                ..
            } = workspace;
            context.close().await;
            drop(snapshot);
            drop(context);

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let disconnected = loop {
                let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                    break false;
                };
                match client.receiver.recv_timeout(left) {
                    Ok(_) => continue,
                    Err(error) => break error.is_disconnected(),
                }
            };
            drop(root);
            assert!(disconnected, "the connection outlived shutdown");
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
