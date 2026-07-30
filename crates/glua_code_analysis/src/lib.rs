#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::panic,
        clippy::panic_in_result_fn
    )
)]

mod ast_util;
mod compilation;
mod config;
mod db_index;
mod diagnostic;
mod gamemode_base;
mod library_collision;
mod profile;
mod resources;
mod semantic;
mod test_lib;
mod vfs;

pub use compilation::*;
pub use config::*;
pub use db_index::*;
pub use diagnostic::*;
pub use gamemode_base::{
    detect_gamemode_base_libraries, find_gamemode_manifest, read_gamemode_base,
};
pub use glua_codestyle::*;
use glua_parser::{
    LineIndex, LuaAstNode, LuaCallExpr, LuaExpr, LuaIndexKey, LuaLocalStat, LuaNameExpr,
    LuaParenExpr, LuaParser, LuaSyntaxTree,
};
pub use library_collision::LibraryDefinitionCollision;
use lsp_types::Uri;
pub use profile::Profile;
use resources::load_resource_std;
use schema_to_glua::SchemaConverter;
pub use semantic::*;
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path};
use std::str::FromStr;
use std::{collections::HashSet, path::PathBuf, sync::Arc};
pub use test_lib::{GMOD_CALL_ARG_BUILTINS_FIXTURE, VirtualWorkspace};
use tokio_util::sync::CancellationToken;
use url::Url;
pub use vfs::*;

#[derive(Default)]
struct InferredGuardSnapshot {
    facts: HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
    consumers: HashMap<LuaInferredGuardOwner, HashSet<FileId>>,
}

#[derive(Default)]
struct InferredGuardReferenceFiles {
    files: HashSet<FileId>,
    alias_calls: HashSet<FileId>,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InferredGuardPropagationStats {
    pub changed_facts: usize,
    pub reference_edges: usize,
    pub frontiers: usize,
    pub reindexed_files: usize,
    pub broad_stabilizations: usize,
}

fn sort_inferred_guard_owners(owners: &mut [LuaInferredGuardOwner]) {
    owners.sort_by(|left, right| {
        (left.source_file_id(), left.source_position(), left.path()).cmp(&(
            right.source_file_id(),
            right.source_position(),
            right.path(),
        ))
    });
}

fn global_path_for_expr(expr: &LuaExpr) -> Option<Vec<smol_str::SmolStr>> {
    let mut path = match expr {
        LuaExpr::NameExpr(name_expr) => {
            Some(vec![name_expr.get_name_token()?.get_name_text().into()])
        }
        LuaExpr::IndexExpr(index_expr) => {
            if index_expr.get_index_token()?.is_colon() {
                return None;
            }
            let mut path = global_path_for_expr(&index_expr.get_prefix_expr()?)?;
            let member = match index_expr.get_index_key()? {
                LuaIndexKey::Name(name) => name.get_name_text().into(),
                LuaIndexKey::String(string) => string.get_value().into(),
                _ => return None,
            };
            path.push(member);
            Some(path)
        }
        _ => None,
    }?;
    canonicalize_global_root_path(&mut path);
    Some(path)
}

fn immutable_local_alias_decl(
    db: &DbIndex,
    file_id: FileId,
    alias_value: &LuaExpr,
) -> Option<LuaDeclId> {
    let alias_value = enclosing_parenthesized_expr(alias_value);
    let local_stat = alias_value.get_parent::<LuaLocalStat>()?;
    let local_name = local_stat.get_local_name_by_value(alias_value.clone())?;
    let decl_id = LuaDeclId::new(file_id, local_name.get_position());
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !matches!(decl.extra, LuaDeclExtra::Local { .. })
        || decl.get_value_syntax_id() != Some(alias_value.get_syntax_id())
        || db
            .get_reference_index()
            .get_decl_references(&file_id, &decl_id)
            .is_none_or(|references| references.mutable)
    {
        return None;
    }
    Some(decl_id)
}

fn enclosing_parenthesized_expr(expr: &LuaExpr) -> LuaExpr {
    let mut expr = expr.clone();
    while let Some(paren_expr) = expr.get_parent::<LuaParenExpr>() {
        if paren_expr
            .get_expr()
            .is_none_or(|inner| inner.get_syntax_id() != expr.get_syntax_id())
        {
            break;
        }
        expr = LuaExpr::ParenExpr(paren_expr);
    }
    expr
}

fn is_call_prefix(expr: &LuaExpr) -> bool {
    let expr = enclosing_parenthesized_expr(expr);
    expr.get_parent::<LuaCallExpr>()
        .and_then(|call| call.get_prefix_expr())
        .is_some_and(|prefix| prefix.get_syntax_id() == expr.get_syntax_id())
}

fn expr_resolves_to_inferred_guard_owner(
    db: &DbIndex,
    caches: &mut HashMap<FileId, LuaInferCache>,
    owner: &LuaInferredGuardOwner,
    file_id: FileId,
    expr: &LuaExpr,
) -> bool {
    let cache = caches
        .entry(file_id)
        .or_insert_with(|| LuaInferCache::new(file_id, Default::default()));
    semantic::infer_expr(db, cache, expr.clone()).ok()
        == Some(LuaType::Signature(owner.signature_id()))
}

fn call_resolves_to_inferred_guard_owner(
    db: &DbIndex,
    caches: &mut HashMap<FileId, LuaInferCache>,
    owner: &LuaInferredGuardOwner,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> bool {
    let prefix_expr = enclosing_parenthesized_expr(prefix_expr);
    let Some(call) = prefix_expr.get_parent::<LuaCallExpr>() else {
        return false;
    };
    if call
        .get_prefix_expr()
        .is_none_or(|prefix| prefix.get_syntax_id() != prefix_expr.get_syntax_id())
    {
        return false;
    }
    let cache = caches
        .entry(file_id)
        .or_insert_with(|| LuaInferCache::new(file_id, Default::default()));
    semantic::get_prefix_expr_signature_id(db, cache, &call) == Some(owner.signature_id())
}

/// True when `call_expr` calls an annotated net operation — a message start, a
/// send terminator, or a payload write/read.
///
/// Shares the analyzer's resolution path, so an alias, a local binding, or an
/// annotated wrapper is classified identically to the `net.*` builtin. Editor
/// handlers should use this instead of re-deriving the answer, and instead of
/// consulting the flow index: an op that forms no complete flow (a bare
/// `net.WriteString` with no `net.Start`) is still a net op and is never
/// recorded in the flow index.
///
/// `cache` is supplied by the caller because classifying a document means asking
/// this for every call in it, and building an inference cache per question would
/// throw away all reuse between them.
pub fn call_expr_is_net_op(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(signature_id) = semantic::get_prefix_expr_signature_id(db, cache, call_expr) else {
        return false;
    };
    db_index::signature_has_net_op_metadata(db, signature_id)
}

pub async fn fetch_schema_urls(urls: Vec<Url>) -> HashMap<Url, String> {
    let mut url_contents = HashMap::new();
    for url in urls {
        if url.scheme() == "file" {
            if let Ok(path) = url.to_file_path()
                && path.exists()
            {
                let result = read_file_with_encoding(&path, "utf-8");
                if let Some(content) = result {
                    url_contents.insert(url, content);
                } else {
                    log::error!("Failed to read schema file: {:?}", path);
                }
            }
        } else {
            let result = reqwest::get(url.as_str()).await;
            if let Ok(response) = result {
                if let Ok(content) = response.text().await {
                    url_contents.insert(url, content);
                } else {
                    log::error!("Failed to read schema content from URL: {:?}", url);
                }
            } else {
                log::error!("Failed to fetch schema from URL: {:?}", url);
            }
        }
    }

    url_contents
}

/// Normalize a workspace root path so it uses the same drive-letter
/// casing that the VFS applies (uppercase on Windows).  Without this,
/// `extract_module_path` would fail to match VFS paths against
/// library workspace roots supplied by the editor with a lowercase
/// drive letter.
fn normalize_workspace_root(root: PathBuf) -> PathBuf {
    file_path_to_uri(&root)
        .and_then(|uri| uri_to_file_path(&uri))
        .unwrap_or(root)
}

pub(crate) fn dependency_site_path_keys(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_path: &str,
) -> Vec<String> {
    let dependency_path = normalize_dependency_path(dependency_path);
    if dependency_path.is_empty() {
        return Vec::new();
    }

    let mut keys = HashSet::new();
    insert_dependency_path_key_variants(&mut keys, dependency_path.clone());

    if let Some(source_parent) = db
        .get_vfs()
        .get_file_path(&source_file_id)
        .and_then(|source_path| source_path.parent())
    {
        let relative_candidate =
            lexically_normalize_path(&source_parent.join(Path::new(&dependency_path)));
        insert_dependency_path_key_variants(&mut keys, normalize_file_path(&relative_candidate));
    }

    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn dependency_path_keys_for_target(db: &DbIndex, target_path: &Path) -> Vec<String> {
    let mut keys = HashSet::new();
    let Some(target_path_text) = target_path.to_str() else {
        return Vec::new();
    };

    let normalized_target_path = normalize_file_path(target_path);
    insert_dependency_path_key_variants(&mut keys, normalized_target_path.clone());

    if let Some(lua_idx) = normalized_target_path.find("/lua/") {
        let lua_relative = normalized_target_path[(lua_idx + 1)..].to_string();
        insert_dependency_path_key_variants(&mut keys, lua_relative.clone());
        insert_dependency_path_key_variants(&mut keys, lua_relative.replace('/', "."));
        if let Some(without_lua) = lua_relative.strip_prefix("lua/") {
            insert_dependency_path_key_variants(&mut keys, without_lua.to_string());
            insert_dependency_path_key_variants(&mut keys, without_lua.replace('/', "."));
        }
    }

    if let Some((module_path, _)) = db.get_module_index().extract_module_path(target_path_text) {
        let module_path = normalize_dependency_path(&module_path.replace('\\', "/"));
        insert_dependency_path_key_variants(&mut keys, module_path.replace('.', "/"));
        insert_dependency_path_key_variants(&mut keys, module_path);
    }

    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn insert_dependency_path_key_variants(keys: &mut HashSet<String>, path: String) {
    let normalized = normalize_dependency_path(&path);
    if normalized.is_empty() {
        return;
    }
    keys.insert(normalized.clone());
    if let Some(without_lua_ext) = normalized.strip_suffix(".lua") {
        keys.insert(without_lua_ext.to_string());
    } else {
        keys.insert(format!("{normalized}.lua"));
    }
}

fn normalize_dependency_path(path: &str) -> String {
    let mut normalized = normalize_path_case(path.replace('\\', "/"));
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }
    normalized.trim_matches('/').to_string()
}

fn normalize_file_path(path: &Path) -> String {
    normalize_path_case(path.to_string_lossy().replace('\\', "/"))
        .trim_end_matches('/')
        .to_string()
}

fn normalize_path_case(path: String) -> String {
    #[cfg(target_os = "windows")]
    {
        path.to_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path
    }
}

fn lexically_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[derive(Debug)]
pub struct EmmyLuaAnalysis {
    pub compilation: LuaCompilation,
    pub diagnostic: LuaDiagnostic,
    pub emmyrc: Arc<Emmyrc>,
    #[cfg(test)]
    pub(crate) inferred_guard_propagation_stats: InferredGuardPropagationStats,
    #[cfg(test)]
    cross_file_stabilization_invocations: usize,
}

impl EmmyLuaAnalysis {
    pub fn new() -> Self {
        let emmyrc = Arc::new(Emmyrc::default());
        Self {
            compilation: LuaCompilation::new(emmyrc.clone()),
            diagnostic: LuaDiagnostic::new(),
            emmyrc,
            #[cfg(test)]
            inferred_guard_propagation_stats: InferredGuardPropagationStats::default(),
            #[cfg(test)]
            cross_file_stabilization_invocations: 0,
        }
    }

    pub fn init_std_lib(&mut self) {
        let is_jit = matches!(self.emmyrc.runtime.version, EmmyrcLuaVersion::LuaJIT);
        let (std_root, files) = load_resource_std(is_jit);
        // Normalize so the root's drive-letter casing matches VFS file paths
        // (the URI round-trip uppercases the Windows drive letter). Without
        // this, `extract_module_path` prefix matching would fail when the
        // env-derived root has a lowercase drive letter.
        let std_root = normalize_workspace_root(std_root);
        self.init_std_lib_from_files(std_root, files);
    }

    /// Register a pre-built set of embedded std files directly into the analysis
    /// workspace without going through the resource-loading pipeline.
    pub(crate) fn init_std_lib_from_files(&mut self, std_root: PathBuf, files: Vec<LuaFileInfo>) {
        self.compilation
            .get_db_mut()
            .get_module_index_mut()
            .add_workspace_root_with_kind(std_root, WorkspaceId::STD, WorkspaceKind::Std);

        let files = files
            .into_iter()
            .filter_map(|file| {
                if file.path.ends_with(".lua") {
                    Some((PathBuf::from(file.path), Some(file.content)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.update_files_by_path(files);
    }

    pub fn get_file_id(&self, uri: &Uri) -> Option<FileId> {
        self.compilation.get_db().get_vfs().get_file_id(uri)
    }

    pub fn get_uri(&self, file_id: FileId) -> Option<Uri> {
        self.compilation.get_db().get_vfs().get_uri(&file_id)
    }

    pub fn add_main_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_main_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Main);
    }

    pub fn add_library_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_library_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Library);
    }

    pub fn update_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                // Text unchanged — if the index is already built (has module info),
                // skip the costly remove+re-add cycle. This avoids unnecessary
                // reindexing when VS Code opens already-loaded files for
                // peek/definition (e.g. annotation/library files).
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_some()
                {
                    return Some(file_id);
                }

                // Index was cleared — fall through to rebuild it.
                self.compilation.remove_index(vec![file_id]);
                self.compilation.update_index(vec![file_id]);
                return Some(file_id);
            }
        } else if text.is_none() {
            return None;
        }

        let is_removed = text.is_none();
        let removed_file_ids = existing_file_id
            .filter(|_| is_removed)
            .into_iter()
            .collect::<HashSet<_>>();
        let mut existing_reindex_file_ids =
            existing_file_id.map(|file_id| self.expand_reindex_file_ids(vec![file_id]));
        if let Some(reindex_file_ids) = &mut existing_reindex_file_ids {
            self.add_vgui_forwarding_removal_seed(&removed_file_ids, reindex_file_ids);
        }
        let old_guard_fact_file_ids = existing_reindex_file_ids
            .iter()
            .flatten()
            .copied()
            .collect::<HashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&old_guard_fact_file_ids);

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, text);
        let incremental_source_file_ids = HashSet::from([file_id]);

        let reindex_file_ids = existing_reindex_file_ids
            .unwrap_or_else(|| self.expand_reindex_file_ids(vec![file_id]));
        self.compilation.remove_index(reindex_file_ids.clone());

        let update_file_ids = reindex_file_ids
            .iter()
            .copied()
            .filter(|id| !is_removed || *id != file_id)
            .collect::<Vec<_>>();
        if !update_file_ids.is_empty() {
            self.compilation.update_index(update_file_ids.clone());
            self.stabilize_cross_file_type_caches(&update_file_ids);
        }
        self.compilation
            .get_db_mut()
            .get_call_site_param_index_mut()
            .refresh_file_source_dependencies(file_id);
        let guard_fact_file_ids = reindex_file_ids.iter().copied().collect::<HashSet<_>>();
        self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &old_guard_facts,
            &reindex_file_ids,
            &incremental_source_file_ids,
        );

        Some(file_id)
    }

    pub fn update_file_preparsed(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
        trigger_reindex: bool,
    ) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(incoming_version), Some(current_version)) = (
                version,
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_version(&file_id),
            ) && incoming_version < current_version
            {
                return None;
            }

            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_some()
                {
                    self.compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .update_file_version(&file_id, version);
                    return Some(file_id);
                }

                if trigger_reindex {
                    self.compilation.remove_index(vec![file_id]);
                    self.compilation.update_index(vec![file_id]);
                }

                self.compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, version);
                return Some(file_id);
            }
        } else if text.is_none() {
            return None;
        }

        let is_removed = text.is_none();
        let (existing_reindex_file_ids, old_guard_facts) = if trigger_reindex {
            let removed_file_ids = existing_file_id
                .filter(|_| is_removed)
                .into_iter()
                .collect::<HashSet<_>>();
            let mut reindex_file_ids =
                existing_file_id.map(|file_id| self.expand_reindex_file_ids(vec![file_id]));
            if let Some(reindex_file_ids) = &mut reindex_file_ids {
                self.add_vgui_forwarding_removal_seed(&removed_file_ids, reindex_file_ids);
            }
            let old_guard_fact_file_ids = reindex_file_ids
                .iter()
                .flatten()
                .copied()
                .collect::<HashSet<_>>();
            (
                reindex_file_ids,
                self.inferred_guard_snapshot(&old_guard_fact_file_ids),
            )
        } else {
            (None, InferredGuardSnapshot::default())
        };

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed(&uri, text, tree, line_index, version)?;
        let incremental_source_file_ids = HashSet::from([file_id]);

        if trigger_reindex {
            let reindex_file_ids = existing_reindex_file_ids
                .unwrap_or_else(|| self.expand_reindex_file_ids(vec![file_id]));
            self.compilation.remove_index(reindex_file_ids.clone());

            let update_file_ids = reindex_file_ids
                .iter()
                .copied()
                .filter(|id| !is_removed || *id != file_id)
                .collect::<Vec<_>>();
            if !update_file_ids.is_empty() {
                self.compilation.update_index(update_file_ids);
            }
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(file_id);
            self.reindex_changed_inferred_guard_references(
                &reindex_file_ids.iter().copied().collect(),
                &old_guard_facts,
                &reindex_file_ids,
                &incremental_source_file_ids,
            );
        }

        Some(file_id)
    }

    pub fn update_file_preparsed_deferred(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<(FileId, DeferredVfsDrop)> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(incoming_version), Some(current_version)) = (
                version,
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_version(&file_id),
            ) && incoming_version < current_version
            {
                return None;
            }

            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                self.compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, version);
                return Some((file_id, DeferredVfsDrop::default()));
            }
        } else if text.is_none() {
            return None;
        }

        self.compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed_deferred(&uri, text, tree, line_index, version)
    }

    /// VFS-only update: parse and store the new text without touching the index.
    /// The index remains stale but functional until `reindex_files` is called.
    /// This is much faster than `update_file_by_uri`
    pub fn update_file_text_only(&mut self, uri: &Uri, text: String) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing_file_id {
            if let Some(old_text) = self
                .compilation
                .get_db()
                .get_vfs()
                .get_file_content(&file_id)
                .map(String::as_str)
            {
                if old_text == text.as_str() {
                    return Some(file_id);
                }
            }
        }

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, Some(text));

        Some(file_id)
    }

    /// Reindex specific files: remove old index entries + run full analysis pipeline.
    /// Call this after `update_file_text_only` once the user has paused typing.
    pub fn reindex_files(&mut self, file_ids: Vec<FileId>) {
        let incremental_source_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        let removed_file_ids = file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_none()
            })
            .collect::<HashSet<_>>();
        let mut file_ids = self.expand_reindex_file_ids(file_ids);
        self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut file_ids);
        let guard_fact_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&guard_fact_file_ids);
        self.compilation.remove_index(file_ids.clone());
        self.compilation.update_index(file_ids.clone());
        self.stabilize_cross_file_type_caches(&file_ids);
        for file_id in &incremental_source_file_ids {
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(*file_id);
        }
        self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &old_guard_facts,
            &file_ids,
            &incremental_source_file_ids,
        );
    }

    fn expand_reindex_file_ids(&self, file_ids: Vec<FileId>) -> Vec<FileId> {
        let mut expanded = file_ids.into_iter().collect::<HashSet<_>>();
        loop {
            // Include/require callers must be rebuilt with their changed target.
            // Traverse the indexed dependency graph; never rescan workspace ASTs.
            let dependency_dependents = self
                .compilation
                .get_db()
                .get_file_dependencies_index()
                .get_file_dependencies()
                .collect_file_dependents(expanded.iter().copied().collect());
            let unresolved_path_dependents = self.unresolved_path_dependency_dependents(&expanded);
            let dependent_files = self
                .compilation
                .get_db()
                .get_type_index()
                .files_with_type_caches_referencing_files(&expanded);
            let inference_dependents = self
                .compilation
                .get_db()
                .get_type_index()
                .files_depending_on_inference_support(&expanded);
            let callback_dependents = self
                .compilation
                .get_db()
                .get_call_site_param_index()
                .collect_source_dependents(&expanded);
            let callback_source_paths = expanded
                .iter()
                .filter_map(|file_id| self.compilation.get_db().get_vfs().get_file_path(file_id))
                .collect::<Vec<_>>();
            let callback_path_dependents = self
                .compilation
                .get_db()
                .get_call_site_param_index()
                .collect_source_path_dependents(callback_source_paths);
            let mut added = false;
            for file_id in dependency_dependents
                .into_iter()
                .chain(unresolved_path_dependents)
                .chain(dependent_files)
                .chain(inference_dependents)
                .chain(callback_dependents)
                .chain(callback_path_dependents)
            {
                added |= expanded.insert(file_id);
            }

            if !added {
                break;
            }
        }

        let mut expanded = expanded.into_iter().collect::<Vec<_>>();
        expanded.sort_unstable();
        expanded
    }

    fn add_vgui_forwarding_removal_seed(
        &self,
        removed_file_ids: &HashSet<FileId>,
        reindex_file_ids: &mut Vec<FileId>,
    ) {
        if removed_file_ids.is_empty() {
            return;
        }
        let db = self.compilation.get_db();
        let vfs = db.get_vfs();
        let module_index = db.get_module_index();
        let gmod_index = db.get_gmod_class_metadata_index();
        let affected_workspace_id = reindex_file_ids
            .iter()
            .filter(|file_id| removed_file_ids.contains(file_id))
            .find_map(|file_id| {
                gmod_index
                    .has_annotated_vgui_parent_calls(*file_id)
                    .then(|| module_index.get_workspace_id(*file_id))
                    .flatten()
            });
        let Some(affected_workspace_id) = affected_workspace_id else {
            return;
        };
        if reindex_file_ids.iter().any(|file_id| {
            !removed_file_ids.contains(file_id) && vfs.get_syntax_tree(file_id).is_some()
        }) {
            return;
        }

        let all_file_ids = vfs.get_all_file_ids();
        let seed_file_id = all_file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                !removed_file_ids.contains(file_id) && vfs.get_syntax_tree(file_id).is_some()
            })
            .find(|file_id| module_index.get_workspace_id(*file_id) == Some(affected_workspace_id))
            .or_else(|| {
                all_file_ids.iter().copied().find(|file_id| {
                    !removed_file_ids.contains(file_id) && vfs.get_syntax_tree(file_id).is_some()
                })
            });
        let Some(seed_file_id) = seed_file_id else {
            return;
        };
        reindex_file_ids.push(seed_file_id);
        reindex_file_ids.sort_unstable();
        reindex_file_ids.dedup();
    }

    fn unresolved_path_dependency_dependents(&self, file_ids: &HashSet<FileId>) -> Vec<FileId> {
        let db = self.compilation.get_db();
        let target_path_keys = file_ids
            .iter()
            .filter_map(|file_id| db.get_vfs().get_file_path(file_id).cloned())
            .flat_map(|target_path| dependency_path_keys_for_target(db, &target_path))
            .collect::<HashSet<_>>();

        db.get_file_dependencies_index()
            .collect_unresolved_path_dependents(target_path_keys)
    }

    fn reindex_changed_inferred_guard_references(
        &mut self,
        source_file_ids: &HashSet<FileId>,
        old_snapshot: &InferredGuardSnapshot,
        already_reindexed: &[FileId],
        incremental_source_file_ids: &HashSet<FileId>,
    ) {
        #[cfg(test)]
        let initial_stabilization_invocations = self.cross_file_stabilization_invocations;
        let profile_enabled = std::env::var_os("GLUALS_PROFILE").is_some();
        let mut profile_changed_facts = 0usize;
        let mut profile_reference_edges = 0usize;
        let mut profile_waves = 0usize;
        let mut profile_reindexed_files = 0usize;
        let mut propagation_reindexed_files = source_file_ids
            .iter()
            .copied()
            .chain(already_reindexed.iter().copied())
            .collect::<HashSet<_>>();
        let mut new_facts = self
            .compilation
            .get_db()
            .get_signature_index()
            .inferred_guard_facts_for_files(source_file_ids);
        let equivalent_owners = self.reconcile_equivalent_inferred_guard_owners(
            old_snapshot,
            &new_facts,
            &propagation_reindexed_files,
        );
        let old_facts = &old_snapshot.facts;
        let mut changed_owners = old_facts
            .keys()
            .chain(new_facts.keys())
            .filter(|owner| {
                !equivalent_owners.contains(*owner)
                    && old_facts.get(*owner) != new_facts.get(*owner)
            })
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if changed_owners.is_empty() {
            #[cfg(test)]
            {
                self.inferred_guard_propagation_stats = InferredGuardPropagationStats::default();
            }
            if profile_enabled {
                eprintln!(
                    "[profile] inferred_guard_incremental changed_facts=0 reference_edges=0 waves=0 reindexed_files=0"
                );
            }
            return;
        }
        profile_changed_facts += changed_owners.len();
        sort_inferred_guard_owners(&mut changed_owners);
        let mut frontier_old_facts = old_snapshot.facts.clone();
        let mut frontier_old_consumers = old_snapshot.consumers.clone();

        while !changed_owners.is_empty() {
            let mut reference_files = HashSet::new();
            for owner in &changed_owners {
                let newly_added =
                    !frontier_old_facts.contains_key(owner) && new_facts.contains_key(owner);
                let old_consumers = frontier_old_consumers
                    .get(owner)
                    .into_iter()
                    .flatten()
                    .copied();
                let current_consumers = self
                    .compilation
                    .get_db()
                    .get_signature_index()
                    .inferred_guard_consumers(owner);
                for file_id in old_consumers.chain(current_consumers) {
                    if !propagation_reindexed_files.contains(&file_id) {
                        profile_reference_edges += 1;
                        reference_files.insert(file_id);
                    }
                }
                if newly_added {
                    let allow_alias_retry =
                        incremental_source_file_ids.contains(&owner.source_file_id());
                    let discovered = self.resolve_inferred_guard_reference_files(owner, true);
                    for file_id in discovered.files {
                        // Cold batches resolve aliases in the main pipeline. Only edits need a
                        // post-publication retry for alias calls analyzed with the old guard fact.
                        let alias_retry = allow_alias_retry
                            && discovered.alias_calls.contains(&file_id)
                            && file_id != owner.source_file_id();
                        if !propagation_reindexed_files.contains(&file_id) || alias_retry {
                            profile_reference_edges += 1;
                            reference_files.insert(file_id);
                        }
                    }
                }
            }
            if reference_files.is_empty() {
                break;
            }

            let mut reindex_file_ids = reference_files.into_iter().collect::<Vec<_>>();
            reindex_file_ids.sort_unstable();
            let wave_file_ids = reindex_file_ids.iter().copied().collect::<HashSet<_>>();
            let old_wave_snapshot = self.inferred_guard_snapshot(&wave_file_ids);
            self.compilation.remove_index(reindex_file_ids.clone());
            let update_file_ids = reindex_file_ids
                .into_iter()
                .filter(|file_id| {
                    self.compilation
                        .get_db()
                        .get_vfs()
                        .get_syntax_tree(file_id)
                        .is_some()
                })
                .collect::<Vec<_>>();
            if update_file_ids.is_empty() {
                break;
            }
            profile_waves += 1;
            profile_reindexed_files += update_file_ids.len();
            propagation_reindexed_files.extend(wave_file_ids.iter().copied());
            self.compilation.update_index(update_file_ids.clone());

            new_facts = self
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_guard_facts_for_files(&wave_file_ids);
            let equivalent_owners = self.reconcile_equivalent_inferred_guard_owners(
                &old_wave_snapshot,
                &new_facts,
                &propagation_reindexed_files,
            );
            changed_owners = old_wave_snapshot
                .facts
                .keys()
                .chain(new_facts.keys())
                .filter(|owner| {
                    !equivalent_owners.contains(*owner)
                        && old_wave_snapshot.facts.get(*owner) != new_facts.get(*owner)
                })
                .cloned()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            frontier_old_facts = old_wave_snapshot.facts;
            frontier_old_consumers = old_wave_snapshot.consumers;
            profile_changed_facts += changed_owners.len();
            sort_inferred_guard_owners(&mut changed_owners);
        }
        if profile_enabled {
            eprintln!(
                "[profile] inferred_guard_incremental changed_facts={} reference_edges={} waves={} reindexed_files={}",
                profile_changed_facts,
                profile_reference_edges,
                profile_waves,
                profile_reindexed_files
            );
        }
        #[cfg(test)]
        {
            self.inferred_guard_propagation_stats = InferredGuardPropagationStats {
                changed_facts: profile_changed_facts,
                reference_edges: profile_reference_edges,
                frontiers: profile_waves,
                reindexed_files: profile_reindexed_files,
                broad_stabilizations: self
                    .cross_file_stabilization_invocations
                    .saturating_sub(initial_stabilization_invocations),
            };
        }
    }

    fn inferred_guard_snapshot(&self, file_ids: &HashSet<FileId>) -> InferredGuardSnapshot {
        let signature_index = self.compilation.get_db().get_signature_index();
        let facts = signature_index.inferred_guard_facts_for_files(file_ids);
        let consumers = facts
            .keys()
            .map(|owner| {
                (
                    owner.clone(),
                    signature_index.inferred_guard_consumers(owner).collect(),
                )
            })
            .collect();
        InferredGuardSnapshot { facts, consumers }
    }

    fn reconcile_equivalent_inferred_guard_owners(
        &mut self,
        old_snapshot: &InferredGuardSnapshot,
        new_facts: &HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
        reindexed_file_ids: &HashSet<FileId>,
    ) -> HashSet<LuaInferredGuardOwner> {
        let mut reconciled = HashSet::new();
        for owner in old_snapshot
            .facts
            .keys()
            .filter(|owner| old_snapshot.facts.get(*owner) == new_facts.get(*owner))
        {
            if let Some(consumers) = old_snapshot.consumers.get(owner) {
                self.compilation
                    .get_db_mut()
                    .get_signature_index_mut()
                    .migrate_inferred_guard_consumers(owner.clone(), consumers, reindexed_file_ids);
            }
            reconciled.insert(owner.clone());
        }

        let mut old_owners = old_snapshot
            .facts
            .keys()
            .filter(|owner| !new_facts.contains_key(*owner))
            .cloned()
            .collect::<Vec<_>>();
        let mut new_owners = new_facts
            .keys()
            .filter(|owner| !old_snapshot.facts.contains_key(*owner))
            .cloned()
            .collect::<Vec<_>>();
        sort_inferred_guard_owners(&mut old_owners);
        sort_inferred_guard_owners(&mut new_owners);

        for old_owner in old_owners {
            let Some(new_idx) = new_owners.iter().position(|new_owner| {
                old_owner.source_file_id() == new_owner.source_file_id()
                    && old_owner.path() == new_owner.path()
                    && old_owner.state_mask() == new_owner.state_mask()
                    && old_snapshot.facts.get(&old_owner) == new_facts.get(new_owner)
            }) else {
                continue;
            };
            let new_owner = new_owners.remove(new_idx);
            if let Some(consumers) = old_snapshot.consumers.get(&old_owner) {
                self.compilation
                    .get_db_mut()
                    .get_signature_index_mut()
                    .migrate_inferred_guard_consumers(
                        new_owner.clone(),
                        consumers,
                        reindexed_file_ids,
                    );
            }
            reconciled.insert(old_owner);
            reconciled.insert(new_owner);
        }
        reconciled
    }

    fn resolve_inferred_guard_reference_files(
        &self,
        owner: &LuaInferredGuardOwner,
        discover_aliases: bool,
    ) -> InferredGuardReferenceFiles {
        let Some(member_name) = owner.path().last() else {
            return InferredGuardReferenceFiles::default();
        };
        let references = if owner.path().len() == 1 {
            self.compilation
                .get_db()
                .get_reference_index()
                .get_global_references(member_name)
        } else {
            self.compilation
                .get_db()
                .get_reference_index()
                .get_index_references(&LuaMemberKey::Name(member_name.clone()))
        };
        let Some(references) = references else {
            return InferredGuardReferenceFiles::default();
        };

        let db = self.compilation.get_db();
        let mut caches = HashMap::<FileId, LuaInferCache>::new();
        let mut matching_references = references
            .into_iter()
            .filter_map(|reference| {
                let root = db
                    .get_vfs()
                    .get_syntax_tree(&reference.file_id)?
                    .get_red_root();
                let expr = LuaExpr::cast(reference.value.to_node_from_root(&root)?)?;
                (global_path_for_expr(&expr).as_deref() == Some(owner.path())
                    && db.get_gmod_infer_index().are_offsets_compatible(
                        &reference.file_id,
                        expr.get_range().start(),
                        &owner.source_file_id(),
                        owner.signature_id().get_position(),
                    ))
                .then_some((reference.file_id, expr))
            })
            .collect::<Vec<_>>();
        matching_references.sort_by_key(|(file_id, expr)| (*file_id, expr.get_range().start()));

        let mut result = InferredGuardReferenceFiles::default();
        let mut alias_queue = VecDeque::new();
        let mut visited_aliases = HashSet::new();
        for (file_id, expr) in matching_references {
            if call_resolves_to_inferred_guard_owner(db, &mut caches, owner, file_id, &expr) {
                result.files.insert(file_id);
            }
            if discover_aliases
                && expr_resolves_to_inferred_guard_owner(db, &mut caches, owner, file_id, &expr)
                && let Some(decl_id) = immutable_local_alias_decl(db, file_id, &expr)
            {
                alias_queue.push_back(decl_id);
            }
        }

        while let Some(decl_id) = alias_queue.pop_front() {
            if !visited_aliases.insert(decl_id) {
                continue;
            }
            let Some(root) = db
                .get_vfs()
                .get_syntax_tree(&decl_id.file_id)
                .map(|tree| tree.get_red_root())
            else {
                continue;
            };
            let Some(decl_references) = db
                .get_reference_index()
                .get_decl_references(&decl_id.file_id, &decl_id)
            else {
                continue;
            };
            let mut cells = decl_references.cells.clone();
            cells.sort_by_key(|cell| cell.range.start());
            for cell in cells {
                if cell.is_write {
                    continue;
                }
                let Some(name_expr) = root
                    .covering_element(cell.range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
                    .filter(|name_expr| name_expr.get_range() == cell.range)
                else {
                    continue;
                };
                let expr = LuaExpr::NameExpr(name_expr);
                if !db.get_gmod_infer_index().are_offsets_compatible(
                    &decl_id.file_id,
                    expr.get_range().start(),
                    &owner.source_file_id(),
                    owner.signature_id().get_position(),
                ) {
                    continue;
                }
                if is_call_prefix(&expr) {
                    result.files.insert(decl_id.file_id);
                    result.alias_calls.insert(decl_id.file_id);
                }
                if let Some(next_decl_id) = immutable_local_alias_decl(db, decl_id.file_id, &expr) {
                    alias_queue.push_back(next_decl_id);
                }
            }
        }

        result
    }

    fn stabilize_cross_file_type_caches(&mut self, file_ids: &[FileId]) {
        #[cfg(test)]
        {
            self.cross_file_stabilization_invocations += 1;
        }
        if file_ids.is_empty() {
            return;
        }

        let changed = file_ids.iter().copied().collect::<HashSet<_>>();
        let all_dependents = self
            .compilation
            .get_db()
            .get_type_index()
            .files_with_cross_file_type_caches_referencing_files(&changed);
        let dependents = select_cross_file_stabilization_dependents(all_dependents, &changed)
            .into_iter()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_some()
            })
            .collect::<Vec<_>>();
        if dependents.is_empty() {
            return;
        }

        self.compilation.remove_index(dependents.clone());
        self.compilation.update_index(dependents);
    }

    pub fn update_remote_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> FileId {
        let is_removed = text.is_none();
        let fid = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_remote_file_content(uri, text);

        let removed_file_ids = is_removed
            .then_some(fid)
            .into_iter()
            .collect::<HashSet<_>>();
        let mut reindex_file_ids = vec![fid];
        self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);
        self.compilation.remove_index(reindex_file_ids.clone());
        let update_file_ids = reindex_file_ids
            .into_iter()
            .filter(|file_id| !removed_file_ids.contains(file_id))
            .collect::<Vec<_>>();
        if !update_file_ids.is_empty() {
            self.compilation.update_index(update_file_ids);
        }
        fid
    }

    pub fn update_file_by_path(&mut self, path: &PathBuf, text: Option<String>) -> Option<FileId> {
        let uri = file_path_to_uri(path)?;
        self.update_file_by_uri(&uri, text)
    }

    pub fn update_files_by_uri(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let mut removed_files = HashSet::new();
        let mut updated_files = HashSet::new();
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| {
            uri_to_file_path(uri)
                .map(|path| crate::vfs::normalize_path_for_ordering(&path.to_string_lossy()))
                .unwrap_or_else(|| uri.as_str().to_string())
        });
        let old_source_file_ids = files
            .iter()
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let removed_source_file_ids = files
            .iter()
            .filter(|(_, text)| text.is_none())
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let mut old_guard_fact_file_ids =
            self.expand_reindex_file_ids(old_source_file_ids.iter().copied().collect());
        self.add_vgui_forwarding_removal_seed(
            &removed_source_file_ids,
            &mut old_guard_fact_file_ids,
        );
        let old_guard_fact_file_ids = old_guard_fact_file_ids.into_iter().collect::<HashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&old_guard_fact_file_ids);

        // Separate files into: unchanged (skip), to-remove, and to-parse
        let mut to_parse: Vec<(Uri, String)> = Vec::new();
        {
            let _p = Profile::new("update files: classify");
            for (uri, text) in files {
                let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
                if let Some(file_id) = existing_file_id {
                    if let (Some(new_text), Some(old_text)) = (
                        text.as_deref(),
                        self.compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(&file_id)
                            .map(String::as_str),
                    ) && old_text == new_text
                    {
                        removed_files.insert(file_id);
                        updated_files.insert(file_id);
                        continue;
                    }
                } else if text.is_none() {
                    continue;
                }

                if let Some(text) = text {
                    to_parse.push((uri, text));
                } else {
                    // File removal: assign ID and mark for removal
                    let file_id = self
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .set_file_content(&uri, None);
                    removed_files.insert(file_id);
                }
            }
        }

        // Parse files — parallel when enough files to benefit
        const PARALLEL_THRESHOLD: usize = 50;
        {
            let _p = Profile::new("update files: parse");
            if to_parse.len() >= PARALLEL_THRESHOLD {
                // Pre-assign file IDs (sequential, fast)
                let file_ids: Vec<FileId> = to_parse
                    .iter()
                    .map(|(uri, _)| self.compilation.get_db_mut().get_vfs_mut().file_id(uri))
                    .collect();

                // Parse in parallel
                let config = self.emmyrc.clone();
                let n_threads = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
                    .min(16);
                let next_idx = std::sync::atomic::AtomicUsize::new(0);

                // Each slot stores the parsed result
                let parsed: Vec<std::sync::Mutex<Option<(LuaSyntaxTree, LineIndex)>>> = (0
                    ..to_parse.len())
                    .map(|_| std::sync::Mutex::new(None))
                    .collect();

                std::thread::scope(|s| {
                    for _ in 0..n_threads {
                        let next = &next_idx;
                        let files = &to_parse;
                        let results = &parsed;
                        let cfg = &config;
                        s.spawn(move || {
                            let mut node_cache = rowan::NodeCache::default();
                            loop {
                                let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if idx >= files.len() {
                                    break;
                                }
                                let (_, text) = &files[idx];
                                let parse_config = cfg.get_parse_config(&mut node_cache);
                                let tree = LuaParser::parse(text, parse_config);
                                let line_index = LineIndex::parse(text);
                                *results[idx].lock().expect("mutex poisoned") =
                                    Some((tree, line_index));
                            }
                        });
                    }
                });

                // Insert pre-parsed results (sequential, fast HashMap inserts)
                let vfs = self.compilation.get_db_mut().get_vfs_mut();
                for (i, ((_uri, text), file_id)) in
                    to_parse.into_iter().zip(file_ids.iter()).enumerate()
                {
                    let (tree, line_index) = parsed[i]
                        .lock()
                        .expect("mutex poisoned")
                        .take()
                        .expect("parsed result missing");
                    vfs.insert_preparsed(*file_id, text, tree, line_index);
                    removed_files.insert(*file_id);
                    updated_files.insert(*file_id);
                }
            } else {
                // Small batch: parse sequentially (avoids thread spawn overhead)
                for (uri, text) in to_parse {
                    let file_id = self
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .set_file_content(&uri, Some(text));
                    removed_files.insert(file_id);
                    updated_files.insert(file_id);
                }
            }
        }

        if removed_files.is_empty() {
            return Vec::new();
        }

        let mut removed_files = self.expand_reindex_file_ids(removed_files.into_iter().collect());
        self.add_vgui_forwarding_removal_seed(&removed_source_file_ids, &mut removed_files);
        let guard_fact_file_ids = removed_files.iter().copied().collect::<HashSet<_>>();
        self.compilation.remove_index(removed_files.clone());
        updated_files.extend(removed_files.into_iter().filter(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(file_id)
                .is_some()
        }));
        let mut updated_files: Vec<FileId> = updated_files.into_iter().collect();
        updated_files.sort();
        self.compilation.update_index(updated_files.clone());
        self.stabilize_cross_file_type_caches(&updated_files);
        for file_id in &old_source_file_ids {
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(*file_id);
        }
        self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &old_guard_facts,
            &updated_files,
            &old_source_file_ids,
        );
        updated_files
    }

    #[allow(unused)]
    pub(crate) fn update_files_by_uri_sorted(
        &mut self,
        files: Vec<(Uri, Option<String>)>,
    ) -> Vec<FileId> {
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| {
            uri_to_file_path(uri)
                .map(|path| crate::vfs::normalize_path_for_ordering(&path.to_string_lossy()))
                .unwrap_or_else(|| uri.as_str().to_string())
        });
        let old_source_file_ids = files
            .iter()
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let removed_source_file_ids = files
            .iter()
            .filter(|(_, text)| text.is_none())
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let mut old_guard_fact_file_ids =
            self.expand_reindex_file_ids(old_source_file_ids.iter().copied().collect());
        self.add_vgui_forwarding_removal_seed(
            &removed_source_file_ids,
            &mut old_guard_fact_file_ids,
        );
        let old_guard_fact_file_ids = old_guard_fact_file_ids.into_iter().collect::<HashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&old_guard_fact_file_ids);
        let mut removed_files = HashSet::new();
        let mut updated_files = HashSet::new();
        {
            let _p = Profile::new("update files");
            for (uri, text) in files {
                let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
                if let Some(file_id) = existing_file_id {
                    if let (Some(new_text), Some(old_text)) = (
                        text.as_deref(),
                        self.compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(&file_id)
                            .map(String::as_str),
                    ) && old_text == new_text
                    {
                        removed_files.insert(file_id);
                        updated_files.insert(file_id);
                        continue;
                    }
                } else if text.is_none() {
                    continue;
                }

                let is_new_text = text.is_some();
                let file_id = self
                    .compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .set_file_content(&uri, text);
                removed_files.insert(file_id);
                if is_new_text {
                    updated_files.insert(file_id);
                }
            }
        }
        if removed_files.is_empty() {
            return Vec::new();
        }

        let mut removed_files = self.expand_reindex_file_ids(removed_files.into_iter().collect());
        self.add_vgui_forwarding_removal_seed(&removed_source_file_ids, &mut removed_files);
        let guard_fact_file_ids = removed_files.iter().copied().collect::<HashSet<_>>();
        self.compilation.remove_index(removed_files.clone());
        updated_files.extend(removed_files.into_iter().filter(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(file_id)
                .is_some()
        }));
        let mut updated_files: Vec<FileId> = updated_files.into_iter().collect();
        updated_files.sort();
        self.compilation.update_index(updated_files.clone());
        self.stabilize_cross_file_type_caches(&updated_files);
        for file_id in &old_source_file_ids {
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(*file_id);
        }
        self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &old_guard_facts,
            &updated_files,
            &old_source_file_ids,
        );
        updated_files
    }

    pub fn remove_file_by_uri(&mut self, uri: &Uri) -> Option<FileId> {
        if let Some(file_id) = self.compilation.get_db().get_vfs().get_file_id(uri) {
            let mut reindex_file_ids = self.expand_reindex_file_ids(vec![file_id]);
            reindex_file_ids.extend(
                self.compilation
                    .get_db()
                    .get_call_site_param_index()
                    .collect_contribution_signature_files(&HashSet::from([file_id])),
            );
            reindex_file_ids.sort_unstable();
            reindex_file_ids.dedup();
            let removed_file_ids = HashSet::from([file_id]);
            self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);
            let guard_fact_file_ids = reindex_file_ids.iter().copied().collect::<HashSet<_>>();
            let old_guard_facts = self.inferred_guard_snapshot(&guard_fact_file_ids);
            self.compilation
                .get_db_mut()
                .get_vfs_mut()
                .remove_file(uri)?;
            log::info!(
                "remove_file_by_uri: uri={} file_id={:?}",
                uri.as_str(),
                file_id
            );
            self.compilation.remove_index(reindex_file_ids.clone());
            let update_file_ids = reindex_file_ids
                .iter()
                .copied()
                .filter(|id| *id != file_id)
                .collect::<Vec<_>>();
            if !update_file_ids.is_empty() {
                self.compilation.update_index(update_file_ids);
            }
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(file_id);
            self.reindex_changed_inferred_guard_references(
                &guard_fact_file_ids,
                &old_guard_facts,
                &reindex_file_ids,
                &HashSet::new(),
            );
            return Some(file_id);
        }

        None
    }

    pub fn update_files_by_path(&mut self, files: Vec<(PathBuf, Option<String>)>) -> Vec<FileId> {
        let files = files
            .into_iter()
            .filter_map(|(path, text)| {
                let uri = file_path_to_uri(&path)?;
                Some((uri, text))
            })
            .collect();
        self.update_files_by_uri(files)
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        let mut refreshed_config = (*config).clone();
        refreshed_config
            .gmod
            .scripted_class_scopes
            .refresh_resolved_definitions();
        let config = Arc::new(refreshed_config);
        self.emmyrc = config.clone();
        self.compilation.update_config(config.clone());
        self.diagnostic.update_config(config);
    }

    pub fn set_workspace_diagnostic_configs(
        &mut self,
        configs: HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
    ) {
        self.diagnostic.set_workspace_configs(configs);
    }

    pub fn get_workspace_id_for_root(&self, root: &Path) -> Option<WorkspaceId> {
        self.compilation
            .get_db()
            .get_module_index()
            .get_workspace_id_for_root(root)
    }

    pub fn get_emmyrc(&self) -> Arc<Emmyrc> {
        self.emmyrc.clone()
    }

    pub fn diagnose_file(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.diagnostic
            .diagnose_file(&self.compilation, file_id, cancel_token)
    }

    pub fn diagnose_file_with_shared(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: std::sync::Arc<diagnostic::SharedDiagnosticData>,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.diagnostic.diagnose_file_with_shared(
            &self.compilation,
            file_id,
            cancel_token,
            shared_data,
        )
    }

    pub fn precompute_diagnostic_shared_data(
        &self,
    ) -> std::sync::Arc<diagnostic::SharedDiagnosticData> {
        self.diagnostic.precompute_shared_data(&self.compilation)
    }

    /// Return main-workspace files in an order that keeps parallel diagnostic
    /// workers busy. Source size is a cheap proxy for diagnostic cost, so
    /// processing larger files first avoids leaving one expensive file on the
    /// critical path after the other workers have gone idle.
    pub fn get_main_workspace_file_ids_for_diagnostics(&self) -> Vec<FileId> {
        let db = self.compilation.get_db();
        let vfs = db.get_vfs();
        let mut file_ids = db.get_module_index().get_main_workspace_file_ids();
        file_ids.sort_unstable_by(|left, right| {
            let left_len = vfs.get_file_content(left).map_or(0, String::len);
            let right_len = vfs.get_file_content(right).map_or(0, String::len);
            right_len.cmp(&left_len).then_with(|| left.cmp(right))
        });
        file_ids
    }

    pub fn reindex(&mut self) {
        let file_ids = self.compilation.get_db().get_vfs().get_all_file_ids();
        self.compilation.clear_index();
        self.compilation.update_index(file_ids);
    }

    /// 清理文件系统中不再存在的文件
    pub fn cleanup_nonexistent_files(&mut self) {
        let mut files_to_remove = Vec::new();

        // 获取所有当前在VFS中的文件
        let vfs = self.compilation.get_db().get_vfs();
        for file_id in vfs.get_all_local_file_ids() {
            if self
                .compilation
                .get_db()
                .get_module_index()
                .is_std(&file_id)
            {
                continue;
            }
            if let Some(path) = vfs.get_file_path(&file_id).filter(|path| !path.exists())
                && let Some(uri) = file_path_to_uri(path)
            {
                log::info!(
                    "cleanup_nonexistent_files: removing file_id={:?} path={}",
                    file_id,
                    path.display(),
                );
                files_to_remove.push(uri);
            }
        }

        if !files_to_remove.is_empty() {
            log::info!(
                "cleanup_nonexistent_files: removing {} files total",
                files_to_remove.len()
            );
        }

        // 移除不存在的文件
        for uri in files_to_remove {
            self.remove_file_by_uri(&uri);
        }
    }

    pub fn check_schema_update(&self) -> bool {
        self.compilation
            .get_db()
            .get_json_schema_index()
            .has_need_resolve_schemas()
    }

    pub fn get_schemas_to_fetch(&self) -> Vec<Url> {
        self.compilation
            .get_db()
            .get_json_schema_index()
            .get_need_resolve_schemas()
    }

    pub fn apply_fetched_schemas(&mut self, url_contents: HashMap<Url, String>) {
        if url_contents.is_empty() {
            return;
        }

        let converter = SchemaConverter::new(true);
        for (url, json_content) in url_contents {
            // let short_name = get_schema_short_name(&url);
            match converter.convert_from_str(&json_content) {
                Ok(convert_result) => {
                    let uri = match Uri::from_str(url.as_str()) {
                        Ok(uri) => uri,
                        Err(e) => {
                            log::error!("Failed to convert URL to URI {:?}: {}", url, e);
                            continue;
                        }
                    };
                    let file_id =
                        self.update_remote_file_by_uri(&uri, Some(convert_result.annotation_text));
                    if let Some(f) = self
                        .compilation
                        .get_db_mut()
                        .get_json_schema_index_mut()
                        .get_schema_file_mut(&url)
                    {
                        *f = JsonSchemaFile::Resolved(LuaTypeDeclId::local(
                            file_id,
                            &convert_result.root_type_name,
                        ));
                    }
                }
                Err(e) => {
                    log::error!("Failed to convert schema from URL {:?}: {}", url, e);
                }
            }
        }

        self.compilation
            .get_db_mut()
            .get_json_schema_index_mut()
            .reset_rest_schemas();
    }

    pub async fn update_schema(&mut self) {
        let urls = self.get_schemas_to_fetch();
        let url_contents = fetch_schema_urls(urls).await;
        self.apply_fetched_schemas(url_contents);
    }
}

fn select_cross_file_stabilization_dependents(
    all_dependents: impl IntoIterator<Item = FileId>,
    changed: &HashSet<FileId>,
) -> Vec<FileId> {
    let mut dependents = all_dependents
        .into_iter()
        .filter(|file_id| !changed.contains(file_id))
        .collect::<Vec<_>>();
    dependents.sort_unstable();
    dependents.dedup();
    dependents
}

impl Default for EmmyLuaAnalysis {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use glua_parser::LuaSyntaxId;
    use lsp_types::Uri;
    use tokio_util::sync::CancellationToken;

    use crate::{
        EmmyLuaAnalysis, FileId, GmodVguiParentCallMetadata, GmodVguiParentCallOrigin,
        GmodVguiParentSource, LuaDependencyKind, select_cross_file_stabilization_dependents,
    };

    #[test]
    fn reindex_expansion_includes_indexed_file_dependents() {
        let changed = FileId { id: 1 };
        let direct_caller = FileId { id: 2 };
        let transitive_caller = FileId { id: 3 };
        let unrelated = FileId { id: 4 };
        let mut analysis = EmmyLuaAnalysis::new();
        let dependencies = analysis
            .compilation
            .get_db_mut()
            .get_file_dependencies_index_mut();
        dependencies.add_dependency_file(direct_caller, changed, LuaDependencyKind::Include);
        dependencies.add_dependency_file(
            transitive_caller,
            direct_caller,
            LuaDependencyKind::Include,
        );
        dependencies.add_dependency_file(unrelated, unrelated, LuaDependencyKind::Include);

        assert_eq!(
            analysis.expand_reindex_file_ids(vec![changed]),
            vec![changed, direct_caller, transitive_caller]
        );
    }

    #[test]
    fn reindex_expansion_includes_unresolved_path_dependents_for_reopened_file() {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_reopen_dependency_workspace");
        let uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let target_uri = uri("lua/mixins/reopened.lua");
        let caller_uri = uri("lua/autorun/reopen_consumer.lua");
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        let old_target = analysis
            .update_file_by_uri(&target_uri, Some("return {}".to_string()))
            .expect("target should be created");
        let caller = analysis
            .update_file_by_uri(
                &caller_uri,
                Some(r#"local reopened = include("mixins/reopened.lua")"#.to_string()),
            )
            .expect("caller should be created");

        analysis
            .remove_file_by_uri(&target_uri)
            .expect("target should be removed");
        assert_ne!(
            analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_id(&target_uri),
            Some(old_target)
        );

        let reopened_target = analysis
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(&target_uri, Some("return {}".to_string()));

        assert!(
            analysis
                .expand_reindex_file_ids(vec![reopened_target])
                .contains(&caller)
        );
    }

    fn reopen_path_expands_to_caller(
        target_path: &str,
        caller_path: &str,
        dependency_expr: &str,
    ) -> bool {
        let workspace = std::env::temp_dir().join(format!(
            "gmod_glua_ls_reopen_path_variants_{}",
            target_path.replace(['/', '.'], "_")
        ));
        let uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let target_uri = uri(target_path);
        let caller_uri = uri(caller_path);
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        analysis
            .update_file_by_uri(&target_uri, Some("return {}".to_string()))
            .expect("target should be created");
        let caller = analysis
            .update_file_by_uri(
                &caller_uri,
                Some(format!("local reopened = {dependency_expr}")),
            )
            .expect("caller should be created");

        analysis
            .remove_file_by_uri(&target_uri)
            .expect("target should be removed");
        let reopened_target = analysis
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(&target_uri, Some("return {}".to_string()));

        analysis
            .expand_reindex_file_ids(vec![reopened_target])
            .contains(&caller)
    }

    #[test]
    fn reindex_expansion_matches_reopened_dependency_path_variants() {
        for (target_path, caller_path, dependency_expr) in [
            (
                "lua/autorun/mixins/parent.lua",
                "lua/autorun/sub/parent_consumer.lua",
                r#"include("../mixins/parent.lua")"#,
            ),
            (
                "lua/mixins/lua_prefixed.lua",
                "lua/autorun/lua_prefixed_consumer.lua",
                r#"include("lua/mixins/lua_prefixed.lua")"#,
            ),
            (
                "lua/mixins/extensionless.lua",
                "lua/autorun/extensionless_consumer.lua",
                r#"include("mixins/extensionless")"#,
            ),
            (
                "lua/mixins/required.lua",
                "lua/autorun/required_consumer.lua",
                r#"require("mixins.required")"#,
            ),
        ] {
            assert!(
                reopen_path_expands_to_caller(target_path, caller_path, dependency_expr),
                "{dependency_expr} should match {target_path}"
            );
        }
    }

    #[test]
    fn reindex_expansion_path_case_matches_vfs_platform_semantics() {
        let matched = reopen_path_expands_to_caller(
            "lua/mixins/CaseSensitive.lua",
            "lua/autorun/case_consumer.lua",
            r#"include("mixins/casesensitive.lua")"#,
        );
        assert_eq!(matched, cfg!(target_os = "windows"));
    }

    #[test]
    fn stabilization_dependents_exclude_files_already_analyzed_in_batch() {
        let changed_a = FileId { id: 1 };
        let changed_b = FileId { id: 2 };
        let unchanged_dependent = FileId { id: 3 };
        let changed = HashSet::from([changed_a, changed_b]);

        assert_eq!(
            select_cross_file_stabilization_dependents(
                [
                    changed_b,
                    unchanged_dependent,
                    changed_a,
                    unchanged_dependent
                ],
                &changed,
            ),
            vec![unchanged_dependent]
        );
    }

    #[test]
    fn diagnostic_file_ids_are_prioritized_by_source_size_then_file_id() {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_diagnostic_priority_workspace");
        let make_uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let small_a = make_uri("small_a.lua");
        let large = make_uri("large.lua");
        let small_b = make_uri("small_b.lua");

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        analysis.update_files_by_uri(vec![
            (small_b.clone(), Some("return 1".to_string())),
            (
                large.clone(),
                Some("local value = { one = 1, two = 2, three = 3 }\nreturn value".to_string()),
            ),
            (small_a.clone(), Some("return 2".to_string())),
        ]);

        let large_id = analysis
            .get_file_id(&large)
            .expect("large file should exist");
        let mut small_ids = [
            analysis
                .get_file_id(&small_a)
                .expect("small_a file should exist"),
            analysis
                .get_file_id(&small_b)
                .expect("small_b file should exist"),
        ];
        small_ids.sort_unstable();

        assert_eq!(
            analysis.get_main_workspace_file_ids_for_diagnostics(),
            vec![large_id, small_ids[0], small_ids[1]]
        );
    }

    fn test_workspace_and_uri() -> (PathBuf, Uri) {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_analysis_test_workspace");
        let test_file = workspace.join("test.lua");
        let uri = Uri::parse_from_file_path(&test_file).expect("uri should parse");
        (workspace, uri)
    }

    #[test]
    fn unchanged_update_file_by_uri_rebuilds_index() {
        let mut analysis = EmmyLuaAnalysis::new();
        let (workspace, uri) = test_workspace_and_uri();
        analysis.add_main_workspace(workspace);

        let content = "local IsValid = IsValid";
        let file_id = analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .expect("file id should exist");

        analysis.compilation.clear_index();
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_none()
        );

        analysis.update_file_by_uri(&uri, Some(content.to_string()));
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );
    }

    #[test]
    fn unchanged_update_files_by_uri_rebuilds_index() {
        let mut analysis = EmmyLuaAnalysis::new();
        let (workspace, uri) = test_workspace_and_uri();
        analysis.add_main_workspace(workspace);

        let content = "local IsValid = IsValid";
        let file_id = analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .expect("file id should exist");

        analysis.compilation.clear_index();
        let updated = analysis.update_files_by_uri(vec![(uri, Some(content.to_string()))]);
        assert_eq!(updated, vec![file_id]);
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );
    }

    #[test]
    fn vfs_update_files_by_uri_assigns_stable_file_ids_for_new_files() {
        let make_uri = |root: &PathBuf, name: &str| {
            let file = root.join(name);
            Uri::parse_from_file_path(&file).expect("uri should parse")
        };

        let workspace_a = std::env::temp_dir().join("gmod_glua_ls_stable_ids_a");
        let workspace_b = std::env::temp_dir().join("gmod_glua_ls_stable_ids_b");

        let mut analysis_a = EmmyLuaAnalysis::new();
        analysis_a.add_main_workspace(workspace_a.clone());
        let a1 = make_uri(&workspace_a, "a.lua");
        let b1 = make_uri(&workspace_a, "b.lua");
        let ids_a = analysis_a.update_files_by_uri(vec![
            (b1.clone(), Some("return 'b'".to_string())),
            (a1.clone(), Some("return 'a'".to_string())),
        ]);
        assert_eq!(ids_a.len(), 2);
        let a1_id = analysis_a
            .get_file_id(&a1)
            .expect("a.lua should have stable file id");
        let b1_id = analysis_a
            .get_file_id(&b1)
            .expect("b.lua should have stable file id");

        let mut analysis_b = EmmyLuaAnalysis::new();
        analysis_b.add_main_workspace(workspace_b.clone());
        let a2 = make_uri(&workspace_b, "a.lua");
        let b2 = make_uri(&workspace_b, "b.lua");
        let ids_b = analysis_b.update_files_by_uri(vec![
            (a2.clone(), Some("return 'a'".to_string())),
            (b2.clone(), Some("return 'b'".to_string())),
        ]);
        assert_eq!(ids_b.len(), 2);
        let a2_id = analysis_b
            .get_file_id(&a2)
            .expect("a.lua should have stable file id");
        let b2_id = analysis_b
            .get_file_id(&b2)
            .expect("b.lua should have stable file id");

        assert_eq!(
            a1_id, a2_id,
            "a.lua file id should be input-order independent"
        );
        assert_eq!(
            b1_id, b2_id,
            "b.lua file id should be input-order independent"
        );
    }

    #[test]
    fn vgui_forwarding_removal_seed_falls_back_to_another_workspace() {
        let root = std::env::temp_dir().join("gmod_glua_ls_vgui_forwarding_seed_workspace");
        let main_workspace = root.join("main");
        let library_workspace = root.join("library");
        let main_uri = Uri::parse_from_file_path(&main_workspace.join("consumer.lua"))
            .expect("uri should parse");
        let helper_uri = Uri::parse_from_file_path(&library_workspace.join("helper.lua"))
            .expect("uri should parse");

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(main_workspace);
        analysis.add_library_workspace(library_workspace);
        let main_file_id = analysis
            .update_file_by_uri(&main_uri, Some("return true".to_string()))
            .expect("main file should be indexed");
        let helper_file_id = analysis
            .update_file_by_uri(&helper_uri, Some("return true".to_string()))
            .expect("helper file should be indexed");
        let helper_syntax_id = LuaSyntaxId::from_node(
            &analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(&helper_file_id)
                .expect("helper syntax tree should exist")
                .get_red_root(),
        );
        analysis
            .compilation
            .get_db_mut()
            .get_gmod_class_metadata_index_mut()
            .add_vgui_parent_call(
                helper_file_id,
                GmodVguiParentCallMetadata {
                    syntax_id: helper_syntax_id,
                    child: GmodVguiParentSource::Unknown,
                    parent: GmodVguiParentSource::Unknown,
                    relations: Vec::new(),
                    origin: GmodVguiParentCallOrigin::Annotated,
                },
            );

        let removed_file_ids = HashSet::from([helper_file_id]);
        let mut reindex_file_ids = vec![helper_file_id];
        analysis.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);

        assert_eq!(reindex_file_ids, vec![main_file_id, helper_file_id]);
    }

    #[test]
    fn multi_file_batch_reindex_matches_clean_build_diagnostics() {
        let incremental_workspace =
            std::env::temp_dir().join("gmod_glua_ls_batch_reindex_incremental_workspace");
        let clean_workspace =
            std::env::temp_dir().join("gmod_glua_ls_batch_reindex_clean_workspace");
        let uris = |workspace: &PathBuf| {
            ["producer.lua", "consumer.lua", "helper.lua"].map(|name| {
                Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
            })
        };
        let incremental_uris = uris(&incremental_workspace);
        let clean_uris = uris(&clean_workspace);

        let initial_producer = r#"
            Registry = Registry or {}
            Registry.item = { name = "old" }
            Registry.legacy = { value = 1 }
        "#;
        let initial_consumer = r#"
            local name = Registry.item.name
            local legacy = Registry.legacy.value
            consume(name, legacy)
        "#;
        let changed_producer = r#"
            Registry = Registry or {}
            Registry.item = { title = "new" }
        "#;
        let changed_consumer = r#"
            local title = Registry.item.title
            local removed = Registry.legacy.value
            consume(title, removed)
        "#;
        let initial_helper = "function consume(name, value) end";
        let changed_helper = "function consume(title, removed, required) end";

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(incremental_workspace);
        analysis.update_files_by_uri(vec![
            (
                incremental_uris[1].clone(),
                Some(initial_consumer.to_string()),
            ),
            (
                incremental_uris[0].clone(),
                Some(initial_producer.to_string()),
            ),
            (
                incremental_uris[2].clone(),
                Some(initial_helper.to_string()),
            ),
        ]);

        let mut file_ids = incremental_uris
            .iter()
            .map(|uri| analysis.get_file_id(&uri).expect("file should be indexed"))
            .collect::<Vec<FileId>>();
        file_ids.sort_unstable();

        analysis.update_file_text_only(&incremental_uris[0], changed_producer.to_string());
        analysis.update_file_text_only(&incremental_uris[1], changed_consumer.to_string());
        analysis.update_file_text_only(&incremental_uris[2], changed_helper.to_string());
        analysis.reindex_files(vec![file_ids[2], file_ids[0], file_ids[1], file_ids[2]]);

        let mut clean_analysis = EmmyLuaAnalysis::new();
        clean_analysis.add_main_workspace(clean_workspace);
        clean_analysis.update_files_by_uri(vec![
            (clean_uris[2].clone(), Some(changed_helper.to_string())),
            (clean_uris[0].clone(), Some(changed_producer.to_string())),
            (clean_uris[1].clone(), Some(changed_consumer.to_string())),
        ]);

        let snapshot = |analysis: &EmmyLuaAnalysis, file_uris: &[Uri]| {
            let shared = analysis.precompute_diagnostic_shared_data();
            file_uris
                .iter()
                .map(|uri| {
                    let file_id = analysis.get_file_id(uri).expect("file should be indexed");
                    analysis
                        .diagnose_file_with_shared(
                            file_id,
                            CancellationToken::new(),
                            shared.clone(),
                        )
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
        };

        let clean_diagnostics = snapshot(&clean_analysis, &clean_uris);
        assert!(
            clean_diagnostics
                .iter()
                .any(|diagnostics| !diagnostics.is_empty()),
            "fixture should exercise observable diagnostics"
        );

        assert_eq!(snapshot(&analysis, &incremental_uris), clean_diagnostics);
        for (incremental_uri, clean_uri) in incremental_uris.iter().zip(&clean_uris) {
            let incremental_file_id = analysis
                .get_file_id(incremental_uri)
                .expect("incremental file should be indexed");
            let clean_file_id = clean_analysis
                .get_file_id(clean_uri)
                .expect("clean file should be indexed");
            assert!(
                analysis
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(incremental_file_id)
                    .is_some(),
                "batch reindex should restore module ownership for {incremental_file_id:?}"
            );
            assert!(
                clean_analysis
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(clean_file_id)
                    .is_some(),
                "clean build should index module ownership for {clean_file_id:?}"
            );
        }
    }
}
