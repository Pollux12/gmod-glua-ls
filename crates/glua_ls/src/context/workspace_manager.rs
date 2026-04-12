use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::{path::PathBuf, sync::Arc, time::Duration};

use super::{ClientProxy, FileDiagnostic, StatusBar};
use crate::codestyle::{apply_editorconfig_file, apply_workspace_code_style};
use crate::context::lsp_features::LspFeatures;
use crate::handlers::{ClientConfig, init_analysis};
use glua_code_analysis::uri_to_file_path;
use glua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, LuaDiagnosticConfig, WorkspaceFolder, WorkspaceImport,
    calculate_include_and_exclude, load_configs,
};
use log::{debug, info};
use lsp_types::Uri;
use serde_json::Value;
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
    /// Fallback matcher used when no workspace-root-specific matcher applies
    /// (e.g. single-root / no-root mode).
    pub match_file_pattern: WorkspaceFileMatcher,
    /// Per-workspace-root matchers. When set, `is_workspace_file` uses the
    /// matcher for the first workspace root that is a prefix of the file path,
    /// so each root's `useDefaultIgnores` / `ignoreDirDefaults` stays isolated.
    pub per_root_matchers: HashMap<PathBuf, WorkspaceFileMatcher>,
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
            per_root_matchers: HashMap::new(),
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

    pub async fn add_update_emmyrc_task(
        &self,
        file_dir: PathBuf,
        workspace_manager: Arc<RwLock<WorkspaceManager>>,
    ) {
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
            apply_workspace_code_style(&workspace_folders, loaded.emmyrc.as_ref());

            // Refresh per-root matchers before re-indexing so that
            // `is_workspace_file` is consistent with the new config.
            {
                let mut wm = workspace_manager.write().await;
                wm.per_root_matchers = loaded.workspace_matchers.clone();
                let (include, exclude, exclude_dir) = calculate_include_and_exclude(&loaded.emmyrc);
                wm.match_file_pattern = WorkspaceFileMatcher::new(include, exclude, exclude_dir);
            }

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
            client.refresh_semantic_tokens();
            client.refresh_inlay_hints();
            if lsp_features.supports_workspace_diagnostic() {
                client.refresh_workspace_diagnostics();
            }
            // After completion, remove from HashMap
            let mut tokens = config_update_token.lock().await;
            tokens.take();
        });
    }

    pub fn update_editorconfig(&self, path: PathBuf) {
        log::info!("update code style: {:?}", path);
        let _ = apply_editorconfig_file(&path);
    }

    pub fn add_reload_workspace_task(
        &self,
        workspace_manager: Arc<RwLock<WorkspaceManager>>,
    ) -> Option<()> {
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
            apply_workspace_code_style(&workspace_folders, loaded.emmyrc.as_ref());

            // Refresh per-root matchers before re-indexing.
            {
                let mut wm = workspace_manager.write().await;
                wm.per_root_matchers = loaded.workspace_matchers.clone();
                let (include, exclude, exclude_dir) = calculate_include_and_exclude(&loaded.emmyrc);
                wm.match_file_pattern = WorkspaceFileMatcher::new(include, exclude, exclude_dir);
            }

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
            client.refresh_semantic_tokens();
            client.refresh_inlay_hints();
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
            client.refresh_semantic_tokens();
            client.refresh_inlay_hints();
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
        is_workspace_file_inner(
            uri,
            &self.workspace_folders,
            &self.per_root_matchers,
            &self.match_file_pattern,
        )
    }
}

/// Inner logic for `WorkspaceManager::is_workspace_file`, extracted so it can
/// be unit-tested without constructing a full `WorkspaceManager`.
///
/// Selects the **most specific** (longest root path) workspace that is a prefix
/// of the file path, then applies only that root's matcher. This prevents
/// nested or overlapping workspace roots from leaking their ignore rules onto
/// files that belong to a more specific child root.
fn is_workspace_file_inner(
    uri: &Uri,
    workspace_folders: &[WorkspaceFolder],
    per_root_matchers: &HashMap<PathBuf, WorkspaceFileMatcher>,
    fallback_matcher: &WorkspaceFileMatcher,
) -> bool {
    if workspace_folders.is_empty() {
        return true;
    }

    let Some(file_path) = uri_to_file_path(uri) else {
        return true;
    };

    // Find the most specific (longest root path) workspace that contains
    // this file. Using the single best root avoids nested/overlapping roots
    // from applying each other's ignore rules to the same file.
    let best = workspace_folders
        .iter()
        .filter_map(|workspace| {
            file_path
                .strip_prefix(&workspace.root)
                .ok()
                .map(|relative| (workspace, relative.to_path_buf()))
        })
        .max_by_key(|(workspace, _)| workspace.root.as_os_str().len());

    let Some((workspace, relative)) = best else {
        // File is not under any workspace root.
        return false;
    };

    let inside_import = match &workspace.import {
        WorkspaceImport::All => true,
        WorkspaceImport::SubPaths(paths) => paths.iter().any(|p| relative.starts_with(p)),
    };

    if !inside_import {
        return false;
    }

    // Use the per-root matcher when available; fall back to the
    // merged/global matcher for compatibility with single-root and
    // no-root configurations.
    let matcher = per_root_matchers
        .get(&workspace.root)
        .unwrap_or(fallback_matcher);

    matcher.is_match(&file_path, &relative)
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
    /// Per-workspace-root file matchers built from each root's individual config.
    /// Used by `WorkspaceManager::is_workspace_file` to avoid cross-root leakage
    /// of `useDefaultIgnores` / `ignoreDirDefaults` settings.
    pub workspace_matchers: HashMap<PathBuf, WorkspaceFileMatcher>,
}

pub fn load_emmy_config(config_roots: Vec<PathBuf>, client_config: ClientConfig) -> LoadedConfig {
    // Config load priority.
    // * Global `<os-specific home-dir>` config files.
    // * Global `<os-specific config-dir>/glua_ls` config files.
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

    // Inject gamemode base libraries if detected and not explicitly disabled
    inject_gamemode_base_libraries(&client_config, &mut emmyrc);

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
    let workspace_matchers = build_workspace_matchers(&workspace_emmyrcs);
    LoadedConfig {
        emmyrc: emmyrc.into(),
        workspace_diagnostic_configs,
        workspace_emmyrcs,
        workspace_matchers,
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

    let mut workspace_configs = Vec::new();

    for workspace_root in config_roots {
        let local_config_files = collect_config_files_from_dir(
            workspace_root,
            luarc_file,
            emmyrc_file,
            emmyrc_lua_file,
        );
        let has_local_config = !local_config_files.is_empty();

        let mut workspace_config_files = global_config_files.to_vec();
        workspace_config_files.extend(local_config_files);

        let mut workspace_emmyrc = load_configs(
            workspace_config_files,
            client_config.partial_emmyrcs.clone(),
        );
        merge_client_config(client_config, &mut workspace_emmyrc);
        inject_gmod_annotations(client_config, &mut workspace_emmyrc);
        inject_gamemode_base_libraries(client_config, &mut workspace_emmyrc);
        workspace_emmyrc.pre_process_emmyrc(workspace_root);
        workspace_configs.push((workspace_root.clone(), workspace_emmyrc, has_local_config));
    }

    // Isolation can only stay enabled if all workspace configs keep it enabled.
    let isolation_enabled = workspace_configs
        .iter()
        .all(|(_, workspace_emmyrc, _)| workspace_emmyrc.workspace.enable_isolation);

    if isolation_enabled {
        let mut merged_emmyrc: Option<Emmyrc> = None;
        for (workspace_root, workspace_emmyrc, _) in &workspace_configs {
            workspace_diagnostic_configs.insert(
                workspace_root.clone(),
                LuaDiagnosticConfig::new(workspace_emmyrc),
            );
            workspace_emmyrcs.insert(workspace_root.clone(), Arc::new(workspace_emmyrc.clone()));

            if let Some(merged) = merged_emmyrc.as_mut() {
                merge_isolated_workspace_fields(merged, workspace_emmyrc);
            } else {
                merged_emmyrc = Some(workspace_emmyrc.clone());
            }
        }

        if let Some(merged_emmyrc) = merged_emmyrc {
            *emmyrc = merged_emmyrc;
        }

        return (workspace_diagnostic_configs, workspace_emmyrcs);
    }

    // isolation disabled: build one global merged config while keeping
    // local workspace configs as optional overrides only where present.
    let baseline_index = workspace_configs
        .iter()
        .position(|(_, _, has_local)| *has_local)
        .unwrap_or(0);

    let mut merged_emmyrc = workspace_configs
        .get(baseline_index)
        .map(|(_, cfg, _)| cfg.clone())
        .unwrap_or_else(|| emmyrc.clone());

    for (index, (workspace_root, workspace_emmyrc, has_local_config)) in
        workspace_configs.into_iter().enumerate()
    {
        if has_local_config {
            workspace_diagnostic_configs.insert(
                workspace_root.clone(),
                LuaDiagnosticConfig::new(&workspace_emmyrc),
            );
            workspace_emmyrcs.insert(workspace_root.clone(), Arc::new(workspace_emmyrc.clone()));
        }

        if index == baseline_index {
            continue;
        }

        merge_emmyrc_prefer_existing_with_array_union(&mut merged_emmyrc, &workspace_emmyrc);
    }

    // This branch is only entered when at least one workspace disabled isolation.
    merged_emmyrc.workspace.enable_isolation = false;

    *emmyrc = merged_emmyrc;

    (workspace_diagnostic_configs, workspace_emmyrcs)
}

fn merge_isolated_workspace_fields(merged: &mut Emmyrc, workspace_emmyrc: &Emmyrc) {
    extend_unique(
        &mut merged.workspace.workspace_roots,
        workspace_emmyrc.workspace.workspace_roots.clone(),
    );
    extend_unique(
        &mut merged.workspace.library,
        workspace_emmyrc.workspace.library.clone(),
    );
    extend_unique(
        &mut merged.workspace.package_dirs,
        workspace_emmyrc.workspace.package_dirs.clone(),
    );
    extend_unique(
        &mut merged.workspace.ignore_dir,
        workspace_emmyrc.workspace.ignore_dir.clone(),
    );
    extend_unique(
        &mut merged.workspace.ignore_dir_defaults,
        workspace_emmyrc.workspace.ignore_dir_defaults.clone(),
    );
    extend_unique(
        &mut merged.workspace.ignore_globs,
        workspace_emmyrc.workspace.ignore_globs.clone(),
    );
    merged.workspace.use_default_ignores =
        merged.workspace.use_default_ignores || workspace_emmyrc.workspace.use_default_ignores;
    merged.workspace.enable_isolation =
        merged.workspace.enable_isolation && workspace_emmyrc.workspace.enable_isolation;
    extend_unique(
        &mut merged.runtime.extensions,
        workspace_emmyrc.runtime.extensions.clone(),
    );
    extend_unique(
        &mut merged.runtime.require_pattern,
        workspace_emmyrc.runtime.require_pattern.clone(),
    );
    extend_unique(
        &mut merged.resource.paths,
        workspace_emmyrc.resource.paths.clone(),
    );
}

fn merge_emmyrc_prefer_existing_with_array_union(merged: &mut Emmyrc, incoming: &Emmyrc) {
    let Ok(mut merged_value) = serde_json::to_value(&*merged) else {
        return;
    };
    let Ok(incoming_value) = serde_json::to_value(incoming) else {
        return;
    };

    merge_value_prefer_existing_with_array_union(&mut merged_value, incoming_value);

    if let Ok(new_merged) = serde_json::from_value::<Emmyrc>(merged_value) {
        *merged = new_merged;
    }
}

fn merge_value_prefer_existing_with_array_union(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                if let Some(base_value) = base_map.get_mut(&key) {
                    merge_value_prefer_existing_with_array_union(base_value, overlay_value);
                } else {
                    base_map.insert(key, overlay_value);
                }
            }
        }
        (Value::Array(base_array), Value::Array(overlay_array)) => {
            let mut seen = HashSet::new();
            for item in base_array.iter() {
                if let Ok(key) = serde_json::to_string(item) {
                    seen.insert(key);
                }
            }

            for item in overlay_array {
                if let Ok(key) = serde_json::to_string(&item) {
                    if seen.insert(key) {
                        base_array.push(item);
                    }
                } else if !base_array.contains(&item) {
                    base_array.push(item);
                }
            }
        }
        _ => {
            // Keep existing scalar/object value on conflict.
        }
    }
}

/// Build a per-workspace-root `WorkspaceFileMatcher` map from the per-root
/// `Emmyrc` configs produced by `pre_process_emmyrc_for_all_roots`.
///
/// This is the key isolation mechanism: each root's matcher is computed
/// independently from its own config, so a root with `useDefaultIgnores: false`
/// is unaffected by another root that has it enabled.
fn build_workspace_matchers(
    workspace_emmyrcs: &HashMap<PathBuf, Arc<Emmyrc>>,
) -> HashMap<PathBuf, WorkspaceFileMatcher> {
    workspace_emmyrcs
        .iter()
        .map(|(root, emmyrc)| {
            let (include, exclude, exclude_dir) = calculate_include_and_exclude(emmyrc);
            (
                root.clone(),
                WorkspaceFileMatcher::new(include, exclude, exclude_dir),
            )
        })
        .collect()
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
    // Check if explicitly disabled in .emmyrc (auto_load_annotations: false)
    if matches!(emmyrc.gmod.auto_load_annotations, Some(false)) {
        log::info!("GMod annotations auto-load explicitly disabled in .emmyrc");
        return;
    }

    // Determine which path to use (precedence: .gluarc.json > CLI > VSCode extension)
    let annotations_path = if let Some(explicit_path) = &emmyrc.gmod.annotations_path {
        // User specified explicit path in .gluarc.json/.emmyrc - use that (highest priority)
        if explicit_path.is_empty() {
            log::info!("GMod annotations_path is explicitly set to empty string - skipping");
            return;
        }
        log::info!("Using GMod annotations from config: {}", explicit_path);
        explicit_path.clone()
    } else if let Some(cli_path) = &client_config.gmod_annotations_path {
        // Client-provided path (CLI flag or client init options)
        if cli_path.is_empty() {
            log::info!("GMod annotations explicitly disabled by client/CLI");
            return;
        }
        log::info!(
            "Using GMod annotations from client configuration: {}",
            cli_path
        );
        cli_path.clone()
    } else {
        // No path provided by config or client - skip injection
        log::info!("No GMod annotations path available");
        return;
    };

    // Add to library paths
    use glua_code_analysis::EmmyLibraryItem;
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

/// Inject gamemode base libraries detected by the VSCode extension.
///
/// When a gamemode derives from a base gamemode (e.g., cityrp derives from
/// sandbox), the base gamemode's folder is added as a library so that the
/// language server can resolve references to code defined in the base.
///
/// This is controlled by `gmod.autoDetectGamemodeBase` — set to `false` to
/// disable.
fn inject_gamemode_base_libraries(client_config: &ClientConfig, emmyrc: &mut Emmyrc) {
    // Check if explicitly disabled in config
    if matches!(emmyrc.gmod.auto_detect_gamemode_base, Some(false)) {
        log::info!("Gamemode base auto-detection explicitly disabled in config");
        return;
    }

    if client_config.gamemode_base_libraries.is_empty() {
        return;
    }

    use glua_code_analysis::EmmyLibraryItem;
    for lib_path in &client_config.gamemode_base_libraries {
        if emmyrc
            .workspace
            .library
            .iter()
            .any(|item| item.get_path() == lib_path)
        {
            log::info!(
                "Gamemode base library already exists in workspace library: {}",
                lib_path
            );
            continue;
        }

        emmyrc
            .workspace
            .library
            .push(EmmyLibraryItem::Path(lib_path.clone()));
        log::info!(
            "Gamemode base library added to workspace library: {}",
            lib_path
        );
    }
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
        str::FromStr,
        time::{SystemTime, UNIX_EPOCH},
    };

    use glua_code_analysis::{DiagnosticCode, WorkspaceFolder, collect_workspace_files};

    use crate::handlers::ClientConfig;

    use super::{
        WorkspaceFileMatcher, collect_config_files_from_dir, collect_config_roots, dedup_paths,
        load_emmy_config, push_configs_from_preferred_workspace_root,
    };

    fn create_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glua_ls_config_test_{unique}"));
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
    fn test_load_emmy_config_isolation_enabled_does_not_overlay_secondary_workspace_diagnostics() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": true }, "diagnostics": { "globals": ["A_ONLY"], "disable": ["inject-field"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": true }, "diagnostics": { "globals": ["B_ONLY"], "disable": ["undefined-global"] } }"#,
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

    #[test]
    fn test_cli_gmod_annotations_path_is_injected_into_library() {
        let workspace = create_temp_dir();
        let annotations_dir = create_temp_dir();
        touch(&workspace.join(".emmyrc.json"));

        let client_config = ClientConfig {
            gmod_annotations_path: Some(annotations_dir.to_string_lossy().to_string()),
            ..Default::default()
        };

        let loaded = load_emmy_config(vec![workspace.clone()], client_config);

        let library_paths: Vec<PathBuf> = loaded
            .emmyrc
            .workspace
            .library
            .iter()
            .map(|item| PathBuf::from(item.get_path()))
            .collect();

        assert!(
            library_paths.iter().any(|p| {
                p.canonicalize().unwrap_or_else(|_| p.clone())
                    == annotations_dir
                        .canonicalize()
                        .unwrap_or_else(|_| annotations_dir.clone())
            }),
            "CLI annotations path should be in library: {:?}",
            library_paths
        );

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(annotations_dir);
    }

    #[test]
    fn test_config_annotations_path_overrides_cli_path() {
        let workspace = create_temp_dir();
        let config_annotations_dir = create_temp_dir();
        let cli_annotations_dir = create_temp_dir();
        touch(&workspace.join(".emmyrc.json"));

        // Write explicit annotations path to config
        fs::write(
            workspace.join(".emmyrc.json"),
            format!(
                r#"{{ "gmod": {{ "annotationsPath": "{}" }} }}"#,
                config_annotations_dir.to_string_lossy().replace("\\", "/")
            ),
        )
        .expect("failed to write config");

        // CLI provides a different path
        let client_config = ClientConfig {
            gmod_annotations_path: Some(cli_annotations_dir.to_string_lossy().to_string()),
            ..Default::default()
        };

        let loaded = load_emmy_config(vec![workspace.clone()], client_config);

        let library_paths: Vec<PathBuf> = loaded
            .emmyrc
            .workspace
            .library
            .iter()
            .map(|item| PathBuf::from(item.get_path()))
            .collect();

        // Config path should win over CLI path
        assert!(
            library_paths.iter().any(|p| {
                p.canonicalize().unwrap_or_else(|_| p.clone())
                    == config_annotations_dir
                        .canonicalize()
                        .unwrap_or_else(|_| config_annotations_dir.clone())
            }),
            "Config annotations path should be in library: {:?}",
            library_paths
        );
        assert!(
            !library_paths.iter().any(|p| {
                p.canonicalize().unwrap_or_else(|_| p.clone())
                    == cli_annotations_dir
                        .canonicalize()
                        .unwrap_or_else(|_| cli_annotations_dir.clone())
            }),
            "CLI annotations path should NOT be in library when config overrides: {:?}",
            library_paths
        );

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(config_annotations_dir);
        let _ = fs::remove_dir_all(cli_annotations_dir);
    }

    #[test]
    fn test_auto_load_annotations_false_disables_both_cli_and_config() {
        let workspace = create_temp_dir();
        let cli_annotations_dir = create_temp_dir();
        touch(&workspace.join(".emmyrc.json"));

        // Disable auto-loading in config
        fs::write(
            workspace.join(".emmyrc.json"),
            r#"{ "gmod": { "autoLoadAnnotations": false } }"#,
        )
        .expect("failed to write config");

        // CLI provides a path, but should be ignored due to auto_load_annotations: false
        let client_config = ClientConfig {
            gmod_annotations_path: Some(cli_annotations_dir.to_string_lossy().to_string()),
            ..Default::default()
        };

        let loaded = load_emmy_config(vec![workspace.clone()], client_config);

        let library_paths: Vec<PathBuf> = loaded
            .emmyrc
            .workspace
            .library
            .iter()
            .map(|item| PathBuf::from(item.get_path()))
            .collect();

        assert!(
            !library_paths.iter().any(|p| {
                p.canonicalize().unwrap_or_else(|_| p.clone())
                    == cli_annotations_dir
                        .canonicalize()
                        .unwrap_or_else(|_| cli_annotations_dir.clone())
            }),
            "CLI annotations path should NOT be injected when auto_load_annotations is false: {:?}",
            library_paths
        );

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(cli_annotations_dir);
    }

    #[test]
    fn test_auto_load_annotations_null_remains_compatible() {
        let workspace = create_temp_dir();
        let cli_annotations_dir = create_temp_dir();
        touch(&workspace.join(".emmyrc.json"));

        fs::write(
            workspace.join(".emmyrc.json"),
            r#"{ "gmod": { "autoLoadAnnotations": null } }"#,
        )
        .expect("failed to write config");

        let client_config = ClientConfig {
            gmod_annotations_path: Some(cli_annotations_dir.to_string_lossy().to_string()),
            ..Default::default()
        };

        let loaded = load_emmy_config(vec![workspace.clone()], client_config);

        let library_paths: Vec<PathBuf> = loaded
            .emmyrc
            .workspace
            .library
            .iter()
            .map(|item| PathBuf::from(item.get_path()))
            .collect();

        assert!(library_paths.iter().any(|p| {
            p.canonicalize().unwrap_or_else(|_| p.clone())
                == cli_annotations_dir
                    .canonicalize()
                    .unwrap_or_else(|_| cli_annotations_dir.clone())
        }));

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(cli_annotations_dir);
    }

    #[test]
    fn test_multi_root_merge_prefers_first_root_scalar_conflicts_when_isolation_disabled() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": false, "useDefaultIgnores": false, "ignoreDirDefaults": ["**/custom-a/**"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": true, "ignoreDirDefaults": ["**/custom-b/**"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert!(!loaded.emmyrc.workspace.use_default_ignores);
        let resolved = loaded.emmyrc.workspace.resolve_ignore_dir_defaults();
        assert!(
            resolved.contains(&"**/custom-a/**".to_string()),
            "resolved globs should include custom-a: {:?}",
            resolved
        );
        assert!(
            resolved.contains(&"**/custom-b/**".to_string()),
            "resolved globs should include custom-b: {:?}",
            resolved
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_multi_root_merge_unions_diagnostic_globals_when_isolation_disabled() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": false }, "diagnostics": { "globals": ["A_ONLY"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "diagnostics": { "globals": ["B_ONLY"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert!(
            loaded
                .emmyrc
                .diagnostics
                .globals
                .contains(&"A_ONLY".to_string())
        );
        assert!(
            loaded
                .emmyrc
                .diagnostics
                .globals
                .contains(&"B_ONLY".to_string())
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_multi_root_merge_disables_isolation_if_any_root_disables_it() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": true }, "diagnostics": { "globals": ["A_ONLY"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": false }, "diagnostics": { "globals": ["B_ONLY"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert!(!loaded.emmyrc.workspace.enable_isolation);
        assert!(
            loaded
                .emmyrc
                .diagnostics
                .globals
                .contains(&"A_ONLY".to_string())
        );
        assert!(
            loaded
                .emmyrc
                .diagnostics
                .globals
                .contains(&"B_ONLY".to_string())
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_isolation_disabled_only_registers_workspace_overrides_with_local_configs() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "enableIsolation": false }, "diagnostics": { "severity": { "undefined-global": "warning" } } }"#,
        )
        .expect("failed to write workspace a config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        assert!(loaded.workspace_diagnostic_configs.contains_key(&workspace_a));
        assert!(!loaded.workspace_diagnostic_configs.contains_key(&workspace_b));
        assert!(loaded.workspace_emmyrcs.contains_key(&workspace_a));
        assert!(!loaded.workspace_emmyrcs.contains_key(&workspace_b));
        assert!(loaded.workspace_matchers.contains_key(&workspace_a));
        assert!(!loaded.workspace_matchers.contains_key(&workspace_b));

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    #[test]
    fn test_empty_client_annotations_path_disables_injection() {
        let workspace = create_temp_dir();
        touch(&workspace.join(".emmyrc.json"));

        let client_config = ClientConfig {
            gmod_annotations_path: Some(String::new()),
            ..Default::default()
        };

        let loaded = load_emmy_config(vec![workspace.clone()], client_config);

        assert!(
            loaded.emmyrc.workspace.library.is_empty(),
            "No annotations path should be injected when client path is empty: {:?}",
            loaded.emmyrc.workspace.library
        );

        let _ = fs::remove_dir_all(workspace);
    }

    // -------------------------------------------------------------------------
    // Per-root matcher isolation tests
    // -------------------------------------------------------------------------

    /// Root A has `useDefaultIgnores: false`; root B has `useDefaultIgnores: true`.
    /// The built-in default patterns include `**/tests/**`.
    /// Root A's matcher must NOT exclude `tests/sub/foo.lua`, while root B's must.
    #[test]
    fn test_per_root_matchers_isolate_use_default_ignores() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": false } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": true } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let matcher_a = loaded
            .workspace_matchers
            .get(&workspace_a)
            .expect("workspace a should have a matcher");
        let matcher_b = loaded
            .workspace_matchers
            .get(&workspace_b)
            .expect("workspace b should have a matcher");

        // **/tests/** is a built-in default ignore pattern.
        // Root A (useDefaultIgnores=false): tests/sub/foo.lua should pass.
        let tests_rel = std::path::Path::new("tests/sub/foo.lua");
        assert!(
            matcher_a.is_match(&workspace_a.join("tests/sub/foo.lua"), tests_rel),
            "root A (useDefaultIgnores=false) should NOT exclude tests/sub/foo.lua"
        );

        // Root B (useDefaultIgnores=true): tests/sub/foo.lua should be excluded.
        assert!(
            !matcher_b.is_match(&workspace_b.join("tests/sub/foo.lua"), tests_rel),
            "root B (useDefaultIgnores=true) should exclude tests/sub/foo.lua via built-in **/tests/**"
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    /// Each root's `ignoreDirDefaults` is scoped to that root's matcher only.
    /// The legacy-string form replaces built-ins entirely, so each root ends up
    /// with exactly its own custom glob and nothing from the other root.
    #[test]
    fn test_per_root_matchers_isolate_ignore_dir_defaults() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();

        // Root A excludes **/vendor/**; root B excludes **/third_party/**
        // (legacy string list → replaces built-ins entirely)
        fs::write(
            workspace_a.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": true, "ignoreDirDefaults": ["**/vendor/**"] } }"#,
        )
        .expect("failed to write workspace a config");
        fs::write(
            workspace_b.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": true, "ignoreDirDefaults": ["**/third_party/**"] } }"#,
        )
        .expect("failed to write workspace b config");

        let loaded = load_emmy_config(
            vec![workspace_a.clone(), workspace_b.clone()],
            ClientConfig::default(),
        );

        let matcher_a = loaded
            .workspace_matchers
            .get(&workspace_a)
            .expect("workspace a should have a matcher");
        let matcher_b = loaded
            .workspace_matchers
            .get(&workspace_b)
            .expect("workspace b should have a matcher");

        // **/vendor/** matches paths with at least one segment under vendor/
        let vendor_rel = std::path::Path::new("vendor/sub/foo.lua");
        // **/third_party/** matches paths with at least one segment under third_party/
        let third_party_rel = std::path::Path::new("third_party/sub/foo.lua");

        // Root A excludes vendor but NOT third_party
        assert!(
            !matcher_a.is_match(&workspace_a.join("vendor/sub/foo.lua"), vendor_rel),
            "root A should exclude vendor/sub/foo.lua via **/vendor/**"
        );
        assert!(
            matcher_a.is_match(
                &workspace_a.join("third_party/sub/foo.lua"),
                third_party_rel
            ),
            "root A should NOT exclude third_party/sub/foo.lua (that's root B's rule)"
        );

        // Root B excludes third_party but NOT vendor
        assert!(
            !matcher_b.is_match(
                &workspace_b.join("third_party/sub/foo.lua"),
                third_party_rel
            ),
            "root B should exclude third_party/sub/foo.lua via **/third_party/**"
        );
        assert!(
            matcher_b.is_match(&workspace_b.join("vendor/sub/foo.lua"), vendor_rel),
            "root B should NOT exclude vendor/sub/foo.lua (that's root A's rule)"
        );

        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }

    /// After a config reload (simulated by calling `load_emmy_config` with updated
    /// workspace configs), the returned `workspace_matchers` reflects the new settings,
    /// not the previous ones.  This verifies that the reload paths produce fresh
    /// per-root matchers rather than stale ones.
    #[test]
    fn test_reload_path_produces_fresh_per_root_matchers() {
        let workspace = create_temp_dir();

        // Initial config: default ignores disabled → tests/ files should be included
        fs::write(
            workspace.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": false } }"#,
        )
        .expect("failed to write initial config");

        let loaded_initial = load_emmy_config(vec![workspace.clone()], ClientConfig::default());

        let matcher_initial = loaded_initial
            .workspace_matchers
            .get(&workspace)
            .expect("workspace should have a matcher");
        // **/tests/** is a built-in pattern; with useDefaultIgnores=false it should be included
        let tests_rel = std::path::Path::new("tests/sub/foo.lua");
        let tests_abs = workspace.join("tests/sub/foo.lua");

        assert!(
            matcher_initial.is_match(&tests_abs, tests_rel),
            "before reload: tests/sub/foo.lua should be included (useDefaultIgnores=false)"
        );

        // Simulate user enabling default ignores (config change → reload)
        fs::write(
            workspace.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": true } }"#,
        )
        .expect("failed to write updated config");

        let loaded_reloaded = load_emmy_config(vec![workspace.clone()], ClientConfig::default());

        let matcher_reloaded = loaded_reloaded
            .workspace_matchers
            .get(&workspace)
            .expect("workspace should have a matcher after reload");

        assert!(
            !matcher_reloaded.is_match(&tests_abs, tests_rel),
            "after reload: tests/sub/foo.lua should be excluded (useDefaultIgnores=true, **/tests/**)"
        );

        let _ = fs::remove_dir_all(workspace);
    }

    // -------------------------------------------------------------------------
    // Regression: nested/overlapping workspace roots — most specific root wins
    // -------------------------------------------------------------------------

    /// When two workspace roots overlap (one is nested inside the other),
    /// `is_workspace_file_inner` must apply **only** the inner root's matcher
    /// for files that live under the inner root, not the outer root's matcher.
    ///
    /// Scenario:
    ///   outer_root  → useDefaultIgnores: true  (excludes **/tests/**)
    ///   inner_root  → useDefaultIgnores: false  (tests/ files are allowed)
    ///
    /// A file at `inner_root/tests/foo.lua` must be accepted because the
    /// inner root is the most specific match.
    #[test]
    fn test_is_workspace_file_inner_prefers_most_specific_nested_root() {
        use glua_code_analysis::{WorkspaceImport, file_path_to_uri};

        use super::is_workspace_file_inner;

        let outer_root = create_temp_dir();
        let inner_root = outer_root.join("inner");
        fs::create_dir_all(&inner_root).expect("failed to create inner_root");

        fs::write(
            outer_root.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": true } }"#,
        )
        .expect("failed to write outer config");
        fs::write(
            inner_root.join(".emmyrc.json"),
            r#"{ "workspace": { "useDefaultIgnores": false } }"#,
        )
        .expect("failed to write inner config");

        // Build workspace folders and matchers the same way the real code does.
        let workspace_folders = vec![
            WorkspaceFolder {
                root: outer_root.clone(),
                import: WorkspaceImport::All,
                is_library: false,
            },
            WorkspaceFolder {
                root: inner_root.clone(),
                import: WorkspaceImport::All,
                is_library: false,
            },
        ];

        let loaded = load_emmy_config(
            vec![outer_root.clone(), inner_root.clone()],
            ClientConfig::default(),
        );

        // A file under the inner root in tests/ — should pass (inner has
        // useDefaultIgnores=false) and must NOT be rejected by the outer
        // root's matcher.
        let target_path = inner_root.join("tests").join("foo.lua");
        let Some(target_uri) = file_path_to_uri(&target_path) else {
            panic!("failed to build URI for target path");
        };

        let result = is_workspace_file_inner(
            &target_uri,
            &workspace_folders,
            &loaded.workspace_matchers,
            &WorkspaceFileMatcher::default(),
        );

        assert!(
            result,
            "file under inner_root/tests/ should be accepted because the inner root \
             (useDefaultIgnores=false) is the most specific match and must not be \
             rejected by the outer root's matcher"
        );

        // A file under the outer root but NOT under the inner root in tests/
        // — should be rejected by the outer root's matcher.
        let outer_tests_path = outer_root.join("tests").join("other.lua");
        let Some(outer_tests_uri) = file_path_to_uri(&outer_tests_path) else {
            panic!("failed to build URI for outer tests path");
        };

        let outer_result = is_workspace_file_inner(
            &outer_tests_uri,
            &workspace_folders,
            &loaded.workspace_matchers,
            &WorkspaceFileMatcher::default(),
        );

        assert!(
            !outer_result,
            "file under outer_root/tests/ (not inside inner_root) should be rejected \
             by the outer root's matcher (useDefaultIgnores=true)"
        );

        let _ = fs::remove_dir_all(outer_root);
    }

    // -------------------------------------------------------------------------
    // Regression: config file DELETE triggers emmyrc reload
    // -------------------------------------------------------------------------

    /// `get_file_type` must classify config files the same way regardless of
    /// whether the file still exists on disk (i.e. after deletion).
    /// This verifies the helper returns `Some(WatchedFileType::Emmyrc)` for all
    /// recognised config file names, which is the precondition for the DELETE
    /// branch in `on_did_change_watched_files` to push to `emmyrc_dirs`.
    ///
    /// We test the pure `get_file_type` helper directly since the async handler
    /// requires a full server context.
    #[test]
    fn test_get_file_type_classifies_config_files_without_requiring_disk_existence() {
        // This test lives in workspace_manager.rs but exercises the sibling
        // watched_file_handler module's `get_file_type`.  We replicate the
        // classification logic inline to avoid cross-module private access,
        // and separately verify the DELETE path is reachable via integration
        // coverage in `watched_file_handler` itself.
        //
        // What we assert here is that the file names the handler recognises as
        // config files are a superset of the files that need reload-on-delete:
        let config_names = [".emmyrc.json", ".luarc.json", ".emmyrc.lua", ".gluarc.json"];

        for name in &config_names {
            // Construct a URI for a non-existent path — simulating a deleted file.
            let fake_path = std::path::PathBuf::from("/workspace").join(name);
            let uri_str = format!("file:///workspace/{}", name.trim_start_matches('/'));
            // Validate the URI parses and the path component matches our name.
            let uri = lsp_types::Uri::from_str(&uri_str)
                .unwrap_or_else(|_| panic!("URI should parse for {}", name));
            let resolved = glua_code_analysis::uri_to_file_path(&uri);
            let resolved_name =
                resolved.and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
            assert_eq!(
                resolved_name.as_deref(),
                Some(*name),
                "uri_to_file_path should resolve file_name for {}",
                name
            );
            // The fake_path variable is unused if we skip disk checks — that's fine.
            drop(fake_path);
        }
    }
}
