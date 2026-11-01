#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::panic,
        clippy::panic_in_result_fn
    )
)]

mod ast_util;
mod compilation;
mod config;
mod db_index;
mod diagnostic;
mod gamemode_base;
mod library_collision;
pub mod profile;
pub mod progress;
mod resources;
mod semantic;
mod test_lib;
mod vfs;

pub use compilation::*;
pub use config::*;
pub use db_index::*;
pub use diagnostic::*;
pub use gamemode_base::detect_gamemode_base_libraries;
pub use glua_codestyle::*;
use glua_parser::{LineIndex, LuaCallExpr, LuaSyntaxTree};
pub use library_collision::LibraryDefinitionCollision;
use lsp_types::Uri;
pub use profile::Profile;
use resources::load_resource_std;
use rustc_hash::FxHashSet;
use schema_to_glua::SchemaConverter;
pub use semantic::*;
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path};
use std::str::FromStr;
use std::{path::PathBuf, sync::Arc};
pub use test_lib::{GMOD_CALL_ARG_BUILTINS_FIXTURE, VirtualWorkspace};
use tokio_util::sync::CancellationToken;
use url::Url;
pub use vfs::*;

/// Stack size for any thread that runs analysis.
///
/// Inference and the type-graph walks recurse to the depth of the source, which
/// a generated or deeply nested GLua file can push past a default thread stack.
/// Every thread the binaries analyse on gets it: their runtime's threads, the
/// thread driving that runtime and the worker pool, so how deep a file may nest
/// does not depend on which thread picked it up. The reservation is virtual and committed lazily, so a worker that never
/// recurses costs nothing.
pub const ANALYSIS_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Minimum stack bytes that must remain to keep inferring.
///
/// Source-controlled nesting (expressions, doc types) recurses per level and
/// can exhaust small worker stacks on generated files. Exhaustion bails to the
/// existing `RecursiveInfer` path (an `Unknown` fallback), the same graceful
/// failure cyclic inputs already produce, instead of aborting the process.
pub(crate) const ANALYSIS_STACK_RESERVE: usize = 256 * 1024;

/// Fail-closed remaining-stack probe shared by expression and doc inference.
///
/// `None` (unsupported target / OS query failure) counts as exhausted rather
/// than safe, mirroring how `stacker::maybe_grow` treats `None` as
/// insufficient.
pub(crate) fn analysis_stack_exhausted() -> bool {
    match stacker::remaining_stack() {
        Some(remaining) => remaining < ANALYSIS_STACK_RESERVE,
        None => true,
    }
}

/// True when `call_expr` calls an annotated net operation — a message start, a
/// send terminator, or a payload write/read.
///
/// Shares the analyzer's resolution path, so an alias, a local binding, or an
/// annotated wrapper is classified identically to the `net.*` builtin. Editor
/// handlers should use this instead of re-deriving the answer, and instead of
/// consulting the flow index: an op that forms no complete flow (a bare
/// `net.WriteString` with no `net.Start`) is still a net op and is never
/// recorded in the flow index.
///
/// `cache` is supplied by the caller because classifying a document means asking
/// this for every call in it, and building an inference cache per question would
/// throw away all reuse between them.
pub fn call_expr_is_net_op(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(signature_id) = semantic::get_prefix_expr_signature_id(db, cache, call_expr) else {
        return false;
    };
    db_index::signature_has_net_op_metadata(db, signature_id)
}

pub async fn fetch_schema_urls(urls: Vec<Url>) -> HashMap<Url, String> {
    let mut url_contents = HashMap::default();
    for url in urls {
        if url.scheme() == "file" {
            if let Ok(path) = url.to_file_path()
                && path.exists()
            {
                let result = read_file_with_encoding(&path, "utf-8");
                if let Some(content) = result {
                    url_contents.insert(url, content);
                } else {
                    log::error!("Failed to read schema file: {:?}", path);
                }
            }
        } else {
            let result = reqwest::get(url.as_str()).await;
            if let Ok(response) = result {
                if let Ok(content) = response.text().await {
                    url_contents.insert(url, content);
                } else {
                    log::error!("Failed to read schema content from URL: {:?}", url);
                }
            } else {
                log::error!("Failed to fetch schema from URL: {:?}", url);
            }
        }
    }

    url_contents
}

/// Normalize a workspace root path so it uses the same drive-letter
/// casing that the VFS applies (uppercase on Windows).  Without this,
/// `extract_module_path` would fail to match VFS paths against
/// library workspace roots supplied by the editor with a lowercase
/// drive letter.
fn normalize_workspace_root(root: PathBuf) -> PathBuf {
    file_path_to_uri(&root)
        .and_then(|uri| uri_to_file_path(&uri))
        .unwrap_or(root)
}

pub(crate) fn dependency_site_path_keys(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_path: &str,
) -> Vec<String> {
    let dependency_path = normalize_dependency_path(dependency_path);
    if dependency_path.is_empty() {
        return Vec::new();
    }

    let mut keys = FxHashSet::default();
    insert_dependency_path_key_variants(&mut keys, dependency_path.clone());

    if let Some(source_parent) = db
        .get_vfs()
        .get_file_path(&source_file_id)
        .and_then(|source_path| source_path.parent())
    {
        let relative_candidate =
            lexically_normalize_path(&source_parent.join(Path::new(&dependency_path)));
        insert_dependency_path_key_variants(&mut keys, normalize_file_path(&relative_candidate));
    }

    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn insert_dependency_path_key_variants(keys: &mut FxHashSet<String>, path: String) {
    let normalized = normalize_dependency_path(&path);
    if normalized.is_empty() {
        return;
    }
    keys.insert(normalized.clone());
    if let Some(without_lua_ext) = normalized.strip_suffix(".lua") {
        keys.insert(without_lua_ext.to_string());
    } else {
        keys.insert(format!("{normalized}.lua"));
    }
}

fn normalize_dependency_path(path: &str) -> String {
    let mut normalized = normalize_path_case(path.replace('\\', "/"));
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }
    normalized.trim_matches('/').to_string()
}

fn normalize_file_path(path: &Path) -> String {
    normalize_path_case(path.to_string_lossy().replace('\\', "/"))
        .trim_end_matches('/')
        .to_string()
}

fn normalize_path_case(path: String) -> String {
    #[cfg(target_os = "windows")]
    {
        path.to_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path
    }
}

fn lexically_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[derive(Debug)]
pub struct EmmyLuaAnalysis {
    pub compilation: LuaCompilation,
    pub diagnostic: LuaDiagnostic,
    pub emmyrc: Arc<Emmyrc>,
}

impl EmmyLuaAnalysis {
    pub fn new() -> Self {
        let emmyrc = Arc::new(Emmyrc::default());
        Self {
            compilation: LuaCompilation::new(emmyrc.clone()),
            diagnostic: LuaDiagnostic::new(),
            emmyrc,
        }
    }

    /// An independent copy-on-write copy: index domains and the vfs are
    /// shared until one side first writes them, and edits to either side never
    /// reach the other.
    pub fn fork(&self) -> Self {
        Self {
            compilation: self.compilation.fork(),
            diagnostic: self.diagnostic.clone(),
            emmyrc: self.emmyrc.clone(),
        }
    }

    pub fn init_std_lib(&mut self) {
        let is_jit = matches!(self.emmyrc.runtime.version, EmmyrcLuaVersion::LuaJIT);
        let (std_root, files) = load_resource_std(is_jit);
        // Normalize so the root's drive-letter casing matches VFS file paths
        // (the URI round-trip uppercases the Windows drive letter). Without
        // this, `extract_module_path` prefix matching would fail when the
        // env-derived root has a lowercase drive letter.
        let std_root = normalize_workspace_root(std_root);
        self.init_std_lib_from_files(std_root, files);
    }

    /// Hands the workspace roots and their load tiers to the VFS, which orders
    /// files by them.
    fn sync_file_order(&mut self) {
        let db = self.compilation.get_db_mut();
        let roots = db
            .get_module_index()
            .workspaces()
            .iter()
            .map(|workspace| (workspace.root.clone(), workspace.kind.merge_rank()))
            .collect::<Vec<_>>();
        db.get_vfs_mut().set_order_roots(roots);
    }

    /// Register a pre-built set of embedded std files directly into the analysis
    /// workspace without going through the resource-loading pipeline.
    pub(crate) fn init_std_lib_from_files(&mut self, std_root: PathBuf, files: Vec<LuaFileInfo>) {
        self.compilation
            .get_db_mut()
            .get_module_index_mut()
            .add_workspace_root_with_kind(std_root, WorkspaceId::STD, WorkspaceKind::Std);
        self.sync_file_order();

        let files = files
            .into_iter()
            .filter_map(|file| {
                if file.path.ends_with(".lua") {
                    Some((PathBuf::from(file.path), Some(file.content)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.update_files_by_path(files);
    }

    pub fn get_file_id(&self, uri: &Uri) -> Option<FileId> {
        self.compilation.get_db().get_vfs().get_file_id(uri)
    }

    pub fn get_uri(&self, file_id: FileId) -> Option<Uri> {
        self.compilation.get_db().get_vfs().get_uri(&file_id)
    }

    pub fn add_main_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_main_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Main);
        self.sync_file_order();
    }

    pub fn add_library_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_library_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Library);
        self.sync_file_order();
    }

    /// Registers `folders` as the main and library workspace roots, in order,
    /// as a first load registers them, unregistering every other one. Returns
    /// whether the index must be rebuilt: the registration changed while
    /// files outside std, whose module paths derive from it, are loaded.
    pub fn set_workspace_roots<'a>(
        &mut self,
        folders: impl IntoIterator<Item = &'a WorkspaceFolder>,
    ) -> bool {
        let before = self
            .compilation
            .get_db()
            .get_module_index()
            .workspaces()
            .to_vec();
        self.compilation
            .get_db_mut()
            .get_module_index_mut()
            .remove_main_and_library_workspaces();
        for folder in folders {
            if folder.is_library {
                self.add_library_workspace(folder.root.clone());
            } else {
                self.add_main_workspace(folder.root.clone());
            }
        }
        self.sync_file_order();
        let db = self.compilation.get_db();
        let module_index = db.get_module_index();
        module_index.workspaces() != before.as_slice()
            && db
                .get_vfs()
                .get_all_local_file_ids()
                .iter()
                .any(|file_id| !module_index.is_std(file_id))
    }

    /// Sets `uri`'s text (`None` removes it) and re-analyses. Returns the file
    /// and every live file whose diagnostics may have changed.
    pub fn update_file_by_uri(
        &mut self,
        uri: &Uri,
        text: Option<String>,
    ) -> Option<(FileId, Vec<FileId>)> {
        let Some(text) = text else {
            let (file_id, affected) = self.remove_file_by_uri(uri);
            return file_id.map(|file_id| (file_id, affected));
        };
        let existing = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing
            && self.text_is_indexed(file_id, Some(&text))
        {
            return Some((file_id, Vec::new()));
        }
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, Some(text));
        let affected = self.reindex_files(vec![file_id]);
        Some((file_id, affected))
    }

    /// [`update_file_by_uri`](Self::update_file_by_uri) with a tree parsed
    /// off the analysis lock. A stale `version` is ignored; `trigger_reindex`
    /// false only stores the text.
    pub fn update_file_preparsed(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
        trigger_reindex: bool,
    ) -> Option<(FileId, Vec<FileId>)> {
        let existing = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing {
            if self.is_stale_version(file_id, version) {
                return None;
            }
            if self.text_is_indexed(file_id, text.as_deref()) {
                self.compilation.set_file_version(&file_id, version);
                return Some((file_id, Vec::new()));
            }
        } else if text.is_none() {
            return None;
        }
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed(&uri, text, tree, line_index, version)?;
        if !trigger_reindex {
            return Some((file_id, Vec::new()));
        }
        let affected = self.reindex_files(vec![file_id]);
        Some((file_id, affected))
    }

    /// Stores a pre-parsed text without re-analysing, handing back the
    /// replaced tree so the caller can drop it off the lock.
    pub fn update_file_preparsed_deferred(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<(FileId, DeferredVfsDrop)> {
        let existing = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing {
            if self.is_stale_version(file_id, version) {
                return None;
            }
            if self.vfs_text_equals(file_id, text.as_deref()) {
                self.compilation.set_file_version(&file_id, version);
                return Some((file_id, DeferredVfsDrop::default()));
            }
        } else if text.is_none() {
            return None;
        }
        self.compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed_deferred(&uri, text, tree, line_index, version)
    }

    /// Forgets `uri`'s editor version, which orders the edits of one
    /// open-document session only.
    pub fn forget_document_version(&mut self, uri: &Uri) {
        if let Some(file_id) = self.compilation.get_db().get_vfs().get_file_id(uri) {
            self.compilation.forget_file_version(&file_id);
        }
    }

    /// Stores `text` without re-analysing; [`reindex_files`](Self::reindex_files)
    /// brings the index up to date.
    pub fn update_file_text_only(&mut self, uri: &Uri, text: String) -> Option<FileId> {
        if let Some(file_id) = self.compilation.get_db().get_vfs().get_file_id(uri)
            && self.vfs_text_equals(file_id, Some(&text))
        {
            return Some(file_id);
        }
        Some(
            self.compilation
                .get_db_mut()
                .get_vfs_mut()
                .set_file_content(uri, Some(text)),
        )
    }

    /// Whether `file_id` holds text no re-analysis has taken yet.
    pub fn has_untaken_text(&self, file_id: FileId) -> bool {
        self.compilation
            .get_db()
            .get_vfs()
            .has_untaken_edit(&file_id)
    }

    /// Starts re-analysing `file_ids` from the text the VFS holds for them, or
    /// takes them into the re-analysis in progress, which settles them first.
    /// A file whose text an earlier take already took is not taken again.
    /// Advance it with [`Self::advance_reindex`].
    pub fn begin_reindex(&mut self, mut file_ids: Vec<FileId>) {
        file_ids.retain(|file_id| self.has_untaken_text(*file_id));
        self.compilation.begin_update(file_ids);
    }

    /// Advances the re-analysis in progress by `slice`. Once it settles,
    /// returns every live file whose diagnostics read a fact that moved.
    pub fn advance_reindex(&mut self, slice: Slice) -> UpdateProgress {
        match self.compilation.advance_update(slice) {
            UpdateProgress::Settled(mut affected) => {
                let vfs = self.compilation.get_db_mut().get_vfs_mut();
                vfs.forget_taken_deletions();
                affected.retain(|file_id| vfs.get_syntax_tree(file_id).is_some());
                UpdateProgress::Settled(affected)
            }
            progress => progress,
        }
    }

    /// Re-analyses `file_ids` from the text the VFS holds for them, settling
    /// any re-analysis in progress with them. Returns every live file whose
    /// diagnostics may have changed: the files themselves and every file whose
    /// diagnostics read a fact that moved.
    pub fn reindex_files(&mut self, file_ids: Vec<FileId>) -> Vec<FileId> {
        let mut affected = self.compilation.update_index(file_ids.clone());
        affected.extend(file_ids);
        let vfs = self.compilation.get_db().get_vfs();
        affected.retain(|file_id| vfs.get_syntax_tree(file_id).is_some());
        affected.sort_unstable();
        affected.dedup();
        affected
    }

    pub fn update_remote_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> FileId {
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_remote_file_content(uri, text);
        self.reindex_files(vec![file_id]);
        file_id
    }

    /// Sets every file's text in one batch and re-analyses them together.
    /// Returns every live file whose diagnostics may have changed.
    pub fn update_files_by_uri(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        self.apply_file_system_changes(files)
    }

    /// Removes a file the way the filesystem did, and reports what to
    /// re-diagnose.
    pub fn remove_file_by_uri(&mut self, uri: &Uri) -> (Option<FileId>, Vec<FileId>) {
        let Some(file_id) = self.compilation.get_db().get_vfs().get_file_id(uri) else {
            return (None, Vec::new());
        };
        let affected = self.apply_file_system_changes(vec![(uri.clone(), None)]);
        (Some(file_id), affected)
    }

    /// Applies a batch of file-system events, the last event per file
    /// winning, and re-analyses what changed.
    pub fn apply_file_system_changes(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let changed = self.stage_file_system_changes(files);
        if changed.is_empty() {
            return Vec::new();
        }
        let affected = self.reindex_files(changed);
        self.compilation
            .get_db_mut()
            .get_vfs_mut()
            .forget_taken_deletions();
        affected
    }

    /// Sets the text of a batch of file-system events, the last event per
    /// file winning, without re-analysing. A deleted file whose text a created
    /// one has is taken as moved there ([`Self::move_file`]). Returns the
    /// files whose text or path changed, deleted ones included, for
    /// [`Self::begin_reindex`].
    pub fn stage_file_system_changes(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let mut files = files;
        let vfs = self.compilation.get_db().get_vfs();
        files.sort_by_cached_key(|(uri, _)| vfs.order_key_for_uri(uri));
        let mut latest: Vec<(Uri, Option<String>)> = Vec::with_capacity(files.len());
        for (uri, text) in files {
            match latest.last_mut() {
                Some(last) if last.0 == uri => last.1 = text,
                _ => latest.push((uri, text)),
            }
        }

        let pending: Vec<(Uri, Option<String>)> = latest
            .into_iter()
            .filter(|(uri, text)| {
                let existing = self.compilation.get_db().get_vfs().get_file_id(uri);
                !existing.is_some_and(|file_id| self.text_is_indexed(file_id, text.as_deref()))
            })
            .collect();
        let (moves, pending) = self.pair_moves(pending);
        let mut changed = moves
            .into_iter()
            .filter_map(|(old, new, text)| self.move_file(&old, &new, text))
            .collect::<Vec<_>>();
        let texts: Vec<&str> = pending
            .iter()
            .filter_map(|(_, text)| text.as_deref())
            .collect();
        let mut parsed = self
            .compilation
            .get_db()
            .get_vfs()
            .parse_concurrently(&texts)
            .map(Vec::into_iter);

        let vfs = self.compilation.get_db_mut().get_vfs_mut();
        for (uri, text) in pending {
            let file_id = match text {
                Some(text) => match parsed.as_mut().and_then(Iterator::next) {
                    Some((tree, line_index)) => {
                        vfs.set_file_content_preparsed(&uri, Some(text), tree, line_index, None)
                    }
                    None => Some(vfs.set_file_content(&uri, Some(text))),
                },
                None => vfs.delete_file_content(&uri),
            };
            changed.extend(file_id);
        }
        changed
    }

    /// Splits `events`, sorted by order key, into the moves among them and
    /// the rest. A deletion of a file pairs with the first creation, in order,
    /// of a path no file has whose text is the deleted file's newest; which of
    /// several equal texts pair decides nothing, since file ids never do.
    #[allow(clippy::type_complexity)]
    fn pair_moves(
        &self,
        events: Vec<(Uri, Option<String>)>,
    ) -> (Vec<(Uri, Uri, String)>, Vec<(Uri, Option<String>)>) {
        let moved_to = {
            let vfs = self.compilation.get_db().get_vfs();
            let mut creations: HashMap<&str, VecDeque<usize>> = HashMap::new();
            for (at, (uri, text)) in events.iter().enumerate() {
                if let Some(text) = text
                    && vfs.get_file_id(uri).is_none()
                {
                    creations.entry(text.as_str()).or_default().push_back(at);
                }
            }
            let mut moved_to = vec![None; events.len()];
            for (at, (uri, text)) in events.iter().enumerate() {
                if text.is_some() {
                    continue;
                }
                let Some(old_text) = vfs
                    .get_file_id(uri)
                    .and_then(|file_id| vfs.latest_content(&file_id))
                else {
                    continue;
                };
                moved_to[at] = creations
                    .get_mut(old_text.as_str())
                    .and_then(VecDeque::pop_front);
            }
            moved_to
        };
        let mut events = events.into_iter().map(Some).collect::<Vec<_>>();
        let mut moves = Vec::new();
        for (at, created) in moved_to.into_iter().enumerate() {
            if let Some(created) = created
                && let (Some((old, _)), Some((new, Some(text)))) =
                    (events[at].take(), events[created].take())
            {
                moves.push((old, new, text));
            }
        }
        (moves, events.into_iter().flatten().collect())
    }

    /// Moves the file at `old` to `new`, whose text is `text`, keeping its id,
    /// without re-analysing. `None` when `old` names no file or `new` already
    /// names one.
    pub fn move_file(&mut self, old: &Uri, new: &Uri, text: String) -> Option<FileId> {
        self.compilation
            .get_db_mut()
            .get_vfs_mut()
            .move_file(old, new, text)
    }

    /// For every file, the other files whose analysis read a fact it owns,
    /// sorted.
    pub fn dependent_files(&self) -> HashMap<FileId, Vec<FileId>> {
        self.compilation
            .get_db()
            .reader_graph()
            .dependent_files()
            .into_iter()
            .map(|(file_id, dependents)| {
                let mut dependents = dependents.into_iter().collect::<Vec<_>>();
                dependents.sort_unstable();
                (file_id, dependents)
            })
            .collect()
    }

    pub fn update_files_by_path(&mut self, files: Vec<(PathBuf, Option<String>)>) -> Vec<FileId> {
        let files = files
            .into_iter()
            .filter_map(|(path, text)| {
                let uri = file_path_to_uri(&path)?;
                Some((uri, text))
            })
            .collect();
        self.update_files_by_uri(files)
    }

    /// Whether the newest text the VFS holds for `file_id` is exactly `text`.
    fn vfs_text_equals(&self, file_id: FileId, text: Option<&str>) -> bool {
        let current = self
            .compilation
            .get_db()
            .get_vfs()
            .latest_content(&file_id)
            .map(String::as_str);
        text.is_some() && current == text
    }

    /// Whether `file_id` already holds `text`, taken or staged for the next
    /// take, and has an index.
    fn text_is_indexed(&self, file_id: FileId, text: Option<&str>) -> bool {
        self.vfs_text_equals(file_id, text) && self.compilation.is_indexed(file_id)
    }

    fn is_stale_version(&self, file_id: FileId, version: Option<i32>) -> bool {
        let current = self.compilation.get_db().get_vfs().latest_version(&file_id);
        matches!((version, current), (Some(incoming), Some(current)) if incoming < current)
    }

    /// Applies `config`, re-analysing every file when the change reaches the
    /// index: facts are derived under the config they were built with.
    pub fn update_config(&mut self, config: Arc<Emmyrc>) -> EmmyrcChange {
        let change = self.apply_config(config);
        if change.affects_index() {
            self.reindex();
        }
        change
    }

    /// Applies `config` without re-analysing: a change that reaches the index
    /// leaves it to be rebuilt, as [`Self::begin_full_reindex`] does.
    pub fn apply_config(&mut self, config: Arc<Emmyrc>) -> EmmyrcChange {
        let mut refreshed_config = (*config).clone();
        refreshed_config
            .gmod
            .scripted_class_scopes
            .refresh_resolved_definitions();
        let change = self.emmyrc.change_to(&refreshed_config);
        let config = Arc::new(refreshed_config);
        self.emmyrc = config.clone();
        self.compilation
            .update_config(config.clone(), change.affects_index());
        self.diagnostic.update_config(config);
        change
    }

    pub fn set_workspace_diagnostic_configs(
        &mut self,
        configs: std::collections::HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
    ) {
        self.diagnostic.set_workspace_configs(configs);
    }

    pub fn get_workspace_id_for_root(&self, root: &Path) -> Option<WorkspaceId> {
        self.compilation
            .get_db()
            .get_module_index()
            .get_workspace_id_for_root(root)
    }

    pub fn get_emmyrc(&self) -> Arc<Emmyrc> {
        self.emmyrc.clone()
    }

    pub fn diagnose_file(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.compilation.record_diagnostic_reads(file_id, || {
            self.diagnostic
                .diagnose_file(&self.compilation, file_id, cancel_token)
        })
    }

    pub fn diagnose_file_with_shared(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: std::sync::Arc<diagnostic::SharedDiagnosticData>,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.compilation.record_diagnostic_reads(file_id, || {
            self.diagnostic.diagnose_file_with_shared(
                &self.compilation,
                file_id,
                cancel_token,
                shared_data,
            )
        })
    }

    /// The data every file's diagnostics share, computed outside any file's
    /// recorded reads, as the language server computes it.
    pub fn precompute_diagnostic_shared_data(
        &self,
    ) -> std::sync::Arc<diagnostic::SharedDiagnosticData> {
        self.diagnostic.precompute_shared_data(&self.compilation)
    }

    /// Return main-workspace files in an order that keeps parallel diagnostic
    /// workers busy: files with the most name and index expressions first, since
    /// those are where inference and flow narrowing spend their time, which raw
    /// source size misses (large data files are cheap, mid-size code files are
    /// not). Dispatching the expensive files first keeps one of them from being
    /// left alone on the critical path after the other workers have gone idle.
    pub fn get_main_workspace_file_ids_for_diagnostics(&self) -> Vec<FileId> {
        let file_ids = self
            .compilation
            .get_db()
            .get_module_index()
            .get_main_workspace_file_ids();
        self.order_for_diagnostics(file_ids)
    }

    /// `file_ids` with the files that cost the most to diagnose first; see
    /// [`Self::get_main_workspace_file_ids_for_diagnostics`].
    pub fn order_for_diagnostics(&self, mut file_ids: Vec<FileId>) -> Vec<FileId> {
        let db = self.compilation.get_db();
        let vfs = db.get_vfs();
        let flow_index = db.get_flow_index();
        file_ids.sort_by_cached_key(|file_id| {
            let bindings = flow_index
                .get_flow_tree(file_id)
                .map_or(0, |flow_tree| flow_tree.binding_count());
            let len = vfs.get_file_content(file_id).map_or(0, String::len);
            (
                std::cmp::Reverse(bindings),
                std::cmp::Reverse(len),
                *file_id,
            )
        });
        file_ids
    }

    pub fn reindex(&mut self) {
        self.begin_full_reindex();
        self.compilation.advance_update(Slice::Whole);
    }

    /// Drops the index and any re-analysis in progress, then starts
    /// re-analysing every file from the text the VFS holds, as a cold build
    /// does. Advance it with [`Self::advance_reindex`].
    pub fn begin_full_reindex(&mut self) {
        let file_ids = self.compilation.get_db().get_vfs().get_all_file_ids();
        self.compilation.clear_index();
        self.compilation.begin_update(file_ids);
    }

    /// Every local file outside the standard library, with its path: what a
    /// check for files deleted behind the editor's back looks at.
    pub fn local_file_paths(&self) -> Vec<(Uri, PathBuf)> {
        let db = self.compilation.get_db();
        let vfs = db.get_vfs();
        vfs.get_all_local_file_ids()
            .into_iter()
            .filter(|file_id| !db.get_module_index().is_std(file_id))
            .filter_map(|file_id| {
                let path = vfs.get_file_path(&file_id)?;
                Some((file_path_to_uri(path)?, path.clone()))
            })
            .collect()
    }

    pub fn check_schema_update(&self) -> bool {
        self.compilation
            .get_db()
            .get_json_schema_index()
            .has_need_resolve_schemas()
    }

    pub fn get_schemas_to_fetch(&self) -> Vec<Url> {
        self.compilation
            .get_db()
            .get_json_schema_index()
            .get_need_resolve_schemas()
    }

    pub fn apply_fetched_schemas(&mut self, url_contents: HashMap<Url, String>) {
        if url_contents.is_empty() {
            return;
        }

        let converter = SchemaConverter::new(true);
        let mut users = Vec::new();
        for (url, json_content) in url_contents {
            // let short_name = get_schema_short_name(&url);
            match converter.convert_from_str(&json_content) {
                Ok(convert_result) => {
                    let uri = match Uri::from_str(url.as_str()) {
                        Ok(uri) => uri,
                        Err(e) => {
                            log::error!("Failed to convert URL to URI {:?}: {}", url, e);
                            continue;
                        }
                    };
                    let file_id =
                        self.update_remote_file_by_uri(&uri, Some(convert_result.annotation_text));
                    if let Some(f) = self
                        .compilation
                        .get_db_mut()
                        .get_json_schema_index_mut()
                        .get_schema_file_mut(&url)
                    {
                        *f = JsonSchemaFile::Resolved(LuaTypeDeclId::local(
                            file_id,
                            &convert_result.root_type_name,
                        ));
                    }
                    users.extend(
                        self.compilation
                            .get_db()
                            .get_json_schema_index()
                            .users_of(&url),
                    );
                }
                Err(e) => {
                    log::error!("Failed to convert schema from URL {:?}: {}", url, e);
                }
            }
        }

        self.compilation
            .get_db_mut()
            .get_json_schema_index_mut()
            .reset_rest_schemas();
        // Their tags bound nothing while the schema was unresolved.
        if !users.is_empty() {
            self.reindex_files(users);
        }
    }

    pub async fn update_schema(&mut self) {
        let urls = self.get_schemas_to_fetch();
        let url_contents = fetch_schema_urls(urls).await;
        self.apply_fetched_schemas(url_contents);
    }
}

impl Default for EmmyLuaAnalysis {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use lsp_types::Uri;
    use tokio_util::sync::CancellationToken;

    use crate::{EmmyLuaAnalysis, FileId};

    #[test]
    fn diagnostic_file_ids_are_prioritized_by_source_size_then_file_id() {
        let workspace =
            crate::test_lib::unique_temp_dir("gmod_glua_ls_diagnostic_priority_workspace");
        let make_uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let small_a = make_uri("small_a.lua");
        let large = make_uri("large.lua");
        let small_b = make_uri("small_b.lua");

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        analysis.update_files_by_uri(vec![
            (small_b.clone(), Some("return 1".to_string())),
            (
                large.clone(),
                Some("local value = { one = 1, two = 2, three = 3 }\nreturn value".to_string()),
            ),
            (small_a.clone(), Some("return 2".to_string())),
        ]);

        let large_id = analysis
            .get_file_id(&large)
            .expect("large file should exist");
        let mut small_ids = [
            analysis
                .get_file_id(&small_a)
                .expect("small_a file should exist"),
            analysis
                .get_file_id(&small_b)
                .expect("small_b file should exist"),
        ];
        small_ids.sort_unstable();

        assert_eq!(
            analysis.get_main_workspace_file_ids_for_diagnostics(),
            vec![large_id, small_ids[0], small_ids[1]]
        );
    }

    fn test_workspace_and_uri() -> (PathBuf, Uri) {
        let workspace = crate::test_lib::unique_temp_dir("gmod_glua_ls_analysis_test_workspace");
        let test_file = workspace.join("test.lua");
        let uri = Uri::parse_from_file_path(&test_file).expect("uri should parse");
        (workspace, uri)
    }

    #[test]
    fn unchanged_update_file_by_uri_rebuilds_index() {
        let mut analysis = EmmyLuaAnalysis::new();
        let (workspace, uri) = test_workspace_and_uri();
        analysis.add_main_workspace(workspace);

        let content = "local IsValid = IsValid";
        let file_id = analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .map(|(id, _)| id)
            .expect("file id should exist");

        analysis.compilation.clear_index();
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_none()
        );

        analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .map(|(id, _)| id);
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );
    }

    #[test]
    fn unchanged_update_files_by_uri_rebuilds_index() {
        let mut analysis = EmmyLuaAnalysis::new();
        let (workspace, uri) = test_workspace_and_uri();
        analysis.add_main_workspace(workspace);

        let content = "local IsValid = IsValid";
        let file_id = analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .map(|(id, _)| id)
            .expect("file id should exist");

        analysis.compilation.clear_index();
        let updated = analysis.update_files_by_uri(vec![(uri, Some(content.to_string()))]);
        assert_eq!(updated, vec![file_id]);
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );
    }

    #[test]
    fn vfs_update_files_by_uri_assigns_stable_file_ids_for_new_files() {
        let make_uri = |root: &PathBuf, name: &str| {
            let file = root.join(name);
            Uri::parse_from_file_path(&file).expect("uri should parse")
        };

        let workspace_a = crate::test_lib::unique_temp_dir("gmod_glua_ls_stable_ids_a");
        let workspace_b = crate::test_lib::unique_temp_dir("gmod_glua_ls_stable_ids_b");

        let mut analysis_a = EmmyLuaAnalysis::new();
        analysis_a.add_main_workspace(workspace_a.clone());
        let a1 = make_uri(&workspace_a, "a.lua");
        let b1 = make_uri(&workspace_a, "b.lua");
        let ids_a = analysis_a.update_files_by_uri(vec![
            (b1.clone(), Some("return 'b'".to_string())),
            (a1.clone(), Some("return 'a'".to_string())),
        ]);
        assert_eq!(ids_a.len(), 2);
        let a1_id = analysis_a
            .get_file_id(&a1)
            .expect("a.lua should have stable file id");
        let b1_id = analysis_a
            .get_file_id(&b1)
            .expect("b.lua should have stable file id");

        let mut analysis_b = EmmyLuaAnalysis::new();
        analysis_b.add_main_workspace(workspace_b.clone());
        let a2 = make_uri(&workspace_b, "a.lua");
        let b2 = make_uri(&workspace_b, "b.lua");
        let ids_b = analysis_b.update_files_by_uri(vec![
            (a2.clone(), Some("return 'a'".to_string())),
            (b2.clone(), Some("return 'b'".to_string())),
        ]);
        assert_eq!(ids_b.len(), 2);
        let a2_id = analysis_b
            .get_file_id(&a2)
            .expect("a.lua should have stable file id");
        let b2_id = analysis_b
            .get_file_id(&b2)
            .expect("b.lua should have stable file id");

        assert_eq!(
            a1_id, a2_id,
            "a.lua file id should be input-order independent"
        );
        assert_eq!(
            b1_id, b2_id,
            "b.lua file id should be input-order independent"
        );
    }

    #[test]
    fn multi_file_batch_reindex_matches_clean_build_diagnostics() {
        let incremental_workspace =
            crate::test_lib::unique_temp_dir("gmod_glua_ls_batch_reindex_incremental_workspace");
        let clean_workspace =
            crate::test_lib::unique_temp_dir("gmod_glua_ls_batch_reindex_clean_workspace");
        let uris = |workspace: &PathBuf| {
            ["producer.lua", "consumer.lua", "helper.lua"].map(|name| {
                Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
            })
        };
        let incremental_uris = uris(&incremental_workspace);
        let clean_uris = uris(&clean_workspace);

        let initial_producer = r#"
            Registry = Registry or {}
            Registry.item = { name = "old" }
            Registry.legacy = { value = 1 }
        "#;
        let initial_consumer = r#"
            local name = Registry.item.name
            local legacy = Registry.legacy.value
            consume(name, legacy)
        "#;
        let changed_producer = r#"
            Registry = Registry or {}
            Registry.item = { title = "new" }
        "#;
        let changed_consumer = r#"
            local title = Registry.item.title
            local removed = Registry.legacy.value
            consume(title, removed)
        "#;
        let initial_helper = "function consume(name, value) end";
        let changed_helper = "function consume(title, removed, required) end";

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(incremental_workspace);
        analysis.update_files_by_uri(vec![
            (
                incremental_uris[1].clone(),
                Some(initial_consumer.to_string()),
            ),
            (
                incremental_uris[0].clone(),
                Some(initial_producer.to_string()),
            ),
            (
                incremental_uris[2].clone(),
                Some(initial_helper.to_string()),
            ),
        ]);

        let mut file_ids = incremental_uris
            .iter()
            .map(|uri| analysis.get_file_id(&uri).expect("file should be indexed"))
            .collect::<Vec<FileId>>();
        file_ids.sort_unstable();

        analysis.update_file_text_only(&incremental_uris[0], changed_producer.to_string());
        analysis.update_file_text_only(&incremental_uris[1], changed_consumer.to_string());
        analysis.update_file_text_only(&incremental_uris[2], changed_helper.to_string());
        analysis.reindex_files(vec![file_ids[2], file_ids[0], file_ids[1], file_ids[2]]);

        let mut clean_analysis = EmmyLuaAnalysis::new();
        clean_analysis.add_main_workspace(clean_workspace);
        clean_analysis.update_files_by_uri(vec![
            (clean_uris[2].clone(), Some(changed_helper.to_string())),
            (clean_uris[0].clone(), Some(changed_producer.to_string())),
            (clean_uris[1].clone(), Some(changed_consumer.to_string())),
        ]);

        let snapshot = |analysis: &EmmyLuaAnalysis, file_uris: &[Uri]| {
            let shared = analysis.precompute_diagnostic_shared_data();
            file_uris
                .iter()
                .map(|uri| {
                    let file_id = analysis.get_file_id(uri).expect("file should be indexed");
                    analysis
                        .diagnose_file_with_shared(
                            file_id,
                            CancellationToken::new(),
                            shared.clone(),
                        )
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
        };

        let clean_diagnostics = snapshot(&clean_analysis, &clean_uris);
        assert!(
            clean_diagnostics
                .iter()
                .any(|diagnostics| !diagnostics.is_empty()),
            "fixture should exercise observable diagnostics"
        );

        assert_eq!(snapshot(&analysis, &incremental_uris), clean_diagnostics);
        for (incremental_uri, clean_uri) in incremental_uris.iter().zip(&clean_uris) {
            let incremental_file_id = analysis
                .get_file_id(incremental_uri)
                .expect("incremental file should be indexed");
            let clean_file_id = clean_analysis
                .get_file_id(clean_uri)
                .expect("clean file should be indexed");
            assert!(
                analysis
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(incremental_file_id)
                    .is_some(),
                "batch reindex should restore module ownership for {incremental_file_id:?}"
            );
            assert!(
                clean_analysis
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(clean_file_id)
                    .is_some(),
                "clean build should index module ownership for {clean_file_id:?}"
            );
        }
    }

    #[test]
    fn a_parse_config_change_reparses_loaded_files() {
        let mut ws = crate::VirtualWorkspace::new();
        let file_id = ws.def(
            "local x = 1 // note
",
        );
        let parse_errors = |ws: &crate::VirtualWorkspace| {
            ws.analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(&file_id)
                .expect("tree")
                .get_errors()
                .len()
        };
        assert_eq!(parse_errors(&ws), 0);

        let mut emmyrc = ws.get_emmyrc();
        emmyrc.runtime.nonstandard_symbol.clear();
        ws.update_emmyrc(emmyrc);

        assert_ne!(parse_errors(&ws), 0);
    }
}
