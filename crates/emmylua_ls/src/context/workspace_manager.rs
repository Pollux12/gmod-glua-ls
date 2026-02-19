use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::{path::PathBuf, sync::Arc, time::Duration};

use super::{ClientProxy, FileDiagnostic, StatusBar};
use crate::context::lsp_features::LspFeatures;
use crate::handlers::{ClientConfig, init_analysis};
use emmylua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, LuaDiagnosticConfig, WorkspaceFolder, WorkspaceImport, load_configs,
};
use emmylua_code_analysis::{update_code_style, uri_to_file_path};
use log::{debug, info};
use lsp_types::Uri;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use wax::Pattern;

pub struct WorkspaceManager {
    analysis: Arc<RwLock<EmmyLuaAnalysis>>,
    client: Arc<ClientProxy>,
    status_bar: Arc<StatusBar>,
    update_token: Arc<Mutex<Option<Arc<ReindexToken>>>>,
    file_diagnostic: Arc<FileDiagnostic>,
    lsp_features: Arc<LspFeatures>,
    pub client_config: ClientConfig,
    pub workspace_folders: Vec<WorkspaceFolder>,
    pub watcher: Option<notify::RecommendedWatcher>,
    pub current_open_files: HashSet<Uri>,
    pub match_file_pattern: WorkspaceFileMatcher,
    workspace_diagnostic_level: Arc<AtomicU8>,
    workspace_version: Arc<AtomicI64>,
}

impl WorkspaceManager {
    pub fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        client: Arc<ClientProxy>,
        status_bar: Arc<StatusBar>,
        file_diagnostic: Arc<FileDiagnostic>,
        lsp_features: Arc<LspFeatures>,
    ) -> Self {
        Self {
            analysis,
            client,
            status_bar,
            client_config: ClientConfig::default(),
            workspace_folders: Vec::new(),
            update_token: Arc::new(Mutex::new(None)),
            file_diagnostic,
            lsp_features,
            watcher: None,
            current_open_files: HashSet::new(),
            match_file_pattern: WorkspaceFileMatcher::default(),
            workspace_diagnostic_level: Arc::new(AtomicU8::new(
                WorkspaceDiagnosticLevel::Fast.to_u8(),
            )),
            workspace_version: Arc::new(AtomicI64::new(0)),
        }
    }

    pub fn get_workspace_diagnostic_level(&self) -> WorkspaceDiagnosticLevel {
        let value = self.workspace_diagnostic_level.load(Ordering::Acquire);
        WorkspaceDiagnosticLevel::from_u8(value)
    }

    pub fn update_workspace_version(&self, level: WorkspaceDiagnosticLevel, add_version: bool) {
        self.workspace_diagnostic_level
            .store(level.to_u8(), Ordering::Release);
        if add_version {
            self.workspace_version.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn get_workspace_version(&self) -> i64 {
        self.workspace_version.load(Ordering::Acquire)
    }

    pub async fn add_update_emmyrc_task(&self, file_dir: PathBuf) {
        let mut update_token = self.update_token.lock().await;
        if let Some(token) = update_token.as_ref() {
            token.cancel();
            debug!("cancel update config: {:?}", file_dir);
        }

        let cancel_token = Arc::new(ReindexToken::new(Duration::from_secs(2)));
        update_token.replace(cancel_token.clone());
        drop(update_token);

        let analysis = self.analysis.clone();
        let workspace_folders = self.workspace_folders.clone();
        let config_update_token = self.update_token.clone();
        let client_config = self.client_config.clone();
        let status_bar = self.status_bar.clone();
        let file_diagnostic = self.file_diagnostic.clone();
        let lsp_features = self.lsp_features.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            cancel_token.wait_for_reindex().await;
            if cancel_token.is_cancelled() {
                return;
            }

            let config_roots = collect_config_roots(&workspace_folders, Some(file_dir.clone()));
            let loaded = load_emmy_config(config_roots, client_config);
            init_analysis(
                &analysis,
                &status_bar,
                &file_diagnostic,
                &lsp_features,
                workspace_folders,
                loaded.emmyrc,
                loaded.workspace_diagnostic_configs,
                loaded.workspace_emmyrcs,
            )
            .await;
            if lsp_features.supports_workspace_diagnostic() {
                client.refresh_workspace_diagnostics();
            }
            // After completion, remove from HashMap
            let mut tokens = config_update_token.lock().await;
            tokens.take();
        });
    }

    pub fn update_editorconfig(&self, path: PathBuf) {
        let parent_dir = path
            .parent()
            .unwrap()
            .to_path_buf()
            .to_string_lossy()
            .to_string()
            .replace("\\", "/");
        let file_normalized = path.to_string_lossy().to_string().replace("\\", "/");
        log::info!("update code style: {:?}", file_normalized);
        update_code_style(&parent_dir, &file_normalized);
    }

    pub fn add_reload_workspace_task(&self) -> Option<()> {
        let config_roots = collect_config_roots(&self.workspace_folders, None);
        let loaded = load_emmy_config(config_roots, self.client_config.clone());
        let analysis = self.analysis.clone();
        let workspace_folders = self.workspace_folders.clone();
        let status_bar = self.status_bar.clone();
        let file_diagnostic = self.file_diagnostic.clone();
        let lsp_features = self.lsp_features.clone();
        let client = self.client.clone();
        let workspace_diagnostic_status = self.workspace_diagnostic_level.clone();
        tokio::spawn(async move {
            // Perform reindex with minimal lock holding time
            init_analysis(
                &analysis,
                &status_bar,
                &file_diagnostic,
                &lsp_features,
                workspace_folders,
                loaded.emmyrc,
                loaded.workspace_diagnostic_configs,
                loaded.workspace_emmyrcs,
            )
            .await;

            // Cancel diagnostics and update status without holding analysis lock
            file_diagnostic.cancel_workspace_diagnostic().await;
            workspace_diagnostic_status
                .store(WorkspaceDiagnosticLevel::Fast.to_u8(), Ordering::Release);

            // Trigger diagnostics refresh
            if lsp_features.supports_workspace_diagnostic() {
                client.refresh_workspace_diagnostics();
            } else {
                file_diagnostic
                    .add_workspace_diagnostic_task(500, true)
                    .await;
            }
        });

        Some(())
    }

    pub async fn extend_reindex_delay(&self) -> Option<()> {
        let update_token = self.update_token.lock().await;
        if let Some(token) = update_token.as_ref() {
            token.set_resleep().await;
        }

        Some(())
    }

    pub async fn reindex_workspace(&self, delay: Duration) -> Option<()> {
        log::info!("refresh workspace with delay: {:?}", delay);
        let mut update_token = self.update_token.lock().await;
        if let Some(token) = update_token.as_ref() {
            token.cancel();
            log::info!("cancel reindex workspace");
        }

        let cancel_token = Arc::new(ReindexToken::new(delay));
        update_token.replace(cancel_token.clone());
        drop(update_token);
        let analysis = self.analysis.clone();
        let file_diagnostic = self.file_diagnostic.clone();
        let lsp_features = self.lsp_features.clone();
        let client = self.client.clone();
        let workspace_diagnostic_status = self.workspace_diagnostic_level.clone();
        tokio::spawn(async move {
            cancel_token.wait_for_reindex().await;
            if cancel_token.is_cancelled() {
                return;
            }

            // Perform reindex with minimal lock holding time
            {
                let mut analysis = analysis.write().await;
                // 在重新索引之前清理不存在的文件
                analysis.cleanup_nonexistent_files();
                // Release lock immediately after cleanup
            }

            // Cancel diagnostics and update status without holding analysis lock
            file_diagnostic.cancel_workspace_diagnostic().await;
            workspace_diagnostic_status
                .store(WorkspaceDiagnosticLevel::Fast.to_u8(), Ordering::Release);

            // Trigger diagnostics refresh
            if lsp_features.supports_workspace_diagnostic() {
                client.refresh_workspace_diagnostics();
            } else {
                file_diagnostic
                    .add_workspace_diagnostic_task(500, true)
                    .await;
            }
        });

        Some(())
    }

    pub fn is_workspace_file(&self, uri: &Uri) -> bool {
        if self.workspace_folders.is_empty() {
            return true;
        }

        let Some(file_path) = uri_to_file_path(uri) else {
            return true;
        };

        let mut is_workspace_file = false;
        for workspace in &self.workspace_folders {
            if let Ok(relative) = file_path.strip_prefix(&workspace.root) {
                let inside_import = match &workspace.import {
                    WorkspaceImport::All => true,
                    WorkspaceImport::SubPaths(paths) => {
                        paths.iter().any(|p| relative.starts_with(p))
                    }
                };

                if !inside_import {
                    continue;
                }

                if self.match_file_pattern.is_match(&file_path, relative) {
                    is_workspace_file = true;
                } else {
                    return false;
                }
            }
        }

        is_workspace_file
    }

    pub async fn check_schema_update(&self) {
        let read_analysis = self.analysis.read().await;
        if read_analysis.check_schema_update() {
            drop(read_analysis);
            let mut write_analysis = self.analysis.write().await;
            write_analysis.update_schema().await;
        }
    }
}

fn collect_config_roots(
    workspace_folders: &[WorkspaceFolder],
    preferred_root: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut config_roots = Vec::new();
    if let Some(preferred_root) = preferred_root {
        config_roots.push(preferred_root);
    }

    config_roots.extend(
        workspace_folders
            .iter()
            .map(|workspace| workspace.root.clone()),
    );
    dedup_paths(config_roots)
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }

    deduped
}

pub struct LoadedConfig {
    pub emmyrc: Arc<Emmyrc>,
    pub workspace_diagnostic_configs: HashMap<PathBuf, LuaDiagnosticConfig>,
    pub workspace_emmyrcs: HashMap<PathBuf, Arc<Emmyrc>>,
}

pub fn load_emmy_config(config_roots: Vec<PathBuf>, client_config: ClientConfig) -> LoadedConfig {
    // Config load priority.
    // * Global `<os-specific home-dir>` config files.
    // * Global `<os-specific config-dir>/emmylua_ls` config files.
    // * Environment-specified config at the $EMMYLUALS_CONFIG path.
    // * Local workspace config files.
    //
    // Merge order in one directory (low → high priority):
    // `.luarc.json` → `.emmyrc.json` → `.emmyrc.lua`.
    // This preserves LuaLS compatibility defaults while allowing Emmy configs to override.
    let luarc_file = ".luarc.json";
    let emmyrc_file = ".emmyrc.json";
    let emmyrc_lua_file = ".emmyrc.lua";
    let mut global_config_files = Vec::new();

    let home_dir = dirs::home_dir();
    if let Some(home_dir) = home_dir {
        push_configs_from_dir(
            &mut global_config_files,
            &home_dir,
            luarc_file,
            emmyrc_file,
            emmyrc_lua_file,
        );
    };

    let emmylua_config_dir = "gluals";
    let config_dir = dirs::config_dir().map(|path| path.join(emmylua_config_dir));
    if let Some(config_dir) = config_dir {
        push_configs_from_dir(
            &mut global_config_files,
            &config_dir,
            luarc_file,
            emmyrc_file,
            emmyrc_lua_file,
        );
    };

    std::env::var("GLUALS_CONFIG")
        .inspect(|path| {
            let config_path = std::path::PathBuf::from(path);
            if config_path.exists() {
                info!("load config from: {:?}", config_path);
                global_config_files.push(config_path);
            }
        })
        .ok();

    let config_roots = dedup_paths(config_roots);
    let mut config_files = global_config_files.clone();
    push_configs_from_preferred_workspace_root(
        &mut config_files,
        &config_roots,
        luarc_file,
        emmyrc_file,
        emmyrc_lua_file,
    );

    let mut emmyrc = load_configs(config_files, client_config.partial_emmyrcs.clone());
    merge_client_config(&client_config, &mut emmyrc);

    // Inject GMod annotations path if provided and not explicitly disabled
    inject_gmod_annotations(&client_config, &mut emmyrc);

    let (workspace_diagnostic_configs, workspace_emmyrcs) = pre_process_emmyrc_for_all_roots(
        &mut emmyrc,
        &config_roots,
        &global_config_files,
        &client_config,
        luarc_file,
        emmyrc_file,
        emmyrc_lua_file,
    );

    log::info!("loaded emmyrc complete");
    LoadedConfig {
        emmyrc: emmyrc.into(),
        workspace_diagnostic_configs,
        workspace_emmyrcs,
    }
}

fn pre_process_emmyrc_for_all_roots(
    emmyrc: &mut Emmyrc,
    config_roots: &[PathBuf],
    global_config_files: &[PathBuf],
    client_config: &ClientConfig,
    luarc_file: &str,
    emmyrc_file: &str,
    emmyrc_lua_file: &str,
) -> (
    HashMap<PathBuf, LuaDiagnosticConfig>,
    HashMap<PathBuf, Arc<Emmyrc>>,
) {
    let mut workspace_diagnostic_configs = HashMap::new();
    let mut workspace_emmyrcs = HashMap::new();

    if config_roots.is_empty() {
        return (workspace_diagnostic_configs, workspace_emmyrcs);
    }

    let mut merged_emmyrc: Option<Emmyrc> = None;
    for workspace_root in config_roots {
        let mut workspace_config_files = global_config_files.to_vec();
        workspace_config_files.extend(collect_config_files_from_dir(
            workspace_root,
            luarc_file,
            emmyrc_file,
            emmyrc_lua_file,
        ));

        let mut workspace_emmyrc = load_configs(
            workspace_config_files,
            client_config.partial_emmyrcs.clone(),
        );
        merge_client_config(client_config, &mut workspace_emmyrc);
        inject_gmod_annotations(client_config, &mut workspace_emmyrc);
        workspace_emmyrc.pre_process_emmyrc(workspace_root);

        // Store per-workspace diagnostic config before merging
        workspace_diagnostic_configs.insert(
            workspace_root.clone(),
            LuaDiagnosticConfig::new(&workspace_emmyrc),
        );
        workspace_emmyrcs.insert(workspace_root.clone(), Arc::new(workspace_emmyrc.clone()));

        if let Some(merged) = merged_emmyrc.as_mut() {
            extend_unique(
                &mut merged.workspace.workspace_roots,
                workspace_emmyrc.workspace.workspace_roots,
            );
            extend_unique(
                &mut merged.workspace.library,
                workspace_emmyrc.workspace.library,
            );
            extend_unique(
                &mut merged.workspace.package_dirs,
                workspace_emmyrc.workspace.package_dirs,
            );
            extend_unique(
                &mut merged.workspace.ignore_dir,
                workspace_emmyrc.workspace.ignore_dir,
            );
            extend_unique(
                &mut merged.workspace.ignore_globs,
                workspace_emmyrc.workspace.ignore_globs,
            );
            extend_unique(
                &mut merged.runtime.extensions,
                workspace_emmyrc.runtime.extensions,
            );
            extend_unique(
                &mut merged.runtime.require_pattern,
                workspace_emmyrc.runtime.require_pattern,
            );
            extend_unique(&mut merged.resource.paths, workspace_emmyrc.resource.paths);
        } else {
            merged_emmyrc = Some(workspace_emmyrc);
        }
    }

    if let Some(merged_emmyrc) = merged_emmyrc {
        *emmyrc = merged_emmyrc;
    }

    (workspace_diagnostic_configs, workspace_emmyrcs)
}

fn extend_unique<T>(target: &mut Vec<T>, incoming: Vec<T>)
where
    T: Eq + Hash + Clone,
{
    let mut seen: HashSet<T> = target.iter().cloned().collect();
    for item in incoming {
        if seen.insert(item.clone()) {
            target.push(item);
        }
    }
}

fn push_configs_from_dir(
    config_files: &mut Vec<PathBuf>,
    dir: &Path,
    luarc_file: &str,
    emmyrc_file: &str,
    emmyrc_lua_file: &str,
) {
    let dir_configs = collect_config_files_from_dir(dir, luarc_file, emmyrc_file, emmyrc_lua_file);
    for config_file in dir_configs {
        info!("load config from: {:?}", config_file);
        config_files.push(config_file);
    }
}

fn push_configs_from_preferred_workspace_root(
    config_files: &mut Vec<PathBuf>,
    config_roots: &[PathBuf],
    luarc_file: &str,
    emmyrc_file: &str,
    emmyrc_lua_file: &str,
) {
    for config_root in config_roots {
        let dir_configs =
            collect_config_files_from_dir(config_root, luarc_file, emmyrc_file, emmyrc_lua_file);

        if dir_configs.is_empty() {
            continue;
        }

        info!("using preferred workspace config root: {:?}", config_root);
        for config_file in dir_configs {
            info!("load config from: {:?}", config_file);
            config_files.push(config_file);
        }
        break;
    }
}

fn collect_config_files_from_dir(
    dir: &Path,
    luarc_file: &str,
    emmyrc_file: &str,
    emmyrc_lua_file: &str,
) -> Vec<PathBuf> {
    // .gluarc.json is the GMod-specific config — if present, it takes exclusive priority
    // and no other config files in this directory are considered.
    let gluarc = dir.join(".gluarc.json");
    if gluarc.exists() {
        return vec![gluarc];
    }
    [
        dir.join(luarc_file),
        dir.join(emmyrc_file),
        dir.join(emmyrc_lua_file),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

fn merge_client_config(client_config: &ClientConfig, emmyrc: &mut Emmyrc) -> Option<()> {
    emmyrc
        .runtime
        .extensions
        .extend(client_config.extensions.clone());
    emmyrc
        .workspace
        .ignore_globs
        .extend(client_config.exclude.clone());
    if client_config.encoding != "utf-8" {
        emmyrc.workspace.encoding = client_config.encoding.clone();
    }

    Some(())
}

/// Inject GMod annotations path into workspace library if appropriate
fn inject_gmod_annotations(client_config: &ClientConfig, emmyrc: &mut Emmyrc) {
    // Check if explicitly disabled in .emmyrc
    if let Some(false) = emmyrc.gmod.auto_load_annotations {
        log::info!("GMod annotations auto-load explicitly disabled in .emmyrc");
        return;
    }

    // Determine which path to use
    let annotations_path = if let Some(explicit_path) = &emmyrc.gmod.annotations_path {
        // User specified explicit path in .emmyrc - use that
        if explicit_path.is_empty() {
            log::info!("GMod annotations_path is explicitly set to empty string - skipping");
            return;
        }
        log::info!("Using GMod annotations from .emmyrc: {}", explicit_path);
        explicit_path.clone()
    } else if let Some(vscode_path) = &client_config.gmod_annotations_path {
        // VSCode extension provided path
        log::info!(
            "Using GMod annotations from VSCode extension: {}",
            vscode_path
        );
        vscode_path.clone()
    } else {
        // No path available
        log::info!("No GMod annotations path available");
        return;
    };

    // Add to library paths
    use emmylua_code_analysis::EmmyLibraryItem;
    if emmyrc
        .workspace
        .library
        .iter()
        .any(|item| item.get_path() == &annotations_path)
    {
        log::info!("GMod annotations path already exists in workspace library");
        return;
    }

    emmyrc
        .workspace
        .library
        .push(EmmyLibraryItem::Path(annotations_path));
    log::info!("GMod annotations added to workspace library");
}

#[derive(Debug)]
pub struct ReindexToken {
    cancel_token: CancellationToken,
    time_sleep: Duration,
    need_re_sleep: Mutex<bool>,
}

impl ReindexToken {
    pub fn new(time_sleep: Duration) -> Self {
        Self {
            cancel_token: CancellationToken::new(),
            time_sleep,
            need_re_sleep: Mutex::new(false),
        }
    }

    pub async fn wait_for_reindex(&self) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.time_sleep) => {
                    // 获取锁来安全地访问和修改 need_re_sleep
                    let mut need_re_sleep = self.need_re_sleep.lock().await;
                    if *need_re_sleep {
                        *need_re_sleep = false;
                    } else {
                        break;
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    break;
                }
            }
        }
    }

    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    pub async fn set_resleep(&self) {
        // 获取锁来安全地修改 need_re_sleep
        let mut need_re_sleep = self.need_re_sleep.lock().await;
        *need_re_sleep = true;
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceFileMatcher {
    include: Vec<String>,
    exclude: Vec<String>,
    exclude_dir: Vec<PathBuf>,
}

impl WorkspaceFileMatcher {
    pub fn new(include: Vec<String>, exclude: Vec<String>, exclude_dir: Vec<PathBuf>) -> Self {
        Self {
            include,
            exclude,
            exclude_dir,
        }
    }
    pub fn is_match(&self, path: &Path, relative_path: &Path) -> bool {
        if self.exclude_dir.iter().any(|dir| path.starts_with(dir)) {
            return false;
        }

        // let path_str = path.to_string_lossy().to_string().replace("\\", "/");
        let exclude_matcher = wax::any(self.exclude.iter().map(|s| s.as_str()));
        if let Ok(exclude_set) = exclude_matcher {
            if exclude_set.is_match(relative_path) {
                return false;
            }
        } else {
            log::error!("Invalid exclude pattern");
        }

        let include_matcher = wax::any(self.include.iter().map(|s| s.as_str()));
        if let Ok(include_set) = include_matcher {
            return include_set.is_match(relative_path);
        } else {
            log::error!("Invalid include pattern");
        }

        true
    }
}

impl Default for WorkspaceFileMatcher {
    fn default() -> Self {
        let include_pattern = vec!["**/*.lua".to_string()];
        Self::new(include_pattern, vec![], vec![])
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDiagnosticLevel {
    None = 0,
    Fast = 1,
    Slow = 2,
}

impl WorkspaceDiagnosticLevel {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Fast,
            2 => Self::Slow,
            _ => Self::None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use emmylua_code_analysis::{DiagnosticCode, WorkspaceFolder, collect_workspace_files};

    use crate::handlers::ClientConfig;

    use super::{
        collect_config_files_from_dir, collect_config_roots, dedup_paths, load_emmy_config,
        push_configs_from_preferred_workspace_root,
    };

    fn create_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("emmylua_ls_config_test_{unique}"));
        fs::create_dir_all(&dir).expect("failed to create temp test dir");
        dir
    }

    fn touch(path: &Path) {
        fs::write(path, "{}").expect("failed to create temp config file");
    }

    #[test]
    fn test_collect_config_files_from_dir_gluarc_json_takes_exclusive_priority() {
        let dir = create_temp_dir();
        let gluarc_json = dir.join(".gluarc.json");
        let emmyrc_json = dir.join(".emmyrc.json");
        let luarc_json = dir.join(".luarc.json");
        touch(&gluarc_json);
        touch(&emmyrc_json);
        touch(&luarc_json);

        let files =
            collect_config_files_from_dir(&dir, ".luarc.json", ".emmyrc.json", ".emmyrc.lua");

        assert_eq!(files, vec![gluarc_json]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_collect_config_files_from_dir_orders_luarc_then_emmyrc_json() {
        let dir = create_temp_dir();
        let emmyrc_json = dir.join(".emmyrc.json");
        let luarc_json = dir.join(".luarc.json");
        touch(&emmyrc_json);
        touch(&luarc_json);

        let files =
            collect_config_files_from_dir(&dir, ".luarc.json", ".emmyrc.json", ".emmyrc.lua");

        assert_eq!(files, vec![luarc_json, emmyrc_json]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_collect_config_files_from_dir_orders_emmyrc_lua_last() {
        let dir = create_temp_dir();
        let emmyrc_lua = dir.join(".emmyrc.lua");
        let emmyrc_json = dir.join(".emmyrc.json");
        let luarc_json = dir.join(".luarc.json");
        touch(&emmyrc_lua);
        touch(&emmyrc_json);
        touch(&luarc_json);

        let files =
            collect_config_files_from_dir(&dir, ".luarc.json", ".emmyrc.json", ".emmyrc.lua");

        assert_eq!(files, vec![luarc_json, emmyrc_json, emmyrc_lua]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_dedup_paths_preserves_order() {
        let path_a = PathBuf::from("/workspace/a");
        let path_b = PathBuf::from("/workspace/b");
        let paths = vec![path_a.clone(), path_b.clone(), path_a.clone()];

        let deduped = dedup_paths(paths);

        assert_eq!(deduped, vec![path_a, path_b]);
    }

    #[test]
    fn test_collect_config_roots_prefers_changed_dir_and_dedups() {
        let workspace_a = WorkspaceFolder::new(PathBuf::from("/workspace/a"), false);
        let workspace_b = WorkspaceFolder::new(PathBuf::from("/workspace/b"), false);
        let preferred = PathBuf::from("/workspace/b");

        let roots = collect_config_roots(&[workspace_a, workspace_b], Some(preferred.clone()));

        assert_eq!(roots, vec![preferred, PathBuf::from("/workspace/a")]);
    }

    #[test]
    fn test_push_configs_from_preferred_workspace_root_uses_first_root_with_config() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "diagnostics": { "globals": ["A_ONLY"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "diagnostics": { "globals": ["B_ONLY"] } }"#,
        )
        .expect("failed to write workspace b config");

        let mut config_files = Vec::new();
        push_configs_from_preferred_workspace_root(
            &mut config_files,
            &[workspace_a.clone(), workspace_b.clone()],
            ".luarc.json",
            ".emmyrc.json",
            ".emmyrc.lua",
        );

        assert_eq!(config_files, vec![workspace_a.join(".emmyrc.json")]);

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_does_not_overlay_with_secondary_workspace_local_config() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "diagnostics": { "globals": ["A_ONLY"], "disable": ["inject-field"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "diagnostics": { "globals": ["B_ONLY"], "disable": ["undefined-global"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert_eq!(
            loaded.emmyrc.diagnostics.globals,
            vec!["A_ONLY".to_string()]
        );
        assert_eq!(
            loaded.emmyrc.diagnostics.disable,
            vec![DiagnosticCode::InjectField]
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_preprocesses_relative_paths_for_each_workspace_root() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();
        touch(&workspace_a.join(".emmyrc.json"));
        touch(&workspace_b.join(".emmyrc.json"));

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "resource": { "paths": ["./lua"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "resource": { "paths": ["./lua"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let workspace_a_lua = workspace_a.join("lua").to_string_lossy().to_string();
        let workspace_b_lua = workspace_b.join("lua").to_string_lossy().to_string();
        assert!(loaded.emmyrc.resource.paths.contains(&workspace_a_lua));
        assert!(loaded.emmyrc.resource.paths.contains(&workspace_b_lua));

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_uses_each_workspace_local_relative_paths() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "resource": { "paths": ["./lua_a"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "resource": { "paths": ["./lua_b"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let workspace_a_lua = workspace_a.join("lua_a").to_string_lossy().to_string();
        let workspace_b_lua = workspace_b.join("lua_b").to_string_lossy().to_string();
        assert!(loaded.emmyrc.resource.paths.contains(&workspace_a_lua));
        assert!(loaded.emmyrc.resource.paths.contains(&workspace_b_lua));

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_merges_runtime_extensions_for_each_workspace() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "runtime": { "extensions": [".luaa"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "runtime": { "extensions": [".luab"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert!(
            loaded
                .emmyrc
                .runtime
                .extensions
                .contains(&".luaa".to_string())
        );
        assert!(
            loaded
                .emmyrc
                .runtime
                .extensions
                .contains(&".luab".to_string())
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_merges_ignore_globs_for_each_workspace() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "ignoreGlobs": ["**/a/**"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "ignoreGlobs": ["**/b/**"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert!(
            loaded
                .emmyrc
                .workspace
                .ignore_globs
                .contains(&"**/a/**".to_string())
        );
        assert!(
            loaded
                .emmyrc
                .workspace
                .ignore_globs
                .contains(&"**/b/**".to_string())
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_uses_each_workspace_local_library_paths() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "library": ["./lua/lib_a"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "library": ["./lua/lib_b"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let workspace_a_lib = workspace_a.join("lua").join("lib_a");
        let workspace_b_lib = workspace_b.join("lua").join("lib_b");
        let library_paths = loaded
            .emmyrc
            .workspace
            .library
            .iter()
            .map(|item| PathBuf::from(item.get_path()))
            .collect::<Vec<_>>();

        assert!(
            library_paths.iter().any(|path| path == &workspace_a_lib),
            "libraries: {:?}",
            loaded.emmyrc.workspace.library
        );
        assert!(
            library_paths.iter().any(|path| path == &workspace_b_lib),
            "libraries: {:?}",
            loaded.emmyrc.workspace.library
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_collect_workspace_files_loads_libraries_from_each_workspace_config() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();
        let library_a = workspace_a.join("lib_a");
        let library_b = workspace_b.join("lib_b");
        fs::create_dir_all(&library_a).expect("failed to create workspace a library");
        fs::create_dir_all(&library_b).expect("failed to create workspace b library");

        fs::write(library_a.join("globals_a.lua"), "LibGlobalA = true")
            .expect("failed to write workspace a library file");
        fs::write(library_b.join("globals_b.lua"), "LibGlobalB = true")
            .expect("failed to write workspace b library file");

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "library": ["./lib_a"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "library": ["./lib_b"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let mut workspaces = vec![
            WorkspaceFolder::new(workspace_a.clone(), false),
            WorkspaceFolder::new(workspace_b.clone(), false),
        ];
        for lib in &loaded.emmyrc.workspace.library {
            workspaces.push(WorkspaceFolder::new(PathBuf::from(lib.get_path()), true));
        }

        let files = collect_workspace_files(&workspaces, &loaded.emmyrc, None, None);
        let loaded_paths = files.into_iter().map(|f| f.path).collect::<Vec<_>>();

        let globals_a_path = library_a
            .join("globals_a.lua")
            .to_string_lossy()
            .to_string();
        let globals_b_path = library_b
            .join("globals_b.lua")
            .to_string_lossy()
            .to_string();
        assert!(
            loaded_paths.iter().any(|path| path == &globals_a_path),
            "loaded paths: {:?}",
            loaded_paths
        );
        assert!(
            loaded_paths.iter().any(|path| path == &globals_b_path),
            "loaded paths: {:?}",
            loaded_paths
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_load_emmy_config_returns_per_workspace_diagnostic_configs() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "diagnostics": { "severity": { "undefined-global": "warning" } } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "diagnostics": { "severity": { "undefined-global": "error" } } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let config_a = loaded
            .workspace_diagnostic_configs
            .get(&workspace_a)
            .expect("workspace_a config should be present");
        let config_b = loaded
            .workspace_diagnostic_configs
            .get(&workspace_b)
            .expect("workspace_b config should be present");

        assert_eq!(
            config_a
                .severity
                .get(&DiagnosticCode::UndefinedGlobal)
                .copied(),
            Some(lsp_types::DiagnosticSeverity::WARNING)
        );
        assert_eq!(
            config_b
                .severity
                .get(&DiagnosticCode::UndefinedGlobal)
                .copied(),
            Some(lsp_types::DiagnosticSeverity::ERROR)
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }
}
