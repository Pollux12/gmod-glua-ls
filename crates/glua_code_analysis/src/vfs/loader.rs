use encoding_rs::{Encoding, UTF_8};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
};
use wax::Pattern;

use ignore::{WalkBuilder, WalkState};
use log::{error, info};

#[derive(Debug)]
pub struct LuaFileInfo {
    pub path: String,
    pub content: String,
}

pub(crate) fn normalize_path_for_ordering(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(target_os = "windows")]
    {
        normalized = normalized.to_lowercase();
    }
    normalized
}

pub(crate) fn sort_lua_files_by_normalized_path(files: &mut [LuaFileInfo]) {
    files.sort_by_cached_key(|file| (normalize_path_for_ordering(&file.path), file.path.clone()));
}

impl LuaFileInfo {
    pub fn into_tuple(self) -> (PathBuf, Option<String>) {
        (PathBuf::from(self.path), Some(self.content))
    }
}

pub fn load_workspace_files(
    root: &Path,
    include_pattern: &[String],
    exclude_pattern: &[String],
    exclude_dir: &[PathBuf],
    encoding: Option<&str>,
) -> Result<Vec<LuaFileInfo>, Box<dyn Error>> {
    let encoding = encoding.unwrap_or("utf-8").to_string();
    if root.is_file() {
        let mut files = Vec::new();
        if let Some(content) = read_file_with_encoding(root, &encoding) {
            files.push(LuaFileInfo {
                path: root.to_string_lossy().to_string(),
                content,
            });
        }
        return Ok(files);
    }

    let include_pattern: Vec<&str> = include_pattern.iter().map(String::as_str).collect();
    let include_set = match wax::any(include_pattern) {
        Ok(glob) => glob,
        Err(e) => {
            error!("Invalid glob pattern: {:?}", e);
            return Ok(Vec::new());
        }
    };
    let exclude_pattern: Vec<&str> = exclude_pattern.iter().map(String::as_str).collect();
    let exclude_set = match wax::any(exclude_pattern) {
        Ok(glob) => glob,
        Err(e) => {
            error!("Invalid ignore glob pattern: {:?}", e);
            return Ok(Vec::new());
        }
    };

    let (tx, rx) = mpsc::channel::<LuaFileInfo>();
    let root_path = root.to_path_buf();
    let exclude_dirs = exclude_dir.to_vec();

    // Honour our own globs only; skip gitignore.
    WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(false)
        .build_parallel()
        .run(|| {
            let tx = tx.clone();
            let root_path = root_path.clone();
            let exclude_dirs = exclude_dirs.clone();
            let include_set = &include_set;
            let exclude_set = &exclude_set;
            let encoding = encoding.clone();
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return WalkState::Continue,
                };
                let path = entry.path();
                if exclude_dirs.iter().any(|d| path.starts_with(d)) {
                    if entry.file_type().is_some_and(|t| t.is_dir()) {
                        return WalkState::Skip;
                    }
                    return WalkState::Continue;
                }
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let Ok(relative) = path.strip_prefix(&root_path) else {
                    return WalkState::Continue;
                };
                if exclude_set.is_match(relative) {
                    return WalkState::Continue;
                }
                if !include_set.is_match(relative) {
                    return WalkState::Continue;
                }
                if let Some(content) = read_file_with_encoding(path, &encoding) {
                    let _ = tx.send(LuaFileInfo {
                        path: path.to_string_lossy().to_string(),
                        content,
                    });
                }
                WalkState::Continue
            })
        });
    drop(tx);

    let mut files: Vec<LuaFileInfo> = rx.into_iter().collect();
    sort_lua_files_by_normalized_path(&mut files);
    Ok(files)
}

pub fn read_file_with_encoding(path: &Path, encoding: &str) -> Option<String> {
    let origin_content = fs::read(path).ok()?;
    let encoding = Encoding::for_label(encoding.as_bytes()).unwrap_or(UTF_8);
    let (content, has_error) = encoding.decode_with_bom_removal(&origin_content);
    if has_error {
        error!("Error decoding file: {:?}", path);
        if encoding == UTF_8 {
            return None;
        }

        info!("Try utf-8 encoding");
        let (content, _, hash_error) = UTF_8.decode(&origin_content);
        if hash_error {
            error!("Try utf8 fail, error decoding file: {:?}", path);
            return None;
        }

        return Some(content.to_string());
    }

    Some(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::{LuaFileInfo, normalize_path_for_ordering, sort_lua_files_by_normalized_path};

    #[test]
    fn vfs_loader_normalizes_slashes_and_trailing_separator() {
        let normalized = normalize_path_for_ordering(r"C:\Workspace\Lua\test.lua\\");
        assert!(!normalized.contains('\\'));
        assert!(!normalized.ends_with('/'));
    }

    #[test]
    fn vfs_loader_sorts_by_normalized_path() {
        let mut files = vec![
            LuaFileInfo {
                path: r"C:\workspace\z.lua".to_string(),
                content: String::new(),
            },
            LuaFileInfo {
                path: r"C:/workspace/A.lua".to_string(),
                content: String::new(),
            },
        ];

        sort_lua_files_by_normalized_path(&mut files);

        assert_eq!(files[0].path, r"C:/workspace/A.lua");
        assert_eq!(files[1].path, r"C:\workspace\z.lua");
    }
}
