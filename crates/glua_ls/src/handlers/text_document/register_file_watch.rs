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
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

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

fn is_watched_path(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy();
    WATCH_FILE_EXTENSIONS.iter().any(|ext| text.ends_with(ext)) || is_gamemode_manifest(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalescedChange {
    Single(FileChangeType),
    /// A deletion followed by a creation/modification on the same path: emit
    /// both so `on_did_change_watched_files` deletes if gone or reloads if
    /// recreated on disk.
    DeletedThen(FileChangeType),
}

impl CoalescedChange {
    fn merge(self, next: FileChangeType) -> Self {
        if next == FileChangeType::DELETED {
            return Self::Single(FileChangeType::DELETED);
        }
        match self {
            Self::Single(FileChangeType::DELETED) => Self::DeletedThen(next),
            Self::Single(FileChangeType::CREATED) => Self::Single(FileChangeType::CREATED),
            Self::Single(_) => Self::Single(next),
            Self::DeletedThen(prev) => {
                if prev == FileChangeType::CREATED {
                    Self::DeletedThen(FileChangeType::CREATED)
                } else {
                    Self::DeletedThen(next)
                }
            }
        }
    }

    fn push_into(self, path: PathBuf, out: &mut Vec<(PathBuf, FileChangeType)>) {
        match self {
            Self::Single(typ) => out.push((path, typ)),
            Self::DeletedThen(next) => {
                out.push((path.clone(), FileChangeType::DELETED));
                out.push((path, next));
            }
        }
    }
}

#[derive(Default)]
struct PendingWatchedFiles {
    entries: HashMap<PathBuf, (u64, CoalescedChange)>,
    next_seq: u64,
    closed: bool,
}

impl PendingWatchedFiles {
    fn push_event(&mut self, event: &Event) -> bool {
        if self.closed {
            return false;
        }
        let mut added = false;
        for (path, typ) in path_changes(event) {
            if !is_watched_path(path) {
                continue;
            }
            self.push_change(path.clone(), typ);
            added = true;
        }
        added
    }

    fn push_change(&mut self, path: PathBuf, typ: FileChangeType) {
        if let Some((_, existing)) = self.entries.get_mut(&path) {
            *existing = existing.merge(typ);
        } else {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.entries
                .insert(path, (seq, CoalescedChange::Single(typ)));
        }
    }

    fn take_batch(&mut self) -> Vec<(PathBuf, FileChangeType)> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let mut ordered: Vec<_> = self.entries.drain().collect();
        ordered.sort_unstable_by_key(|(_, (seq, _))| *seq);
        let mut changes = Vec::with_capacity(ordered.len());
        for (path, (_, change)) in ordered {
            change.push_into(path, &mut changes);
        }
        changes
    }
}

struct SharedWatchQueue {
    pending: Mutex<PendingWatchedFiles>,
    notify: Notify,
}

impl SharedWatchQueue {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, PendingWatchedFiles> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct WatchQueueSender(Arc<SharedWatchQueue>);

impl Drop for WatchQueueSender {
    fn drop(&mut self) {
        self.0.lock_pending().closed = true;
        self.0.notify.notify_one();
    }
}

async fn register_files_watch_use_fsnotify(context: ServerContextSnapshot) -> Option<()> {
    let queue = Arc::new(SharedWatchQueue {
        pending: Mutex::new(PendingWatchedFiles::default()),
        notify: Notify::new(),
    });
    let sender = WatchQueueSender(queue.clone());
    let config = Config::default().with_poll_interval(Duration::from_secs(5));
    let mut watcher = match RecommendedWatcher::new(
        move |res| {
            if let Ok(event) = res {
                let added = sender.0.lock_pending().push_event(&event);
                if added {
                    sender.0.notify.notify_one();
                }
            }
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
        let shutdown = context.debounced_analysis_arc().shutdown_token();
        loop {
            let batch = loop {
                let notified = queue.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                let taken = {
                    let mut pending = queue.lock_pending();
                    if !pending.entries.is_empty() {
                        Some(pending.take_batch())
                    } else if pending.closed {
                        return;
                    } else {
                        None
                    }
                };

                if let Some(batch) = taken {
                    break batch;
                }

                tokio::select! {
                    _ = notified => {}
                    _ = shutdown.cancelled() => {
                        let mut pending = queue.lock_pending();
                        pending.closed = true;
                        pending.entries.clear();
                        return;
                    }
                }
            };

            let file_events = batch
                .into_iter()
                .filter_map(|(path, typ)| {
                    Some(FileEvent {
                        uri: file_path_to_uri(&path)?,
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

    #[test]
    fn pending_watch_queue_filters_and_coalesces_per_path() {
        let mut pending = PendingWatchedFiles::default();
        let make = |kind, path: &str| Event::new(kind).add_path(PathBuf::from(path));

        // Non-watched paths are ignored before entering the queue.
        assert!(!pending.push_event(&make(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            "README.md"
        )));
        assert!(pending.entries.is_empty());

        // Repeated edits to a.lua coalesce to one entry; b.lua create + modify stays CREATED.
        assert!(pending.push_event(&make(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            "a.lua"
        )));
        assert!(pending.push_event(&make(EventKind::Create(CreateKind::File), "b.lua")));
        assert!(pending.push_event(&make(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            "b.lua"
        )));
        assert!(pending.push_event(&make(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            "a.lua"
        )));

        // Delete followed by create or modify preserves both events so a
        // missing file is still deleted while a recreated file is reloaded.
        assert!(pending.push_event(&make(EventKind::Remove(RemoveKind::File), "c.lua")));
        assert!(pending.push_event(&make(EventKind::Create(CreateKind::File), "c.lua")));
        assert!(pending.push_event(&make(EventKind::Remove(RemoveKind::File), "d.lua")));
        assert!(pending.push_event(&make(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            "d.lua"
        )));

        assert_eq!(pending.entries.len(), 4);
        assert_eq!(
            pending.take_batch(),
            vec![
                (PathBuf::from("a.lua"), FileChangeType::CHANGED),
                (PathBuf::from("b.lua"), FileChangeType::CREATED),
                (PathBuf::from("c.lua"), FileChangeType::DELETED),
                (PathBuf::from("c.lua"), FileChangeType::CREATED),
                (PathBuf::from("d.lua"), FileChangeType::DELETED),
                (PathBuf::from("d.lua"), FileChangeType::CHANGED),
            ]
        );
        assert!(pending.entries.is_empty());
    }
}
