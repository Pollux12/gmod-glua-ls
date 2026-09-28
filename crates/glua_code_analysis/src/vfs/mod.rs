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
use glua_parser::{
    LineIndex, LuaAstNode, LuaParseError, LuaParser, LuaSyntaxNode, LuaSyntaxTree, LuaTokenKind,
};
pub(crate) use loader::normalize_path_for_ordering;
pub use loader::{LuaFileInfo, load_workspace_files, read_file_with_encoding};
use lsp_types::Uri;
use rowan::NodeCache;
use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
pub use virtual_url::VirtualUrlGenerator;

use crate::{Emmyrc, PositionMap, WorkspaceKind};

#[derive(Debug)]
pub struct Vfs {
    file_id_map: FxHashMap<PathBuf, u32>,
    file_path_map: FxHashMap<u32, PathBuf>,
    /// Each file's [`Vfs::file_order_key`], made when the file is first named
    /// and again whenever the workspace roots change.
    file_order_keys: FxHashMap<u32, Arc<str>>,
    /// Normalized workspace roots with their load tier, longest first; see
    /// [`Vfs::order_key_for_path`].
    order_roots: Vec<(String, u8)>,
    remote_file_id_map: FxHashMap<Uri, FileId>,
    file_data: Vec<Option<FileContent>>,
    line_index_map: FxHashMap<FileId, LineIndex>,
    tree_map: FxHashMap<FileId, LuaSyntaxTree>,
    emmyrc: Option<Arc<Emmyrc>>,
    node_cache: NodeCache,
    /// Monotonic counter bumped whenever file *content* (or existence) changes.
    /// Lets workspace-wide derived caches (e.g. the gmod helper registry) detect
    /// that their inputs are unchanged and skip a full rescan. It is deliberately
    /// NOT bumped by analysis/index updates, so the cold-index main pass and its
    /// stabilization re-run share a revision and the second build is a cache hit.
    content_revision: u64,
    /// The text each changed file had when it was last indexed (`None` for a
    /// file that had none), until the next index takes its edit.
    edit_bases: FxHashMap<FileId, Option<String>>,
    /// Files deleted from disk, kept until a re-analysis has taken the
    /// deletion; see [`Vfs::forget_taken_deletions`].
    deleted: FxHashSet<FileId>,
}

/// The child of `tree`'s top-level block that contains `range`.
fn top_level_statement(tree: &LuaSyntaxTree, range: TextRange) -> Option<LuaSyntaxNode> {
    tree.get_chunk_node()
        .get_block()?
        .syntax()
        .child_or_token_at_range(range)?
        .into_node()
}

fn trees_semantically_equal(left: &LuaSyntaxTree, right: &LuaSyntaxTree) -> bool {
    // Trivia is NOT skippable mid-file: the parser groups doc comments by
    // the number of `TkEndOfLine` tokens between them and classifies a
    // comment as inline-trailer vs leading-block by whether an EOL or plain
    // whitespace precedes it, so `\n` -> ` ` (one Join Lines keystroke)
    // re-attaches an annotation while every non-trivia token stays
    // identical. The streams are therefore compared in full — kind, range
    // and text of every token — and only trivia *after* the last
    // significant token (a trailing newline, the one shape an EOF-append
    // edit produces) may differ.
    let all_tokens = |tree: &LuaSyntaxTree| {
        tree.get_red_root()
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .collect::<Vec<_>>()
    };
    let is_trailing_trivia = |token: &glua_parser::LuaSyntaxToken| {
        matches!(
            token.kind().to_token(),
            LuaTokenKind::TkWhitespace | LuaTokenKind::TkEndOfLine | LuaTokenKind::TkEof
        )
    };
    let left_tokens = all_tokens(left);
    let right_tokens = all_tokens(right);
    let significant_len = |tokens: &[glua_parser::LuaSyntaxToken]| {
        tokens
            .iter()
            .rposition(|token| !is_trailing_trivia(token))
            .map_or(0, |index| index + 1)
    };
    let left_len = significant_len(&left_tokens);
    let right_len = significant_len(&right_tokens);
    left_len == right_len
        && left_tokens[..left_len]
            .iter()
            .zip(&right_tokens[..right_len])
            .all(|(a, b)| {
                a.kind() == b.kind() && a.text_range() == b.text_range() && a.text() == b.text()
            })
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

/// The parser's `NodeCache` only deduplicates green nodes, so a clone starts
/// with an empty one.
impl Clone for Vfs {
    fn clone(&self) -> Self {
        Vfs {
            file_id_map: self.file_id_map.clone(),
            file_path_map: self.file_path_map.clone(),
            file_order_keys: self.file_order_keys.clone(),
            order_roots: self.order_roots.clone(),
            remote_file_id_map: self.remote_file_id_map.clone(),
            file_data: self.file_data.clone(),
            line_index_map: self.line_index_map.clone(),
            tree_map: self.tree_map.clone(),
            emmyrc: self.emmyrc.clone(),
            node_cache: NodeCache::default(),
            content_revision: self.content_revision,
            edit_bases: self.edit_bases.clone(),
            deleted: self.deleted.clone(),
        }
    }
}

impl Vfs {
    pub fn new() -> Self {
        Vfs {
            file_id_map: FxHashMap::default(),
            file_path_map: FxHashMap::default(),
            file_order_keys: FxHashMap::default(),
            order_roots: Vec::new(),
            remote_file_id_map: FxHashMap::default(),
            file_data: Vec::new(),
            line_index_map: FxHashMap::default(),
            tree_map: FxHashMap::default(),
            emmyrc: None,
            node_cache: NodeCache::default(),
            content_revision: 0,
            edit_bases: FxHashMap::default(),
            deleted: FxHashSet::default(),
        }
    }

    /// Current content revision. Increases on every file content/existence change.
    pub fn content_revision(&self) -> u64 {
        self.content_revision
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
            self.file_order_keys
                .insert(id, Arc::from(self.order_key_for_path(&path)));
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

    /// The file's place in a stable cross-file ordering; see
    /// [`Vfs::order_key_for_path`].
    ///
    /// File ids are handed out in whatever order a batch reached the files, so
    /// they are not the same between a cold build and an incremental session.
    /// Anything that has to elect one file's write over another's — merged
    /// documentation, table writers, load sites — orders on this instead. A file
    /// the VFS does not know sorts last.
    pub fn file_order_key(&self, id: &FileId) -> Arc<str> {
        self.file_order_keys
            .get(&id.id)
            .cloned()
            .unwrap_or_else(|| Arc::from(""))
    }

    /// Sets the workspace roots with their load tier and re-keys every file.
    pub fn set_order_roots(&mut self, roots: impl IntoIterator<Item = (PathBuf, u8)>) {
        let mut roots = roots
            .into_iter()
            .map(|(root, tier)| (normalize_path_for_ordering(&root.to_string_lossy()), tier))
            .collect::<Vec<_>>();
        roots.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.cmp(b)));
        self.order_roots = roots;
        let keys = self
            .file_path_map
            .iter()
            .map(|(id, path)| (*id, Arc::from(self.order_key_for_path(path))))
            .collect::<Vec<_>>();
        self.file_order_keys.extend(keys);
    }

    /// Where a file at `path` goes in the order files load and elect writes:
    /// by the load tier of the workspace it belongs to (the std lib, then
    /// annotations and other libraries, then main workspaces), then by that
    /// workspace's root, then by its path within the root. Where the
    /// workspaces sit on disk decides nothing but the order of two roots of
    /// one tier.
    pub fn order_key_for_path(&self, path: &Path) -> String {
        let normalized = normalize_path_for_ordering(&path.to_string_lossy());
        let (root, tier) = self
            .order_roots
            .iter()
            .find(|(root, _)| {
                normalized.len() > root.len()
                    && normalized.starts_with(root.as_str())
                    && normalized.as_bytes()[root.len()] == b'/'
            })
            .map_or(("", WorkspaceKind::MERGE_RANK_NONE), |(root, tier)| {
                (root.as_str(), *tier)
            });
        format!("{tier}\u{1}{root}\u{1}{}", &normalized[root.len()..])
    }

    /// [`Vfs::order_key_for_path`] for a URI; one that names no path sorts by
    /// its text, after every file.
    pub fn order_key_for_uri(&self, uri: &Uri) -> String {
        match uri_to_file_path(uri) {
            Some(path) => self.order_key_for_path(&path),
            None => format!(
                "{}\u{1}{}",
                WorkspaceKind::MERGE_RANK_NONE + 1,
                uri.as_str()
            ),
        }
    }

    /// Whether `data` parses to the same significant token stream (kind,
    /// range and text) as the tree currently stored for `file_id`. Comments
    /// count as significant — annotations live in them — so only pure
    /// whitespace changes that shift no offset (e.g. a trailing newline at
    /// end of file) can match.
    pub fn content_semantically_matches(&mut self, file_id: FileId, data: &str) -> bool {
        if !self.tree_map.contains_key(&file_id) {
            return false;
        }
        let parse_config = self
            .emmyrc
            .as_ref()
            .expect("emmyrc set")
            .get_parse_config(&mut self.node_cache);
        let new_tree = LuaParser::parse(data, parse_config);
        let old_tree = &self.tree_map[&file_id];
        trees_semantically_equal(old_tree, &new_tree)
    }

    pub fn set_file_content(&mut self, uri: &Uri, data: Option<String>) -> FileId {
        self.content_revision += 1;
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
        self.note_edit_base(fid);
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

        self.content_revision += 1;
        self.note_edit_base(fid);

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

    /// Insert pre-parsed content for a file whose FileId was already assigned via `file_id()`.
    /// Used by parallel parsing to avoid re-assigning IDs.
    pub fn insert_preparsed(
        &mut self,
        fid: FileId,
        content: String,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
    ) {
        self.content_revision += 1;
        self.note_edit_base(fid);
        self.tree_map.insert(fid, tree);
        self.line_index_map.insert(fid, line_index);
        self.file_data[fid.id as usize] = Some(FileContent {
            content,
            is_remote: false,
            version: None,
        });
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

        self.content_revision += 1;
        self.note_edit_base(fid);

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
        self.content_revision += 1;
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
        self.note_edit_base(fid);
        self.file_data[fid.id as usize] = data.map(|content| FileContent {
            content,
            is_remote: true,
            version: None,
        });
        fid
    }

    /// Takes `uri`'s text away as its deletion from disk. The file keeps its
    /// entry until a re-analysis has taken the deletion, which re-analyses
    /// what the file held.
    pub fn delete_file_content(&mut self, uri: &Uri) -> Option<FileId> {
        let fid = self.get_file_id(uri)?;
        self.set_file_content(uri, None);
        self.deleted.insert(fid);
        Some(fid)
    }

    /// Forgets the deleted files whose deletion a re-analysis has taken and
    /// whose text did not come back.
    pub fn forget_taken_deletions(&mut self) {
        let taken = self
            .deleted
            .iter()
            .filter(|fid| !self.edit_bases.contains_key(fid))
            .copied()
            .collect::<Vec<_>>();
        for fid in taken {
            self.deleted.remove(&fid);
            if self
                .file_data
                .get(fid.id as usize)
                .is_some_and(Option::is_none)
                && let Some(path) = self.file_path_map.remove(&fid.id)
            {
                self.file_id_map.remove(&path);
                self.file_order_keys.remove(&fid.id);
            }
        }
    }

    pub fn remove_file(&mut self, uri: &Uri) -> Option<FileId> {
        let fid = self.get_file_id(uri)?;
        self.content_revision += 1;
        if let Some(path) = self.file_path_map.remove(&fid.id) {
            self.file_id_map.remove(&path);
        }
        self.file_order_keys.remove(&fid.id);
        self.note_edit_base(fid);
        if let Some(data) = self.file_data.get_mut(fid.id as usize) {
            data.take();
        }
        self.line_index_map.remove(&fid);
        self.tree_map.remove(&fid);
        Some(fid)
    }

    fn note_edit_base(&mut self, fid: FileId) {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.edit_bases.entry(fid) {
            entry.insert(
                self.file_data
                    .get(fid.id as usize)
                    .and_then(Option::as_ref)
                    .map(|content| content.content.clone()),
            );
        }
    }

    /// Whether `file_id`'s text changed since a re-analysis last took it.
    pub fn has_untaken_edit(&self, file_id: &FileId) -> bool {
        self.edit_bases.contains_key(file_id)
    }

    /// The files whose text changed since a re-analysis last took them,
    /// sorted.
    pub fn files_changed_since_taken(&self) -> Vec<FileId> {
        let mut files = self.edit_bases.keys().copied().collect::<Vec<_>>();
        files.sort_unstable();
        files
    }

    /// Where the edits since `file_ids` were last indexed moved their
    /// positions, for the files that had text then and still have it; the
    /// edits are forgotten.
    pub fn take_edit_maps(&mut self, file_ids: &[FileId]) -> Vec<(FileId, PositionMap)> {
        let mut maps = Vec::new();
        for file_id in file_ids {
            let Some(Some(old_text)) = self.edit_bases.remove(file_id) else {
                continue;
            };
            if let Some(content) = self
                .file_data
                .get(file_id.id as usize)
                .and_then(Option::as_ref)
            {
                maps.push((*file_id, PositionMap::new(&old_text, &content.content)));
            }
        }
        maps
    }

    pub fn update_config(&mut self, emmyrc: Arc<Emmyrc>) {
        self.emmyrc = Some(emmyrc);
    }

    pub fn get_file_content(&self, id: &FileId) -> Option<&String> {
        crate::db_index::derivation::note_file_read(*id);
        let opt = &self.file_data[id.id as usize];
        if let Some(s) = opt {
            Some(&s.content)
        } else {
            None
        }
    }

    /// The fingerprint of `id`'s text, for [`FactKey::FileText`](crate::db_index::derivation::FactKey::FileText).
    pub(crate) fn text_fingerprint(&self, id: &FileId) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_opt(
            self.file_data
                .get(id.id as usize)
                .and_then(Option::as_ref)
                .map(|data| &data.content),
        )
    }

    /// `id`'s syntax tree, for a caller that inspects only the top-level
    /// statement containing `range`: that statement is what it reads, not the
    /// whole file, so edits elsewhere in the file leave the read standing.
    pub fn syntax_tree_around(&self, id: &FileId, range: TextRange) -> Option<&LuaSyntaxTree> {
        let tree = self.tree_map.get(id)?;
        if crate::db_index::derivation::recording() {
            match top_level_statement(tree, range) {
                Some(statement) => {
                    crate::db_index::derivation::note_syntax_read(*id, statement.text_range())
                }
                None => crate::db_index::derivation::note_file_read(*id),
            }
        }
        Some(tree)
    }

    /// The fingerprint of the top-level statement at `site`, for
    /// [`FactKey::Syntax`](crate::db_index::derivation::FactKey::Syntax).
    pub(crate) fn statement_fingerprint(
        &self,
        site: &InFiled<TextRange>,
    ) -> crate::db_index::derivation::Fingerprint {
        let statement = self
            .tree_map
            .get(&site.file_id)
            .and_then(|tree| top_level_statement(tree, site.value))
            .filter(|statement| statement.text_range() == site.value)
            .and_then(|statement| {
                let content = &self
                    .file_data
                    .get(site.file_id.id as usize)?
                    .as_ref()?
                    .content;
                let text = content.get(std::ops::Range::<usize>::from(site.value))?;
                Some((site, statement.kind(), text))
            });
        crate::db_index::derivation::fingerprint_opt(statement.as_ref())
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
        crate::db_index::derivation::note_file_read(*id);
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

    /// The fingerprint of the file set, for [`FactKey::FileSet`](crate::db_index::derivation::FactKey::FileSet).
    pub(crate) fn file_set_fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_of(
            &self
                .file_data
                .iter()
                .enumerate()
                .filter_map(|(fid, content)| {
                    let content = content.as_ref()?;
                    let file_id = FileId { id: fid as u32 };
                    Some((fid, content.is_remote, self.tree_map.contains_key(&file_id)))
                })
                .collect::<Vec<_>>(),
        )
    }

    pub fn get_all_local_file_ids(&self) -> Vec<FileId> {
        crate::db_index::derivation::note_read(|| crate::db_index::derivation::FactKey::FileSet);
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
        crate::db_index::derivation::note_read(|| crate::db_index::derivation::FactKey::FileSet);
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
        self.edit_bases.clear();
        self.deleted.clear();
        self.emmyrc = None;
        self.node_cache = NodeCache::default();
    }
}

#[derive(Debug, Clone)]
struct FileContent {
    content: String,
    is_remote: bool,
    version: Option<i32>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use glua_parser::{LineIndex, LuaParser};
    use lsp_types::Uri;
    use rowan::NodeCache;

    use crate::Emmyrc;

    use super::Vfs;

    fn parse_lua(text: &str) -> (glua_parser::LuaSyntaxTree, LineIndex) {
        let mut node_cache = NodeCache::default();
        let emmyrc = Emmyrc::default();
        let parse_config = emmyrc.get_parse_config(&mut node_cache);
        (LuaParser::parse(text, parse_config), LineIndex::parse(text))
    }

    fn file_uri() -> Uri {
        "file:///test.lua".parse().expect("valid test uri")
    }

    fn new_vfs() -> Vfs {
        let mut vfs = Vfs::new();
        vfs.update_config(Arc::new(Emmyrc::default()));
        vfs
    }

    #[test]
    fn stale_preparsed_update_does_not_bump_content_revision() {
        let mut vfs = new_vfs();
        let uri = file_uri();
        let (tree, line_index) = parse_lua("local a = 1");
        vfs.set_file_content_preparsed(
            &uri,
            Some("local a = 1".to_string()),
            tree,
            line_index,
            Some(2),
        )
        .expect("initial update should be accepted");
        let revision = vfs.content_revision();

        let (tree, line_index) = parse_lua("local a = 0");
        let result = vfs.set_file_content_preparsed(
            &uri,
            Some("local a = 0".to_string()),
            tree,
            line_index,
            Some(1),
        );

        assert!(result.is_none(), "stale update should be rejected");
        assert_eq!(vfs.content_revision(), revision);
    }

    #[test]
    fn stale_deferred_preparsed_update_does_not_bump_content_revision() {
        let mut vfs = new_vfs();
        let uri = file_uri();
        let (tree, line_index) = parse_lua("local a = 1");
        vfs.set_file_content_preparsed_deferred(
            &uri,
            Some("local a = 1".to_string()),
            tree,
            line_index,
            Some(2),
        )
        .expect("initial update should be accepted");
        let revision = vfs.content_revision();

        let (tree, line_index) = parse_lua("local a = 0");
        let result = vfs.set_file_content_preparsed_deferred(
            &uri,
            Some("local a = 0".to_string()),
            tree,
            line_index,
            Some(1),
        );

        assert!(result.is_none(), "stale update should be rejected");
        assert_eq!(vfs.content_revision(), revision);
    }

    #[test]
    fn semantic_match_accepts_only_offset_preserving_whitespace() {
        let mut vfs = new_vfs();
        let uri = file_uri();
        let original = "local a = 1\nreturn a";
        let file_id = vfs.set_file_content(&uri, Some(original.to_string()));

        // Trailing whitespace shifts no token offset: still the same program.
        assert!(vfs.content_semantically_matches(file_id, "local a = 1\nreturn a\n"));
        assert!(vfs.content_semantically_matches(file_id, original));

        // Token text, leading whitespace (offsets shift) and comments (they
        // carry annotations) are all significant.
        assert!(!vfs.content_semantically_matches(file_id, "local a = 2\nreturn a"));
        assert!(!vfs.content_semantically_matches(file_id, "\nlocal a = 1\nreturn a"));
        assert!(!vfs.content_semantically_matches(file_id, "local a = 1 --c\nreturn a"));
        assert!(!vfs.content_semantically_matches(file_id, "local a = 1  \nreturn a"));
    }

    /// Trivia *kind* decides comment attachment: an EOL-to-space swap turns a
    /// leading doc annotation into an inline trailer on the previous statement
    /// (the Join Lines keystroke), and the blank-line count between `---` lines
    /// decides whether they form one doc block or two. Byte-length-preserving
    /// variants of both must never pass the gate.
    #[test]
    fn semantic_match_rejects_trivia_kind_mutations() {
        let mut vfs = new_vfs();
        let uri = file_uri();

        let annotated = "local a = 1\n---@type integer\nlocal b = x";
        let file_id = vfs.set_file_content(&uri, Some(annotated.to_string()));
        // Join Lines: `\n` -> ` `, every non-trivia token keeps kind/range/text.
        assert!(
            !vfs.content_semantically_matches(file_id, "local a = 1 ---@type integer\nlocal b = x")
        );

        let block = "--- a\r\n--- b\nlocal c = 1";
        let file_id = vfs.set_file_content(&uri, Some(block.to_string()));
        // Same byte length, but the blank line splits the doc block in two.
        assert!(!vfs.content_semantically_matches(file_id, "--- a\n\n--- b\nlocal c = 1"));
    }

    /// VFS ownership drops a guard-tripped tree safely on a production-sized
    /// (2 MB) thread. Drives the real replace/remove/stale/deferred entries —
    /// not a synthetic drop call — so the LSP/VFS boundary teardown is what is
    /// exercised. Rowan frees green nodes iteratively, so every ordinary drop
    /// below is O(1) stack.
    #[test]
    fn deep_tree_vfs_replace_and_remove_drop_safely_on_small_stack() {
        fn deep_blocks(depth: usize) -> String {
            let mut body = String::new();
            for _ in 0..depth {
                body.push_str("do ");
            }
            body.push('x');
            for _ in 0..depth {
                body.push_str(" end");
            }
            body.push('\n');
            body
        }

        let deep = deep_blocks(20000);
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let mut vfs = new_vfs();
                let uri = file_uri();

                let (tree, line_index) = parse_lua(&deep);
                assert!(
                    tree.get_errors()
                        .iter()
                        .any(|error| error.message.contains("too deeply nested")),
                    "deep blocks must trip the stack reserve guard"
                );
                vfs.set_file_content_preparsed(&uri, Some(deep.clone()), tree, line_index, Some(1))
                    .expect("initial deep insert must be accepted");

                let (shallow_tree, shallow_index) = parse_lua("local x = 1");
                vfs.set_file_content_preparsed(
                    &uri,
                    Some("local x = 1".to_string()),
                    shallow_tree,
                    shallow_index,
                    Some(2),
                )
                .expect("replacement must be accepted");

                let (tree, line_index) = parse_lua(&deep);
                vfs.set_file_content_preparsed(&uri, Some(deep.clone()), tree, line_index, Some(3))
                    .expect("deep re-insert must be accepted");
                vfs.remove_file(&uri).expect("remove must succeed");

                let (tree, line_index) = parse_lua("local y = 2");
                vfs.set_file_content_preparsed(
                    &uri,
                    Some("local y = 2".to_string()),
                    tree,
                    line_index,
                    Some(5),
                )
                .expect("fresh insert after remove must be accepted");
                let (stale_tree, stale_index) = parse_lua(&deep);
                assert!(
                    vfs.set_file_content_preparsed(
                        &uri,
                        Some(deep.clone()),
                        stale_tree,
                        stale_index,
                        Some(4),
                    )
                    .is_none(),
                    "stale preparsed update must be rejected"
                );

                let (tree, line_index) = parse_lua(&deep);
                let (_, deferred) = vfs
                    .set_file_content_preparsed_deferred(
                        &uri,
                        Some(deep.clone()),
                        tree,
                        line_index,
                        Some(6),
                    )
                    .expect("deferred deep insert must be accepted");
                drop(deferred);

                let (tree, line_index) = parse_lua("local z = 3");
                let (_, deferred) = vfs
                    .set_file_content_preparsed_deferred(
                        &uri,
                        Some("local z = 3".to_string()),
                        tree,
                        line_index,
                        Some(7),
                    )
                    .expect("deferred replacement must be accepted");
                drop(deferred);
            })
            .expect("worker thread should spawn")
            .join()
            .expect("production-sized worker must not overflow when VFS drops deep trees");
    }

    /// VFS `set_file_content` (real parse through the shared `NodeCache`)
    /// followed by `clear` and by VFS destruction must drop safely on a
    /// production-sized (2 MB) thread — including a red root retained past
    /// `clear`, whose final release previously aborted in recursive `GreenNode`
    /// destruction. Rowan now frees green nodes iteratively, so the retained
    /// red, the cleared trees, the dropped `NodeCache`, and the destroyed VFS
    /// below are all safe on this 2 MB stack; join-based like the existing
    /// 2 MB test.
    #[test]
    fn deep_tree_vfs_clear_and_destroy_drop_safely_on_small_stack() {
        fn deep_blocks(depth: usize) -> String {
            let mut body = String::new();
            for _ in 0..depth {
                body.push_str("do ");
            }
            body.push('x');
            for _ in 0..depth {
                body.push_str(" end");
            }
            body.push('\n');
            body
        }

        let deep = deep_blocks(20000);
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let mut vfs = new_vfs();
                let uri = file_uri();
                let file_id = vfs.set_file_content(&uri, Some(deep.clone()));
                assert!(
                    vfs.get_file_parse_error(&file_id)
                        .unwrap_or_default()
                        .iter()
                        .any(|error| error.message.contains("too deeply nested")),
                    "deep blocks must trip the stack reserve guard"
                );
                // Retain a red root past `clear`: the trees are gone but the
                // green stays pinned until this last red dies below.
                let red = vfs
                    .get_syntax_tree(&file_id)
                    .expect("deep file must have a tree")
                    .get_red_root();
                vfs.clear();
                drop(red);

                // And the destruction path: `Vfs` has no `Drop` impl, so every
                // tree and the whole `NodeCache` release inline here on 2 MB.
                let mut vfs = new_vfs();
                vfs.set_file_content(&uri, Some(deep.clone()));
                drop(vfs);
            })
            .expect("worker thread should spawn")
            .join()
            .expect(
                "production-sized worker must not overflow when VFS clears or drops deep trees",
            );
    }
}
