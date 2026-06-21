use std::path::{Path, PathBuf};

use include_dir::{Dir, DirEntry, include_dir};

use crate::LuaFileInfo;

static RESOURCE_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/resources");

/// Stable virtual path used to prefix embedded std-lib file paths for VFS
/// registration.  The directory does **not** need to exist on disk; it only
/// needs to be an absolute path so that [`crate::file_path_to_uri`] produces
/// valid `file://` URIs.
fn virtual_resources_dir() -> PathBuf {
    // Match the platform layout the old `get_best_resources_dir()` would
    // return so downstream VFS registration is unchanged.
    #[cfg(target_os = "windows")]
    {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                // `C:` is drive-relative (not absolute); `C:\\` is absolute.
                // Use current_exe parent as a more robust fallback when the
                // env var is unset; fall back to an absolute drive root.
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| PathBuf::from("C:\\"))
            })
            .join("glua_ls")
            .join("resources")
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("glua_ls")
            .join("resources")
    }
}

pub fn load_resource_std(is_jit: bool) -> (PathBuf, Vec<LuaFileInfo>) {
    let resources_dir = virtual_resources_dir();
    let std_root = resources_dir.join("std");

    let raw_files = load_resource_from_include_dir();
    let mut files: Vec<LuaFileInfo> = raw_files
        .into_iter()
        .filter(|file| file.path.ends_with(".lua"))
        .map(|file| {
            let path = resources_dir
                .join(&file.path)
                .to_str()
                .expect("UTF-8 paths")
                .to_string();
            LuaFileInfo {
                path,
                content: file.content,
            }
        })
        .collect();

    if !is_jit {
        remove_jit_resource(&mut files);
    }

    (std_root, files)
}

pub(crate) fn remove_jit_resource(files: &mut Vec<LuaFileInfo>) {
    const JIT_FILES_TO_REMOVE: &[&str] = &[
        "jit.lua",
        "jit/profile.lua",
        "jit/util.lua",
        "string/buffer.lua",
        "table/clear.lua",
        "table/new.lua",
        "ffi.lua",
    ];
    files.retain(|file| {
        let path = Path::new(&file.path);
        !JIT_FILES_TO_REMOVE
            .iter()
            .any(|suffix| path.ends_with(suffix))
    });
}

pub(crate) fn load_resource_from_include_dir() -> Vec<LuaFileInfo> {
    let mut files = Vec::new();
    walk_resource_dir(&RESOURCE_DIR, &mut files);
    files
}

fn walk_resource_dir(dir: &Dir, files: &mut Vec<LuaFileInfo>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::File(file) => {
                let path = file.path();
                let content = file.contents_utf8().expect("UTF-8 paths");

                files.push(LuaFileInfo {
                    path: path.to_str().expect("UTF-8 paths").to_string(),
                    content: content.to_string(),
                });
            }
            DirEntry::Dir(subdir) => {
                walk_resource_dir(subdir, files);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::*;
    use crate::normalize_workspace_root;

    #[gtest]
    fn embedded_std_contains_call_arg_attribute_defs() {
        let files = load_resource_from_include_dir();
        let builtin = files.iter().find(|f| f.path.ends_with("builtin.lua"));
        expect_that!(builtin, some(anything()));
        let builtin = builtin.unwrap();
        expect_that!(builtin.content.contains("---@attribute call_arg"), eq(true));
        expect_that!(
            builtin.content.contains("---@attribute overload_call_arg"),
            eq(true)
        );
    }

    #[gtest]
    fn virtual_resources_dir_is_absolute() {
        let dir = virtual_resources_dir();
        expect_that!(dir.is_absolute(), eq(true));
    }

    #[gtest]
    fn virtual_resources_dir_std_subdir_is_absolute() {
        let dir = virtual_resources_dir().join("std");
        expect_that!(dir.is_absolute(), eq(true));
    }

    #[gtest]
    fn load_resource_std_returns_absolute_std_root() {
        let (std_root, _files) = load_resource_std(true);
        expect_that!(std_root.is_absolute(), eq(true));
    }

    #[gtest]
    fn init_std_lib_classifies_builtin_as_std_workspace() {
        let mut analysis = crate::EmmyLuaAnalysis::new();
        analysis.init_std_lib();

        // Find the builtin.lua file in the VFS and verify it's classified as STD.
        let vfs = analysis.compilation.get_db().get_vfs();
        let module_index = analysis.compilation.get_db().get_module_index();

        let mut found_builtin = false;
        for file_id in vfs.get_all_file_ids() {
            if let Some(path) = vfs.get_file_path(&file_id) {
                if path.to_string_lossy().ends_with("builtin.lua") {
                    found_builtin = true;
                    expect_that!(module_index.is_std(&file_id), eq(true));
                    break;
                }
            }
        }
        expect_that!(found_builtin, eq(true));
    }

    #[gtest]
    fn normalize_workspace_root_preserves_absolute_paths() {
        // Use a platform-appropriate absolute path.
        let root = if cfg!(windows) {
            PathBuf::from("C:/some/absolute/path")
        } else {
            PathBuf::from("/some/absolute/path")
        };
        let normalized = normalize_workspace_root(root);
        expect_that!(normalized.is_absolute(), eq(true));
    }

    #[cfg(target_os = "windows")]
    #[gtest]
    fn normalize_workspace_root_uppercases_drive_letter() {
        let lowercase = PathBuf::from("c:/Users/test/resources");
        let normalized = normalize_workspace_root(lowercase);
        let normalized_str = normalized.to_string_lossy();
        // After URI round-trip, the drive letter should be uppercase.
        expect_that!(normalized_str.starts_with("C:"), eq(true));
    }

    #[cfg(target_os = "windows")]
    #[gtest]
    fn virtual_resources_dir_has_uppercase_drive_on_windows() {
        let dir = virtual_resources_dir();
        let dir_str = dir.to_string_lossy();
        // Absolute Windows paths should have an uppercase drive letter
        // (e.g. C:\ not c:\).
        if dir_str.len() >= 2 && dir_str.as_bytes()[1] == b':' {
            expect_that!(dir_str.as_bytes()[0].is_ascii_uppercase(), eq(true));
        }
    }
}
