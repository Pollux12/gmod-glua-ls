use std::collections::HashMap;
use std::path::PathBuf;

use glua_code_analysis::{read_file_with_encoding, uri_to_file_path};
use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, Uri};

use crate::{codestyle::should_apply_editorconfig_updates, context::ServerContextSnapshot};

pub async fn on_did_change_watched_files(
    context: ServerContextSnapshot,
    params: DidChangeWatchedFilesParams,
) -> Option<()> {
    // Classify events and read files from disk WITHOUT
    // the analysis write lock so that slow disk I/O does not block
    // hover, completion, and diagnostic handlers.
    let (encoding, interval, apply_editorconfig_updates) = {
        let analysis = context.analysis().read().await;
        let emmyrc = analysis.get_emmyrc();
        (
            emmyrc.workspace.encoding.clone(),
            emmyrc.diagnostics.diagnostic_interval.unwrap_or(500),
            should_apply_editorconfig_updates(&emmyrc),
        )
    };

    let lsp_features = context.lsp_features();
    let mut lua_batch: Vec<(Uri, Option<String>)> = Vec::new();
    let mut editorconfig_paths: Vec<PathBuf> = Vec::new();
    let mut emmyrc_dirs: Vec<PathBuf> = Vec::new();

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
                            lua_batch.push((file_event.uri.clone(), None));
                        }
                        continue;
                    }

                    if !workspace.current_open_files.contains(&file_event.uri)
                        && workspace.is_workspace_file(&file_event.uri)
                    {
                        collect_lua_files(
                            &mut lua_batch,
                            file_event.uri,
                            file_event.typ,
                            &encoding,
                        );
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
                    // Treat DELETE the same as CREATE/CHANGE: the config is
                    // gone so the workspace must reload with defaults (or a
                    // parent config).  Derive the parent dir from the URI
                    // directly since the file no longer exists on disk.
                    if let Some(path) = uri_to_file_path(&file_event.uri) {
                        if let Some(dir) = path.parent() {
                            emmyrc_dirs.push(dir.to_path_buf());
                        }
                    }
                }
                None => {}
            }
        }
    } // workspace read lock released here, before any write lock

    // Apply mutations under the write lock. Changes and deletions settle in one
    // batch: the analyser derives its facts over a whole batch, and a branch
    // switch that deletes several files would otherwise pay one ripple each.
    let final_deleted = finally_deleted_uris(&lua_batch);
    let file_ids = {
        let mut analysis = context.analysis().write().await;

        // Deletions and changes arrive as one order-preserving batch; the
        // analyser sorts, keeps only the last event per URI and settles in one
        // pass, so a delete/create pair lands as the final state on disk.
        let batch = lua_batch;

        // The files the settle touched, which is what still needs diagnosing.
        // It is not every file whose diagnostics could differ — a reader that
        // resolves a changed fact only when it is diagnosed caches nothing the
        // export diff can name — but that is already true of every keystroke:
        // the edit path publishes for the edited file and leaves the rest to a
        // pull. Paying a full dependency expansion here bought a refresh the
        // dominant path never performed.
        let file_ids = analysis.apply_file_system_changes(batch);
        if !file_ids.is_empty() || !final_deleted.is_empty() {
            context
                .file_diagnostic()
                .invalidate_shared_diagnostic_data();
        }
        file_ids
    };

    // Schedule diagnostics and config reloads (no locks needed). A URI whose
    // final batch event leaves the file present is skipped in both branches:
    // identical bytes stay valid in any cached report, and distinct bytes are
    // re-diagnosed via the file_ids task below.
    if !lsp_features.supports_pull_diagnostic() {
        for uri in final_deleted {
            context
                .file_diagnostic()
                .clear_push_file_diagnostics(uri.clone())
                .await;
        }
    } else {
        // Never replay a report for a file that no longer exists.
        for uri in &final_deleted {
            context
                .file_diagnostic()
                .forget_cached_file_diagnostics(uri)
                .await;
        }
    }

    context
        .file_diagnostic()
        .add_files_diagnostic_task(file_ids, interval, Some(context.debounced_analysis_arc()))
        .await;

    // Handle editorconfig / emmyrc updates
    {
        let workspace = context.workspace_manager().read().await;
        if apply_editorconfig_updates {
            for path in &editorconfig_paths {
                workspace.update_editorconfig(path.clone());
            }
        } else if !editorconfig_paths.is_empty() {
            log::info!(
                "skipping .editorconfig watched-file update because format.configPrecedence=preferGluarc"
            );
        }
        for dir in emmyrc_dirs {
            workspace
                .add_update_emmyrc_task(dir, context.workspace_manager_arc())
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

fn collect_lua_files(
    watched_lua_files: &mut Vec<(Uri, Option<String>)>,
    uri: Uri,
    file_change_event: FileChangeType,
    encoding: &str,
) {
    match file_change_event {
        FileChangeType::CREATED | FileChangeType::CHANGED => {
            let Some(path) = uri_to_file_path(&uri) else {
                return;
            };
            // Only push the file if we can actually read it. A transient read
            // failure (file locked by antivirus, editor mid-save, etc.) must NOT
            // be treated as a deletion — just skip the update for this event.
            if let Some(text) = read_file_with_encoding(&path, encoding) {
                watched_lua_files.push((uri, Some(text)));
            }
        }
        FileChangeType::DELETED => {
            watched_lua_files.push((uri, None));
        }
        _ => {}
    }
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
