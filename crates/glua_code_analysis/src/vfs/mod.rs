mod collect_workspace_files;
mod document;
mod file_id;
mod file_renames;
mod file_uri_handler;
mod loader;
mod virtual_url;

pub use collect_workspace_files::*;
pub use document::LuaDocument;
pub use file_id::{FileId, InFiled};
pub use file_renames::FileRenames;
pub use file_uri_handler::{file_path_to_uri, uri_to_file_path};
use glua_parser::{
    LineIndex, LuaAstNode, LuaParseError, LuaParser, LuaStat, LuaSyntaxId, LuaSyntaxNode,
    LuaSyntaxTree,
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

use crate::compilation::analyzer::parallel::map_items_init;
use crate::{Emmyrc, PositionMap, WorkspaceKind};

#[derive(Debug, Clone)]
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
    /// Monotonic counter bumped whenever file *content* (or existence) changes.
    /// Lets workspace-wide derived caches (e.g. the gmod helper registry) detect
    /// that their inputs are unchanged and skip a full rescan. It is deliberately
    /// NOT bumped by analysis/index updates, so the cold-index main pass and its
    /// stabilization re-run share a revision and the second build is a cache hit.
    content_revision: u64,
    /// Each changed file's newest text until a re-analysis takes it; readers
    /// see the text taken last until then, which is what the index holds.
    staged: FxHashMap<FileId, Staged>,
    /// Files deleted from disk, kept until a re-analysis has taken the
    /// deletion; see [`Vfs::forget_taken_deletions`].
    deleted: FxHashSet<FileId>,
    /// The paths each file moved or was removed from, until a re-analysis
    /// takes them; see [`Vfs::take_left_paths`].
    left_paths: FxHashMap<FileId, Vec<PathBuf>>,
}

/// The child of `tree`'s top-level block that contains `range`.
fn top_level_statement(tree: &LuaSyntaxTree, range: TextRange) -> Option<LuaSyntaxNode> {
    tree.get_chunk_node()
        .get_block()?
        .syntax()
        .child_or_token_at_range(range)?
        .into_node()
}

/// The innermost statement of `tree` that contains `range`, or the child of
/// the top-level block that does when no statement does, such as a comment.
fn statement_around(tree: &LuaSyntaxTree, range: TextRange) -> Option<LuaSyntaxNode> {
    let root = tree.get_red_root();
    if !root.text_range().contains_range(range) {
        return None;
    }
    let node = match root.covering_element(range) {
        rowan::NodeOrToken::Node(node) => node,
        rowan::NodeOrToken::Token(token) => token.parent()?,
    };
    node.ancestors()
        .find(|node| LuaStat::can_cast(node.kind().into()))
        .or_else(|| top_level_statement(tree, range))
}

/// Staged text a later stage replaced, to drop off the lock.
#[derive(Default)]
pub struct DeferredVfsDrop {
    _replaced: Option<Staged>,
}

/// A file's newest text until a re-analysis takes it.
#[derive(Debug, Clone)]
struct Staged {
    /// `None` when the file is deleted.
    content: Option<FileContent>,
    parsed: Option<(LuaSyntaxTree, LineIndex)>,
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
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
            content_revision: 0,
            staged: FxHashMap::default(),
            deleted: FxHashSet::default(),
            left_paths: FxHashMap::default(),
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

    /// The id `uri`'s path is filed under, its text staged or taken. Analysis
    /// asks [`Vfs::file_at`] instead.
    pub fn get_file_id(&self, uri: &Uri) -> Option<FileId> {
        let path = uri_to_file_path(uri)?;
        self.file_id_map.get(&path).map(|&id| FileId { id })
    }

    /// The file at `uri` whose text readers see, recorded as a read of that
    /// path's occupant: a file created at, deleted from or moved to or from
    /// the path re-runs the reader.
    pub fn file_at(&self, uri: &Uri) -> Option<FileId> {
        let path = uri_to_file_path(uri)?;
        crate::db_index::derivation::note_read(|| {
            crate::db_index::derivation::FactKey::PathFile(Arc::from(path.as_path()))
        });
        self.occupant(&path)
    }

    fn occupant(&self, path: &Path) -> Option<FileId> {
        let id = *self.file_id_map.get(path)?;
        self.file_data
            .get(id as usize)?
            .as_ref()
            .map(|_| FileId { id })
    }

    /// The fingerprint of the file at `path`, for
    /// [`FactKey::PathFile`](crate::db_index::derivation::FactKey::PathFile).
    pub(crate) fn occupant_fingerprint(
        &self,
        path: &Path,
    ) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_opt(self.occupant(path).as_ref())
    }

    /// The paths `id` moved or was removed from since a re-analysis last took
    /// them.
    pub fn take_left_paths(&mut self, id: &FileId) -> Vec<PathBuf> {
        self.left_paths.remove(id).unwrap_or_default()
    }

    /// Whether `id` moved or was removed from a path since a re-analysis last
    /// took its paths.
    pub fn has_left_paths(&self, id: &FileId) -> bool {
        self.left_paths.contains_key(id)
    }

    pub fn get_uri(&self, id: &FileId) -> Option<Uri> {
        crate::db_index::derivation::note_file_path_read(*id);
        let path = self.file_path_map.get(&id.id)?;
        file_path_to_uri(path)
    }

    pub fn get_file_path(&self, id: &FileId) -> Option<&PathBuf> {
        crate::db_index::derivation::note_file_path_read(*id);
        self.file_path_map.get(&id.id)
    }

    /// The fingerprint of `id`'s path and order key, for
    /// [`FactKey::FilePath`](crate::db_index::derivation::FactKey::FilePath).
    pub(crate) fn path_fingerprint(&self, id: &FileId) -> crate::db_index::derivation::Fingerprint {
        let path = self.file_path_map.get(&id.id);
        let order_key = self.file_order_keys.get(&id.id);
        crate::db_index::derivation::fingerprint_opt(path.map(|path| (path, order_key)).as_ref())
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
        crate::db_index::derivation::note_file_path_read(*id);
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

    /// Parses `texts` on the analysis pool, each tree with its line index,
    /// aligned with `texts`. `None` when the batch is too small to be worth
    /// the pool; callers then go through [`Vfs::set_file_content`].
    pub fn parse_concurrently(&self, texts: &[&str]) -> Option<Vec<(LuaSyntaxTree, LineIndex)>> {
        let emmyrc = self.emmyrc.as_ref()?;
        map_items_init(texts, NodeCache::default, |node_cache, text| {
            let tree = LuaParser::parse(text, emmyrc.get_parse_config(node_cache));
            (tree, LineIndex::parse(text))
        })
    }

    fn parse(&self, text: &str) -> (LuaSyntaxTree, LineIndex) {
        let mut node_cache = NodeCache::default();
        let parse_config = self
            .emmyrc
            .as_ref()
            .expect("emmyrc set")
            .get_parse_config(&mut node_cache);
        (LuaParser::parse(text, parse_config), LineIndex::parse(text))
    }

    /// Stages `content` as `fid`'s newest text, replacing what an earlier
    /// stage left untaken, which it hands back.
    fn stage(
        &mut self,
        fid: FileId,
        content: Option<String>,
        parsed: Option<(LuaSyntaxTree, LineIndex)>,
        is_remote: bool,
        version: Option<i32>,
    ) -> Option<Staged> {
        self.content_revision += 1;
        let revision = self.content_revision;
        if content.is_none() && !is_remote {
            self.deleted.insert(fid);
        }
        let content = content.map(|content| FileContent {
            content,
            is_remote,
            version,
            revision,
        });
        let parsed = parsed.filter(|_| content.is_some());
        self.staged.insert(fid, Staged { content, parsed })
    }

    /// Stages `data` as `uri`'s text, or its deletion when `None`; readers
    /// see it once [`Vfs::take_staged`] takes it.
    pub fn set_file_content(&mut self, uri: &Uri, data: Option<String>) -> FileId {
        let fid = self.file_id(uri);
        log::debug!("file_id: {:?}, uri: {}", fid, uri.as_str());
        let parsed = data.as_deref().map(|data| self.parse(data));
        self.stage(fid, data, parsed, false, None);
        fid
    }

    /// [`Vfs::set_file_content`] with a tree parsed elsewhere. A `version`
    /// older than the file's latest is refused.
    pub fn set_file_content_preparsed(
        &mut self,
        uri: &Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<FileId> {
        self.set_file_content_preparsed_deferred(uri, text, tree, line_index, version)
            .map(|(fid, _)| fid)
    }

    /// [`Vfs::set_file_content_preparsed`], handing back the staged text it
    /// replaced so the caller can drop it off the lock.
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
        log::debug!("file_id (preparsed): {:?}, uri: {}", fid, uri.as_str());

        if let (Some(incoming_version), Some(current_version)) =
            (version, self.latest_version(&fid))
            && incoming_version < current_version
        {
            return None;
        }

        let replaced = self.stage(fid, text, Some((tree, line_index)), false, version);
        Some((
            fid,
            DeferredVfsDrop {
                _replaced: replaced,
            },
        ))
    }

    pub fn set_remote_file_content(&mut self, uri: &Uri, data: Option<String>) -> FileId {
        let fid = self.virtual_file_id(uri);
        log::debug!("virtual file_id: {:?}, uri: {}", fid, uri.as_str());
        let parsed = data.as_deref().map(|data| self.parse(data));
        self.stage(fid, data, parsed, true, None);
        fid
    }

    /// Stages `uri`'s deletion from disk. The file keeps its entry until a
    /// re-analysis has taken the deletion, which re-analyses what the file
    /// held.
    pub fn delete_file_content(&mut self, uri: &Uri) -> Option<FileId> {
        let fid = self.get_file_id(uri)?;
        self.set_file_content(uri, None);
        Some(fid)
    }

    /// Forgets the deleted files whose deletion a re-analysis has taken and
    /// whose text did not come back.
    pub fn forget_taken_deletions(&mut self) {
        let taken = self
            .deleted
            .iter()
            .filter(|fid| !self.staged.contains_key(fid))
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

    /// Moves `old`'s file to `new`, staging `text` as its text so a
    /// re-analysis takes the move as it takes an edit. The file keeps its id.
    /// `None` when `old` names no file or `new` already names one.
    pub fn move_file(&mut self, old: &Uri, new: &Uri, text: String) -> Option<FileId> {
        let fid = self.get_file_id(old)?;
        let new_path = uri_to_file_path(new)?;
        if self.file_id_map.contains_key(&new_path) {
            return None;
        }
        if let Some(old_path) = self.file_path_map.remove(&fid.id) {
            self.file_id_map.remove(&old_path);
            self.left_paths.entry(fid).or_default().push(old_path);
        }
        self.file_id_map.insert(new_path.clone(), fid.id);
        self.file_order_keys
            .insert(fid.id, Arc::from(self.order_key_for_path(&new_path)));
        self.file_path_map.insert(fid.id, new_path);
        let reused = match self.staged.get(&fid) {
            Some(staged) => staged.parsed.clone(),
            None => self
                .tree_map
                .get(&fid)
                .cloned()
                .zip(self.line_index_map.get(&fid).cloned()),
        };
        let parsed = reused
            .filter(|_| self.latest_content(&fid) == Some(&text))
            .unwrap_or_else(|| self.parse(&text));
        self.stage(fid, Some(text), Some(parsed), false, None);
        Some(fid)
    }

    /// Forgets `uri`'s path at once and stages the removal of its text.
    pub fn remove_file(&mut self, uri: &Uri) -> Option<FileId> {
        let fid = self.get_file_id(uri)?;
        if let Some(path) = self.file_path_map.remove(&fid.id) {
            self.file_id_map.remove(&path);
            self.left_paths.entry(fid).or_default().push(path);
        }
        self.file_order_keys.remove(&fid.id);
        self.stage(fid, None, None, false, None);
        Some(fid)
    }

    /// Whether `file_id` has staged text no re-analysis has taken yet.
    pub fn has_untaken_edit(&self, file_id: &FileId) -> bool {
        self.staged.contains_key(file_id)
    }

    /// Whether any file has staged text no re-analysis has taken yet.
    pub fn holds_untaken_text(&self) -> bool {
        !self.staged.is_empty()
    }

    /// The files with staged text no re-analysis has taken yet, sorted.
    pub fn files_changed_since_taken(&self) -> Vec<FileId> {
        let mut files = self.staged.keys().copied().collect::<Vec<_>>();
        files.sort_unstable();
        files
    }

    /// The newest of `file_id`'s text, staged or taken, for the code that
    /// stages edits; readers use [`Vfs::get_file_content`].
    pub fn latest_content(&self, file_id: &FileId) -> Option<&String> {
        self.latest(file_id).map(|data| &data.content)
    }

    /// The editor version of `file_id`'s newest text, staged or taken.
    pub fn latest_version(&self, file_id: &FileId) -> Option<i32> {
        self.latest(file_id).and_then(|data| data.version)
    }

    fn latest(&self, file_id: &FileId) -> Option<&FileContent> {
        match self.staged.get(file_id) {
            Some(staged) => staged.content.as_ref(),
            None => self.file_data.get(file_id.id as usize)?.as_ref(),
        }
    }

    /// Makes the staged text of `file_ids` what readers see. Returns where
    /// each one's edit moved its positions, for the files that had text
    /// before and still have it.
    pub fn take_staged(&mut self, file_ids: &[FileId]) -> Vec<(FileId, PositionMap)> {
        let mut maps = Vec::new();
        for file_id in file_ids {
            let Some(staged) = self.staged.remove(file_id) else {
                continue;
            };
            let slot = &mut self.file_data[file_id.id as usize];
            if let (Some(old), Some(new)) = (slot.as_ref(), staged.content.as_ref()) {
                maps.push((*file_id, PositionMap::new(&old.content, &new.content)));
            }
            *slot = staged.content;
            match staged.parsed {
                Some((tree, line_index)) => {
                    self.tree_map.insert(*file_id, tree);
                    self.line_index_map.insert(*file_id, line_index);
                }
                None => {
                    self.tree_map.remove(file_id);
                    self.line_index_map.remove(file_id);
                }
            }
        }
        maps
    }

    /// Applies `emmyrc`, parsing every held text again when it parses
    /// differently: a tree is only ever the parse of its text under the
    /// current config.
    pub fn update_config(&mut self, emmyrc: Arc<Emmyrc>) {
        let reparse = self
            .emmyrc
            .as_ref()
            .is_some_and(|old| !old.runtime.parses_like(&emmyrc.runtime));
        self.emmyrc = Some(emmyrc);
        if reparse {
            self.reparse_all();
        }
    }

    fn reparse_all(&mut self) {
        let (ids, trees) = {
            let mut taken = self
                .tree_map
                .keys()
                .filter_map(|id| {
                    let data = self.file_data.get(id.id as usize)?.as_ref()?;
                    Some((*id, data.content.as_str()))
                })
                .collect::<Vec<_>>();
            taken.sort_by_key(|(id, _)| *id);
            let texts = taken.iter().map(|(_, text)| *text).collect::<Vec<_>>();
            let trees = self.parse_concurrently(&texts).unwrap_or_else(|| {
                texts
                    .iter()
                    .map(|text| self.parse(text))
                    .collect::<Vec<_>>()
            });
            (
                taken.into_iter().map(|(id, _)| id).collect::<Vec<_>>(),
                trees,
            )
        };
        for (id, (tree, _)) in ids.into_iter().zip(trees) {
            self.tree_map.insert(id, tree);
        }
        let staged = self
            .staged
            .iter()
            .filter_map(|(id, staged)| {
                let content = staged.content.as_ref()?;
                Some((*id, self.parse(&content.content)))
            })
            .collect::<Vec<_>>();
        for (id, parsed) in staged {
            if let Some(staged) = self.staged.get_mut(&id) {
                staged.parsed = Some(parsed);
            }
        }
    }

    pub fn get_file_content(&self, id: &FileId) -> Option<&String> {
        crate::db_index::derivation::note_file_read(*id);
        self.file_data
            .get(id.id as usize)?
            .as_ref()
            .map(|data| &data.content)
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
        self.syntax_tree_reading(id, range, top_level_statement)
    }

    /// The node `syntax_id` names in `id`'s tree, reading only the top-level
    /// statement that holds it.
    pub fn syntax_node_around(&self, id: &FileId, syntax_id: LuaSyntaxId) -> Option<LuaSyntaxNode> {
        let tree = self.syntax_tree_around(id, syntax_id.get_range())?;
        syntax_id.to_node_from_root(&tree.get_red_root())
    }

    /// `id`'s syntax tree, for a caller that inspects only the innermost
    /// statement containing `range` and the kinds and ranges of the nodes
    /// around it: edits elsewhere in the statements enclosing it leave the
    /// read standing.
    pub fn syntax_tree_at_statement(
        &self,
        id: &FileId,
        range: TextRange,
    ) -> Option<&LuaSyntaxTree> {
        self.syntax_tree_reading(id, range, statement_around)
    }

    fn syntax_tree_reading(
        &self,
        id: &FileId,
        range: TextRange,
        statement: fn(&LuaSyntaxTree, TextRange) -> Option<LuaSyntaxNode>,
    ) -> Option<&LuaSyntaxTree> {
        let tree = self.tree_map.get(id)?;
        if crate::db_index::derivation::recording() {
            match statement(tree, range) {
                Some(statement) => {
                    crate::db_index::derivation::note_syntax_read(*id, statement.text_range())
                }
                None => crate::db_index::derivation::note_file_read(*id),
            }
        }
        Some(tree)
    }

    /// The fingerprint of the statement at `site`, for
    /// [`FactKey::Syntax`](crate::db_index::derivation::FactKey::Syntax): its
    /// kind and text, and the kind and range of every node between it and the
    /// top-level block.
    pub(crate) fn statement_fingerprint(
        &self,
        site: &InFiled<TextRange>,
    ) -> crate::db_index::derivation::Fingerprint {
        let statement = self
            .tree_map
            .get(&site.file_id)
            .and_then(|tree| statement_around(tree, site.value))
            .filter(|statement| statement.text_range() == site.value)
            .and_then(|statement| {
                let content = &self
                    .file_data
                    .get(site.file_id.id as usize)?
                    .as_ref()?
                    .content;
                let text = content.get(std::ops::Range::<usize>::from(site.value))?;
                let context = statement
                    .ancestors()
                    .skip(1)
                    .take_while(|node| node.parent().and_then(|parent| parent.parent()).is_some())
                    .map(|node| (node.kind(), InFiled::new(site.file_id, node.text_range())))
                    .collect::<Vec<_>>();
                Some((site, statement.kind(), text, context))
            });
        crate::db_index::derivation::fingerprint_opt(statement.as_ref())
    }

    pub fn get_file_version(&self, id: &FileId) -> Option<i32> {
        self.file_data
            .get(id.id as usize)
            .and_then(Option::as_ref)
            .and_then(|content| content.version)
    }

    /// The content revision `id`'s current text was set at; it changes only
    /// when the text does.
    pub fn get_file_revision(&self, id: &FileId) -> Option<u64> {
        self.file_data
            .get(id.id as usize)
            .and_then(Option::as_ref)
            .map(|content| content.revision)
    }

    /// Sets the editor version of `id`'s newest text, staged or taken.
    pub fn update_file_version(&mut self, id: &FileId, version: Option<i32>) {
        let latest = match self.staged.get_mut(id) {
            Some(staged) => staged.content.as_mut(),
            None => self
                .file_data
                .get_mut(id.id as usize)
                .and_then(Option::as_mut),
        };
        if let Some(content) = latest {
            content.version = version;
        }
    }

    /// Forgets the editor version of `id`'s text, staged and taken.
    pub fn forget_file_version(&mut self, id: &FileId) {
        let staged = self
            .staged
            .get_mut(id)
            .and_then(|staged| staged.content.as_mut());
        let taken = self
            .file_data
            .get_mut(id.id as usize)
            .and_then(Option::as_mut);
        for content in staged.into_iter().chain(taken) {
            content.version = None;
        }
    }

    pub fn get_document(&self, id: &FileId) -> Option<LuaDocument<'_>> {
        let path = self.get_file_path(id)?;
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
        self.file_order_keys.clear();
        self.file_data.clear();
        self.line_index_map.clear();
        self.tree_map.clear();
        self.staged.clear();
        self.deleted.clear();
        self.left_paths.clear();
        self.emmyrc = None;
    }
}

#[derive(Debug, Clone)]
struct FileContent {
    content: String,
    is_remote: bool,
    version: Option<i32>,
    /// The content revision this text was set at.
    revision: u64,
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

    /// Readers see the text a take installed, never text staged after it,
    /// while the staging side sees the newest.
    #[test]
    fn staged_text_is_read_only_once_taken() {
        let mut vfs = new_vfs();
        let uri = file_uri();
        let file_id = vfs.set_file_content(
            &uri,
            Some(
                "local long_name = 1
"
                .to_string(),
            ),
        );
        assert_eq!(vfs.get_file_content(&file_id), None);
        vfs.take_staged(&[file_id]);

        vfs.set_file_content(
            &uri,
            Some(
                "local a = 2
"
                .to_string(),
            ),
        );
        vfs.set_file_content(
            &uri,
            Some(
                "x = 3
"
                .to_string(),
            ),
        );

        let read = |vfs: &Vfs| {
            let tree = vfs.get_syntax_tree(&file_id).expect("a tree");
            (
                vfs.get_file_content(&file_id).cloned(),
                tree.get_red_root().text().to_string(),
            )
        };
        let taken = "local long_name = 1
"
        .to_string();
        assert_eq!(read(&vfs), (Some(taken.clone()), taken));
        assert_eq!(
            vfs.latest_content(&file_id).map(String::as_str),
            Some(
                "x = 3
"
            )
        );
        assert!(vfs.has_untaken_edit(&file_id));

        let maps = vfs.take_staged(&[file_id]);
        assert_eq!(maps.len(), 1, "one map from the taken text to the newest");
        assert_eq!(
            read(&vfs),
            (
                Some(
                    "x = 3
"
                    .to_string()
                ),
                "x = 3
"
                .to_string()
            )
        );
        assert!(!vfs.has_untaken_edit(&file_id));
    }

    /// A staged deletion leaves the file readable until it is taken.
    #[test]
    fn a_staged_deletion_is_read_only_once_taken() {
        let mut vfs = new_vfs();
        let uri = file_uri();
        let file_id = vfs.set_file_content(
            &uri,
            Some(
                "local a = 1
"
                .to_string(),
            ),
        );
        vfs.take_staged(&[file_id]);

        vfs.delete_file_content(&uri);
        assert!(vfs.get_syntax_tree(&file_id).is_some());
        assert_eq!(vfs.latest_content(&file_id), None);

        assert!(vfs.take_staged(&[file_id]).is_empty());
        assert!(vfs.get_syntax_tree(&file_id).is_none());
        assert_eq!(vfs.get_file_content(&file_id), None);
    }

    /// An editor version set on staged text is the staged text's; the taken
    /// text keeps its own until the take.
    #[test]
    fn a_version_belongs_to_the_newest_text() {
        let mut vfs = new_vfs();
        let uri = file_uri();
        let (tree, line_index) = parse_lua("local a = 1");
        let file_id = vfs
            .set_file_content_preparsed(
                &uri,
                Some("local a = 1".to_string()),
                tree,
                line_index,
                Some(1),
            )
            .expect("text is staged");
        vfs.take_staged(&[file_id]);
        let (tree, line_index) = parse_lua("local a = 2");
        vfs.set_file_content_preparsed(
            &uri,
            Some("local a = 2".to_string()),
            tree,
            line_index,
            Some(2),
        )
        .expect("text is staged");

        assert_eq!(vfs.get_file_version(&file_id), Some(1));
        assert_eq!(vfs.latest_version(&file_id), Some(2));
        vfs.take_staged(&[file_id]);
        assert_eq!(vfs.get_file_version(&file_id), Some(2));
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
                let fid = vfs
                    .set_file_content_preparsed(&uri, Some(deep.clone()), tree, line_index, Some(1))
                    .expect("initial deep insert must be accepted");
                vfs.take_staged(&[fid]);

                let (shallow_tree, shallow_index) = parse_lua("local x = 1");
                vfs.set_file_content_preparsed(
                    &uri,
                    Some("local x = 1".to_string()),
                    shallow_tree,
                    shallow_index,
                    Some(2),
                )
                .expect("replacement must be accepted");
                vfs.take_staged(&[fid]);

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
    fn concurrent_parsing_matches_serial_parsing_and_keeps_id_order() {
        let uris: Vec<Uri> = (0..40)
            .map(|index| {
                format!("file:///workspace/lua/f{index}.lua")
                    .parse()
                    .expect("valid test uri")
            })
            .collect();
        let texts: Vec<String> = (0..uris.len())
            .map(|index| match index % 5 {
                0 => format!("local a{index} = {index}\nreturn a{index}"),
                1 => format!("---@class C{index}\nlocal C = {{}}\nfunction C:run() end"),
                2 => format!("if x{index} then\n  print(\"é{index}\")\n"),
                3 => String::new(),
                _ => format!("for i = 1, {index} do local t = {{ i, {index} }} end\r\nreturn"),
            })
            .collect();

        let mut serial = new_vfs();
        let serial_ids: Vec<_> = uris
            .iter()
            .zip(&texts)
            .map(|(uri, text)| serial.set_file_content(uri, Some(text.clone())))
            .collect();

        let _threads = crate::compilation::analyzer::parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::compilation::analyzer::parallel::set_analysis_threads(4);
        let mut concurrent = new_vfs();
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let parsed = concurrent.parse_concurrently(&borrowed);
        crate::compilation::analyzer::parallel::set_analysis_threads(0);
        let parsed = parsed.expect("a batch this size parses on the pool");

        let concurrent_ids: Vec<_> = uris
            .iter()
            .zip(texts.iter().zip(parsed))
            .map(|(uri, (text, (tree, line_index)))| {
                concurrent
                    .set_file_content_preparsed(uri, Some(text.clone()), tree, line_index, None)
                    .expect("text is set")
            })
            .collect();

        assert_eq!(serial_ids, concurrent_ids);
        serial.take_staged(&serial_ids);
        concurrent.take_staged(&concurrent_ids);
        for id in serial_ids {
            let (expected, actual) = (
                serial.get_syntax_tree(&id).expect("serial tree"),
                concurrent.get_syntax_tree(&id).expect("concurrent tree"),
            );
            assert_eq!(
                expected.get_red_root().green(),
                actual.get_red_root().green()
            );
            assert_eq!(
                format!("{:?}", expected.get_errors()),
                format!("{:?}", actual.get_errors())
            );
            assert_eq!(
                format!("{:?}", serial.get_document(&id)),
                format!("{:?}", concurrent.get_document(&id))
            );
        }
    }

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
                vfs.take_staged(&[file_id]);
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
