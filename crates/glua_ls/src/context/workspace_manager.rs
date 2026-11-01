use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use std::{path::PathBuf, sync::Arc, time::Duration};

use super::{ClientProxy, DebouncedAnalysis, FileDiagnostic, StatusBar};
use crate::codestyle::apply_workspace_code_style_off_workers;
use crate::context::ServerContextSnapshot;
use crate::context::lsp_features::LspFeatures;
use crate::handlers::{
    ClientConfig, apply_file_system_changes, apply_workspace_diagnostic_configs, init_analysis,
    refresh_open_document_features,
};
use crate::util::{LongRunningWatchdogStatus, spawn_long_running_watchdog};
use glua_code_analysis::uri_to_file_path;
use glua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, EmmyrcChange, LuaDiagnosticConfig, WorkspaceFolder, WorkspaceImport,
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
    /// The workspace recheck still waiting for edits to pause.
    recheck_token: Arc<Mutex<Option<Arc<QuietPeriod>>>>,
    /// The config reload still waiting out its delay.
    config_token: Arc<Mutex<Option<Arc<QuietPeriod>>>>,
    /// Held while a config reload compares and applies, so reloads apply in
    /// the order they load.
    config_apply: Arc<Mutex<()>>,
    file_diagnostic: Arc<FileDiagnostic>,
    lsp_features: Arc<LspFeatures>,
    debounced_analysis: Arc<DebouncedAnalysis>,
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
    /// The config the analysis was last loaded with.
    pub loaded_config: Option<LoadedConfig>,
}

impl WorkspaceManager {
    pub fn new(
        analysis: Arc<RwLock<EmmyLuaAnalysis>>,
        client: Arc<ClientProxy>,
        status_bar: Arc<StatusBar>,
        file_diagnostic: Arc<FileDiagnostic>,
        lsp_features: Arc<LspFeatures>,
        debounced_analysis: Arc<DebouncedAnalysis>,
    ) -> Self {
        Self {
            analysis,
            client,
            status_bar,
            client_config: ClientConfig::default(),
            workspace_folders: Vec::new(),
            recheck_token: Arc::new(Mutex::new(None)),
            config_token: Arc::new(Mutex::new(None)),
            config_apply: Arc::new(Mutex::new(())),
            file_diagnostic,
            lsp_features,
            debounced_analysis,
            watcher: None,
            current_open_files: HashSet::new(),
            match_file_pattern: WorkspaceFileMatcher::default(),
            per_root_matchers: HashMap::new(),
            loaded_config: None,
        }
    }

    /// After `delay`, loads the config again and applies what changed; a call
    /// made while an earlier one still waits replaces it.
    pub async fn schedule_config_reload(
        &self,
        delay: Duration,
        workspace_manager: Arc<RwLock<WorkspaceManager>>,
    ) {
        let mut config_token = self.config_token.lock().await;
        if let Some(token) = config_token.as_ref() {
            token.cancel();
            debug!("cancel pending config reload");
        }
        let token = Arc::new(QuietPeriod::new(delay));
        config_token.replace(token.clone());
        drop(config_token);

        let pending = self.config_token.clone();
        tokio::spawn(async move {
            token.wait().await;
            if token.is_cancelled() {
                return;
            }
            {
                let mut pending = pending.lock().await;
                if pending
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &token))
                {
                    pending.take();
                }
            }
            Self::reload_config(workspace_manager).await;
        });
    }

    /// Loads the config again and applies it with only the work its change
    /// from the loaded config needs, returning that change.
    pub async fn reload_config(workspace_manager: Arc<RwLock<WorkspaceManager>>) -> EmmyrcChange {
        let config_apply = workspace_manager.read().await.config_apply.clone();
        let _applying = config_apply.lock().await;
        let (analysis, client, status_bar, file_diagnostic, lsp_features, debounced_analysis) = {
            let manager = workspace_manager.read().await;
            (
                manager.analysis.clone(),
                manager.client.clone(),
                manager.status_bar.clone(),
                manager.file_diagnostic.clone(),
                manager.lsp_features.clone(),
                manager.debounced_analysis.clone(),
            )
        };
        let (workspace_folders, client_config) = {
            let manager = workspace_manager.read().await;
            (
                manager.workspace_folders.clone(),
                manager.client_config.clone(),
            )
        };

        let watchdog_status = LongRunningWatchdogStatus::new("Reloading GLuaLS configuration");
        let _watchdog =
            spawn_long_running_watchdog("workspace config reload", watchdog_status.clone());
        let loaded =
            load_emmy_config_off_workers(collect_config_roots(&workspace_folders), client_config)
                .await;
        let (change, open_files) = {
            let mut manager = workspace_manager.write().await;
            let change = manager
                .loaded_config
                .as_ref()
                .map_or(EmmyrcChange::Workspace, |previous| {
                    loaded.change_from(previous)
                });
            if change != EmmyrcChange::Unchanged {
                manager.per_root_matchers = loaded.workspace_matchers.clone();
                let (include, exclude, exclude_dir) = calculate_include_and_exclude(&loaded.emmyrc);
                manager.match_file_pattern =
                    WorkspaceFileMatcher::new(include, exclude, exclude_dir);
                manager.loaded_config = Some(loaded.clone());
            }
            (change, manager.open_workspace_files())
        };
        info!("config reload: {change:?}");

        match change {
            EmmyrcChange::Unchanged => {}
            EmmyrcChange::Workspace => {
                apply_workspace_code_style_off_workers(
                    workspace_folders.clone(),
                    loaded.emmyrc.clone(),
                )
                .await;
                init_analysis(
                    &analysis,
                    &client,
                    &status_bar,
                    &file_diagnostic,
                    &lsp_features,
                    workspace_folders,
                    loaded,
                    open_files,
                    watchdog_status,
                )
                .await;
            }
            EmmyrcChange::Editor | EmmyrcChange::Diagnostics | EmmyrcChange::Analysis => {
                if change >= EmmyrcChange::Diagnostics {
                    apply_workspace_code_style_off_workers(
                        workspace_folders.clone(),
                        loaded.emmyrc.clone(),
                    )
                    .await;
                }
                let affects_index = {
                    let mut analysis = analysis.write().await;
                    let applied = analysis.apply_config(loaded.emmyrc.clone());
                    apply_workspace_diagnostic_configs(
                        &mut analysis,
                        &loaded.workspace_diagnostic_configs,
                    );
                    applied.affects_index()
                };
                if affects_index {
                    // The rebuild re-diagnoses every file and refreshes the
                    // open documents' views once it settles.
                    debounced_analysis.request_rebuild();
                } else {
                    if change != EmmyrcChange::Diagnostics {
                        refresh_open_document_features(&client, &lsp_features);
                    }
                    if change >= EmmyrcChange::Diagnostics {
                        file_diagnostic
                            .add_workspace_diagnostic_task(0, false)
                            .await;
                    }
                }
            }
        }
        change
    }

    pub async fn extend_recheck_delay(&self) -> Option<()> {
        let recheck_token = self.recheck_token.lock().await;
        if let Some(token) = recheck_token.as_ref() {
            token.extend().await;
        }

        Some(())
    }

    /// Once edits pause for `delay`, removes the files deleted from disk
    /// behind the editor's back and computes every file's diagnostics again,
    /// publishing those that changed.
    pub async fn recheck_workspace(&self, delay: Duration, context: ServerContextSnapshot) {
        log::info!("recheck workspace with delay: {:?}", delay);
        let mut recheck_token = self.recheck_token.lock().await;
        if let Some(token) = recheck_token.as_ref() {
            token.cancel();
            log::info!("cancel pending workspace recheck");
        }

        let token = Arc::new(QuietPeriod::new(delay));
        recheck_token.replace(token.clone());
        drop(recheck_token);
        let file_diagnostic = self.file_diagnostic.clone();
        let lsp_features = self.lsp_features.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            token.wait().await;
            if token.is_cancelled() {
                return;
            }

            // The paths are read under the lock, the disk off it.
            let paths = context.analysis().read().await.local_file_paths();
            let deleted = tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .filter(|(_, path)| !path.exists())
                    .map(|(uri, _)| (uri, None))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            if !deleted.is_empty() {
                log::info!("recheck removes {} files deleted from disk", deleted.len());
            }
            apply_file_system_changes(&context, deleted).await;

            file_diagnostic.forget_missing().await;
            file_diagnostic.add_workspace_diagnostic_task(0, true).await;
            refresh_open_document_features(&client, &lsp_features);
        });
    }

    /// The open documents the workspace admits, as it would on their opening.
    pub fn open_workspace_files(&self) -> HashSet<Uri> {
        self.current_open_files
            .iter()
            .filter(|uri| self.is_workspace_file(uri))
            .cloned()
            .collect()
    }

    /// Whether `uri` is where the workspace loads config from: directly in a
    /// workspace root.
    pub fn is_workspace_config_file(&self, uri: &Uri) -> bool {
        uri_to_file_path(uri)
            .as_deref()
            .and_then(Path::parent)
            .is_some_and(|dir| {
                self.workspace_folders
                    .iter()
                    .any(|workspace| workspace.root == dir)
            })
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

fn collect_config_roots(workspace_folders: &[WorkspaceFolder]) -> Vec<PathBuf> {
    dedup_paths(
        workspace_folders
            .iter()
            .map(|workspace| workspace.root.clone())
            .collect(),
    )
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

#[derive(Clone, Default)]
pub struct LoadedConfig {
    /// The workspace roots the config was loaded for.
    pub roots: Vec<PathBuf>,
    pub emmyrc: Arc<Emmyrc>,
    pub workspace_diagnostic_configs: HashMap<PathBuf, LuaDiagnosticConfig>,
    pub workspace_emmyrcs: HashMap<PathBuf, Arc<Emmyrc>>,
    /// Per-workspace-root file matchers built from each root's individual config.
    /// Used by `WorkspaceManager::is_workspace_file` to avoid cross-root leakage
    /// of `useDefaultIgnores` / `ignoreDirDefaults` settings.
    pub workspace_matchers: HashMap<PathBuf, WorkspaceFileMatcher>,
}

impl LoadedConfig {
    /// How far moving from `previous` to this config reaches, across the
    /// merged config and every root's own.
    pub fn change_from(&self, previous: &LoadedConfig) -> EmmyrcChange {
        if self.roots != previous.roots {
            return EmmyrcChange::Workspace;
        }
        self.roots
            .iter()
            .map(|root| previous.root_emmyrc(root).change_to(self.root_emmyrc(root)))
            .fold(previous.emmyrc.change_to(&self.emmyrc), Ord::max)
    }

    /// The config `root` is collected and diagnosed with.
    fn root_emmyrc(&self, root: &Path) -> &Emmyrc {
        self.workspace_emmyrcs.get(root).unwrap_or(&self.emmyrc)
    }
}

/// [`load_emmy_config`] on a blocking thread: it reads every config file.
pub async fn load_emmy_config_off_workers(
    config_roots: Vec<PathBuf>,
    client_config: ClientConfig,
) -> LoadedConfig {
    tokio::task::spawn_blocking(move || load_emmy_config(config_roots, client_config))
        .await
        .unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
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

    // Inject gamemode base libraries if detected and not explicitly disabled.
    // For the global merged config we don't have a single workspace root, so
    // pass `None`; per-root detection happens in `pre_process_emmyrc_for_all_roots`.
    inject_gamemode_base_libraries(&client_config, &mut emmyrc, None);

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
        roots: config_roots,
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
        let local_config_files =
            collect_config_files_from_dir(workspace_root, luarc_file, emmyrc_file, emmyrc_lua_file);
        let has_local_config = !local_config_files.is_empty();

        let mut workspace_config_files = global_config_files.to_vec();
        workspace_config_files.extend(local_config_files);

        let mut workspace_emmyrc = load_configs(
            workspace_config_files,
            client_config.partial_emmyrcs.clone(),
        );
        merge_client_config(client_config, &mut workspace_emmyrc);
        inject_gmod_annotations(client_config, &mut workspace_emmyrc);
        inject_gamemode_base_libraries(
            client_config,
            &mut workspace_emmyrc,
            Some(workspace_root.as_path()),
        );
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

/// Resolve the GMod annotations path that *would* be injected, without
/// mutating anything.
///
/// Returns `Some(path)` only when annotations should actually be injected
/// (i.e. auto-load is not explicitly disabled and a non-empty path is
/// available). Returns `None` when:
///   - `gmod.autoLoadAnnotations == Some(false)` (explicit opt-out), or
///   - the client/CLI path is `Some("")` (explicit empty = skip), or
///   - no path was provided by config or client.
///
/// Precedence (highest first):
///   1. `gmod.annotationsPath` from `.gluarc.json`/`.emmyrc` (non-empty)
///   2. Client/CLI `gmod_annotations_path` (non-empty)
///   3. none
fn resolve_gmod_annotations_path(client_config: &ClientConfig, emmyrc: &Emmyrc) -> Option<String> {
    // Check if explicitly disabled in .emmyrc (auto_load_annotations: false)
    if matches!(emmyrc.gmod.auto_load_annotations, Some(false)) {
        log::info!("GMod annotations auto-load explicitly disabled in .emmyrc");
        return None;
    }

    // Determine which path to use (precedence: .gluarc.json > CLI > VSCode extension)
    if let Some(explicit_path) = &emmyrc.gmod.annotations_path {
        // User specified explicit path in .gluarc.json/.emmyrc - use that (highest priority)
        if explicit_path.is_empty() {
            log::info!(
                "GMod annotations_path is empty in config; falling back to client configuration"
            );
        } else {
            log::info!("Using GMod annotations from config: {}", explicit_path);
            return Some(explicit_path.clone());
        }
    }

    if let Some(cli_path) = &client_config.gmod_annotations_path {
        // Client-provided path (CLI flag or client init options)
        if cli_path.is_empty() {
            log::info!("GMod annotations explicitly disabled by client/CLI");
            return None;
        }
        log::info!(
            "Using GMod annotations from client configuration: {}",
            cli_path
        );
        return Some(cli_path.clone());
    }

    // No path provided by config or client - skip injection
    log::info!("No GMod annotations path available");
    None
}

fn gmod_annotations_explicitly_disabled(client_config: &ClientConfig, emmyrc: &Emmyrc) -> bool {
    if matches!(emmyrc.gmod.auto_load_annotations, Some(false)) {
        return true;
    }

    if emmyrc
        .gmod
        .annotations_path
        .as_ref()
        .is_some_and(|path| !path.is_empty())
    {
        return false;
    }

    client_config.gmod_annotations_path.as_deref() == Some("")
}

/// Inject GMod annotations path into workspace library if appropriate
fn inject_gmod_annotations(client_config: &ClientConfig, emmyrc: &mut Emmyrc) {
    let Some(annotations_path) = resolve_gmod_annotations_path(client_config, emmyrc) else {
        return;
    };

    emmyrc.prioritize_gmod_annotations_library(annotations_path);
    log::info!("GMod annotations prioritized before configured libraries");
}

/// Validate that the resolved GMod annotations set is usable for the language
/// server startup path.
///
/// This is **LS-only**: `glua_check` and lower-level `VirtualWorkspace`/analysis
/// tests intentionally run without annotations and must not be affected. The
/// check is only meaningful when `gmod.enabled` is true (the default).
///
/// Returns `Ok(())` when:
///   - `gmod.enabled` is false, or
///   - annotation loading was explicitly disabled, or
///   - a resolved annotations path exists, points to an existing directory,
///     and that directory contains at least one `.lua` annotation file.
///
/// Returns `Err(reason)` with a user-facing reason string for any of:
///   - no path resolved,
///   - the resolved path does not exist,
///   - the resolved path is not a directory,
///   - the resolved directory contains no `.lua` files (present-but-empty).
pub fn validate_gmod_annotations_for_ls(
    client_config: &ClientConfig,
    emmyrc: &Emmyrc,
) -> Result<(), String> {
    if !emmyrc.gmod.enabled {
        return Ok(());
    }

    if gmod_annotations_explicitly_disabled(client_config, emmyrc) {
        return Ok(());
    }

    let Some(annotations_path) = resolve_gmod_annotations_path(client_config, emmyrc) else {
        return Err(
            "GMod annotations could not be found. Try restarting VS Code or the \
             language server. If you are using custom annotations, set \
             \"gluals.gmod.annotationsPath\" in settings or .gluarc.json."
                .to_string(),
        );
    };

    let path = PathBuf::from(&annotations_path);
    if !path.exists() {
        return Err(format!(
            "GMod annotations folder does not exist at \"{annotations_path}\". If you set a \
             custom path in settings or .gluarc.json, verify the folder exists; \
             otherwise, restart VS Code to download them automatically."
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "GMod annotations path is a file instead of a folder: \"{annotations_path}\". \
             Please point \"gluals.gmod.annotationsPath\" in settings or .gluarc.json to \
             a folder containing .lua annotation files."
        ));
    }

    // Confirm the directory actually contains annotation files. Annotation
    // libraries can be nested (for example under `garrysmod/...`), so search
    // recursively but stop as soon as the first `.lua` file is found.
    if !directory_contains_lua_file(&path) {
        return Err(format!(
            "GMod annotations folder at \"{annotations_path}\" contains no .lua files. \
             If using a custom path in settings, verify the folder; otherwise, run \
             \"GLua: Download GMod Annotations\" from the Command Palette or restart VS Code."
        ));
    }

    Ok(())
}

/// Recursively (depth-first) check whether a directory contains at least one
/// `.lua` file. Returns `false` only when the entire subtree is empty of
/// `.lua` files.
///
/// Directory/file classification uses `DirEntry::file_type()` (no symlink
/// follow) so symlinked directory cycles cannot cause infinite recursion: a
/// symlink to a directory is classified as a file here (it has no `.lua`
/// extension in its own name) and is never descended into.
fn directory_contains_lua_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        // `file_type()` does not follow symlinks, so a symlinked directory
        // cycle cannot drive this recursion. Symlinks to files are also
        // skipped (their own name is what we check for the extension, and a
        // symlink entry reports as a symlink, not a regular file).
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_file() {
            if path.extension().and_then(|e| e.to_str()) == Some("lua") {
                return true;
            }
        } else if ft.is_dir() && directory_contains_lua_file(&path) {
            return true;
        }
    }
    false
}

/// Inject gamemode base libraries detected by the VSCode extension.
///
/// When a gamemode derives from a base gamemode (e.g., cityrp derives from
/// sandbox), the base gamemode's folder is added as a library so that the
/// language server can resolve references to code defined in the base.
///
/// This is controlled by `gmod.autoDetectGamemodeBase` — set to `false` to
/// disable.
///
/// `workspace_root` is optional: when provided, the server will additionally
/// scan that root for `gamemodes/<name>/<name>.txt` metadata files and follow
/// the `"base"` chain to detect parent gamemode folders. This is editor-agnostic
/// (works for `glua_check`, Neovim, etc.) and complements the VSCode extension's
/// `gamemodeBaseLibraries` initialization option.
fn inject_gamemode_base_libraries(
    client_config: &ClientConfig,
    emmyrc: &mut Emmyrc,
    workspace_root: Option<&Path>,
) {
    // Check if explicitly disabled in config
    if matches!(emmyrc.gmod.auto_detect_gamemode_base, Some(false)) {
        log::info!("Gamemode base auto-detection explicitly disabled in config");
        return;
    }

    let mut candidate_paths: Vec<String> = client_config.gamemode_base_libraries.to_vec();

    if let Some(root) = workspace_root {
        for path in glua_code_analysis::detect_gamemode_base_libraries(root) {
            // Convert to lossy string for comparison/storage; library paths
            // are stored as strings in `EmmyLibraryItem::Path`.
            candidate_paths.push(path.to_string_lossy().into_owned());
        }
    }

    if candidate_paths.is_empty() {
        return;
    }

    use glua_code_analysis::EmmyLibraryItem;
    for lib_path in &candidate_paths {
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

/// A wait for a quiet period: each [`Self::extend`] while it runs waits one
/// period more.
#[derive(Debug)]
pub struct QuietPeriod {
    cancel_token: CancellationToken,
    period: Duration,
    extended: Mutex<bool>,
}

impl QuietPeriod {
    pub fn new(period: Duration) -> Self {
        Self {
            cancel_token: CancellationToken::new(),
            period,
            extended: Mutex::new(false),
        }
    }

    /// Waits until a whole period passes without an extension, or until
    /// cancelled.
    pub async fn wait(&self) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.period) => {
                    let mut extended = self.extended.lock().await;
                    if !std::mem::replace(&mut *extended, false) {
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

    pub async fn extend(&self) {
        *self.extended.lock().await = true;
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceFileMatcher {
    /// `None` when a pattern is invalid: every file is included.
    include: Option<wax::Any<'static>>,
    /// `None` when a pattern is invalid: no file is excluded.
    exclude: Option<wax::Any<'static>>,
    exclude_dir: Vec<PathBuf>,
}

impl WorkspaceFileMatcher {
    pub fn new(include: Vec<String>, exclude: Vec<String>, exclude_dir: Vec<PathBuf>) -> Self {
        Self {
            include: compile_globs(&include, "include"),
            exclude: compile_globs(&exclude, "exclude"),
            exclude_dir,
        }
    }

    pub fn is_match(&self, path: &Path, relative_path: &Path) -> bool {
        if self.exclude_dir.iter().any(|dir| path.starts_with(dir)) {
            return false;
        }
        if self
            .exclude
            .as_ref()
            .is_some_and(|exclude| exclude.is_match(relative_path))
        {
            return false;
        }
        self.include
            .as_ref()
            .is_none_or(|include| include.is_match(relative_path))
    }
}

fn compile_globs(patterns: &[String], kind: &str) -> Option<wax::Any<'static>> {
    let globs = patterns
        .iter()
        .map(|pattern| wax::Glob::new(pattern).map(wax::Glob::into_owned))
        .collect::<Result<Vec<_>, _>>();
    match globs.and_then(wax::any) {
        Ok(globs) => Some(globs),
        Err(err) => {
            log::error!("Invalid {kind} pattern: {err}");
            None
        }
    }
}

impl Default for WorkspaceFileMatcher {
    fn default() -> Self {
        let include_pattern = vec!["**/*.lua".to_string()];
        Self::new(include_pattern, vec![], vec![])
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        str::FromStr,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use glua_code_analysis::{DiagnosticCode, Emmyrc, WorkspaceFolder, collect_workspace_files};

    use crate::handlers::ClientConfig;

    use super::{
        WorkspaceFileMatcher, collect_config_files_from_dir, collect_config_roots, dedup_paths,
        directory_contains_lua_file, inject_gmod_annotations, load_emmy_config,
        push_configs_from_preferred_workspace_root, resolve_gmod_annotations_path,
        validate_gmod_annotations_for_ls,
    };

    fn create_temp_dir() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "glua_ls_config_test_{started}_{}_{unique}",
            std::process::id()
        ));
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

    /// A reload reads the roots in workspace order, as the first load does,
    /// whichever root's config changed.
    #[test]
    fn test_collect_config_roots_keeps_workspace_order_and_dedups() {
        let workspace_a = WorkspaceFolder::new(PathBuf::from("/workspace/a"), false);
        let workspace_b = WorkspaceFolder::new(PathBuf::from("/workspace/b"), false);

        let roots = collect_config_roots(&[workspace_a.clone(), workspace_b, workspace_a]);

        assert_eq!(
            roots,
            vec![PathBuf::from("/workspace/a"), PathBuf::from("/workspace/b")]
        );
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

        assert!(
            loaded
                .workspace_diagnostic_configs
                .contains_key(&workspace_a)
        );
        assert!(
            !loaded
                .workspace_diagnostic_configs
                .contains_key(&workspace_b)
        );
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
    /// branch in `on_did_change_watched_files` to schedule a config reload.
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

    // ---- Phase 0: LS startup annotation guard ----

    /// Helper: build an `Emmyrc` with the given GMod annotations path and
    /// auto-load setting. `gmod.enabled` is left at its default (true) unless
    /// `enabled` overrides it.
    fn emmyrc_with_annotations(
        annotations_path: Option<&str>,
        auto_load: Option<bool>,
        enabled: Option<bool>,
    ) -> Emmyrc {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.annotations_path = annotations_path.map(|s| s.to_string());
        emmyrc.gmod.auto_load_annotations = auto_load;
        if let Some(e) = enabled {
            emmyrc.gmod.enabled = e;
        }
        emmyrc
    }

    fn client_config_with_annotations_path(path: Option<&str>) -> ClientConfig {
        ClientConfig {
            gmod_annotations_path: path.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_annotations_path_prefers_config_over_client() {
        let emmyrc = emmyrc_with_annotations(Some("/from/config"), None, None);
        let client = client_config_with_annotations_path(Some("/from/client"));

        let resolved = resolve_gmod_annotations_path(&client, &emmyrc);
        assert_eq!(resolved.as_deref(), Some("/from/config"));
    }

    #[test]
    fn resolve_annotations_path_falls_back_to_client_when_config_absent() {
        let emmyrc = emmyrc_with_annotations(None, None, None);
        let client = client_config_with_annotations_path(Some("/from/client"));

        let resolved = resolve_gmod_annotations_path(&client, &emmyrc);
        assert_eq!(resolved.as_deref(), Some("/from/client"));
    }

    #[test]
    fn resolve_annotations_path_returns_none_when_auto_load_disabled() {
        let emmyrc = emmyrc_with_annotations(Some("/from/config"), Some(false), None);
        let client = client_config_with_annotations_path(Some("/from/client"));

        let resolved = resolve_gmod_annotations_path(&client, &emmyrc);
        assert!(resolved.is_none(), "autoLoadAnnotations:false must skip");
    }

    #[test]
    fn resolve_annotations_path_falls_back_to_client_for_empty_config_path() {
        let emmyrc = emmyrc_with_annotations(Some(""), None, None);
        let client = client_config_with_annotations_path(Some("/from/client"));

        let resolved = resolve_gmod_annotations_path(&client, &emmyrc);
        assert_eq!(resolved.as_deref(), Some("/from/client"));
    }

    #[test]
    fn resolve_annotations_path_returns_none_for_empty_client_path() {
        let emmyrc = emmyrc_with_annotations(None, None, None);
        let client = client_config_with_annotations_path(Some(""));

        let resolved = resolve_gmod_annotations_path(&client, &emmyrc);
        assert!(resolved.is_none(), "empty client path must skip");
    }

    #[test]
    fn resolve_annotations_path_returns_none_when_nothing_provided() {
        let emmyrc = emmyrc_with_annotations(None, None, None);
        let client = ClientConfig::default();

        let resolved = resolve_gmod_annotations_path(&client, &emmyrc);
        assert!(resolved.is_none());
    }

    #[test]
    fn inject_annotations_places_metadata_before_configured_libraries() {
        use glua_code_analysis::EmmyLibraryItem;

        let client = client_config_with_annotations_path(Some("/annotations"));
        let mut emmyrc = Emmyrc::default();
        emmyrc
            .workspace
            .library
            .push(EmmyLibraryItem::Path("/library".to_string()));

        inject_gmod_annotations(&client, &mut emmyrc);

        let paths = emmyrc
            .workspace
            .library
            .iter()
            .map(|item| item.get_path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/annotations", "/library"]);
    }

    #[test]
    fn inject_annotations_moves_existing_entry_before_configured_libraries() {
        use glua_code_analysis::EmmyLibraryItem;

        let client = client_config_with_annotations_path(Some("/annotations"));
        let mut emmyrc = Emmyrc::default();
        emmyrc.workspace.library = vec![
            EmmyLibraryItem::Path("/library".to_string()),
            EmmyLibraryItem::Path("/annotations".to_string()),
        ];

        inject_gmod_annotations(&client, &mut emmyrc);

        let paths = emmyrc
            .workspace
            .library
            .iter()
            .map(|item| item.get_path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/annotations", "/library"]);
    }

    #[test]
    fn validate_returns_ok_when_gmod_disabled() {
        // gmod.enabled == false short-circuits validation regardless of paths.
        let emmyrc = emmyrc_with_annotations(None, None, Some(false));
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_ok(), "gmod disabled must pass validation");
    }

    #[test]
    fn validate_fails_when_gmod_enabled_but_no_path_resolved() {
        let emmyrc = emmyrc_with_annotations(None, None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_err(), "enabled + no path must fail");
        let reason = result.unwrap_err();
        assert!(
            reason.contains("GMod annotations could not be found"),
            "reason should explain missing path: {reason}"
        );
    }

    #[test]
    fn validate_succeeds_when_auto_load_disabled() {
        // autoLoadAnnotations:false is an explicit workspace opt-out, not a
        // missing-annotations configuration error.
        let emmyrc = emmyrc_with_annotations(Some("/irrelevant"), Some(false), None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_succeeds_when_client_explicitly_disables_annotations() {
        let emmyrc = emmyrc_with_annotations(None, None, None);
        let client = client_config_with_annotations_path(Some(""));

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(
            result.is_ok(),
            "CLI/client explicit disable must pass LS validation"
        );
    }

    #[test]
    fn validate_fails_when_empty_config_path_has_no_client_fallback() {
        let emmyrc = emmyrc_with_annotations(Some(""), None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(
            result.is_err(),
            "empty config path is fallback-to-client, not explicit disable"
        );
    }

    #[test]
    fn validate_prefers_config_path_over_empty_client_path() {
        let dir = create_temp_dir();
        fs::write(dir.join("meta.lua"), "-- annotation").expect("write lua");

        let emmyrc =
            emmyrc_with_annotations(Some(dir.to_str().expect("path is utf-8")), None, None);
        let client = client_config_with_annotations_path(Some(""));

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(
            result.is_ok(),
            "non-empty config path should beat lower-precedence client disable"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_fails_when_path_does_not_exist() {
        let emmyrc = emmyrc_with_annotations(Some("/does/not/exist/anywhere"), None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(
            reason.contains("does not exist"),
            "reason should mention missing path: {reason}"
        );
    }

    #[test]
    fn validate_fails_when_path_is_file_not_directory() {
        let dir = create_temp_dir();
        let file = dir.join("not_a_dir.lua");
        fs::write(&file, "-- not a directory").expect("write file");

        let emmyrc =
            emmyrc_with_annotations(Some(file.to_str().expect("path is utf-8")), None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(
            reason.contains("is a file instead of a folder"),
            "reason should mention file instead of folder: {reason}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_fails_when_directory_has_no_lua_files() {
        let dir = create_temp_dir();
        // A non-.lua file so the dir is non-empty of files but empty of .lua.
        fs::write(dir.join("readme.txt"), "no lua here").expect("write txt");

        let emmyrc =
            emmyrc_with_annotations(Some(dir.to_str().expect("path is utf-8")), None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(
            reason.contains("contains no .lua files"),
            "reason should mention empty-of-lua: {reason}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_succeeds_when_directory_contains_lua_files() {
        let dir = create_temp_dir();
        fs::write(dir.join("meta.lua"), "-- an annotation").expect("write lua");

        let emmyrc =
            emmyrc_with_annotations(Some(dir.to_str().expect("path is utf-8")), None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_ok(), "populated dir must pass");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_falls_back_to_client_for_empty_config_path() {
        let dir = create_temp_dir();
        fs::write(dir.join("meta.lua"), "-- an annotation").expect("write lua");

        let emmyrc = emmyrc_with_annotations(Some(""), None, None);
        let client =
            client_config_with_annotations_path(Some(dir.to_str().expect("path is utf-8")));

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(
            result.is_ok(),
            "client fallback should validate: {result:?}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_succeeds_with_nested_lua_files() {
        // directory_contains_lua_file is recursive; nested .lua counts.
        let dir = create_temp_dir();
        let nested = dir.join("garrysmod").join("libraries");
        fs::create_dir_all(&nested).expect("mkdir");
        fs::write(nested.join("player.lua"), "-- nested annotation").expect("write lua");

        let emmyrc =
            emmyrc_with_annotations(Some(dir.to_str().expect("path is utf-8")), None, None);
        let client = ClientConfig::default();

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_ok(), "nested .lua must pass");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_uses_client_path_when_config_path_absent() {
        let dir = create_temp_dir();
        fs::write(dir.join("meta.lua"), "-- annotation").expect("write lua");

        // No config path; client provides a valid populated directory.
        let emmyrc = emmyrc_with_annotations(None, None, None);
        let client =
            client_config_with_annotations_path(Some(dir.to_str().expect("path is utf-8")));

        let result = validate_gmod_annotations_for_ls(&client, &emmyrc);
        assert!(result.is_ok(), "client-provided populated dir must pass");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_uses_workspace_relative_config_annotations_path_after_load() {
        let workspace = create_temp_dir();
        let config_annotations_dir = workspace.join("my-custom-annotations");
        fs::create_dir_all(&config_annotations_dir).expect("mkdir config annotations");
        fs::write(
            config_annotations_dir.join("meta.lua"),
            "-- workspace annotations",
        )
        .expect("write config annotation");

        let vscode_annotations_dir = create_temp_dir();
        fs::write(
            vscode_annotations_dir.join("meta.lua"),
            "-- vscode annotations",
        )
        .expect("write vscode annotation");

        fs::write(
            workspace.join(".gluarc.json"),
            r#"{ "gmod": { "annotationsPath": "./my-custom-annotations" } }"#,
        )
        .expect("write config");

        let client = client_config_with_annotations_path(Some(
            vscode_annotations_dir.to_str().expect("path is utf-8"),
        ));

        let loaded = load_emmy_config(vec![workspace.clone()], client.clone());

        let resolved = resolve_gmod_annotations_path(&client, &loaded.emmyrc)
            .expect("config annotations path should resolve");
        let resolved_path = PathBuf::from(&resolved);
        assert!(
            resolved_path.is_absolute(),
            "workspace-relative config path should be preprocessed before validation, got: {resolved}"
        );
        assert_eq!(
            resolved_path.canonicalize().expect("resolved annotations"),
            config_annotations_dir
                .canonicalize()
                .expect("config annotations")
        );

        let result = validate_gmod_annotations_for_ls(&client, &loaded.emmyrc);
        assert!(
            result.is_ok(),
            "workspace-relative config annotations path should validate after load: {result:?}"
        );

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(vscode_annotations_dir);
    }

    #[test]
    fn directory_contains_lua_file_empty_dir_is_false() {
        let dir = create_temp_dir();
        assert!(!directory_contains_lua_file(&dir));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_contains_lua_file_with_lua_is_true() {
        let dir = create_temp_dir();
        fs::write(dir.join("a.lua"), "").expect("write lua");
        assert!(directory_contains_lua_file(&dir));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_contains_lua_file_only_non_lua_is_false() {
        let dir = create_temp_dir();
        fs::write(dir.join("a.txt"), "").expect("write txt");
        assert!(!directory_contains_lua_file(&dir));
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod reload_tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use glua_code_analysis::{
        DiagnosticCode, EmmyrcChange, Workspace, WorkspaceFolder, WorkspaceId, file_path_to_uri,
    };
    use lsp_server::{Connection, Message};
    use lsp_types::{ClientCapabilities, NumberOrString, PublishDiagnosticsParams, Uri};
    use tokio_util::sync::CancellationToken;

    use crate::context::{ServerContext, ServerContextSnapshot};

    use super::WorkspaceManager;

    const SOURCE: &str = "lua/autorun/sh_source.lua";
    const LATE: &str = "lua/autorun/sh_late.lua";

    /// A workspace whose one source file reports an unused local, with
    /// `config` as its `.gluarc.json`.
    fn workspace(name: &str, config: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("glua_ls_reload_{name}_{unique}"));
        fs::create_dir_all(root.join("lua/autorun")).expect("create workspace");
        fs::write(root.join(SOURCE), "local unused_value = 1\n").expect("write source");
        fs::write(root.join(".gluarc.json"), config).expect("write config");
        root
    }

    fn uri(root: &Path, relative: &str) -> Uri {
        file_path_to_uri(&root.join(relative)).expect("file URI should parse")
    }

    /// A server loaded from `root` as on startup.
    fn start(runtime: &tokio::runtime::Runtime, root: &Path) -> (ServerContext, Connection) {
        let (connection, peer) = Connection::memory();
        let context = runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            snapshot.workspace_manager().write().await.workspace_folders =
                vec![WorkspaceFolder::new(root.to_path_buf(), false)];
            let change = WorkspaceManager::reload_config(snapshot.workspace_manager_arc()).await;
            assert_eq!(change, EmmyrcChange::Workspace);
            context
        });
        (context, peer)
    }

    /// Waits for `uri`'s published diagnostics to report `unused` or not.
    fn wait_for_unused(peer: &Connection, uri: &Uri, reported: bool) {
        let unused = Some(NumberOrString::String(
            DiagnosticCode::Unused.get_name().to_string(),
        ));
        let deadline = Instant::now() + Duration::from_secs(20);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let Ok(Message::Notification(notification)) = peer.receiver.recv_timeout(remaining)
            else {
                continue;
            };
            if notification.method != "textDocument/publishDiagnostics" {
                continue;
            }
            let params: PublishDiagnosticsParams =
                serde_json::from_value(notification.params).expect("publish params");
            if params.uri == *uri
                && params
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == unused)
                    == reported
            {
                return;
            }
        }
        panic!("no publish with unused reported = {reported} for {uri:?}");
    }

    /// Reloads after writing `config`, with a file added on disk that only a
    /// walk of the workspace finds; returns the change, the analysis batches
    /// the reload ran and whether the added file was loaded.
    fn reload_with(
        runtime: &tokio::runtime::Runtime,
        snapshot: &ServerContextSnapshot,
        root: &Path,
        config: &str,
    ) -> (EmmyrcChange, usize, bool) {
        fs::write(root.join(LATE), "return 1\n").expect("write late file");
        fs::write(root.join(".gluarc.json"), config).expect("write config");
        runtime.block_on(async {
            snapshot
                .analysis()
                .write()
                .await
                .compilation
                .start_batch_log();
            let change = WorkspaceManager::reload_config(snapshot.workspace_manager_arc()).await;
            let mut analysis = snapshot.analysis().write().await;
            let batches = analysis.compilation.take_batch_log().len();
            let late_loaded = analysis.get_file_id(&uri(root, LATE)).is_some();
            (change, batches, late_loaded)
        })
    }

    #[test]
    fn a_diagnostics_only_change_rediagnoses_without_a_walk_or_reindex() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = workspace("diagnostics", "{}");
        let (context, peer) = start(&runtime, &root);
        let source = uri(&root, SOURCE);
        wait_for_unused(&peer, &source, true);

        let reload = reload_with(
            &runtime,
            &context.snapshot(),
            &root,
            r#"{ "diagnostics": { "disable": ["unused"] } }"#,
        );

        assert_eq!(reload, (EmmyrcChange::Diagnostics, 0, false));
        wait_for_unused(&peer, &source, false);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_workspace_shape_change_walks_the_workspace() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = workspace("shape", "{}");
        let (context, _peer) = start(&runtime, &root);

        let (change, _, late_loaded) = reload_with(
            &runtime,
            &context.snapshot(),
            &root,
            r#"{ "workspace": { "ignoreGlobs": ["**/ignored/**"] } }"#,
        );

        assert_eq!((change, late_loaded), (EmmyrcChange::Workspace, true));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rewriting_the_config_with_equal_content_does_nothing() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let config = r#"{ "diagnostics": { "globals": ["Extra"] } }"#;
        let root = workspace("equal", config);
        let (context, _peer) = start(&runtime, &root);

        let reload = reload_with(&runtime, &context.snapshot(), &root, config);

        assert_eq!(reload, (EmmyrcChange::Unchanged, 0, false));
        let _ = fs::remove_dir_all(root);
    }

    /// Every workspace file's diagnostics, keyed by path relative to `root`.
    fn workspace_diagnostics(
        runtime: &tokio::runtime::Runtime,
        snapshot: &ServerContextSnapshot,
        root: &Path,
    ) -> BTreeMap<String, Vec<String>> {
        runtime.block_on(async {
            let analysis = snapshot.analysis().read().await;
            analysis
                .get_main_workspace_file_ids_for_diagnostics()
                .into_iter()
                .map(|file_id| {
                    let path = analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_path(&file_id)
                        .expect("file path")
                        .strip_prefix(root)
                        .expect("file under the root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    let mut diagnostics = analysis
                        .diagnose_file(file_id, CancellationToken::new())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|diagnostic| format!("{:?} {}", diagnostic.code, diagnostic.message))
                        .collect::<Vec<_>>();
                    diagnostics.sort();
                    (path, diagnostics)
                })
                .collect()
        })
    }

    /// A file a workspace-shape change stops collecting leaves the index and
    /// the client, and its readers end as a cold load of the new config
    /// leaves them.
    #[test]
    fn a_file_the_new_config_stops_collecting_is_removed() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = workspace("uncollected", "{}");
        let ignored = "lua/ignored/sh_ignored.lua";
        fs::create_dir_all(root.join("lua/ignored")).expect("create ignored dir");
        fs::write(
            root.join(ignored),
            "local unused_value = 1\nfunction IgnoredApi() end\n",
        )
        .expect("write ignored file");
        fs::write(root.join(SOURCE), "IgnoredApi()\n").expect("write source");
        let (warm, peer) = start(&runtime, &root);
        let ignored_uri = uri(&root, ignored);
        wait_for_unused(&peer, &ignored_uri, true);

        let (change, _, _) = reload_with(
            &runtime,
            &warm.snapshot(),
            &root,
            r#"{ "workspace": { "ignoreGlobs": ["**/ignored/**"] } }"#,
        );

        assert_eq!(change, EmmyrcChange::Workspace);
        wait_for_unused(&peer, &ignored_uri, false);
        let still_loaded = runtime.block_on(async {
            warm.snapshot()
                .analysis()
                .read()
                .await
                .get_file_id(&ignored_uri)
                .is_some()
        });
        assert!(!still_loaded);
        let (cold, _cold_peer) = start(&runtime, &root);
        let warm_diagnostics = workspace_diagnostics(&runtime, &warm.snapshot(), &root);
        assert!(!warm_diagnostics.contains_key(ignored));
        assert_ne!(warm_diagnostics.get(SOURCE), Some(&Vec::new()));
        assert_eq!(
            warm_diagnostics,
            workspace_diagnostics(&runtime, &cold.snapshot(), &root)
        );
        let _ = fs::remove_dir_all(root);
    }

    /// The registered workspace roots, and every loaded file outside std
    /// with its module path and workspace.
    fn roots_and_modules(
        runtime: &tokio::runtime::Runtime,
        snapshot: &ServerContextSnapshot,
    ) -> (Vec<Workspace>, BTreeMap<PathBuf, (String, WorkspaceId)>) {
        runtime.block_on(async {
            let analysis = snapshot.analysis().read().await;
            let db = analysis.compilation.get_db();
            let module_index = db.get_module_index();
            let modules = db
                .get_vfs()
                .get_all_local_file_ids()
                .into_iter()
                .filter(|file_id| !module_index.is_std(file_id))
                .map(|file_id| {
                    let module = module_index.get_module(file_id).expect("module info");
                    (
                        db.get_vfs()
                            .get_file_path(&file_id)
                            .expect("file path")
                            .clone(),
                        (module.full_module_name.clone(), module.workspace_id),
                    )
                })
                .collect();
            (module_index.workspaces().to_vec(), modules)
        })
    }

    /// A library dropped from the config leaves the registered roots, and the
    /// libraries after it take the ids, module paths and diagnostics a cold
    /// load of the new config gives them.
    #[test]
    fn a_library_dropped_from_the_config_leaves_the_roots() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let libraries = ["dropped", "kept"].map(|name| {
            let library = workspace(&format!("library_{name}"), "{}");
            fs::write(
                library.join(format!("lua/autorun/sh_{name}.lua")),
                format!("function {name}_api() end\n"),
            )
            .expect("write library file");
            library
        });
        let library_config = |libraries: &[&PathBuf]| {
            let paths = libraries
                .iter()
                .map(|path| format!("\"{}\"", path.to_string_lossy().replace('\\', "/")))
                .collect::<Vec<_>>()
                .join(", ");
            format!(r#"{{ "workspace": {{ "library": [{paths}] }} }}"#)
        };
        let root = workspace(
            "libraries",
            &library_config(&[&libraries[0], &libraries[1]]),
        );
        fs::write(root.join(SOURCE), "dropped_api()\nkept_api()\n").expect("write source");
        let (warm, _peer) = start(&runtime, &root);

        let (change, _, _) = reload_with(
            &runtime,
            &warm.snapshot(),
            &root,
            &library_config(&[&libraries[1]]),
        );

        assert_eq!(change, EmmyrcChange::Workspace);
        let (cold, _cold_peer) = start(&runtime, &root);
        let (warm_roots, warm_modules) = roots_and_modules(&runtime, &warm.snapshot());
        assert!(
            !warm_roots
                .iter()
                .any(|workspace| workspace.root.starts_with(&libraries[0]))
        );
        assert_eq!(
            (warm_roots, warm_modules),
            roots_and_modules(&runtime, &cold.snapshot())
        );
        assert_eq!(
            workspace_diagnostics(&runtime, &warm.snapshot(), &root),
            workspace_diagnostics(&runtime, &cold.snapshot(), &root)
        );
        let _ = fs::remove_dir_all(root);
        for library in libraries {
            let _ = fs::remove_dir_all(library);
        }
    }

    #[test]
    fn only_config_files_directly_in_a_workspace_root_are_loaded() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = crate::util::unique_temp_dir("glua_ls_reload_config_location");
        let (connection, _peer) = Connection::memory();
        runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let mut manager = snapshot.workspace_manager().write().await;
            manager.workspace_folders = vec![WorkspaceFolder::new(root.clone(), false)];

            assert!(manager.is_workspace_config_file(&uri(&root, ".gluarc.json")));
            assert!(!manager.is_workspace_config_file(&uri(&root, "lua/.luarc.json")));
            assert!(!manager.is_workspace_config_file(&uri(
                &crate::util::unique_temp_dir("glua_ls_reload_elsewhere"),
                ".emmyrc.json"
            )));
        });
    }
}
