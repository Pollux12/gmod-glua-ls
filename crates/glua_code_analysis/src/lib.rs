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
mod inferred_guard;
#[cfg(test)]
pub(crate) use inferred_guard::InferredGuardPropagationStats;
pub(crate) use inferred_guard::InferredGuardSnapshot;
mod library_collision;
pub mod profile;
pub mod progress;
mod resources;
mod semantic;
mod test_lib;
mod vfs;

pub use compilation::*;
pub use config::*;
pub use db_index::*;
pub use diagnostic::*;
pub use gamemode_base::detect_gamemode_base_libraries;
pub use glua_codestyle::*;
use glua_parser::{LineIndex, LuaCallExpr, LuaLocalStat, LuaParser, LuaSyntaxTree};
pub use library_collision::LibraryDefinitionCollision;
use lsp_types::Uri;
pub use profile::Profile;
use resources::load_resource_std;
use rustc_hash::{FxHashMap, FxHashSet};
use schema_to_glua::SchemaConverter;
pub use semantic::*;
use smol_str::SmolStr;
use std::collections::VecDeque;
use std::path::{Component, Path};
use std::str::FromStr;
use std::{path::PathBuf, sync::Arc};
pub use test_lib::{GMOD_CALL_ARG_BUILTINS_FIXTURE, VirtualWorkspace};
use tokio_util::sync::CancellationToken;
use url::Url;
pub use vfs::*;

/// Ordering key for a file that has no id yet.
///
/// The batch entry points sort their input before anything is added to the VFS,
/// so they cannot use [`Vfs::file_order_key`]; a URI that does not name a path
/// falls back to its own text.
fn uri_sort_key(uri: &Uri) -> String {
    uri_to_file_path(uri)
        .map(|path| crate::vfs::normalize_path_for_ordering(&path.to_string_lossy()))
        .unwrap_or_else(|| uri.as_str().to_string())
}

/// Stack size for any thread that runs analysis.
///
/// Inference and the type-graph walks recurse to the depth of the source, which
/// a generated or deeply nested GLua file can push past a default thread stack.
/// The binaries spawn their analysis thread with this; the worker pool uses it
/// too, so how deep a file may nest does not depend on which thread picked it
/// up. The reservation is virtual and committed lazily, so a worker that never
/// recurses costs nothing.
pub const ANALYSIS_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Minimum stack bytes that must remain to keep inferring.
///
/// Source-controlled nesting (expressions, doc types) recurses per level and
/// can exhaust small worker stacks on generated files. Exhaustion bails to the
/// existing `RecursiveInfer` path (an `Unknown` fallback), the same graceful
/// failure cyclic inputs already produce, instead of aborting the process.
pub(crate) const ANALYSIS_STACK_RESERVE: usize = 256 * 1024;

/// Fail-closed remaining-stack probe shared by expression and doc inference.
///
/// `None` (unsupported target / OS query failure) counts as exhausted rather
/// than safe, mirroring how `stacker::maybe_grow` treats `None` as
/// insufficient.
pub(crate) fn analysis_stack_exhausted() -> bool {
    match stacker::remaining_stack() {
        Some(remaining) => remaining < ANALYSIS_STACK_RESERVE,
        None => true,
    }
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

pub async fn fetch_schema_urls(urls: Vec<Url>) -> FxHashMap<Url, String> {
    let mut url_contents = FxHashMap::default();
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

    let mut keys = FxHashSet::default();
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

fn select_cross_file_stabilization_dependents(
    all_dependents: impl IntoIterator<Item = FileId>,
    changed: &FxHashSet<FileId>,
) -> Vec<FileId> {
    let mut dependents = all_dependents
        .into_iter()
        .filter(|file_id| !changed.contains(file_id))
        .collect::<Vec<_>>();
    dependents.sort_unstable();
    dependents.dedup();
    dependents
}

/// Pre-edit names for one changed file, so a removed export still contributes
/// the name another file would use to name it.
///
/// Empirically, a removed [`LuaMemberId`]/[`LuaDeclId`] no longer resolves in
/// the post-edit index (`get_member`/`get_decl` return `None` once
/// `remove_index` has run). The snapshot therefore covers every member (owner
/// always, key name when the key is a [`LuaMemberKey::Name`]) and every decl
/// (name + locality): a non-name member such as an integer-indexed slot still
/// names its owner, and [`LuaTypeDeclId`] carries its name inside the
/// [`ExportKey`] itself and needs no snapshot.
struct PreEditNameSnapshot {
    members: FxHashMap<LuaMemberId, (Option<SmolStr>, Option<LuaMemberOwner>)>,
    decls: FxHashMap<LuaDeclId, (SmolStr, bool)>,
}

fn snapshot_pre_edit_names(db: &DbIndex, file_id: FileId) -> PreEditNameSnapshot {
    let mut members = FxHashMap::default();
    let mut decls = FxHashMap::default();
    for member in db.get_member_index().get_file_members(file_id) {
        let name = match member.get_key() {
            LuaMemberKey::Name(name) => Some(name.clone()),
            LuaMemberKey::Integer(_) | LuaMemberKey::None | LuaMemberKey::ExprType(_) => None,
        };
        let owner = db
            .get_member_index()
            .get_member_owner(&member.get_id())
            .cloned();
        members.insert(member.get_id(), (name, owner));
    }
    if let Some(tree) = db.get_decl_index().get_decl_tree(&file_id) {
        for (decl_id, decl) in tree.get_decls() {
            let is_local = decl.is_local();
            decls.insert(*decl_id, (SmolStr::new(decl.get_name()), is_local));
        }
    }
    PreEditNameSnapshot { members, decls }
}

fn insert_global_path_parts(out: &mut FxHashSet<SmolStr>, path: &str) {
    for part in path.split('.') {
        if !part.is_empty() {
            out.insert(SmolStr::new(part));
        }
    }
}

fn insert_type_name(out: &mut FxHashSet<SmolStr>, type_id: &LuaTypeDeclId) {
    out.insert(SmolStr::new(type_id.get_name()));
    let simple = type_id.get_simple_name();
    if simple != type_id.get_name() {
        out.insert(SmolStr::new(simple));
    }
}

fn insert_member_owner_names(out: &mut FxHashSet<SmolStr>, owner: &LuaMemberOwner) {
    match owner {
        LuaMemberOwner::GlobalPath(path) => insert_global_path_parts(out, path.get_name()),
        LuaMemberOwner::Type(type_id) => insert_type_name(out, type_id),
        LuaMemberOwner::Element(_) | LuaMemberOwner::LocalUnresolve => {}
    }
}

fn insert_member_id_names(
    db: &DbIndex,
    out: &mut FxHashSet<SmolStr>,
    member_id: &LuaMemberId,
    pre: Option<&PreEditNameSnapshot>,
) {
    // Emit both sides: a rename reuses the same position, so the post-edit
    // entry exists but names the new key while the pre-edit snapshot still
    // holds the old one. A removal only has the pre side; an addition only
    // the post side. Either side also emits its owner's names, so a non-name
    // member (integer slot, dynamic key) still names the global path or type
    // another file uses to reach it.
    if let Some(member) = db.get_member_index().get_member(member_id) {
        if let LuaMemberKey::Name(name) = member.get_key() {
            out.insert(name.clone());
        }
        if let Some(owner) = db.get_member_index().get_member_owner(member_id) {
            insert_member_owner_names(out, owner);
        }
    }
    // Removed — or renamed away from this key — the post-edit index no longer
    // holds the old name (`get_member` returns `None` after `remove_index`,
    // verified empirically), so the pre-edit snapshot supplies it.
    if let Some((name, owner)) = pre.and_then(|pre| pre.members.get(member_id)) {
        if let Some(name) = name {
            out.insert(name.clone());
        }
        if let Some(owner) = owner {
            insert_member_owner_names(out, owner);
        }
    }
}

fn insert_decl_id_names(
    db: &DbIndex,
    out: &mut FxHashSet<SmolStr>,
    decl_id: &LuaDeclId,
    pre: Option<&PreEditNameSnapshot>,
) {
    if let Some(decl) = db.get_decl_index().get_decl(decl_id) {
        if !decl.is_local() {
            out.insert(SmolStr::new(decl.get_name()));
        }
    }
    if let Some((name, is_local)) = pre.and_then(|pre| pre.decls.get(decl_id)) {
        if !is_local {
            out.insert(name.clone());
        }
    }
}

/// Names of the exported owner a changed signature belongs to.
///
/// A [`LuaSignatureId`] names no Lua name itself; its owner does. The owner
/// lives in the signature's own file (`signature_id.get_file_id()`), which for
/// a [`ExportKey::ContributedParam`] is the callee's file rather than the
/// contributor's. Two legs cover the two shapes that export a function:
/// - a function stat (`function M.Fn() end`, `function G() end`) shares its
///   property with the signature, so the property index maps the member or
///   decl owner back to it;
/// - an assignment (`M.Fn = function() end`) stores the signature in the
///   member's or global's type cache, so scanning the file's type caches for
///   the signature finds it.
///
/// Only exported owners count (non-local decls, members): an anonymous or
/// purely local closure contributes nothing, which is what keeps a local-only
/// signature change from widening diagnostics across the workspace.
fn insert_signature_owner_names(
    db: &DbIndex,
    out: &mut FxHashSet<SmolStr>,
    signature_id: &LuaSignatureId,
    pre: &FxHashMap<FileId, PreEditNameSnapshot>,
) {
    let owner_file = signature_id.get_file_id();
    let file_pre = pre.get(&owner_file);
    let property_index = db.get_property_index();
    for (owner_id, _) in property_index.properties_in_file(owner_file) {
        if property_index.get_signature_owner(owner_id) != Some(*signature_id) {
            continue;
        }
        match owner_id {
            LuaSemanticDeclId::TypeDecl(type_id) => {
                insert_type_name(out, type_id);
            }
            LuaSemanticDeclId::Member(member_id) => {
                insert_member_id_names(db, out, member_id, file_pre);
            }
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                insert_decl_id_names(db, out, decl_id, file_pre);
            }
            LuaSemanticDeclId::Signature(_) => {}
        }
    }
    let Some(owners) = db.get_type_index().file_type_owners(owner_file) else {
        return;
    };
    for owner in owners {
        let exported = match owner {
            LuaTypeOwner::Decl(decl_id) => db
                .get_decl_index()
                .get_decl(decl_id)
                .is_some_and(|decl| !decl.is_local()),
            LuaTypeOwner::Member(_) => true,
            LuaTypeOwner::SyntaxId(_) => false,
        };
        if !exported {
            continue;
        }
        let Some(cache) = db.get_type_index().get_type_cache(owner) else {
            continue;
        };
        let mut found = false;
        TypeVisitTrait::visit_type(cache.as_type(), &mut |inner| {
            if let LuaType::Signature(id) = inner
                && id == signature_id
            {
                found = true;
            }
        });
        if !found {
            continue;
        }
        match owner {
            LuaTypeOwner::Decl(decl_id) => {
                insert_decl_id_names(db, out, decl_id, file_pre);
            }
            LuaTypeOwner::Member(member_id) => {
                insert_member_id_names(db, out, member_id, file_pre);
            }
            LuaTypeOwner::SyntaxId(_) => {}
        }
    }
}

/// Old and new names of non-local declarations the edit renamed without
/// moving any export.
///
/// A same-position rename (`function Old() end` to `function New() end`) keeps
/// every position-derived [`ExportKey`] identical, so the export diff is empty
/// and this check runs independently of it, before the empty-diff early exit.
/// Pre-edit positions are mapped through the edit's [`PositionMap`]: a pre
/// decl whose image holds a post decl under a different name contributes both
/// names; pre decls lost inside the hunk paired with otherwise-unmatched post
/// decls are an in-hunk rename and contribute both sides as well. One-sided
/// leftovers are a pure addition or removal, which the export diff already
/// covers, and are skipped. Diagnostic-only: the names never enter
/// `dirty.files`.
fn decl_rename_refresh_names(
    db: &DbIndex,
    file_id: FileId,
    map: &PositionMap,
    pre: Option<&PreEditNameSnapshot>,
) -> FxHashSet<SmolStr> {
    let mut names = FxHashSet::default();
    let Some(pre) = pre else {
        return names;
    };
    let Some(tree) = db.get_decl_index().get_decl_tree(&file_id) else {
        return names;
    };
    let mut unmatched_post: FxHashMap<rowan::TextSize, SmolStr> = FxHashMap::default();
    for (decl_id, decl) in tree.get_decls() {
        if decl_id.file_id != file_id || decl.is_local() {
            continue;
        }
        unmatched_post.insert(decl_id.position, SmolStr::new(decl.get_name()));
    }
    let mut unmatched_pre: Vec<SmolStr> = Vec::new();
    for (decl_id, (name, is_local)) in &pre.decls {
        if *is_local || decl_id.file_id != file_id {
            continue;
        }
        let Some(mapped) = map.map(decl_id.position) else {
            // Inside the hunk: a rename candidate, or a deletion the diff
            // covers when no post side is left unmatched.
            unmatched_pre.push(name.clone());
            continue;
        };
        // A mapped position with no post decl means the declaration is gone;
        // the export diff reports that as removed keys, so nothing is added.
        if let Some(post_name) = unmatched_post.remove(&mapped)
            && post_name != *name
        {
            names.insert(name.clone());
            names.insert(post_name);
        }
    }
    if !unmatched_pre.is_empty() && !unmatched_post.is_empty() {
        names.extend(unmatched_pre);
        names.extend(unmatched_post.into_values());
    }
    names
}

/// Names from one export diff that another file could reference textually.
///
/// Covers every [`ExportKey`] variant that names a Lua name:
/// - `Member` / `TypeCache(Member)` / `Realm(Member)` / `Property(Member)`:
///   the member key plus its owner's global-path segments or type name, on
///   both the pre and the post side.
/// - `TypeDecl` / `Operator(Type)` / `Property(TypeDecl)`: the type name.
/// - `TypeCache(Decl)` / `Realm(Decl)` / `Property(Decl)`: the decl name.
/// - `Signature` / `ContributedParam` / `Property(Signature)`: the changed
///   signature's actual exported owner, resolved in the signature's own file
///   (for a contributed param that is the callee's file, so the contributor's
///   names stay out). An anonymous signature contributes nothing.
/// - `InferredGuard(GlobalPath)`: its path segments directly; a guard that
///   narrows differently without moving paths names nothing further.
/// - `NetFlow` (a string-literal message name), `Metatable` (a literal range),
///   `Operator(Table)` and `TypeCache(SyntaxId)` name no Lua reference and are
///   skipped; file-level `ModuleExport`/`LoadEdges`/`Namespace`/
///   `FileRealmMetadata` already expand through file dependencies.
fn export_diff_changed_names(
    db: &DbIndex,
    file_id: FileId,
    diff: &ChangedExports,
    pre: &FxHashMap<FileId, PreEditNameSnapshot>,
) -> FxHashSet<SmolStr> {
    let mut names = FxHashSet::default();
    let file_pre = pre.get(&file_id);
    for key in diff.keys() {
        match key {
            ExportKey::Member(member_id) => {
                insert_member_id_names(db, &mut names, member_id, file_pre);
            }
            ExportKey::TypeDecl(type_id) => {
                insert_type_name(&mut names, type_id);
            }
            ExportKey::TypeCache(owner) => match owner {
                LuaTypeOwner::Member(member_id) => {
                    insert_member_id_names(db, &mut names, member_id, file_pre);
                }
                LuaTypeOwner::Decl(decl_id) => {
                    insert_decl_id_names(db, &mut names, decl_id, file_pre);
                }
                LuaTypeOwner::SyntaxId(_) => {}
            },
            ExportKey::Signature(signature_id) => {
                insert_signature_owner_names(db, &mut names, signature_id, pre);
            }
            ExportKey::ContributedParam(signature_id, _) => {
                insert_signature_owner_names(db, &mut names, signature_id, pre);
            }
            ExportKey::InferredGuard(owner) => {
                for part in owner.path() {
                    if !part.is_empty() {
                        names.insert(part.clone());
                    }
                }
            }
            ExportKey::Property(semantic_id) => match semantic_id {
                LuaSemanticDeclId::TypeDecl(type_id) => {
                    insert_type_name(&mut names, type_id);
                }
                LuaSemanticDeclId::Member(member_id) => {
                    insert_member_id_names(db, &mut names, member_id, file_pre);
                }
                LuaSemanticDeclId::LuaDecl(decl_id) => {
                    insert_decl_id_names(db, &mut names, decl_id, file_pre);
                }
                LuaSemanticDeclId::Signature(signature_id) => {
                    insert_signature_owner_names(db, &mut names, signature_id, pre);
                }
            },
            ExportKey::Operator(owner, _) => match owner {
                LuaOperatorOwner::Table(_) => {}
                LuaOperatorOwner::Type(type_id) => {
                    insert_type_name(&mut names, type_id);
                }
            },
            ExportKey::NetFlow(_, _) | ExportKey::Metatable(_) => {}
            ExportKey::Realm(subject) => match subject {
                RealmSubject::Decl(decl_id) => {
                    insert_decl_id_names(db, &mut names, decl_id, file_pre);
                }
                RealmSubject::Member(member_id) => {
                    insert_member_id_names(db, &mut names, member_id, file_pre);
                }
            },
            ExportKey::FileRealmMetadata
            | ExportKey::ModuleExport
            | ExportKey::LoadEdges
            | ExportKey::Namespace => {}
        }
    }
    names
}

fn dependency_path_keys_for_target(db: &DbIndex, target_path: &Path) -> Vec<String> {
    let mut keys = FxHashSet::default();
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

fn insert_dependency_path_key_variants(keys: &mut FxHashSet<String>, path: String) {
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
    /// The text each file's current index entries were built from, stashed by
    /// the first write that does not re-index.
    ///
    /// Absent means the file's VFS content is still the text the index was
    /// built from. The oldest stash is the one that matches the index, so a
    /// burst of writes before an index costs one diff, not one per write.
    pending_indexed_text: rustc_hash::FxHashMap<FileId, Arc<str>>,
}

/// The files an edit left needing re-analysis, and what the ripple has to know
/// about the edit that produced them.
#[derive(Debug, Default)]
pub struct DirtySet {
    files: FxHashSet<FileId>,
    /// The files whose text this burst changed. Drives removal seeds, guard
    /// propagation sources and `refresh_file_source_dependencies`.
    sources: FxHashSet<FileId>,
    removed: FxHashSet<FileId>,
    guard_snapshot: InferredGuardSnapshot,
    /// Whether guard facts or call-site-inferred params could have moved.
    ///
    /// Their consumers are reached through the propagation passes rather than
    /// through the export graph, so a set with no dirty files can still owe
    /// work.
    guard_work: bool,
    /// The edited files whose exports actually moved.
    #[cfg(test)]
    changed_sources: FxHashSet<FileId>,
    /// Names whose textual referencers owe a diagnostic refresh.
    ///
    /// Derived from the batch's export diff in [`EmmyLuaAnalysis::self_index_and_diff`]:
    /// each changed/added/removed [`ExportKey`] contributes the Lua name(s) another
    /// file would use to name it, resolved against the post-edit index with a
    /// pre-edit snapshot as fallback for removed keys (whose index entries are
    /// already gone). The ripple's own export diffs contribute a second sideband
    /// set through `ripple_with_refresh_names`, merged with this one after the
    /// ripple. Neither set drives reindexing: they stay out of `pending` and
    /// `dirty.files` and only widen the watched-file method's returned
    /// diagnostic list. They are deliberately ignored by [`DirtySet::is_empty`] and
    /// [`DirtySet::dirty_len`].
    textual_refresh_names: FxHashSet<SmolStr>,
}

impl DirtySet {
    /// Whether the ripple has nothing to do. The edited files themselves are
    /// already settled by phase 1.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.removed.is_empty() && !self.guard_work
    }

    /// Unions another phase-1 result into this one, keeping the oldest guard
    /// facts: a later batch sees facts an earlier self-index already overwrote.
    pub fn extend(&mut self, other: DirtySet) {
        self.files.extend(other.files);
        self.sources.extend(other.sources);
        self.removed.extend(other.removed);
        self.guard_work |= other.guard_work;
        self.textual_refresh_names
            .extend(other.textual_refresh_names);
        #[cfg(test)]
        self.changed_sources.extend(other.changed_sources);
        self.guard_snapshot.merge(other.guard_snapshot);
    }

    /// How many files the ripple still owes.
    ///
    /// Textual refresh names never count: they owe diagnostics, not reanalysis.
    pub fn dirty_len(&self) -> usize {
        self.files.len()
    }

    #[cfg(test)]
    pub fn files(&self) -> &FxHashSet<FileId> {
        &self.files
    }

    #[cfg(test)]
    pub fn changed_sources(&self) -> &FxHashSet<FileId> {
        &self.changed_sources
    }

    #[cfg(test)]
    pub fn textual_refresh_names(&self) -> &FxHashSet<SmolStr> {
        &self.textual_refresh_names
    }
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
            pending_indexed_text: rustc_hash::FxHashMap::default(),
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

        // An edit whose significant token stream is unchanged — same kinds,
        // same offsets, same texts, comments included — cannot change any
        // derived fact, so re-indexing it (and its whole dependency
        // expansion) would only re-derive facts the index already holds.
        // Store the text and stop.
        if let (Some(file_id), Some(new_text)) = (existing_file_id, text.as_deref())
            && self
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
            && self
                .compilation
                .get_db_mut()
                .get_vfs_mut()
                .content_semantically_matches(file_id, new_text)
        {
            self.compilation
                .get_db_mut()
                .get_vfs_mut()
                .set_file_content(uri, text);
            return Some(file_id);
        }

        // Two-phase incremental edit. Phase 1 re-indexes the edited file and
        // re-homes every reference into it, so what the diff of its exports
        // names is exactly the facts another file could be reading. Phase 2
        // re-analyses those readers, and nothing else: a body-local edit
        // produces no key at all and costs one file's analysis.
        // A file that was not there - never seen, or deleted and now
        // reopened - is not reachable from any dependency edge: a read that
        // failed to resolve it recorded nothing to find it by.
        let was_absent = existing_file_id.is_none_or(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(&file_id)
                .is_none()
        });
        let is_removed = text.is_none();
        if let Some(existing) = existing_file_id {
            // Before the mutation: this is the text the index was built from.
            // For a deletion it is also what says every old offset is gone.
            self.stash_indexed_text(existing);
        }
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, text);

        let mut dirty = profile::phase("edit/self-index", || {
            self.self_index_and_diff(vec![file_id])
        });
        if was_absent || is_removed {
            self.dirty_existence_dependents(&mut dirty, file_id);
        }
        profile::phase("edit/ripple", || self.ripple(dirty));
        profile::phase_report("update_file_by_uri");

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
                    // Through `self_index_files`, so the anchor stash an
                    // earlier text-only write left is consumed and applied.
                    // Re-indexing without it leaves the stash describing a tree
                    // two edits back, and the next edit would then remap from
                    // ranges the index no longer holds.
                    self.self_index_files(vec![file_id]);
                    self.pending_indexed_text.remove(&file_id);
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

        // The text the index was built from has to be read before the VFS
        // mutation drops it. When this call also re-indexes, the diff below
        // consumes it; otherwise it is left for whichever pass does index the
        // file, because until then the index still describes the old text.
        let was_absent = existing_file_id.is_none_or(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(&file_id)
                .is_none()
        });
        let is_removed = text.is_none();
        if let Some(fid) = existing_file_id {
            self.stash_indexed_text(fid);
        }

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed(&uri, text, tree, line_index, version)?;

        if trigger_reindex {
            let mut dirty = self.self_index_and_diff(vec![file_id]);
            if was_absent || is_removed {
                self.dirty_existence_dependents(&mut dirty, file_id);
            }
            self.ripple(dirty);
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

        if let Some(fid) = existing_file_id {
            self.stash_indexed_text(fid);
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
            self.stash_indexed_text(file_id);
        }

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, Some(text));

        Some(file_id)
    }

    /// Settles the index after these files' text changed under
    /// [`update_file_text_only`](Self::update_file_text_only).
    ///
    /// Both phases of an edit: re-index the changed files and re-home the
    /// references into them, then re-analyse whatever read a fact the edit
    /// moved.
    pub fn reindex_files(&mut self, file_ids: Vec<FileId>) {
        let dirty = self.self_index_and_diff(file_ids);
        self.ripple(dirty);
    }

    /// [`reindex_files`](Self::reindex_files) against an expansion that was
    /// computed earlier.
    ///
    /// The expansion has to be derived from the state *before* the edit landed,
    /// so a caller that wants to do anything in between — re-index the edited
    /// file on its own first, say, and release the write lock so a completion
    /// can be answered — has to capture it up front and hand it back here.
    /// Recomputing it against a partly-updated index under-expands badly:
    /// measured on a gamemode workspace, an expansion of 739 files collapsed to
    /// 8 and the workspace ended up with 18 diagnostics that a cold build does
    /// not produce.
    pub fn reindex_expanded_files(&mut self, file_ids: Vec<FileId>, expansion: Vec<FileId>) {
        self.reindex_expanded_files_inner(file_ids, expansion);
    }

    /// Re-analyses `expansion` with `file_ids` as the files that changed.
    ///
    fn reindex_expanded_files_inner(&mut self, file_ids: Vec<FileId>, expansion: Vec<FileId>) {
        let guard_fact_file_ids = expansion.iter().copied().collect::<FxHashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&guard_fact_file_ids);
        self.reindex_expanded_files_with_old_snapshot(file_ids, expansion, old_guard_facts);
    }

    fn reindex_expanded_files_with_old_snapshot(
        &mut self,
        file_ids: Vec<FileId>,
        expansion: Vec<FileId>,
        old_guard_facts: InferredGuardSnapshot,
    ) {
        let incremental_source_file_ids = file_ids.iter().copied().collect::<FxHashSet<_>>();
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
            .collect::<FxHashSet<_>>();

        self.compilation
            .get_db_mut()
            .get_call_site_param_index_mut()
            .forget_removed_files(&removed_file_ids);

        let mut file_ids = expansion;
        self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut file_ids);
        let guard_fact_file_ids = file_ids.iter().copied().collect::<FxHashSet<_>>();
        self.compilation.remove_index(file_ids.clone());
        let update_file_ids = file_ids
            .iter()
            .copied()
            .filter(|file_id| !removed_file_ids.contains(file_id))
            .collect::<Vec<_>>();
        if !update_file_ids.is_empty() {
            self.compilation.update_index(update_file_ids.clone());
            self.stabilize_cross_file_type_caches(&update_file_ids);
        }
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
        self.reindex_changed_inferred_param_consumers(&old_guard_facts, &file_ids);
    }

    /// Records the text the file's current index entries were built from,
    /// unless an earlier write already recorded some.
    ///
    /// Must run before the VFS mutation. The oldest stash is the one that
    /// matches the index: a write that does not re-index leaves the index
    /// describing the text from before it.
    fn stash_indexed_text(&mut self, file_id: FileId) {
        if self.pending_indexed_text.contains_key(&file_id) {
            return;
        }
        let Some(text) = self
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
        else {
            return;
        };
        let text: Arc<str> = Arc::from(text.as_str());
        self.pending_indexed_text.insert(file_id, text);
    }

    /// Where the edit moved the file's offsets: from the stashed indexed text
    /// to the text now in the VFS.
    ///
    /// No stash means nothing has written since the file was indexed, so
    /// nothing moved.
    fn take_position_map(&mut self, file_id: FileId) -> PositionMap {
        let Some(old) = self.pending_indexed_text.remove(&file_id) else {
            return PositionMap::identity();
        };
        match self
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
        {
            Some(new) => PositionMap::new(&old, new),
            None => PositionMap::whole_file(old.len(), 0),
        }
    }

    /// Phase 1 of an edit: re-index the edited files themselves, re-home every
    /// other file's references into them, and report which files the edit
    /// actually invalidated.
    ///
    /// Preconditions: the new text is already in the VFS and
    /// [`stash_indexed_text`](Self::stash_indexed_text) ran before each write.
    ///
    /// The edited files are settled against their own text when this returns,
    /// which is all a request positioned inside one of them needs. The dirty
    /// set is what the caller still owes [`ripple`](Self::ripple).
    pub fn self_index_and_diff(&mut self, file_ids: Vec<FileId>) -> DirtySet {
        let mut dirty = DirtySet {
            sources: file_ids.iter().copied().collect(),
            ..DirtySet::default()
        };
        dirty.removed = file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_none()
            })
            .collect();
        // Before any removal: a self-index overwrites the facts propagation
        // has to diff against.
        dirty.guard_snapshot = self.inferred_guard_snapshot(&dirty.sources);
        dirty.guard_work = !dirty.guard_snapshot.is_empty();

        // Taken before any removal: the seed is chosen from the metadata the
        // removed file still holds, and a live file has to be re-analysed for
        // the forwarding pass to observe the removal at all.
        let mut seeded = dirty.removed.iter().copied().collect::<Vec<_>>();
        seeded.sort_unstable();
        self.add_vgui_forwarding_removal_seed(&dirty.removed, &mut seeded);
        dirty.files.extend(seeded);

        // Every old map first, then one batch re-index, then every new map.
        // The batch matters: analyser phases run over a whole batch, so
        // re-indexing the files one at a time would derive different facts
        // than a cold build does for the same set.
        let mut pre_edit = Vec::with_capacity(file_ids.len());
        // Pre-edit names for textual refresh: a removed export's index entries
        // are gone after `remove_index`, so its old name only survives here.
        let mut pre_names: FxHashMap<FileId, PreEditNameSnapshot> = FxHashMap::default();
        for &file_id in &file_ids {
            pre_names.insert(
                file_id,
                snapshot_pre_edit_names(self.compilation.get_db(), file_id),
            );
            let map = self.take_position_map(file_id);
            let removed = dirty.removed.contains(&file_id);
            // Expressed in the new text's coordinates already, so it compares
            // key for key against the map taken after the re-index.
            let old = export_map(
                self.compilation.get_db(),
                file_id,
                // A deleted file has no new coordinates to express its old
                // exports in, and mapping them all onto nothing would drop
                // every key instead of reporting it removed.
                &if removed {
                    FileRemap::identity(file_id)
                } else {
                    FileRemap::unvalidated(file_id, map)
                },
            );
            pre_edit.push((file_id, map, old));
        }

        self.compilation.remove_index(file_ids.clone());
        let live = file_ids
            .iter()
            .copied()
            .filter(|file_id| !dirty.removed.contains(file_id))
            .collect::<Vec<_>>();
        if !live.is_empty() {
            self.compilation.update_index(live);
        }

        for (file_id, map, old) in pre_edit {
            let removed = dirty.removed.contains(&file_id);
            let remap = FileRemap::validated(self.compilation.get_db(), file_id, map);
            let lost = remap_into_file(self.compilation.get_db_mut(), &remap);
            dirty.files.extend(lost);

            let new = if removed {
                ExportMap::default()
            } else {
                export_map(
                    self.compilation.get_db(),
                    file_id,
                    &FileRemap::identity(file_id),
                )
            };
            let diff = diff_exports(&old, &new);
            // Diagnostic-refresh-only renames, independent of the export diff:
            // a same-position rename moves no key, so without this its names
            // never reach the watched-file refresh. Runs before the early exit
            // and never touches `dirty.files`.
            {
                let renamed = decl_rename_refresh_names(
                    self.compilation.get_db(),
                    file_id,
                    &map,
                    pre_names.get(&file_id),
                );
                dirty.textual_refresh_names.extend(renamed);
            }
            if diff.is_empty() {
                continue;
            }
            #[cfg(test)]
            dirty.changed_sources.insert(file_id);
            // Diagnostic-refresh-only: name the Lua names this diff moved so the
            // watched-file path can refresh their textual referencers without
            // reindexing them. Never touches `dirty.files`.
            {
                let changed = export_diff_changed_names(
                    self.compilation.get_db(),
                    file_id,
                    &diff,
                    &pre_names,
                );
                dirty.textual_refresh_names.extend(changed);
            }
            dirty.guard_work |= diff.keys().any(|key| {
                matches!(
                    key,
                    ExportKey::InferredGuard(_) | ExportKey::ContributedParam(..)
                )
            });
            dirty
                .files
                .extend(dependents_of(self.compilation.get_db(), file_id, &diff));
            let evidence = self.evidence_dependents(file_id);
            dirty.files.extend(evidence);
            // A file whose `include` path this edit changed is reached through
            // the dependency index; one whose path did not resolve before is
            // not in it at all.
            if diff.keys().any(|key| {
                matches!(
                    key,
                    ExportKey::LoadEdges
                        | ExportKey::ModuleExport
                        | ExportKey::Namespace
                        | ExportKey::FileRealmMetadata
                )
            }) {
                let unresolved =
                    self.unresolved_path_dependency_dependents(&FxHashSet::from_iter([file_id]));
                dirty.files.extend(unresolved);
            }
        }

        for file_id in dirty.sources.iter().chain(dirty.removed.iter()) {
            dirty.files.remove(file_id);
        }
        dirty
    }

    /// The files that read this file through evidence rather than through a
    /// cached type.
    ///
    /// A call site contributes a parameter type, a callback source is matched
    /// by path, and an inferred return reads source files. None is a type cache,
    /// so none is reachable from the export diff. These edges are one hop, not
    /// a transitive expansion, and are only paid when the file exported
    /// something new.
    ///
    /// Inference support is deliberately absent: `dependents_of` already
    /// queries it node by node for the nodes the diff names, and the
    /// file-granular rollup would add every reader of every support node in
    /// the file - including the ones this edit left byte for byte alone.
    fn evidence_dependents(&self, file_id: FileId) -> Vec<FileId> {
        let files = FxHashSet::from_iter([file_id]);
        let db = self.compilation.get_db();
        let call_site_index = db.get_call_site_param_index();
        let mut dependents = call_site_index.collect_source_dependents(&files);
        dependents.extend(call_site_index.collect_contributor_files(&files));
        dependents.extend(
            call_site_index.collect_source_path_dependents(db.get_vfs().get_file_path(&file_id)),
        );
        dependents.extend(
            db.get_signature_index()
                .inferred_return_dependents_for_files(&files),
        );
        dependents
    }

    /// Phase 2 of an edit: re-analyse the files the edit invalidated, and
    /// whatever their own facts then invalidate, until nothing changes.
    ///
    /// Each round re-analyses a batch and diffs each file's exports against
    /// the map taken just before it, so a file that re-derives the same facts
    /// adds nothing. A file is re-analysed at most once per ripple.
    ///
    /// Returns every live file whose diagnostics this edit may have moved:
    /// the files re-analysed, propagation included, plus the readers the diff
    /// named that needed no re-index of their own. A caller that publishes
    /// diagnostics needs that set and not just the files whose text changed —
    /// the point of the ripple is the files nobody edited.
    pub fn ripple(&mut self, dirty: DirtySet) -> Vec<FileId> {
        self.ripple_with_refresh_names(dirty).0
    }

    /// [`ripple`](Self::ripple), plus the sideband textual names every ripple
    /// export diff moved.
    ///
    /// Each batch is snapshotted before its re-index (a rippled file's removed
    /// entries are gone afterwards, exactly like an edited file's), and the
    /// names resolve after the ripple under a shared borrow. They never enter
    /// `pending` or `dirty.files` and steer no reindex decision: which files
    /// the ripple settled is decided by the change-driven pass alone. The
    /// late parameter-consumer pass contributes its own sideband below, merged
    /// the same way.
    pub(crate) fn ripple_with_refresh_names(
        &mut self,
        mut dirty: DirtySet,
    ) -> (Vec<FileId>, FxHashSet<SmolStr>) {
        if dirty.is_empty() {
            return (Vec::new(), FxHashSet::default());
        }
        self.compilation
            .get_db_mut()
            .get_call_site_param_index_mut()
            .forget_removed_files(&dirty.removed);

        let mut settled = dirty.sources.clone();
        settled.extend(dirty.removed.iter().copied());
        let mut reanalyzed = dirty.sources.iter().copied().collect::<Vec<_>>();
        reanalyzed.sort_unstable();
        let mut pending = std::mem::take(&mut dirty.files);
        // Every file the diff named, whether or not it ends up re-indexed. A
        // reader can need no new index entries and still diagnose differently,
        // because it resolves the fact that moved when it is diagnosed rather
        // than when it is indexed. Reporting only what was re-analysed leaves
        // exactly those files holding a stale report.
        let mut touched: FxHashSet<FileId> = dirty.sources.iter().copied().collect();
        touched.extend(pending.iter().copied());
        let mut round = 0usize;
        // Sideband textual names from every ripple export diff, resolved after
        // the ripple. Diagnostic-only: never enters `pending` or `touched`
        // below, and steers no reindex decision.
        let mut sideband: FxHashSet<SmolStr> = FxHashSet::default();

        while !pending.is_empty() {
            let mut batch = pending
                .drain()
                .filter(|file_id| !settled.contains(file_id))
                // A file with a stashed text owes its own self-index, which is
                // the only pass that can re-home the references into it.
                // Re-indexing it here would drop that stash and strand them.
                .filter(|file_id| !self.pending_indexed_text.contains_key(file_id))
                .filter(|file_id| {
                    self.compilation
                        .get_db()
                        .get_vfs()
                        .get_syntax_tree(file_id)
                        .is_some()
                })
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            batch.sort_unstable();
            round += 1;
            if round == 8 {
                log::warn!(
                    "ripple has not converged after 8 rounds; {} file(s) still changing",
                    batch.len()
                );
            }

            let before = batch
                .iter()
                .map(|file_id| {
                    (
                        *file_id,
                        export_map(
                            self.compilation.get_db(),
                            *file_id,
                            &FileRemap::identity(*file_id),
                        ),
                    )
                })
                .collect::<FxHashMap<_, _>>();
            // Their pre-batch guard facts join the snapshot for the same
            // reason the edited files' did: the re-index below overwrites them.
            // The pre-batch name snapshots serve the sideband for the same
            // reason: a rippled file's removed entries are gone afterwards.
            let batch_set = batch.iter().copied().collect::<FxHashSet<_>>();
            dirty
                .guard_snapshot
                .merge(self.inferred_guard_snapshot(&batch_set));
            let mut batch_pres: FxHashMap<FileId, PreEditNameSnapshot> = FxHashMap::default();
            for file_id in &batch {
                batch_pres.insert(
                    *file_id,
                    snapshot_pre_edit_names(self.compilation.get_db(), *file_id),
                );
            }

            let profile_enabled = std::env::var_os("GLUALS_PROFILE").is_some();
            if profile_enabled {
                eprintln!("[profile] ripple round {round}: {} file(s)", batch.len());
            }
            self.compilation.remove_index(batch.clone());
            self.compilation.update_index(batch.clone());
            reanalyzed.extend(batch.iter().copied());
            settled.extend(batch.iter().copied());

            for file_id in &batch {
                let after = export_map(
                    self.compilation.get_db(),
                    *file_id,
                    &FileRemap::identity(*file_id),
                );
                let diff = diff_exports(&before[file_id], &after);
                // Sideband before the early exit: a rippled file whose own
                // exports moved names its textual referencers, which hold no
                // cached edge the change-driven pass could follow.
                {
                    let mut changed = export_diff_changed_names(
                        self.compilation.get_db(),
                        *file_id,
                        &diff,
                        &batch_pres,
                    );
                    changed.extend(decl_rename_refresh_names(
                        self.compilation.get_db(),
                        *file_id,
                        &PositionMap::identity(),
                        batch_pres.get(file_id),
                    ));
                    sideband.extend(changed);
                }
                if diff.is_empty() {
                    continue;
                }
                if profile_enabled {
                    let keys = diff
                        .keys()
                        .take(6)
                        .map(|key| {
                            let text = format!("{key:?}");
                            text.chars().take(110).collect::<String>()
                        })
                        .collect::<Vec<_>>();
                    eprintln!(
                        "[profile] ripple round {round}: {:?} changed {} export(s): {keys:?}",
                        self.compilation.get_db().get_vfs().get_file_path(file_id),
                        diff.keys().count()
                    );
                }
                let evidence = self.evidence_dependents(*file_id);
                for dependent in dependents_of(self.compilation.get_db(), *file_id, &diff)
                    .into_iter()
                    .chain(evidence)
                {
                    touched.insert(dependent);
                    if !settled.contains(&dependent) {
                        pending.insert(dependent);
                    }
                }
            }
        }

        for file_id in &dirty.sources {
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(*file_id);
        }
        let guard_fact_file_ids = dirty.guard_snapshot.snapshot_file_ids().clone();
        let guard_reindexed = self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &dirty.guard_snapshot,
            &reanalyzed,
            &dirty.sources,
        );
        // The late callee reindex can move exports of its own (a
        // contributed-parameter change altering a differently named export),
        // and its own ripple can move more. Both name sets bubble here;
        // diagnostic-only, like every other sideband merge.
        let (param_reindexed, param_names) = self
            .reindex_changed_inferred_param_consumers_with_refresh_names(
                &dirty.guard_snapshot,
                &reanalyzed,
            );

        reanalyzed.extend(guard_reindexed);
        reanalyzed.extend(param_reindexed);
        sideband.extend(param_names);
        reanalyzed.extend(touched);
        reanalyzed.retain(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(file_id)
                .is_some()
        });
        reanalyzed.sort_unstable();
        reanalyzed.dedup();
        (reanalyzed, sideband)
    }

    /// Rebuilds only these files' own index entries.
    ///
    /// Nothing cross-file is settled: dependents keep whatever they inferred
    /// before, and the caller still owes them a
    /// [`reindex_expanded_files`](Self::reindex_expanded_files) against an
    /// expansion captured beforehand. What this does buy is that the edited
    /// file's declarations, members and signatures line up with its text again,
    /// which is all a request positioned *inside that file* needs — the index
    /// entries are keyed by position, so an edit that shifts offsets is exactly
    /// what makes them stop matching the tree.
    pub fn self_index_files(&mut self, file_ids: Vec<FileId>) {
        self.compilation.remove_index(file_ids.clone());
        self.compilation.update_index(file_ids);
    }

    fn stabilize_cross_file_type_caches(&mut self, file_ids: &[FileId]) {
        if file_ids.is_empty() {
            return;
        }

        let changed = file_ids.iter().copied().collect::<FxHashSet<_>>();
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

    /// Re-analyses exactly `file_ids`, skipping dependency expansion.
    pub fn reindex_files_without_expansion(&mut self, file_ids: Vec<FileId>) {
        // Re-derived from the current text, so a stashed pre-index text now
        // describes a state that no longer exists.
        for file_id in &file_ids {
            self.pending_indexed_text.remove(file_id);
        }
        self.compilation.remove_index(file_ids.clone());
        self.compilation.update_index(file_ids.clone());
        self.stabilize_cross_file_type_caches(&file_ids);
    }

    pub fn expand_reindex_file_ids(&self, file_ids: Vec<FileId>) -> Vec<FileId> {
        let _p = Profile::new("expand_reindex_file_ids");
        let mut expanded = file_ids.into_iter().collect::<FxHashSet<_>>();
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
            let contributor_dependents = self
                .compilation
                .get_db()
                .get_call_site_param_index()
                .collect_contributor_files(&expanded);
            let callback_source_paths = expanded
                .iter()
                .filter_map(|file_id| self.compilation.get_db().get_vfs().get_file_path(file_id))
                .collect::<Vec<_>>();
            let callback_path_dependents = self
                .compilation
                .get_db()
                .get_call_site_param_index()
                .collect_source_path_dependents(callback_source_paths);
            let inferred_return_dependents = self
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_return_dependents_for_files(&expanded);
            let settled_signature_dependents = self
                .compilation
                .get_db()
                .get_signature_index()
                .settled_read_dependents_for_files(&expanded);
            let mut added = false;
            for file_id in dependency_dependents
                .into_iter()
                .chain(unresolved_path_dependents)
                .chain(dependent_files)
                .chain(inference_dependents)
                .chain(callback_dependents)
                .chain(contributor_dependents)
                .chain(callback_path_dependents)
                .chain(inferred_return_dependents)
                .chain(settled_signature_dependents)
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
        removed_file_ids: &FxHashSet<FileId>,
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

    /// Marks the files that have to be re-analysed because `file_id` appeared or
    /// vanished, rather than because its contents changed.
    ///
    /// Whether a file exists is not a fact that file exports, so the export diff
    /// cannot see it. Every `include` of the path resolves differently now, both
    /// the ones that resolved to it and the ones that failed to resolve at all.
    /// The file itself is dropped from the set: the caller has already indexed
    /// it, or it is gone.
    fn dirty_existence_dependents(&self, dirty: &mut DirtySet, file_id: FileId) {
        let dependents = self
            .compilation
            .get_db()
            .get_file_dependencies_index()
            .get_file_dependencies()
            .collect_file_dependents(vec![file_id]);
        dirty.files.extend(dependents);
        let unresolved =
            self.unresolved_path_dependency_dependents(&FxHashSet::from_iter([file_id]));
        dirty.files.extend(unresolved);
        dirty.files.remove(&file_id);
    }

    fn unresolved_path_dependency_dependents(&self, file_ids: &FxHashSet<FileId>) -> Vec<FileId> {
        let db = self.compilation.get_db();
        let target_path_keys = file_ids
            .iter()
            .filter_map(|file_id| db.get_vfs().get_file_path(file_id).cloned())
            .flat_map(|target_path| dependency_path_keys_for_target(db, &target_path))
            .collect::<FxHashSet<_>>();

        db.get_file_dependencies_index()
            .collect_unresolved_path_dependents(target_path_keys)
    }

    /// Textual diagnostic-refresh candidates for `names`, without reindexing.
    ///
    /// Each name is looked up in the reference index (`files_referencing_name`,
    /// which unions global and member-key references) and the union is filtered
    /// to live diagnostic-eligible workspace files: a file must still have a
    /// syntax tree, must belong to a `Main` module workspace, and must not
    /// already be in `exclude` (the change-driven settle set). Takes `&self`
    /// only, so diagnosing the result mutates nothing and no index snapshot
    /// moves; in particular `file_reference_revision` is untouched.
    ///
    /// Eligibility decision (documented per review): `Main` only. `Library`
    /// (shipped annotations and other library workspaces), `Std` and `Remote`
    /// files are excluded: the language server only pulls/diagnoses main
    /// workspace files (see `get_main_workspace_file_ids_for_diagnostics`),
    /// and refreshing annotations would fan out over every consumer of a hub
    /// name for files nobody diagnoses. The changed file's own workspace does
    /// not matter: a library edit still refreshes its main-workspace
    /// referencers; only candidates are filtered.
    ///
    /// No caps or prefilters: every referencer is reported, however large the
    /// fanout, and per-name fanout is logged without truncation so a hub name
    /// stays visible.
    fn textual_refresh_candidates(
        &self,
        names: &FxHashSet<SmolStr>,
        exclude: &FxHashSet<FileId>,
    ) -> Vec<FileId> {
        if names.is_empty() {
            return Vec::new();
        }
        let db = self.compilation.get_db();
        let reference_index = db.get_reference_index();
        let module_index = db.get_module_index();
        let vfs = db.get_vfs();
        let mut sorted_names: Vec<&SmolStr> = names.iter().collect();
        sorted_names.sort_unstable();
        let mut candidates = FxHashSet::default();
        for name in sorted_names {
            let referencers = reference_index.files_referencing_name(name);
            // No caps: report the full fanout even for hub names.
            log::info!(
                "textual refresh: name `{}` fans out to {} file(s)",
                name.as_str(),
                referencers.len()
            );
            candidates.extend(referencers);
        }
        let mut filtered: Vec<FileId> = candidates
            .into_iter()
            .filter(|file_id| !exclude.contains(file_id))
            .filter(|file_id| vfs.get_syntax_tree(file_id).is_some())
            .filter(|file_id| module_index.is_main(file_id))
            .collect();
        filtered.sort_unstable();
        filtered.dedup();
        if !filtered.is_empty() {
            log::info!(
                "textual refresh: {} additional file(s) from {} name(s)",
                filtered.len(),
                names.len()
            );
        }
        filtered
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
            .collect::<FxHashSet<_>>();
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

    pub fn update_files_by_uri(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let mut removed_files = FxHashSet::default();
        let mut updated_files = FxHashSet::default();
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| uri_sort_key(uri));
        let old_source_file_ids = files
            .iter()
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<FxHashSet<_>>();
        let removed_source_file_ids = files
            .iter()
            .filter(|(_, text)| text.is_none())
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<FxHashSet<_>>();
        let mut old_guard_fact_file_ids =
            self.expand_reindex_file_ids(old_source_file_ids.iter().copied().collect());
        self.add_vgui_forwarding_removal_seed(
            &removed_source_file_ids,
            &mut old_guard_fact_file_ids,
        );
        let old_guard_fact_file_ids = old_guard_fact_file_ids
            .into_iter()
            .collect::<FxHashSet<_>>();
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

                // Parse in parallel on rayon's persistent pool. `collect` on an
                // indexed parallel iterator preserves input order, so `parsed`
                // stays aligned with `to_parse` and `file_ids`.
                use rayon::prelude::*;
                let config = self.emmyrc.clone();
                crate::compilation::analyzer::parallel::init_pool();
                let parsed: Vec<(LuaSyntaxTree, LineIndex)> = to_parse
                    .par_iter()
                    .map_init(rowan::NodeCache::default, |node_cache, (_, text)| {
                        let parse_config = config.get_parse_config(node_cache);
                        let tree = LuaParser::parse(text, parse_config);
                        let line_index = LineIndex::parse(text);
                        (tree, line_index)
                    })
                    .collect();

                // Insert pre-parsed results (sequential, fast FxHashMap inserts)
                let vfs = self.compilation.get_db_mut().get_vfs_mut();
                for (((_uri, text), file_id), (tree, line_index)) in
                    to_parse.into_iter().zip(file_ids.iter()).zip(parsed)
                {
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
        let guard_fact_file_ids = removed_files.iter().copied().collect::<FxHashSet<_>>();
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
        {
            let _p = Profile::new("post: stabilize_cross_file_type_caches");
            self.stabilize_cross_file_type_caches(&updated_files);
        }
        {
            let _p = Profile::new("post: refresh_file_source_dependencies");
            for file_id in &old_source_file_ids {
                self.compilation
                    .get_db_mut()
                    .get_call_site_param_index_mut()
                    .refresh_file_source_dependencies(*file_id);
            }
        }
        {
            let _p = Profile::new("post: reindex_changed_inferred_guard_references");
            self.reindex_changed_inferred_guard_references(
                &guard_fact_file_ids,
                &old_guard_facts,
                &updated_files,
                &old_source_file_ids,
            );
        }
        {
            let _p = Profile::new("post: reindex_changed_inferred_param_consumers");
            self.reindex_changed_inferred_param_consumers(&old_guard_facts, &updated_files);
        }
        updated_files
    }

    #[allow(unused)]
    pub(crate) fn update_files_by_uri_sorted(
        &mut self,
        files: Vec<(Uri, Option<String>)>,
    ) -> Vec<FileId> {
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| uri_sort_key(uri));
        let old_source_file_ids = files
            .iter()
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<FxHashSet<_>>();
        let removed_source_file_ids = files
            .iter()
            .filter(|(_, text)| text.is_none())
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<FxHashSet<_>>();
        let mut old_guard_fact_file_ids =
            self.expand_reindex_file_ids(old_source_file_ids.iter().copied().collect());
        self.add_vgui_forwarding_removal_seed(
            &removed_source_file_ids,
            &mut old_guard_fact_file_ids,
        );
        let old_guard_fact_file_ids = old_guard_fact_file_ids
            .into_iter()
            .collect::<FxHashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&old_guard_fact_file_ids);
        let mut removed_files = FxHashSet::default();
        let mut updated_files = FxHashSet::default();
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
        let guard_fact_file_ids = removed_files.iter().copied().collect::<FxHashSet<_>>();
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
        self.reindex_changed_inferred_param_consumers(&old_guard_facts, &updated_files);
        updated_files
    }

    /// Removes a file the way the filesystem did.
    ///
    /// One deletion is a one-item batch. The whole-file purge the removal used
    /// to do by hand — a `PositionMap::whole_file` remap, so members other files
    /// own on this file's table literals lose every offset — is what
    /// `take_position_map` already returns once the VFS holds no text for the
    /// file, and the batch feeds the owners it reports back into the dirty set
    /// instead of discarding them. The contribution-signature seed the expansion
    /// needed has no counterpart here because a contributed parameter is an
    /// exported fact: the diff against an emptied export map names its readers.
    pub fn remove_file_by_uri(&mut self, uri: &Uri) -> Option<FileId> {
        let file_id = self.compilation.get_db().get_vfs().get_file_id(uri)?;
        log::info!(
            "remove_file_by_uri: uri={} file_id={:?}",
            uri.as_str(),
            file_id
        );
        self.apply_file_system_changes(vec![(uri.clone(), None)]);
        Some(file_id)
    }

    /// Applies a batch of filesystem changes — the creates, changes and deletes
    /// a watched-file notification delivers together — and returns every live
    /// file whose diagnostics the batch may have moved.
    ///
    /// One [`self_index_and_diff`](Self::self_index_and_diff) over the whole
    /// batch and one [`ripple`](Self::ripple), so a branch switch costs the
    /// files the change reached rather than the union of their dependency
    /// expansions. The batch is one call rather than one per file because the
    /// analyser derives its facts over a whole batch; settling the files one at
    /// a time does not land where a cold build of the same set does.
    ///
    /// The returned set is what a caller has to re-diagnose. It is deliberately
    /// not just the files whose text changed: the files the ripple settled are
    /// the ones nobody edited and nobody would otherwise refresh. Past the
    /// change-driven settle, textual referencers of the changed names — from
    /// both the batch and the ripple diffs — are appended as
    /// diagnostic-refresh-only candidates: they are collected via
    /// `files_referencing_name` after the ripple, under a shared borrow, so
    /// they raise no `DirtySet::dirty_len` and move no index snapshot.
    ///
    /// `None` means the file is gone from disk. Unlike
    /// [`update_file_by_uri`](Self::update_file_by_uri), which leaves a
    /// tombstone, the URI is detached once the settle is done so a later file at
    /// the same path is a new [`FileId`]. The detach has to come last: until
    /// then the path is what finds the files whose reads of it have to be
    /// re-derived.
    pub fn apply_file_system_changes(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| uri_sort_key(uri));
        // One URI can arrive several times in a burst; only the last event is
        // the state on disk.
        let mut latest: Vec<(Uri, Option<String>)> = Vec::with_capacity(files.len());
        for (uri, text) in files {
            match latest.last_mut() {
                Some(last) if last.0 == uri => last.1 = text,
                _ => latest.push((uri, text)),
            }
        }

        let mut changed = Vec::new();
        let mut existence_changed = Vec::new();
        let mut removed_uris = Vec::new();
        for (uri, text) in latest {
            let existing = self.compilation.get_db().get_vfs().get_file_id(&uri);
            if let Some(file_id) = existing
                && let (Some(new_text), Some(old_text)) = (
                    text.as_deref(),
                    self.compilation
                        .get_db()
                        .get_vfs()
                        .get_file_content(&file_id)
                        .map(String::as_str),
                )
                && old_text == new_text
            {
                // Same bytes. Already indexed means nothing to do; an index that
                // was cleared still owes its rebuild, and with no stash the
                // remap is the identity, which is what an unchanged file wants.
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_none()
                {
                    changed.push(file_id);
                }
                continue;
            }
            if existing.is_none() && text.is_none() {
                continue;
            }

            let was_absent = existing.is_none_or(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(&file_id)
                    .is_none()
            });
            let is_removed = text.is_none();
            if let Some(file_id) = existing {
                self.stash_indexed_text(file_id);
            }
            let file_id = self
                .compilation
                .get_db_mut()
                .get_vfs_mut()
                .set_file_content(&uri, text);
            if was_absent || is_removed {
                existence_changed.push(file_id);
            }
            if is_removed {
                removed_uris.push(uri);
            }
            changed.push(file_id);
        }

        if changed.is_empty() {
            return Vec::new();
        }
        changed.sort_unstable();
        changed.dedup();

        let mut dirty = self.self_index_and_diff(changed.clone());
        for file_id in existence_changed {
            self.dirty_existence_dependents(&mut dirty, file_id);
        }
        // Cloned before the ripple consumes `dirty`: diagnosing under a shared
        // borrow mutates nothing, so these names must not travel in the dirty
        // set itself.
        let refresh_names = dirty.textual_refresh_names.clone();
        let mut affected = changed;
        let (rippled, ripple_names) = self.ripple_with_refresh_names(dirty);
        affected.extend(rippled);
        // Diagnostic-refresh-only, after the change-driven settle and before
        // the detach: batch and ripple names merged, then resolved to textual
        // candidates via `files_referencing_name`, filtered to live `Main`
        // files, sorted and deduped. No reindex, no `dirty_len`.
        {
            let mut all_names = refresh_names;
            all_names.extend(ripple_names);
            let exclude: FxHashSet<FileId> = affected.iter().copied().collect();
            let textual = self.textual_refresh_candidates(&all_names, &exclude);
            affected.extend(textual);
        }

        for uri in &removed_uris {
            self.compilation.get_db_mut().get_vfs_mut().remove_file(uri);
        }

        affected.retain(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(file_id)
                .is_some()
        });
        affected.sort_unstable();
        affected.dedup();
        affected
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
        configs: std::collections::HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
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

    pub fn apply_fetched_schemas(&mut self, url_contents: FxHashMap<Url, String>) {
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
    fn reindex_expansion_includes_inferred_return_producers() {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_inferred_return_dependencies");
        let producer_uri = Uri::parse_from_file_path(&workspace.join("producer.lua"))
            .expect("producer URI should parse");
        let returner_uri = Uri::parse_from_file_path(&workspace.join("returner.lua"))
            .expect("returner URI should parse");
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        let producer = analysis
            .update_file_by_uri(
                &producer_uri,
                Some("State = {}\nState.value = 1".to_string()),
            )
            .expect("producer should be indexed");
        let returner = analysis
            .update_file_by_uri(
                &returner_uri,
                Some("function GetStateValue() return State.value end".to_string()),
            )
            .expect("returner should be indexed");

        assert!(
            analysis
                .expand_reindex_file_ids(vec![producer])
                .contains(&returner),
            "a producer edit must reindex functions whose inferred return reads it"
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
        let changed = HashSet::from_iter([changed_a, changed_b]);

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
                    resolved_source: None,
                    origin: GmodVguiParentCallOrigin::Annotated,
                },
            );

        let removed_file_ids = HashSet::from_iter([helper_file_id]);
        let mut reindex_file_ids = vec![helper_file_id];
        analysis.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);

        assert_eq!(reindex_file_ids, vec![main_file_id, helper_file_id]);
    }

    /// The language server re-indexes an edited file on its own before running
    /// its dependency ripple, so a completion positioned in that file can be
    /// answered without waiting seconds for the ripple. This checks the split
    /// path reaches the same diagnostics as doing it in one go.
    ///
    /// It does **not** pin the ordering constraint that makes the split safe —
    /// that the expansion is captured *before* the self-index, because a
    /// self-index drops the edited file's declarations and inbound dependency
    /// edges and an expansion taken afterwards under-invalidates. That was
    /// measured on a gamemode workspace (739 files before the self-index, 6
    /// after) and this fixture is far too small to reproduce it: it passes with
    /// the two swapped. The real guard is `tools/determinism` against a real
    /// workspace, and the reasoning lives on `reindex_expanded_files`.
    #[test]
    fn two_phase_reindex_matches_single_phase_diagnostics() {
        // A class definition plus a consumer whose inferred type references it:
        // the expansion reaches the consumer through the type-cache relation,
        // which is the one that collapses once the definition site is dropped.
        let producer_source = |member: &str| {
            format!(
                "---@class Thing
local Thing = {{}}
function Thing:{member}() end
return Thing
"
            )
        };
        let consumer_source = "---@type Thing
local thing
thing:name()
consume(thing)
";
        let helper_source = "function consume(value) end
";

        let build = |dir: &str| {
            let workspace = std::env::temp_dir().join(dir);
            let uri = |name: &str| {
                Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
            };
            let uris = [uri("producer.lua"), uri("consumer.lua"), uri("helper.lua")];
            let mut analysis = EmmyLuaAnalysis::new();
            analysis.add_main_workspace(workspace);
            analysis.update_files_by_uri(vec![
                (uris[0].clone(), Some(producer_source("name"))),
                (uris[1].clone(), Some(consumer_source.to_string())),
                (uris[2].clone(), Some(helper_source.to_string())),
            ]);
            (analysis, uris)
        };

        let snapshot = |analysis: &EmmyLuaAnalysis, uris: &[Uri; 3]| {
            let shared = analysis.precompute_diagnostic_shared_data();
            uris.iter()
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

        // Renaming the produced field is a real change: the consumer reads the
        // old name, so the edit has to reach it.
        let (mut single, single_uris) = build("gmod_glua_ls_two_phase_single");
        let single_producer = single
            .update_file_text_only(&single_uris[0], producer_source("title"))
            .expect("producer should exist");
        single.reindex_files(vec![single_producer]);

        let (mut split, split_uris) = build("gmod_glua_ls_two_phase_split");
        let split_producer = split
            .get_file_id(&split_uris[0])
            .expect("producer should be indexed");
        let expansion = split.expand_reindex_file_ids(vec![split_producer]);
        split
            .update_file_text_only(&split_uris[0], producer_source("title"))
            .expect("producer should exist");
        split.self_index_files(vec![split_producer]);
        split.reindex_expanded_files(vec![split_producer], expansion);

        let single_diagnostics = snapshot(&single, &single_uris);
        assert!(
            single_diagnostics
                .iter()
                .any(|diagnostics| !diagnostics.is_empty()),
            "fixture should exercise observable diagnostics"
        );
        assert_eq!(
            snapshot(&split, &split_uris),
            single_diagnostics,
            "self-indexing the edited file first must not change the outcome"
        );
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
