use std::{collections::HashSet, path::PathBuf};

use crate::{
    EmmyLibraryItem, Emmyrc, LuaFileInfo, load_workspace_files,
    vfs::loader::normalize_path_for_ordering,
};

#[derive(Clone, Debug)]
pub enum WorkspaceImport {
    All,
    SubPaths(Vec<PathBuf>),
}

#[derive(Clone, Debug)]
pub struct WorkspaceFolder {
    pub root: PathBuf,
    pub import: WorkspaceImport,
    pub is_library: bool,
}

impl WorkspaceFolder {
    pub fn new(root: PathBuf, is_library: bool) -> Self {
        Self {
            root,
            import: WorkspaceImport::All,
            is_library,
        }
    }

    pub fn with_sub_paths(root: PathBuf, sub_paths: Vec<PathBuf>, is_library: bool) -> Self {
        Self {
            root,
            import: WorkspaceImport::SubPaths(sub_paths),
            is_library,
        }
    }
}

#[derive(Debug)]
pub(crate) struct WorkspaceFileCandidate {
    pub workspace_priority: usize,
    pub normalized_path: String,
    pub file: LuaFileInfo,
}

pub(crate) fn dedupe_workspace_files_deterministic(
    mut candidates: Vec<WorkspaceFileCandidate>,
) -> Vec<LuaFileInfo> {
    candidates.sort_by(|a, b| {
        a.workspace_priority
            .cmp(&b.workspace_priority)
            .then_with(|| a.normalized_path.cmp(&b.normalized_path))
            .then_with(|| a.file.path.cmp(&b.file.path))
    });

    let mut loaded_paths = HashSet::new();
    let mut files = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if loaded_paths.insert(candidate.normalized_path) {
            files.push(candidate.file);
        } else {
            log::debug!("Skipping duplicate file: {:?}", candidate.file.path);
        }
    }
    files
}

pub fn collect_workspace_files(
    workspaces: &Vec<WorkspaceFolder>,
    emmyrc: &Emmyrc,
    extra_include: Option<Vec<String>>,
    extra_exclude: Option<Vec<String>>,
) -> Vec<LuaFileInfo> {
    let mut candidates = Vec::new();
    let (mut match_pattern, mut exclude, exclude_dir) = calculate_include_and_exclude(emmyrc);
    if let Some(extra_include) = extra_include {
        match_pattern.extend_from_slice(&extra_include);
        match_pattern.sort();
        match_pattern.dedup();
    }
    if let Some(extra_exclude) = extra_exclude {
        exclude.extend_from_slice(&extra_exclude);
        exclude.sort();
        exclude.dedup();
    }

    let encoding = &emmyrc.workspace.encoding;

    log::info!(
        "collect_files from: {:?} match_pattern: {:?} exclude: {:?}, exclude_dir: {:?}",
        workspaces,
        match_pattern,
        exclude,
        exclude_dir
    );

    for (idx, workspace) in workspaces.iter().enumerate() {
        // Build exclude_dirs for this workspace by finding child workspaces
        let mut workspace_exclude_dir = exclude_dir.clone();

        // Find all other workspaces that are children of current workspace
        for (other_idx, other_workspace) in workspaces.iter().enumerate() {
            if idx != other_idx {
                // Check if other_workspace is a child of current workspace
                if let Ok(relative) = other_workspace.root.strip_prefix(&workspace.root) {
                    if relative.components().count() > 0 {
                        // other_workspace is a child, add it to exclude_dir
                        workspace_exclude_dir.push(other_workspace.root.clone());
                        log::debug!(
                            "Excluding child workspace {:?} from parent {:?}",
                            other_workspace.root,
                            workspace.root
                        );
                    }
                }
            }
        }

        match &workspace.import {
            WorkspaceImport::All => {
                let loaded = if workspace.is_library {
                    let (lib_exclude, lib_exclude_dir) = find_library_exclude(workspace, emmyrc);
                    // Merge library exclude with workspace exclude
                    let mut merged_exclude = exclude.clone();
                    merged_exclude.extend(lib_exclude);
                    merged_exclude.sort();
                    merged_exclude.dedup();

                    let mut merged_exclude_dir = workspace_exclude_dir.clone();
                    merged_exclude_dir.extend(lib_exclude_dir);

                    load_workspace_files(
                        &workspace.root,
                        &match_pattern,
                        &merged_exclude,
                        &merged_exclude_dir,
                        Some(encoding),
                    )
                    .ok()
                } else {
                    load_workspace_files(
                        &workspace.root,
                        &match_pattern,
                        &exclude,
                        &workspace_exclude_dir,
                        Some(encoding),
                    )
                    .ok()
                };
                if let Some(loaded) = loaded {
                    for file in loaded {
                        candidates.push(WorkspaceFileCandidate {
                            workspace_priority: idx,
                            normalized_path: normalize_path_for_ordering(&file.path),
                            file,
                        });
                    }
                }
            }
            WorkspaceImport::SubPaths(paths) => {
                for sub in paths {
                    let target = workspace.root.join(sub);
                    let loaded = if workspace.is_library {
                        let (lib_exclude, lib_exclude_dir) =
                            find_library_exclude(workspace, emmyrc);
                        // Merge library exclude with workspace exclude
                        let mut merged_exclude = exclude.clone();
                        merged_exclude.extend(lib_exclude);
                        merged_exclude.sort();
                        merged_exclude.dedup();

                        let mut merged_exclude_dir = workspace_exclude_dir.clone();
                        merged_exclude_dir.extend(lib_exclude_dir);

                        load_workspace_files(
                            &target,
                            &match_pattern,
                            &merged_exclude,
                            &merged_exclude_dir,
                            Some(encoding),
                        )
                        .ok()
                    } else {
                        load_workspace_files(
                            &target,
                            &match_pattern,
                            &exclude,
                            &workspace_exclude_dir,
                            Some(encoding),
                        )
                        .ok()
                    };
                    if let Some(loaded) = loaded {
                        for file in loaded {
                            candidates.push(WorkspaceFileCandidate {
                                workspace_priority: idx,
                                normalized_path: normalize_path_for_ordering(&file.path),
                                file,
                            });
                        }
                    }
                }
            }
        }
    }
    let files = dedupe_workspace_files_deterministic(candidates);

    log::info!("load files from workspace count: {:?}", files.len());

    for file in &files {
        log::debug!("loaded file: {:?}", file.path);
    }

    files
}

pub fn calculate_include_and_exclude(emmyrc: &Emmyrc) -> (Vec<String>, Vec<String>, Vec<PathBuf>) {
    let mut include = vec!["**/*.lua".to_string()];
    let mut exclude = Vec::new();
    let mut exclude_dirs = Vec::new();

    for extension in &emmyrc.runtime.extensions {
        if extension.starts_with(".") {
            include.push(format!("**/*{}", extension));
        } else if extension.starts_with("*.") {
            include.push(format!("**/{}", extension));
        } else {
            include.push(extension.clone());
        }
    }

    for ignore_glob in &emmyrc.workspace.ignore_globs {
        exclude.push(ignore_glob.clone());
    }

    for dir in &emmyrc.workspace.ignore_dir {
        exclude_dirs.push(PathBuf::from(dir));
    }

    // Merge default ignore globs if enabled (use_default_ignores defaults to true)
    if emmyrc.workspace.use_default_ignores {
        for glob_pattern in emmyrc.workspace.resolve_ignore_dir_defaults() {
            exclude.push(glob_pattern);
        }
    }

    // remove duplicate
    include.sort();
    include.dedup();

    // remove duplicate
    exclude.sort();
    exclude.dedup();

    (include, exclude, exclude_dirs)
}

fn find_library_exclude(library: &WorkspaceFolder, emmyrc: &Emmyrc) -> (Vec<String>, Vec<PathBuf>) {
    let mut exclude = Vec::new();
    let mut exclude_dirs = Vec::new();

    for lib in &emmyrc.workspace.library {
        if let EmmyLibraryItem::Config(detail_config) = &lib {
            let lib_path = PathBuf::from(&detail_config.path);
            if lib_path == library.root {
                exclude = detail_config.ignore_globs.clone();
                exclude_dirs = detail_config.ignore_dir.iter().map(PathBuf::from).collect();
                break;
            }
        }
    }

    (exclude, exclude_dirs)
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceFileCandidate, dedupe_workspace_files_deterministic};
    use crate::LuaFileInfo;

    #[test]
    fn vfs_collect_dedupe_is_deterministic_with_workspace_priority() {
        let candidates = vec![
            WorkspaceFileCandidate {
                workspace_priority: 1,
                normalized_path: "c:/addon/lua/autorun/shared/init.lua".to_string(),
                file: LuaFileInfo {
                    path: "C:/addon/lua/autorun/shared/init.lua".to_string(),
                    content: "from workspace 1".to_string(),
                },
            },
            WorkspaceFileCandidate {
                workspace_priority: 0,
                normalized_path: "c:/addon/lua/autorun/shared/init.lua".to_string(),
                file: LuaFileInfo {
                    path: "c:\\addon\\lua\\autorun\\shared\\init.lua".to_string(),
                    content: "from workspace 0".to_string(),
                },
            },
        ];

        let deduped = dedupe_workspace_files_deterministic(candidates);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].content, "from workspace 0");
    }
}
