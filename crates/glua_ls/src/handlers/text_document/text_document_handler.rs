use glua_code_analysis::{
    DeferredVfsDrop, Emmyrc, FileId, fetch_schema_urls, read_file_with_encoding, uri_to_file_path,
};
use glua_parser::{LineIndex, LuaParser, LuaSyntaxTree};
use lsp_types::{
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    VersionedTextDocumentIdentifier,
};
use rowan::NodeCache;
use std::sync::Arc;
use std::time::Duration;

use crate::context::ServerContextSnapshot;

struct PreparsedDocument {
    tree: LuaSyntaxTree,
    line_index: LineIndex,
}

fn spawn_deferred_drop(deferred_drop: DeferredVfsDrop) {
    tokio::task::spawn_blocking(move || drop(deferred_drop));
}

/// Whether an update opens the document, starting a new editor session, or
/// changes it within the session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DocumentUpdate {
    Open,
    Change,
}

fn should_drop_stale_version(
    context: &ServerContextSnapshot,
    uri: &lsp_types::Uri,
    version: i32,
) -> bool {
    context.has_newer_seen_document_version(uri, version)
}

async fn apply_document_update_without_queuing(
    context: &ServerContextSnapshot,
    uri: &lsp_types::Uri,
    text: String,
    version: i32,
    update: DocumentUpdate,
    mut preparsed: Option<PreparsedDocument>,
) -> Option<FileId> {
    if should_drop_stale_version(context, uri, version) {
        return None;
    }

    // Fair-queued `write().await`, not a `try_write` spin, which can starve
    // for seconds under a stream of readers.
    //
    // Profiling: when a didChange accumulator is scoped, split the wait for
    // the write lock from the hold so `[profile] ls_did_change` can report
    // `analysis_write_wait_ms` vs `analysis_write_hold_ms`. Zero clock reads
    // when unset or when called outside a didChange (e.g. didOpen).
    let profile = crate::util::ls_profile::ls_profile_enabled()
        && crate::util::ls_profile::LS_DID_CHANGE_ACCUM
            .try_with(|_| ())
            .is_ok();
    let wait_start = profile.then(std::time::Instant::now);
    let mut analysis = context.analysis().write().await;
    if let Some(start) = wait_start {
        crate::util::ls_profile::record_did_change_write_wait(start.elapsed());
    }
    let hold_start = profile.then(std::time::Instant::now);

    // The lock wait is unbounded, so re-check staleness now that we hold it.
    if should_drop_stale_version(context, uri, version) {
        drop(analysis);
        if let Some(start) = hold_start {
            crate::util::ls_profile::record_did_change_write_hold(start.elapsed());
        }
        return None;
    }

    // Versions order one session's edits: a reopened document starts over,
    // even if the close never got to forget the last session's version.
    if update == DocumentUpdate::Open {
        analysis.forget_document_version(uri);
    }

    // Only the text is stored; the debounced loop re-analyses it and
    // invalidates the shared diagnostic data under its own write lock.
    let mut deferred_drop = None;
    let outcome = if let Some(preparsed) = preparsed.take() {
        let Some((file_id, drop)) = analysis.update_file_preparsed_deferred(
            uri.clone(),
            Some(text),
            preparsed.tree,
            preparsed.line_index,
            Some(version),
        ) else {
            drop(analysis);
            if let Some(start) = hold_start {
                crate::util::ls_profile::record_did_change_write_hold(start.elapsed());
            }
            return None;
        };
        deferred_drop = Some(drop);
        Some(file_id)
    } else {
        analysis.update_file_text_only(uri, text)
    };
    drop(analysis);
    if let Some(start) = hold_start {
        crate::util::ls_profile::record_did_change_write_hold(start.elapsed());
    }

    if let Some(deferred_drop) = deferred_drop {
        spawn_deferred_drop(deferred_drop);
    }

    outcome
}

async fn check_schema_update(context: &ServerContextSnapshot) {
    let urls = {
        let read_analysis = context.analysis().read().await;
        if !read_analysis.check_schema_update() {
            return;
        }

        read_analysis.get_schemas_to_fetch()
    };

    if urls.is_empty() {
        return;
    }

    let url_contents = fetch_schema_urls(urls).await;

    let mut write_analysis = context.analysis().write().await;
    write_analysis.apply_fetched_schemas(url_contents);
}

async fn preparse_document(text: String, emmyrc: Arc<Emmyrc>) -> Option<PreparsedDocument> {
    let parsed = tokio::task::spawn_blocking(move || {
        let mut node_cache = NodeCache::default();
        let line_index = LineIndex::parse(&text);
        let parse_config = emmyrc.get_parse_config(&mut node_cache);
        let tree = LuaParser::parse(&text, parse_config);
        PreparsedDocument { tree, line_index }
    })
    .await;
    parsed
        .inspect_err(|err| log::error!("failed to preparse text document: {}", err))
        .ok()
}

pub async fn on_did_open_text_document(
    context: ServerContextSnapshot,
    params: DidOpenTextDocumentParams,
) -> Option<()> {
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;
    // Check if file should be filtered before acquiring locks
    // Follow lock order: workspace_manager (read) -> analysis (write)
    let should_process = {
        let analysis = context.analysis().read().await;
        let old_file_id = analysis.get_file_id(&uri);
        if old_file_id.is_some() {
            true
        } else {
            drop(analysis);
            let workspace_manager = context.workspace_manager().read().await;
            workspace_manager.is_workspace_file(&uri)
        }
    };

    if !should_process {
        context.mark_document_closed(&uri);
        return None;
    }

    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let emmyrc = {
        let analysis = context.analysis().read().await;
        analysis.get_emmyrc()
    };
    let preparsed = preparse_document(text.clone(), emmyrc).await;
    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let file_id = apply_document_update_without_queuing(
        &context,
        &uri,
        text,
        version,
        DocumentUpdate::Open,
        preparsed,
    )
    .await;
    if let Some(file_id) = file_id {
        context.note_document_applied_version(&uri, version);
        // Text that differs from what is indexed settles through the
        // debounced loop, as an edit does.
        let store = context.file_diagnostic().store();
        store.set_open(file_id, true);
        let analysis = context.analysis().read().await;
        if analysis.has_untaken_text(file_id) {
            drop(analysis);
            context
                .debounced_analysis()
                .schedule_at_once(file_id, uri.clone())
                .await;
        } else {
            store.publish_opened(file_id, version);
        }
    }

    Some(())
}

pub async fn on_did_save_text_document(
    context: ServerContextSnapshot,
    _: DidSaveTextDocumentParams,
) -> Option<()> {
    let emmyrc = context.analysis().read().await.get_emmyrc();
    if emmyrc.workspace.enable_reindex {
        let delay = Duration::from_millis(emmyrc.workspace.reindex_duration.max(1000));
        let workspace = context.workspace_manager().read().await;
        workspace.recheck_workspace(delay, context.clone()).await;
    }

    check_schema_update(&context).await;
    Some(())
}

/// Applies a document's full text after a didChange, already merged from the
/// notification's changes by [`crate::context::EditorDocuments`].
pub async fn on_did_change_text_document(
    context: ServerContextSnapshot,
    document: VersionedTextDocumentIdentifier,
    text: String,
) -> Option<()> {
    let VersionedTextDocumentIdentifier { uri, version } = document;

    // Single read-lock acquisition: get file_id + emmyrc + should_process
    let (emmyrc, should_process) = {
        let analysis = context.analysis().read().await;
        let file_id = analysis.get_file_id(&uri);
        let emmyrc = analysis.get_emmyrc();
        if file_id.is_some() {
            (emmyrc, true)
        } else {
            drop(analysis);
            let workspace_manager = context.workspace_manager().read().await;
            let should = workspace_manager.is_workspace_file(&uri);
            (emmyrc, should)
        }
    };

    if !should_process {
        context.mark_document_closed(&uri);
        return None;
    }

    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    // Profiling: time the off-lock parse so `ls_did_change` can report
    // `preparse_ms`. Only when a didChange accumulator is scoped; didOpen and
    // unprofiled runs pay no clock read.
    let preparse_start = (crate::util::ls_profile::ls_profile_enabled()
        && crate::util::ls_profile::LS_DID_CHANGE_ACCUM
            .try_with(|_| ())
            .is_ok())
    .then(std::time::Instant::now);
    let preparsed = preparse_document(text.clone(), emmyrc.clone()).await;
    if let Some(start) = preparse_start {
        crate::util::ls_profile::record_did_change_preparse(start.elapsed());
    }
    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let file_id = apply_document_update_without_queuing(
        &context,
        &uri,
        text,
        version,
        DocumentUpdate::Change,
        preparsed,
    )
    .await;
    if file_id.is_some() {
        context.note_document_applied_version(&uri, version);
    }

    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    // Only an open document is changed, and its didOpen may have been
    // superseded by this change before it took the document's ownership.
    if let Some(file_id) = file_id {
        context.file_diagnostic().store().set_open(file_id, true);
    }

    // Schedule debounced reindex — rapid edits into a single reindex.
    //
    // Profiling: `notify_to_schedule_ms` covers the applied-version note
    // through `schedule()` returning (the handoff that lets a waiting
    // request observe the edit).
    if let Some(file_id) = file_id {
        let schedule_start = (crate::util::ls_profile::ls_profile_enabled()
            && crate::util::ls_profile::LS_DID_CHANGE_ACCUM
                .try_with(|_| ())
                .is_ok())
        .then(std::time::Instant::now);
        context
            .debounced_analysis()
            .schedule(file_id, uri.clone())
            .await;
        if let Some(start) = schedule_start {
            crate::util::ls_profile::record_did_change_notify_to_schedule(start.elapsed());
        }
    }

    // Handle reindex without holding locks
    if emmyrc.workspace.enable_reindex {
        let workspace = context.workspace_manager().read().await;
        workspace.extend_recheck_delay().await;
    }

    Some(())
}

pub async fn on_did_close_document(
    context: ServerContextSnapshot,
    params: DidCloseTextDocumentParams,
) -> Option<()> {
    let uri = &params.text_document.uri;
    let encoding = {
        let analysis = context.analysis().read().await;
        if let Some(file_id) = analysis.get_file_id(uri) {
            context.file_diagnostic().store().set_open(file_id, false);
        }
        analysis.get_emmyrc().workspace.encoding.clone()
    };

    // Only remove from the index when the file no longer exists on disk
    // (e.g. it was deleted while open). Files that still exist on disk —
    // including library/annotation files opened via "Go to Definition" —
    // must stay in the index, but their in-memory contents need to revert
    // to the on-disk state once the editor buffer closes.
    let Some(file_path) = uri_to_file_path(uri) else {
        return Some(());
    };
    let text = tokio::task::spawn_blocking(move || {
        if file_path.exists() {
            read_file_with_encoding(&file_path, &encoding).map(Some)
        } else {
            Some(None)
        }
    })
    .await
    .ok()??;
    if !context.is_document_closed(uri) {
        return Some(());
    }

    // Only the text changes under the write lock; the debounced loop
    // re-analyses the file and whatever read what moved.
    let deleted = text.is_none();
    let (staged, file_id) = {
        let mut analysis = context.analysis().write().await;
        if !context.is_document_closed(uri) {
            return Some(());
        }
        analysis.forget_document_version(uri);
        let staged = analysis
            .stage_file_system_changes(vec![(uri.clone(), text)])
            .into_iter()
            .next();
        (staged, analysis.get_file_id(uri))
    };
    let store = context.file_diagnostic().store();
    if deleted {
        if let Some(file_id) = file_id {
            store.remove(file_id, uri);
        }
    }
    if let Some(staged) = staged {
        context
            .debounced_analysis()
            .schedule_at_once(staged, uri.clone())
            .await;
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ServerContext;
    use crate::util::TempDir;
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::{
        ClientCapabilities, SemanticTokensWorkspaceClientCapabilities, TextDocumentItem, Uri,
        WorkspaceClientCapabilities,
    };
    use std::str::FromStr;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// The client asks for an opened document's tokens itself, so opening one
    /// sends no workspace-wide semantic token refresh.
    #[gtest]
    fn opening_a_document_sends_no_semantic_tokens_refresh() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (proxy_connection, peer_connection) = Connection::memory();
        let capabilities = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                semantic_tokens: Some(SemanticTokensWorkspaceClientCapabilities {
                    refresh_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        runtime.block_on(async {
            let context = ServerContext::new(proxy_connection, capabilities);
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///test.lua").unwrap();
            snapshot
                .analysis()
                .write()
                .await
                .update_file_by_uri(&uri, Some("local x = 1".to_string()));

            open(&snapshot, &uri, 1, "local x = 2").await;
            verify_that!(
                snapshot
                    .wait_until_latest_document_version_applied(&uri, &CancellationToken::new())
                    .await,
                eq(true)
            )?;

            let refreshed = peer_connection.receiver.try_iter().any(|message| {
                matches!(message, lsp_server::Message::Request(request)
                    if request.method == "workspace/semanticTokens/refresh")
            });
            verify_that!(refreshed, eq(false))
        })
    }

    const DISK_TEXT: &str = "local x = 1\n";
    const NEW_TEXT: &str = "local x = 1\nfunction Added() end\n";

    /// A context holding `name` on disk and indexed from that text, with the
    /// client's end of its connection and the folder holding the file.
    async fn context_with_disk_file(name: &str) -> (ServerContext, Connection, Uri, TempDir) {
        let (connection, peer) = Connection::memory();
        let context = ServerContext::new(connection, ClientCapabilities::default());
        let folder = TempDir::new(&format!("gmod_glua_ls_{name}"));
        let path = folder.join("document.lua");
        std::fs::write(&path, DISK_TEXT).expect("test file should be written");
        let uri = glua_code_analysis::file_path_to_uri(&path).expect("file URI should parse");
        context
            .snapshot()
            .analysis()
            .write()
            .await
            .update_file_by_uri(&uri, Some(DISK_TEXT.to_string()));
        (context, peer, uri, folder)
    }

    async fn open(snapshot: &ServerContextSnapshot, uri: &Uri, version: i32, text: &str) {
        snapshot.note_document_seen_version(uri, version);
        on_did_open_text_document(
            snapshot.clone(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "lua".to_string(),
                    version,
                    text: text.to_string(),
                },
            },
        )
        .await;
    }

    async fn change(snapshot: &ServerContextSnapshot, uri: &Uri, version: i32, text: &str) {
        snapshot.note_document_seen_version(uri, version);
        on_did_change_text_document(
            snapshot.clone(),
            VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            text.to_string(),
        )
        .await;
    }

    async fn close(snapshot: &ServerContextSnapshot, uri: &Uri) {
        on_did_close_document(
            snapshot.clone(),
            DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            },
        )
        .await;
    }

    /// The reopened session's edit is indexed, and a request waiting for the
    /// latest version (as semantic tokens do) is answered.
    async fn verify_new_session_applied(snapshot: &ServerContextSnapshot, uri: &Uri) -> Result<()> {
        let held = {
            let analysis = snapshot.analysis().read().await;
            let vfs = analysis.compilation.get_db().get_vfs();
            analysis
                .get_file_id(uri)
                .and_then(|file_id| vfs.latest_content(&file_id).cloned())
        };
        verify_that!(held.as_deref(), some(eq(NEW_TEXT)))?;
        let answered = tokio::time::timeout(
            Duration::from_secs(5),
            snapshot.wait_until_latest_document_version_applied(uri, &CancellationToken::new()),
        )
        .await;
        verify_that!(answered, ok(eq(&true)))
    }

    /// An edit undone back to the disk text reaches the server as one change
    /// with the disk text, so the close finds nothing to restage.
    #[gtest]
    fn a_document_reopened_after_an_undone_edit_takes_its_new_edits() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (context, _peer, uri, _folder) = context_with_disk_file("reopen_after_undo").await;
            let snapshot = context.snapshot();
            open(&snapshot, &uri, 1, DISK_TEXT).await;
            change(&snapshot, &uri, 3, DISK_TEXT).await;
            snapshot.mark_document_closed(&uri);
            close(&snapshot, &uri).await;

            open(&snapshot, &uri, 1, DISK_TEXT).await;
            change(&snapshot, &uri, 2, NEW_TEXT).await;

            verify_new_session_applied(&snapshot, &uri).await
        })
    }

    /// A reopen that arrives before the close handler runs leaves the close
    /// nothing to do; the reopened session still starts over.
    #[gtest]
    fn a_reopen_that_overtakes_the_close_takes_its_new_edits() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (context, _peer, uri, _folder) =
                context_with_disk_file("reopen_overtakes_close").await;
            let snapshot = context.snapshot();
            open(&snapshot, &uri, 1, DISK_TEXT).await;
            change(&snapshot, &uri, 3, "local x = 3\n").await;
            snapshot.mark_document_closed(&uri);
            snapshot.note_document_seen_version(&uri, 1);
            close(&snapshot, &uri).await;

            open(&snapshot, &uri, 1, DISK_TEXT).await;
            change(&snapshot, &uri, 2, NEW_TEXT).await;

            verify_new_session_applied(&snapshot, &uri).await
        })
    }

    /// A change that supersedes the document's didOpen before it ran makes
    /// the document open all the same.
    #[gtest]
    fn a_change_that_supersedes_the_open_opens_the_document() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (context, _peer, uri, _folder) = context_with_disk_file("open_superseded").await;
            let snapshot = context.snapshot();
            snapshot.note_document_seen_version(&uri, 2);
            on_did_open_text_document(
                snapshot.clone(),
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "lua".to_string(),
                        version: 1,
                        text: DISK_TEXT.to_string(),
                    },
                },
            )
            .await;
            change(&snapshot, &uri, 2, NEW_TEXT).await;

            let file_id = snapshot
                .analysis()
                .read()
                .await
                .get_file_id(&uri)
                .expect("the document is indexed");
            let store = snapshot.file_diagnostic().store();
            verify_that!(store.any_open(&[file_id], &[]), eq(true))?;

            close(&snapshot, &uri).await;
            verify_that!(store.any_open(&[file_id], &[]), eq(false))
        })
    }

    /// An edit is staged until a take installs it; a request that reads
    /// only the document's syntax waits for that take and then reads the
    /// edited text.
    #[gtest]
    fn a_syntax_request_reads_the_edit_once_it_is_taken() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (context, _peer, uri, _folder) =
                context_with_disk_file("syntax_request_taken").await;
            let snapshot = context.snapshot();
            open(&snapshot, &uri, 1, DISK_TEXT).await;
            change(&snapshot, &uri, 2, NEW_TEXT).await;

            let taken = tokio::time::timeout(
                Duration::from_secs(5),
                snapshot.debounced_analysis().wait_until_text_taken_for(
                    &CancellationToken::new(),
                    "textDocument/formatting",
                    &uri,
                ),
            )
            .await;
            verify_that!(taken, ok(eq(&crate::context::Freshness::Fresh)))?;
            let read = {
                let analysis = snapshot.analysis().read().await;
                let vfs = analysis.compilation.get_db().get_vfs();
                analysis
                    .get_file_id(&uri)
                    .and_then(|file_id| vfs.get_file_content(&file_id).cloned())
            };
            verify_that!(read.as_deref(), some(eq(NEW_TEXT)))
        })
    }
}
