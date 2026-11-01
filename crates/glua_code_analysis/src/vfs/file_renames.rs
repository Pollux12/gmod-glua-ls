use std::path::{Path, PathBuf};

use crate::{FileId, Vfs};

/// A batch of renamed files and folders, as (old path, new path) pairs.
#[derive(Debug, Clone, Default)]
pub struct FileRenames {
    renames: Vec<(PathBuf, PathBuf)>,
}

impl FileRenames {
    pub fn new(renames: Vec<(PathBuf, PathBuf)>) -> Self {
        Self { renames }
    }

    pub fn is_empty(&self) -> bool {
        self.renames.is_empty()
    }

    /// Where the batch moves `path`: the renamed file itself, or a file under
    /// a renamed folder.
    pub fn moved(&self, path: &Path) -> Option<PathBuf> {
        self.renames.iter().find_map(|(old, new)| {
            let rest = path.strip_prefix(old).ok()?;
            Some(if rest.as_os_str().is_empty() {
                new.clone()
            } else {
                new.join(rest)
            })
        })
    }

    /// Where `path` is once the batch is applied.
    pub fn after(&self, path: &Path) -> PathBuf {
        self.moved(path).unwrap_or_else(|| path.to_path_buf())
    }

    /// The VFS files the batch moves, with their new paths, by file id.
    pub fn moved_files(&self, vfs: &Vfs) -> Vec<(FileId, PathBuf)> {
        if self.is_empty() {
            return Vec::new();
        }
        vfs.get_all_local_file_ids()
            .into_iter()
            .filter_map(|file_id| Some((file_id, self.moved(vfs.get_file_path(&file_id)?)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::FileRenames;

    fn renames(pairs: &[(&str, &str)]) -> FileRenames {
        FileRenames::new(
            pairs
                .iter()
                .map(|(old, new)| (PathBuf::from(old), PathBuf::from(new)))
                .collect(),
        )
    }

    #[test]
    fn a_folder_rename_moves_the_files_under_it_and_nothing_beside_it() {
        let renames = renames(&[("/addon/lua/autorun", "/addon/lua/startup")]);

        assert_eq!(
            renames.moved(Path::new("/addon/lua/autorun/server/sv_a.lua")),
            Some(PathBuf::from("/addon/lua/startup/server/sv_a.lua"))
        );
        assert_eq!(
            renames.moved(Path::new("/addon/lua/autorun_extra/sh_b.lua")),
            None
        );
    }
}
