use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use glua_code_analysis::{read_file_with_encoding, uri_to_file_path};
use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, Uri};

use crate::{
    codestyle::{apply_editorconfig_file, should_apply_editorconfig_updates},
    context::ServerContextSnapshot,
};

pub async fn on_did_change_watched_files(
    context: ServerContextSnapshot,
    params: DidChangeWatchedFilesParams,
) -> Option<()> {
    let started = crate::util::ls_profile::profile_instant();
    let mut stages = Vec::new();
    let mut stage = |name: &'static str, at: &mut Option<std::time::Instant>| {
        if let Some(since) = at.replace(std::time::Instant::now()) {
            stages.push((name, since.elapsed()));
        }
    };
    let mut at = started;
    // Classify events and read files from disk WITHOUT
    // the analysis write lock so that slow disk I/O does not block
    // hover, completion, and diagnostic handlers.
    let (encoding, apply_editorconfig_updates) = {
        let analysis = context.analysis().read().await;
        let emmyrc = analysis.get_emmyrc();
        (
            emmyrc.workspace.encoding.clone(),
            should_apply_editorconfig_updates(&emmyrc),
        )
    };

    let mut lua_events: Vec<(Uri, FileChangeType)> = Vec::new();
    let mut editorconfig_paths: Vec<PathBuf> = Vec::new();
    let mut config_changed = false;

    {
        let workspace = context.workspace_manager().read().await;

        for file_event in params.changes.into_iter() {
            let file_type = get_file_type(&file_event.uri);
            match file_type {
                // The editor owns an open document's text, deleted on disk
                // or not: closing it reads the disk again. Library files
                // (e.g. downloaded annotations) may receive spurious delete
                // events and must not be purged from the index.
                Some(WatchedFileType::Lua) => {
                    if !workspace.current_open_files.contains(&file_event.uri)
                        && workspace.is_workspace_file(&file_event.uri)
                    {
                        lua_events.push((file_event.uri, file_event.typ));
                    }
                }
                Some(WatchedFileType::Editorconfig) => {
                    if file_event.typ != FileChangeType::DELETED {
                        if let Some(path) = uri_to_file_path(&file_event.uri) {
                            editorconfig_paths.push(path);
                        }
                    }
                }
                Some(WatchedFileType::Emmyrc) => {
                    // A deletion counts too: another config file, or none,
                    // takes its place.
                    config_changed |= workspace.is_workspace_config_file(&file_event.uri);
                }
                None => {}
            }
        }
    } // workspace read lock released here, before any write lock
    stage("classify", &mut at);

    let Ok(lua_batch) =
        tokio::task::spawn_blocking(move || read_lua_events(lua_events, &encoding)).await
    else {
        return None;
    };

    stage("read", &mut at);
    let lua_batch = without_applied_renames(&context, lua_batch).await;
    stage("renames", &mut at);
    let files = lua_batch.len();
    apply_file_system_changes(&context, lua_batch).await;
    stage("apply", &mut at);
    crate::util::ls_profile::emit_ls_watched(files, &stages);

    // Handle editorconfig / emmyrc updates
    if apply_editorconfig_updates {
        if !editorconfig_paths.is_empty() {
            let _ = tokio::task::spawn_blocking(move || {
                for path in &editorconfig_paths {
                    log::info!("update code style: {:?}", path);
                    let _ = apply_editorconfig_file(path);
                }
            })
            .await;
        }
    } else if !editorconfig_paths.is_empty() {
        log::info!(
            "skipping .editorconfig watched-file update because format.configPrecedence=preferGluarc"
        );
    }
    {
        let workspace = context.workspace_manager().read().await;
        if config_changed {
            workspace
                .schedule_config_reload(Duration::from_secs(2), context.workspace_manager_arc())
                .await;
        }
    }

    Some(())
}

/// Applies a batch of file-system changes: each URI's new text, or `None` for
/// a deletion.
pub(crate) async fn apply_file_system_changes(
    context: &ServerContextSnapshot,
    batch: Vec<(Uri, Option<String>)>,
) {
    if batch.is_empty() {
        return;
    }
    // Only the text changes under the write lock: the analyser sorts the batch
    // and keeps the last event per URI, so a delete/create pair lands as the
    // final state on disk. The re-analysis settles in the debounced loop's
    // slices, as an edit does, so requests are answered while it runs.
    let final_deleted = finally_deleted_uris(&batch);
    let waiting = crate::util::ls_profile::profile_instant();
    let (staged, wait, hold) = {
        let mut analysis = context.analysis().write().await;
        let holding = crate::util::ls_profile::profile_instant();
        let staged = analysis
            .stage_file_system_changes(batch)
            .into_iter()
            .filter_map(|file_id| Some((file_id, analysis.get_uri(file_id)?)))
            .collect::<Vec<_>>();
        let wait = waiting
            .zip(holding)
            .map(|(waiting, holding)| holding - waiting);
        (staged, wait, holding.map(|holding| holding.elapsed()))
    };
    let scheduling = crate::util::ls_profile::profile_instant();
    context
        .debounced_analysis_arc()
        .schedule_all_at_once(&staged)
        .await;
    if let (Some(wait), Some(hold), Some(scheduling)) = (wait, hold, scheduling) {
        crate::util::ls_profile::emit_ls_watched(
            staged.len(),
            &[
                ("write_wait", wait),
                ("write_hold", hold),
                ("schedule", scheduling.elapsed()),
            ],
        );
    }
    let store = context.file_diagnostic().store();
    for (file_id, uri) in staged.iter().filter(|(_, uri)| final_deleted.contains(uri)) {
        store.remove(*file_id, uri);
    }
}

/// The batch without the watcher's events for a rename `didRenameFiles`
/// already applied: events for its URIs whose change the VFS already holds.
async fn without_applied_renames(
    context: &ServerContextSnapshot,
    batch: Vec<(Uri, Option<String>)>,
) -> Vec<(Uri, Option<String>)> {
    let renamed = context.take_renamed_uris(batch.iter().map(|(uri, _)| uri));
    if renamed.is_empty() {
        return batch;
    }
    let analysis = context.analysis().read().await;
    let vfs = analysis.compilation.get_db().get_vfs();
    batch
        .into_iter()
        .filter(|(uri, text)| {
            let current = vfs
                .get_file_id(uri)
                .and_then(|file_id| vfs.latest_content(&file_id));
            !renamed.contains(uri) || current.map(String::as_str) != text.as_deref()
        })
        .collect()
}

/// URIs whose final event in the batch is a deletion.
///
/// `apply_file_system_changes` sorts the batch (a stable sort, preserving
/// event order for equal keys) and keeps only the last event per URI, so a
/// delete/create pair settles as the file's final on-disk state. Mirror that
/// last-wins fold here so clearing/forgetting diagnostics only happens for
/// URIs that are actually gone after the batch, never for ones that were
/// re-created with identical or distinct bytes.
fn finally_deleted_uris(batch: &[(Uri, Option<String>)]) -> Vec<Uri> {
    let mut final_state: HashMap<&Uri, bool> = HashMap::new();
    for (uri, text) in batch {
        final_state.insert(uri, text.is_some());
    }
    let mut deleted: Vec<Uri> = final_state
        .into_iter()
        .filter(|(_, present)| !*present)
        .map(|(uri, _)| uri.clone())
        .collect();
    deleted.sort_by_cached_key(|uri| uri.to_string());
    deleted.dedup();
    deleted
}

/// Reads the text of each created or changed file, in event order, for the batch.
fn read_lua_events(
    events: Vec<(Uri, FileChangeType)>,
    encoding: &str,
) -> Vec<(Uri, Option<String>)> {
    events
        .into_iter()
        .filter_map(|(uri, file_change_event)| match file_change_event {
            FileChangeType::CREATED | FileChangeType::CHANGED => {
                let path = uri_to_file_path(&uri)?;
                // Only push the file if we can actually read it. A transient read
                // failure (file locked by antivirus, editor mid-save, etc.) must NOT
                // be treated as a deletion — just skip the update for this event.
                let text = read_file_with_encoding(&path, encoding)?;
                Some((uri, Some(text)))
            }
            FileChangeType::DELETED => Some((uri, None)),
            _ => None,
        })
        .collect()
}

enum WatchedFileType {
    Lua,
    Editorconfig,
    Emmyrc,
}

/// The documents a batch of watched-file changes can change: its own Lua
/// files, or every document (`None`) when a config or `.editorconfig` file is
/// among them.
pub(crate) fn watched_change_scope(params: &DidChangeWatchedFilesParams) -> Option<Vec<Uri>> {
    params
        .changes
        .iter()
        .map(|change| {
            matches!(get_file_type(&change.uri), Some(WatchedFileType::Lua))
                .then(|| change.uri.clone())
        })
        .collect()
}

fn get_file_type(uri: &Uri) -> Option<WatchedFileType> {
    let path = uri_to_file_path(uri)?;
    let file_name = path.file_name()?.to_str()?;
    match file_name {
        ".editorconfig" => Some(WatchedFileType::Editorconfig),
        ".emmyrc.json" | ".luarc.json" | ".emmyrc.lua" | ".gluarc.json" => {
            Some(WatchedFileType::Emmyrc)
        }
        _ => Some(WatchedFileType::Lua),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::finally_deleted_uris;
    use lsp_types::Uri;

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).expect("uri should parse")
    }

    /// A watched deletion only takes the file's text away under the write
    /// lock; the debounced loop re-analyses it in slices, so requests are
    /// answered while it settles.
    #[test]
    fn a_watched_deletion_is_left_to_the_debounced_loop() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = lsp_server::Connection::memory();
        runtime.block_on(async {
            let context = crate::context::ServerContext::new(
                connection,
                lsp_types::ClientCapabilities::default(),
            );
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_watched_deletion");
            let data_uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/sh_data.lua"))
                .expect("data URI should parse");
            let data = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace.clone());
                analysis
                    .update_file_by_uri(
                        &data_uri,
                        Some(
                            "Data = {}
"
                            .to_string(),
                        ),
                    )
                    .expect("data file is added")
                    .0
            };

            super::on_did_change_watched_files(
                snapshot.clone(),
                lsp_types::DidChangeWatchedFilesParams {
                    changes: vec![lsp_types::FileEvent {
                        uri: data_uri,
                        typ: lsp_types::FileChangeType::DELETED,
                    }],
                },
            )
            .await;

            let still_indexed = snapshot
                .analysis()
                .read()
                .await
                .compilation
                .is_indexed(data);
            assert!(still_indexed);
            assert!(snapshot.debounced_analysis().is_dirty());
        });
    }

    /// A file deleted on disk while open keeps the editor's text until the
    /// document closes.
    #[test]
    fn a_watched_deletion_of_an_open_document_waits_for_its_close() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = lsp_server::Connection::memory();
        runtime.block_on(async {
            let context = crate::context::ServerContext::new(
                connection,
                lsp_types::ClientCapabilities::default(),
            );
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_watched_open_deletion");
            let data_uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/sh_data.lua"))
                .expect("data URI should parse");
            {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace.clone());
                analysis
                    .update_file_by_uri(&data_uri, Some("Data = {}\n".to_string()))
                    .expect("data file is added");
            }
            snapshot
                .workspace_manager()
                .write()
                .await
                .current_open_files
                .insert(data_uri.clone());

            super::on_did_change_watched_files(
                snapshot.clone(),
                lsp_types::DidChangeWatchedFilesParams {
                    changes: vec![lsp_types::FileEvent {
                        uri: data_uri,
                        typ: lsp_types::FileChangeType::DELETED,
                    }],
                },
            )
            .await;

            assert!(!snapshot.debounced_analysis().is_dirty());
        });
    }

    /// A file changed on disk is diagnosed as soon as its own re-analysis
    /// settles, ahead of the files its change ripples into.
    #[test]
    fn a_file_changed_on_disk_is_diagnosed_first() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = lsp_server::Connection::memory();
        runtime.block_on(async {
            let context = crate::context::ServerContext::new(
                connection,
                lsp_types::ClientCapabilities::default(),
            );
            let snapshot = context.snapshot();
            let workspace = crate::util::unique_temp_dir("gmod_glua_ls_changed_on_disk");
            let data_uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/sh_data.lua"))
                .expect("data URI should parse");
            let data = {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(workspace.clone());
                analysis
                    .update_file_by_uri(&data_uri, Some("Data = {}\n".to_string()))
                    .expect("data file is added")
                    .0
            };

            super::apply_file_system_changes(
                &snapshot,
                vec![(data_uri, Some("Data = { count = 1 }\n".to_string()))],
            )
            .await;

            let priority = snapshot.file_diagnostic().store().dirty_priority(data);
            assert_eq!(
                priority,
                Some(crate::context::diagnostic_store::Priority::Edited)
            );
        });
    }

    #[test]
    fn deleted_then_changed_distinct_is_not_deleted() {
        let batch = vec![
            (uri("file:///a.lua"), None),
            (uri("file:///a.lua"), Some("return 1".to_string())),
        ];
        assert!(finally_deleted_uris(&batch).is_empty());
    }

    #[test]
    fn deleted_then_changed_same_bytes_is_not_deleted() {
        let batch = vec![
            (uri("file:///a.lua"), None),
            (uri("file:///a.lua"), Some("local x = 1".to_string())),
        ];
        assert!(finally_deleted_uris(&batch).is_empty());
    }

    #[test]
    fn changed_then_deleted_is_deleted() {
        let batch = vec![
            (uri("file:///a.lua"), Some("return 1".to_string())),
            (uri("file:///a.lua"), None),
        ];
        let deleted = finally_deleted_uris(&batch);
        assert_eq!(deleted, vec![uri("file:///a.lua")]);
    }

    #[test]
    fn deleted_twice_appears_once() {
        let batch = vec![(uri("file:///a.lua"), None), (uri("file:///a.lua"), None)];
        let deleted = finally_deleted_uris(&batch);
        assert_eq!(deleted, vec![uri("file:///a.lua")]);
    }

    #[test]
    fn multi_uri_result_is_sorted() {
        let batch = vec![
            (uri("file:///z.lua"), None),
            (uri("file:///b.lua"), Some("return 2".to_string())),
            (uri("file:///a.lua"), None),
            (uri("file:///m.lua"), None),
        ];
        let deleted = finally_deleted_uris(&batch);
        assert_eq!(
            deleted,
            vec![
                uri("file:///a.lua"),
                uri("file:///m.lua"),
                uri("file:///z.lua"),
            ]
        );
    }
}
