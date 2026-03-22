#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::panic,
        clippy::panic_in_result_fn
    )
)]

mod compilation;
mod config;
mod db_index;
mod diagnostic;
mod locale;
mod profile;
mod resources;
mod semantic;
mod test_lib;
mod vfs;

pub use compilation::*;
pub use config::*;
pub use db_index::*;
pub use diagnostic::*;
pub use glua_codestyle::*;
use glua_parser::{LineIndex, LuaParser, LuaSyntaxTree};
pub use locale::get_locale_code;
use lsp_types::Uri;
pub use profile::Profile;
pub use resources::get_best_resources_dir;
pub use resources::load_resource_from_include_dir;
use resources::load_resource_std;
use schema_to_glua::SchemaConverter;
pub use semantic::*;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::{collections::HashSet, path::PathBuf, sync::Arc};
pub use test_lib::VirtualWorkspace;
use tokio_util::sync::CancellationToken;
use url::Url;
pub use vfs::*;

#[macro_use]
extern crate rust_i18n;

rust_i18n::i18n!("./locales", fallback = "en");

pub fn set_locale(locale: &str) {
    rust_i18n::set_locale(locale);
}

pub async fn fetch_schema_urls(urls: Vec<Url>) -> HashMap<Url, String> {
    let mut url_contents = HashMap::new();
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

    pub fn init_std_lib(&mut self, create_resources_dir: Option<String>) {
        let is_jit = matches!(self.emmyrc.runtime.version, EmmyrcLuaVersion::LuaJIT);
        let (std_root, files) = load_resource_std(create_resources_dir, is_jit);
        self.compilation
            .get_db_mut()
            .get_module_index_mut()
            .add_workspace_root_with_kind(std_root, WorkspaceId::STD, WorkspaceKind::Std);

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
    }

    pub fn add_library_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_library_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Library);
    }

    pub fn update_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                // Text unchanged — if the index is already built (has module info),
                // skip the costly remove+re-add cycle. This avoids unnecessary
                // reindexing when VS Code opens already-loaded files for
                // peek/definition (e.g. annotation/library files).
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_some()
                {
                    return Some(file_id);
                }

                // Index was cleared — fall through to rebuild it.
                self.compilation.remove_index(vec![file_id]);
                self.compilation.update_index(vec![file_id]);
                return Some(file_id);
            }
        } else if text.is_none() {
            return None;
        }

        let is_removed = text.is_none();
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, text);

        self.compilation.remove_index(vec![file_id]);

        if !is_removed {
            self.compilation.update_index(vec![file_id]);
        }

        Some(file_id)
    }

    pub fn update_file_preparsed(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
        trigger_reindex: bool,
    ) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(incoming_version), Some(current_version)) = (
                version,
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_version(&file_id),
            ) && incoming_version < current_version
            {
                return None;
            }

            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_some()
                {
                    self.compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .update_file_version(&file_id, version);
                    return Some(file_id);
                }

                if trigger_reindex {
                    self.compilation.remove_index(vec![file_id]);
                    self.compilation.update_index(vec![file_id]);
                }

                self.compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, version);
                return Some(file_id);
            }
        } else if text.is_none() {
            return None;
        }

        let is_removed = text.is_none();
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed(&uri, text, tree, line_index, version)?;

        if trigger_reindex {
            self.compilation.remove_index(vec![file_id]);

            if !is_removed {
                self.compilation.update_index(vec![file_id]);
            }
        }

        Some(file_id)
    }

    pub fn update_file_preparsed_deferred(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<(FileId, DeferredVfsDrop)> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(incoming_version), Some(current_version)) = (
                version,
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_version(&file_id),
            ) && incoming_version < current_version
            {
                return None;
            }

            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                self.compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, version);
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

    /// VFS-only update: parse and store the new text without touching the index.
    /// The index remains stale but functional until `reindex_files` is called.
    /// This is much faster than `update_file_by_uri`
    pub fn update_file_text_only(&mut self, uri: &Uri, text: String) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing_file_id {
            if let Some(old_text) = self
                .compilation
                .get_db()
                .get_vfs()
                .get_file_content(&file_id)
                .map(String::as_str)
            {
                if old_text == text.as_str() {
                    return Some(file_id);
                }
            }
        }

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, Some(text));

        Some(file_id)
    }

    /// Reindex specific files: remove old index entries + run full analysis pipeline.
    /// Call this after `update_file_text_only` once the user has paused typing.
    pub fn reindex_files(&mut self, file_ids: Vec<FileId>) {
        self.compilation.remove_index(file_ids.clone());
        self.compilation.update_index(file_ids);
    }

    pub fn update_remote_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> FileId {
        let is_removed = text.is_none();
        let fid = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_remote_file_content(uri, text);

        self.compilation.remove_index(vec![fid]);
        if !is_removed {
            self.compilation.update_index(vec![fid]);
        }
        fid
    }

    pub fn update_file_by_path(&mut self, path: &PathBuf, text: Option<String>) -> Option<FileId> {
        let uri = file_path_to_uri(path)?;
        self.update_file_by_uri(&uri, text)
    }

    pub fn update_files_by_uri(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let mut removed_files = HashSet::new();
        let mut updated_files = HashSet::new();

        // Separate files into: unchanged (skip), to-remove, and to-parse
        let mut to_parse: Vec<(Uri, String)> = Vec::new();
        {
            let _p = Profile::new("update files: classify");
            for (uri, text) in files {
                let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
                if let Some(file_id) = existing_file_id {
                    if let (Some(new_text), Some(old_text)) = (
                        text.as_deref(),
                        self.compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(&file_id)
                            .map(String::as_str),
                    ) && old_text == new_text
                    {
                        removed_files.insert(file_id);
                        updated_files.insert(file_id);
                        continue;
                    }
                } else if text.is_none() {
                    continue;
                }

                if let Some(text) = text {
                    to_parse.push((uri, text));
                } else {
                    // File removal: assign ID and mark for removal
                    let file_id = self
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .set_file_content(&uri, None);
                    removed_files.insert(file_id);
                }
            }
        }

        // Parse files — parallel when enough files to benefit
        const PARALLEL_THRESHOLD: usize = 50;
        {
            let _p = Profile::new("update files: parse");
            if to_parse.len() >= PARALLEL_THRESHOLD {
                // Pre-assign file IDs (sequential, fast)
                let file_ids: Vec<FileId> = to_parse
                    .iter()
                    .map(|(uri, _)| {
                        self.compilation.get_db_mut().get_vfs_mut().file_id(uri)
                    })
                    .collect();

                // Parse in parallel
                let config = self.emmyrc.clone();
                let n_threads = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
                    .min(16);
                let next_idx = std::sync::atomic::AtomicUsize::new(0);

                // Each slot stores the parsed result
                let parsed: Vec<std::sync::Mutex<Option<(LuaSyntaxTree, LineIndex)>>> =
                    (0..to_parse.len())
                        .map(|_| std::sync::Mutex::new(None))
                        .collect();

                std::thread::scope(|s| {
                    for _ in 0..n_threads {
                        let next = &next_idx;
                        let files = &to_parse;
                        let results = &parsed;
                        let cfg = &config;
                        s.spawn(move || {
                            let mut node_cache = rowan::NodeCache::default();
                            loop {
                                let idx = next.fetch_add(
                                    1,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                if idx >= files.len() {
                                    break;
                                }
                                let (_, text) = &files[idx];
                                let parse_config =
                                    cfg.get_parse_config(&mut node_cache);
                                let tree = LuaParser::parse(text, parse_config);
                                let line_index = LineIndex::parse(text);
                                *results[idx].lock().expect("mutex poisoned") =
                                    Some((tree, line_index));
                            }
                        });
                    }
                });

                // Insert pre-parsed results (sequential, fast HashMap inserts)
                let vfs = self.compilation.get_db_mut().get_vfs_mut();
                for (i, ((_uri, text), file_id)) in
                    to_parse.into_iter().zip(file_ids.iter()).enumerate()
                {
                    let (tree, line_index) = parsed[i]
                        .lock()
                        .expect("mutex poisoned")
                        .take()
                        .expect("parsed result missing");
                    vfs.insert_preparsed(*file_id, text, tree, line_index);
                    removed_files.insert(*file_id);
                    updated_files.insert(*file_id);
                }
            } else {
                // Small batch: parse sequentially (avoids thread spawn overhead)
                for (uri, text) in to_parse {
                    let file_id = self
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .set_file_content(&uri, Some(text));
                    removed_files.insert(file_id);
                    updated_files.insert(file_id);
                }
            }
        }

        if removed_files.is_empty() {
            return Vec::new();
        }

        self.compilation
            .remove_index(removed_files.into_iter().collect());
        let mut updated_files: Vec<FileId> = updated_files.into_iter().collect();
        updated_files.sort();
        self.compilation.update_index(updated_files.clone());
        updated_files
    }

    #[allow(unused)]
    pub(crate) fn update_files_by_uri_sorted(
        &mut self,
        files: Vec<(Uri, Option<String>)>,
    ) -> Vec<FileId> {
        let mut removed_files = HashSet::new();
        let mut updated_files = HashSet::new();
        {
            let _p = Profile::new("update files");
            for (uri, text) in files {
                let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
                if let Some(file_id) = existing_file_id {
                    if let (Some(new_text), Some(old_text)) = (
                        text.as_deref(),
                        self.compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(&file_id)
                            .map(String::as_str),
                    ) && old_text == new_text
                    {
                        removed_files.insert(file_id);
                        updated_files.insert(file_id);
                        continue;
                    }
                } else if text.is_none() {
                    continue;
                }

                let is_new_text = text.is_some();
                let file_id = self
                    .compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .set_file_content(&uri, text);
                removed_files.insert(file_id);
                if is_new_text {
                    updated_files.insert(file_id);
                }
            }
        }
        if removed_files.is_empty() {
            return Vec::new();
        }

        self.compilation
            .remove_index(removed_files.into_iter().collect());
        let mut updated_files: Vec<FileId> = updated_files.into_iter().collect();
        updated_files.sort();
        self.compilation.update_index(updated_files.clone());
        updated_files
    }

    pub fn remove_file_by_uri(&mut self, uri: &Uri) -> Option<FileId> {
        if let Some(file_id) = self.compilation.get_db_mut().get_vfs_mut().remove_file(uri) {
            log::info!(
                "remove_file_by_uri: uri={} file_id={:?}",
                uri.as_str(),
                file_id
            );
            self.compilation.remove_index(vec![file_id]);
            return Some(file_id);
        }

        None
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

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        self.emmyrc = config.clone();
        self.compilation.update_config(config.clone());
        self.diagnostic.update_config(config);
    }

    pub fn set_workspace_diagnostic_configs(
        &mut self,
        configs: HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
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
        self.diagnostic
            .diagnose_file(&self.compilation, file_id, cancel_token)
    }

    pub fn diagnose_file_with_shared(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: std::sync::Arc<diagnostic::SharedDiagnosticData>,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.diagnostic
            .diagnose_file_with_shared(&self.compilation, file_id, cancel_token, shared_data)
    }

    pub fn precompute_diagnostic_shared_data(
        &self,
    ) -> std::sync::Arc<diagnostic::SharedDiagnosticData> {
        self.diagnostic.precompute_shared_data(&self.compilation)
    }

    pub fn reindex(&mut self) {
        let file_ids = self.compilation.get_db().get_vfs().get_all_file_ids();
        self.compilation.clear_index();
        self.compilation.update_index(file_ids);
    }

    /// 清理文件系统中不再存在的文件
    pub fn cleanup_nonexistent_files(&mut self) {
        let mut files_to_remove = Vec::new();

        // 获取所有当前在VFS中的文件
        let vfs = self.compilation.get_db().get_vfs();
        for file_id in vfs.get_all_local_file_ids() {
            if self
                .compilation
                .get_db()
                .get_module_index()
                .is_std(&file_id)
            {
                continue;
            }
            if let Some(path) = vfs.get_file_path(&file_id).filter(|path| !path.exists())
                && let Some(uri) = file_path_to_uri(path)
            {
                log::info!(
                    "cleanup_nonexistent_files: removing file_id={:?} path={}",
                    file_id,
                    path.display(),
                );
                files_to_remove.push(uri);
            }
        }

        if !files_to_remove.is_empty() {
            log::info!(
                "cleanup_nonexistent_files: removing {} files total",
                files_to_remove.len()
            );
        }

        // 移除不存在的文件
        for uri in files_to_remove {
            self.remove_file_by_uri(&uri);
        }
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

    use crate::EmmyLuaAnalysis;

    fn test_workspace_and_uri() -> (PathBuf, Uri) {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_analysis_test_workspace");
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

        analysis.update_file_by_uri(&uri, Some(content.to_string()));
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
}
