use glua_code_analysis::file_path_to_uri;
use log::{info, warn};
use lsp_types::{
    ClientCapabilities, DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    FileChangeType, FileEvent, FileSystemWatcher, GlobPattern, Registration, RegistrationParams,
    WatchKind,
};
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{ModifyKind, RenameMode},
};
use std::{path::PathBuf, time::Duration};

use crate::{
    context::{ClientProxy, ServerContextSnapshot},
    handlers::text_document::{is_gamemode_manifest, on_did_change_watched_files},
};

pub async fn register_files_watch(
    context: ServerContextSnapshot,
    client_capabilities: &ClientCapabilities,
) {
    let lsp_client_can_watch_files = is_lsp_client_can_watch_files(client_capabilities);

    if lsp_client_can_watch_files {
        register_files_watch_use_lsp_client(context.client());
    } else {
        info!("use notify to watch files");
        register_files_watch_use_fsnotify(context).await;
    }
}

fn is_lsp_client_can_watch_files(client_capabilities: &ClientCapabilities) -> bool {
    client_capabilities
        .workspace
        .as_ref()
        .and_then(|ws| ws.did_change_watched_files.as_ref())
        .and_then(|d| d.dynamic_registration)
        .unwrap_or_default()
}

fn register_files_watch_use_lsp_client(client: &ClientProxy) {
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.lua".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.editorconfig".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.luarc.json".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.emmyrc.json".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.emmyrc.lua".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.gluarc.json".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            // A gamemode's manifest names the gamemode it derives from.
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/gamemodes/*/*.txt".into()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
        ],
    };

    let registration = Registration {
        id: "gluals_watch_files".to_string(),
        method: "workspace/didChangeWatchedFiles".to_string(),
        register_options: Some(serde_json::to_value(options).unwrap()),
    };
    client.dynamic_register_capability(RegistrationParams {
        registrations: vec![registration],
    });
}

const WATCH_FILE_EXTENSIONS: [&str; 6] = [
    ".lua",
    ".editorconfig",
    ".luarc.json",
    ".emmyrc.json",
    ".emmyrc.lua",
    ".gluarc.json",
];

/// The watched-file change `event` makes to each of its paths. Reads and
/// events of unknown kind change no file.
fn path_changes(event: &Event) -> Vec<(&PathBuf, FileChangeType)> {
    let typ = match event.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            FileChangeType::CREATED
        }
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            FileChangeType::DELETED
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            return event
                .paths
                .iter()
                .zip([FileChangeType::DELETED, FileChangeType::CREATED])
                .collect();
        }
        EventKind::Modify(_) => FileChangeType::CHANGED,
        EventKind::Access(_) | EventKind::Any | EventKind::Other => return Vec::new(),
    };
    event.paths.iter().map(|path| (path, typ)).collect()
}

async fn register_files_watch_use_fsnotify(context: ServerContextSnapshot) -> Option<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let config = Config::default().with_poll_interval(Duration::from_secs(5));
    let mut watcher = match RecommendedWatcher::new(
        move |res| {
            if let Ok(event) = res {
                match tx.send(event) {
                    Ok(_) => {}
                    Err(e) => {
                        warn!("send notify event failed: {:?}", e);
                    }
                };
            };
        },
        config,
    ) {
        Ok(watcher) => watcher,
        Err(e) => {
            log::error!("create notify watcher failed: {:?}", e);
            return None;
        }
    };

    let mut workspace_manager = context.workspace_manager().write().await;
    for workspace in &workspace_manager.workspace_folders {
        if let Err(e) = watcher.watch(&workspace.root, RecursiveMode::Recursive) {
            warn!("can not watch {:?}: {:?}", workspace.root, e);
        }
    }
    workspace_manager.watcher = Some(watcher);
    drop(workspace_manager);

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let file_events = path_changes(&event)
                .into_iter()
                .filter(|(path, _)| {
                    let text = path.as_os_str().to_string_lossy();
                    WATCH_FILE_EXTENSIONS.iter().any(|ext| text.ends_with(ext))
                        || is_gamemode_manifest(path)
                })
                .filter_map(|(path, typ)| {
                    Some(FileEvent {
                        uri: file_path_to_uri(path)?,
                        typ,
                    })
                })
                .collect::<Vec<_>>();
            if file_events.is_empty() {
                continue;
            }
            let params = DidChangeWatchedFilesParams {
                changes: file_events,
            };
            let in_flight = context.debounced_analysis_arc().begin_in_flight_change();
            on_did_change_watched_files(context.clone(), params).await;
            in_flight.finish().await;
        }
    });

    info!("watch files use notify success");

    Some(())
}

#[cfg(test)]
mod tests {
    use notify::event::{AccessKind, CreateKind, DataChange, RemoveKind};

    use super::*;

    fn changes(kind: EventKind, paths: &[&str]) -> Vec<(PathBuf, FileChangeType)> {
        let event = paths.iter().fold(Event::new(kind), |event, path| {
            event.add_path(PathBuf::from(path))
        });
        path_changes(&event)
            .into_iter()
            .map(|(path, typ)| (path.clone(), typ))
            .collect()
    }

    /// A read or an event of unknown kind changes no file, and the watch goes
    /// on to the events after it.
    #[test]
    fn events_that_change_no_file_report_nothing() {
        assert!(changes(EventKind::Access(AccessKind::Any), &["a.lua"]).is_empty());
        assert!(changes(EventKind::Any, &["a.lua"]).is_empty());
        assert!(changes(EventKind::Other, &["a.lua"]).is_empty());
    }

    /// A renamed file is gone from its old path and created at its new one.
    #[test]
    fn a_rename_deletes_the_old_path_and_creates_the_new_one() {
        let rename = |mode| EventKind::Modify(ModifyKind::Name(mode));
        assert_eq!(
            (
                changes(rename(RenameMode::Both), &["old.lua", "new.lua"]),
                changes(rename(RenameMode::From), &["old.lua"]),
                changes(rename(RenameMode::To), &["new.lua"]),
            ),
            (
                vec![
                    (PathBuf::from("old.lua"), FileChangeType::DELETED),
                    (PathBuf::from("new.lua"), FileChangeType::CREATED),
                ],
                vec![(PathBuf::from("old.lua"), FileChangeType::DELETED)],
                vec![(PathBuf::from("new.lua"), FileChangeType::CREATED)],
            )
        );
    }

    #[test]
    fn creates_edits_and_removals_keep_their_kind() {
        assert_eq!(
            (
                changes(EventKind::Create(CreateKind::File), &["a.lua"]),
                changes(
                    EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                    &["a.lua"]
                ),
                changes(EventKind::Remove(RemoveKind::File), &["a.lua"]),
            ),
            (
                vec![(PathBuf::from("a.lua"), FileChangeType::CREATED)],
                vec![(PathBuf::from("a.lua"), FileChangeType::CHANGED)],
                vec![(PathBuf::from("a.lua"), FileChangeType::DELETED)],
            )
        );
    }
}
