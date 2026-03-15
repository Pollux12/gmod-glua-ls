mod collect_workspace_files;
mod document;
mod file_id;
mod file_uri_handler;
mod loader;
mod virtual_url;

pub use collect_workspace_files::*;
pub use document::LuaDocument;
pub use file_id::{FileId, InFiled};
pub use file_uri_handler::{file_path_to_uri, uri_to_file_path};
use glua_parser::{LineIndex, LuaParseError, LuaParser, LuaSyntaxTree};
pub use loader::{LuaFileInfo, load_workspace_files, read_file_with_encoding};
use lsp_types::Uri;
use rowan::NodeCache;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
pub use virtual_url::VirtualUrlGenerator;

use crate::Emmyrc;

#[derive(Debug)]
pub struct Vfs {
    file_id_map: HashMap<PathBuf, u32>,
    file_path_map: HashMap<u32, PathBuf>,
    remote_file_id_map: HashMap<Uri, FileId>,
    file_data: Vec<Option<FileContent>>,
    line_index_map: HashMap<FileId, LineIndex>,
    tree_map: HashMap<FileId, LuaSyntaxTree>,
    emmyrc: Option<Arc<Emmyrc>>,
    node_cache: NodeCache,
}

#[derive(Default)]
pub struct DeferredVfsDrop {
    old_file_data: Option<FileContent>,
    old_line_index: Option<LineIndex>,
    old_tree: Option<LuaSyntaxTree>,
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Vfs {
    pub fn new() -> Self {
        Vfs {
            file_id_map: HashMap::new(),
            file_path_map: HashMap::new(),
            remote_file_id_map: HashMap::new(),
            file_data: Vec::new(),
            line_index_map: HashMap::new(),
            tree_map: HashMap::new(),
            emmyrc: None,
            node_cache: NodeCache::default(),
        }
    }

    pub fn file_id(&mut self, uri: &Uri) -> FileId {
        let path = match uri_to_file_path(uri) {
            Some(path) => path,
            None => {
                log::warn!("uri {} can not cover to file path", uri.as_str());
                let id = self.file_data.len() as u32;
                self.file_data.push(None);
                return FileId { id };
            }
        };
        if let Some(&id) = self.file_id_map.get(&path) {
            FileId { id }
        } else {
            let id = self.file_data.len() as u32;
            self.file_id_map.insert(path.clone(), id);
            self.file_path_map.insert(id, path);
            self.file_data.push(None);
            FileId { id }
        }
    }

    fn virtual_file_id(&mut self, uri: &Uri) -> FileId {
        if let Some(id) = self.remote_file_id_map.get(uri) {
            *id
        } else {
            let id = self.file_data.len() as u32;
            self.remote_file_id_map.insert(uri.clone(), FileId { id });
            self.file_data.push(None);
            FileId { id }
        }
    }

    pub fn get_file_id(&self, uri: &Uri) -> Option<FileId> {
        let path = uri_to_file_path(uri)?;
        self.file_id_map.get(&path).map(|&id| FileId { id })
    }

    pub fn get_uri(&self, id: &FileId) -> Option<Uri> {
        let path = self.file_path_map.get(&id.id)?;
        file_path_to_uri(path)
    }

    pub fn get_file_path(&self, id: &FileId) -> Option<&PathBuf> {
        self.file_path_map.get(&id.id)
    }

    pub fn set_file_content(&mut self, uri: &Uri, data: Option<String>) -> FileId {
        let fid = self.file_id(uri);
        log::debug!("file_id: {:?}, uri: {}", fid, uri.as_str());

        if let Some(data) = &data {
            let line_index = LineIndex::parse(data);
            let parse_config = self
                .emmyrc
                .as_ref()
                .expect("emmyrc set")
                .get_parse_config(&mut self.node_cache);
            let tree = LuaParser::parse(data, parse_config);
            self.tree_map.insert(fid, tree);
            self.line_index_map.insert(fid, line_index);
        } else {
            self.line_index_map.remove(&fid);
            self.tree_map.remove(&fid);
        }
        self.file_data[fid.id as usize] = data.map(|content| FileContent {
            content,
            is_remote: false,
            version: None,
        });
        fid
    }

    pub fn set_file_content_preparsed(
        &mut self,
        uri: &Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<FileId> {
        let existing_file_id = self.get_file_id(uri);
        if text.is_none() && existing_file_id.is_none() {
            return None;
        }

        let fid = existing_file_id.unwrap_or_else(|| self.file_id(uri));
        log::debug!("file_id (preparsed): {:?}, uri: {}", fid, uri.as_str());

        let current_version = self
            .file_data
            .get(fid.id as usize)
            .and_then(Option::as_ref)
            .and_then(|content| content.version);
        if let (Some(incoming_version), Some(current_version)) = (version, current_version)
            && incoming_version < current_version
        {
            return None;
        }

        match text {
            Some(content) => {
                self.tree_map.insert(fid, tree);
                self.line_index_map.insert(fid, line_index);
                self.file_data[fid.id as usize] = Some(FileContent {
                    content,
                    is_remote: false,
                    version,
                });
            }
            None => {
                self.line_index_map.remove(&fid);
                self.tree_map.remove(&fid);
                self.file_data[fid.id as usize] = None;
            }
        }

        Some(fid)
    }

    pub fn set_file_content_preparsed_deferred(
        &mut self,
        uri: &Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<(FileId, DeferredVfsDrop)> {
        let existing_file_id = self.get_file_id(uri);
        if text.is_none() && existing_file_id.is_none() {
            return None;
        }

        let fid = existing_file_id.unwrap_or_else(|| self.file_id(uri));
        log::debug!(
            "file_id (preparsed deferred): {:?}, uri: {}",
            fid,
            uri.as_str()
        );

        let current_version = self
            .file_data
            .get(fid.id as usize)
            .and_then(Option::as_ref)
            .and_then(|content| content.version);
        if let (Some(incoming_version), Some(current_version)) = (version, current_version)
            && incoming_version < current_version
        {
            return None;
        }

        let mut deferred_drop = DeferredVfsDrop::default();

        match text {
            Some(content) => {
                deferred_drop.old_tree = self.tree_map.insert(fid, tree);
                deferred_drop.old_line_index = self.line_index_map.insert(fid, line_index);
                deferred_drop.old_file_data =
                    self.file_data[fid.id as usize].replace(FileContent {
                        content,
                        is_remote: false,
                        version,
                    });
            }
            None => {
                deferred_drop.old_line_index = self.line_index_map.remove(&fid);
                deferred_drop.old_tree = self.tree_map.remove(&fid);
                deferred_drop.old_file_data = self.file_data[fid.id as usize].take();
            }
        }

        Some((fid, deferred_drop))
    }

    pub fn set_remote_file_content(&mut self, uri: &Uri, data: Option<String>) -> FileId {
        let fid = self.virtual_file_id(&uri);
        log::debug!("virtual file_id: {:?}, uri: {}", fid, uri.as_str());

        if let Some(data) = &data {
            let line_index = LineIndex::parse(&data);
            let parse_config = self
                .emmyrc
                .as_ref()
                .expect("emmyrc set")
                .get_parse_config(&mut self.node_cache);
            let tree = LuaParser::parse(&data, parse_config);
            self.tree_map.insert(fid, tree);
            self.line_index_map.insert(fid, line_index);
        } else {
            self.line_index_map.remove(&fid);
            self.tree_map.remove(&fid);
        }
        self.file_data[fid.id as usize] = data.map(|content| FileContent {
            content,
            is_remote: true,
            version: None,
        });
        fid
    }

    pub fn remove_file(&mut self, uri: &Uri) -> Option<FileId> {
        let fid = self.get_file_id(uri)?;
        if let Some(path) = self.file_path_map.remove(&fid.id) {
            self.file_id_map.remove(&path);
        }
        if let Some(data) = self.file_data.get_mut(fid.id as usize) {
            data.take();
        }
        self.line_index_map.remove(&fid);
        self.tree_map.remove(&fid);
        Some(fid)
    }

    pub fn update_config(&mut self, emmyrc: Arc<Emmyrc>) {
        self.emmyrc = Some(emmyrc);
    }

    pub fn get_file_content(&self, id: &FileId) -> Option<&String> {
        let opt = &self.file_data[id.id as usize];
        if let Some(s) = opt {
            Some(&s.content)
        } else {
            None
        }
    }

    pub fn get_file_version(&self, id: &FileId) -> Option<i32> {
        self.file_data
            .get(id.id as usize)
            .and_then(Option::as_ref)
            .and_then(|content| content.version)
    }

    pub fn update_file_version(&mut self, id: &FileId, version: Option<i32>) {
        if let Some(Some(content)) = self.file_data.get_mut(id.id as usize) {
            content.version = version;
        }
    }

    pub fn get_document(&self, id: &FileId) -> Option<LuaDocument<'_>> {
        let path = self.file_path_map.get(&id.id)?;
        let text = self.get_file_content(id)?;
        let line_index = self.line_index_map.get(id)?;
        Some(LuaDocument::new(*id, path, text, line_index))
    }

    pub fn get_syntax_tree(&self, id: &FileId) -> Option<&LuaSyntaxTree> {
        self.tree_map.get(id)
    }

    pub fn get_file_parse_error(&self, id: &FileId) -> Option<Vec<LuaParseError>> {
        let tree = self.tree_map.get(id)?;
        let errors = tree.get_errors();
        if errors.is_empty() {
            return None;
        }

        Some(errors.to_vec())
    }

    pub fn get_all_local_file_ids(&self) -> Vec<FileId> {
        self.file_data
            .iter()
            .enumerate()
            .filter_map(|(fid, opt_content)| {
                if let Some(content) = opt_content {
                    if !content.is_remote {
                        Some(FileId { id: fid as u32 })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn get_all_file_ids(&self) -> Vec<FileId> {
        self.file_data
            .iter()
            .enumerate()
            .filter_map(|(fid, opt_content)| {
                if opt_content.is_some() {
                    Some(FileId { id: fid as u32 })
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn is_remote_file(&self, id: &FileId) -> bool {
        if let Some(opt_content) = self.file_data.get(id.id as usize) {
            if let Some(content) = opt_content {
                content.is_remote
            } else {
                false
            }
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.file_id_map.clear();
        self.file_path_map.clear();
        self.file_data.clear();
        self.line_index_map.clear();
        self.tree_map.clear();
        self.emmyrc = None;
        self.node_cache = NodeCache::default();
    }
}

#[derive(Debug)]
struct FileContent {
    content: String,
    is_remote: bool,
    version: Option<i32>,
}
