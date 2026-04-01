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
    let mut watched_lua_files: Vec<(Uri, Option<String>)> = Vec::new();
    let mut deleted_lua_uris: Vec<Uri> = Vec::new();
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
                            deleted_lua_uris.push(file_event.uri);
                        }
                        continue;
                    }

                    if !workspace.current_open_files.contains(&file_event.uri)
                        && workspace.is_workspace_file(&file_event.uri)
                    {
                        collect_lua_files(
                            &mut watched_lua_files,
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

    // Apply mutations under the write lock
    let file_ids = {
        let mut analysis = context.analysis().write().await;

        for uri in &deleted_lua_uris {
            analysis.remove_file_by_uri(uri);
        }

        analysis.update_files_by_uri(watched_lua_files)
    };

    // Schedule diagnostics and config reloads (no locks needed)
    if !lsp_features.supports_pull_diagnostic() {
        for uri in &deleted_lua_uris {
            context
                .file_diagnostic()
                .clear_push_file_diagnostics(uri.clone())
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
