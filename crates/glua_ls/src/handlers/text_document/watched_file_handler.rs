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
                Some(WatchedFileType::Lua) => {
                    if file_event.typ == FileChangeType::DELETED {
                        // Only remove files that belong to this workspace.
                        // Library files (e.g. downloaded annotations) may
                        // receive spurious delete events and must not be
                        // purged from the index.
                        if workspace.is_workspace_file(&file_event.uri) {
                            lua_events.push((file_event.uri, FileChangeType::DELETED));
                        }
                        continue;
                    }

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

    let Ok(lua_batch) =
        tokio::task::spawn_blocking(move || read_lua_events(lua_events, &encoding)).await
    else {
        return None;
    };

    // Only the text changes under the write lock: the analyser sorts the batch
    // and keeps the last event per URI, so a delete/create pair lands as the
    // final state on disk. The re-analysis settles in the debounced loop's
    // slices, as an edit does, so requests are answered while it runs.
    let final_deleted = finally_deleted_uris(&lua_batch);
    let staged = {
        let mut analysis = context.analysis().write().await;
        let staged = analysis
            .stage_file_system_changes(lua_batch)
            .into_iter()
            .filter_map(|file_id| Some((file_id, analysis.get_uri(file_id)?)))
            .collect::<Vec<_>>();
        staged
    };
    let debounced_analysis = context.debounced_analysis_arc();
    for (file_id, uri) in &staged {
        debounced_analysis.schedule(*file_id, uri.clone()).await;
    }
    // The files whose text changed still need diagnosing; the loop has the
    // store diagnose the other files the re-analysis moves.
    let (deleted, changed): (Vec<_>, Vec<_>) = staged
        .into_iter()
        .partition(|(_, uri)| final_deleted.contains(uri));
    let store = context.file_diagnostic().store();
    for (file_id, uri) in &deleted {
        store.remove(*file_id, uri);
    }
    let file_ids = changed
        .into_iter()
        .map(|(file_id, _)| file_id)
        .collect::<Vec<_>>();
    store.invalidate(file_ids, &[]);

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
            let workspace = std::env::temp_dir().join("gmod_glua_ls_watched_deletion");
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
