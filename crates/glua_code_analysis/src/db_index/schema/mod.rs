mod schema_file;

use crate::db_index::cow_shards::CowCell;
use rustc_hash::{FxHashMap, FxHashSet};

use url::Url;

use crate::{FileId, LuaIndex};
pub use schema_file::*;

#[derive(Debug, Clone)]
pub struct JsonSchemaIndex {
    schema_files: CowCell<FxHashMap<Url, JsonSchemaFile>>,
    /// The files whose `---@schema` tags name each URL.
    users: CowCell<FxHashMap<Url, FxHashSet<FileId>>>,
}

impl JsonSchemaIndex {
    pub fn new() -> Self {
        Self {
            schema_files: Default::default(),
            users: Default::default(),
        }
    }

    /// Notes that a `---@schema` tag in `file_id` names `url`.
    pub fn add_user(&mut self, url: &Url, file_id: FileId) {
        if !self
            .users
            .get(url)
            .is_some_and(|users| users.contains(&file_id))
        {
            let mut users = self.users.get(url).cloned().unwrap_or_default();
            users.insert(file_id);
            self.users.insert(url.clone(), users);
        }
    }

    /// The files whose `---@schema` tags name `url`.
    pub fn users_of(&self, url: &Url) -> impl Iterator<Item = FileId> + '_ {
        self.users.get(url).into_iter().flatten().copied()
    }

    pub fn get_schema_file(&self, url: &Url) -> Option<&JsonSchemaFile> {
        self.schema_files.get(url)
    }

    /// Every schema URL entry and its resolution state.
    pub fn iter_schema_files(&self) -> impl Iterator<Item = (&Url, &JsonSchemaFile)> {
        self.schema_files.iter()
    }

    pub fn get_schema_file_mut(&mut self, url: &Url) -> Option<&mut JsonSchemaFile> {
        self.schema_files.get_mut(url)
    }

    pub fn add_schema_file(&mut self, url: Url, schema_file: JsonSchemaFile) {
        self.schema_files.insert(url, schema_file);
    }

    pub fn has_need_resolve_schemas(&self) -> bool {
        self.schema_files
            .values()
            .any(|schema_file| matches!(schema_file, JsonSchemaFile::NeedResolve))
    }

    pub fn get_need_resolve_schemas(&self) -> Vec<Url> {
        self.schema_files
            .iter()
            .filter_map(|(url, schema_file)| {
                if let JsonSchemaFile::NeedResolve = schema_file {
                    Some(url.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn reset_rest_schemas(&mut self) {
        for schema_file in self.schema_files.values_mut() {
            if let JsonSchemaFile::NeedResolve = schema_file {
                *schema_file = JsonSchemaFile::BadUrl;
            }
        }
    }
}

impl LuaIndex for JsonSchemaIndex {
    fn remove(&mut self, file_id: FileId) {
        let named = self
            .users
            .iter()
            .filter(|(_, users)| users.contains(&file_id))
            .map(|(url, _)| url.clone())
            .collect::<Vec<_>>();
        for url in named {
            let mut users = self.users.get(&url).cloned().unwrap_or_default();
            users.remove(&file_id);
            if users.is_empty() {
                self.users.remove(&url);
            } else {
                self.users.insert(url, users);
            }
        }
    }

    fn clear(&mut self) {
        // TODO clear all schema index
    }
}

crate::db_index::cow_shards::task_fork_fields!(JsonSchemaIndex {
    schema_files,
    users
});
