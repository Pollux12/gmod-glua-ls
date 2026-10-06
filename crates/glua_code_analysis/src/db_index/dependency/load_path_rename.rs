use std::collections::BTreeMap;
use std::path::Path;

use glua_parser::{LuaAstToken, LuaStringToken, LuaSyntaxToken};
use rowan::{NodeOrToken, TextRange, TextSize};

use crate::{DbIndex, FileId, FileRenames, LuaDependencyKind, LuaDependencySite};

/// A load path string to rewrite so it still names the file it loads after a
/// rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadPathEdit {
    pub file_id: FileId,
    /// The string's contents, without its delimiters.
    pub range: TextRange,
    pub new_path: String,
}

/// The `include`, `AddCSLuaFile`, `require` and other load path strings that
/// `renames` breaks, rewritten for the files' new places, by file and range.
///
/// Only the load sites of moved files and the sites loading them are visited.
pub fn load_path_rename_edits(db: &DbIndex, renames: &FileRenames) -> Vec<LoadPathEdit> {
    let dependencies = db.get_file_dependencies_index();
    let module_index = db.get_module_index();
    let mut edits = BTreeMap::new();
    for (file_id, _) in renames.moved_files(db.get_vfs()) {
        let sites = dependencies
            .get_dependency_sites(&file_id)
            .unwrap_or_default()
            .iter()
            .chain(dependencies.get_sites_loading(file_id));
        for site in sites.filter(|site| !module_index.is_std(&site.source_file_id)) {
            if let Some(edit) = site_edit(db, renames, site) {
                edits
                    .entry((edit.file_id, edit.range.start()))
                    .or_insert(edit);
            }
        }
    }
    edits.into_values().collect()
}

fn site_edit(
    db: &DbIndex,
    renames: &FileRenames,
    site: &LuaDependencySite,
) -> Option<LoadPathEdit> {
    let path = site.path.as_deref()?;
    let vfs = db.get_vfs();
    let source = vfs.get_file_path(&site.source_file_id)?;
    let target = vfs.get_file_path(&site.target_file_id?)?;
    let new_path = rewritten_load_path(renames, site.kind, path, source, target)?;
    if new_path == path {
        return None;
    }
    Some(LoadPathEdit {
        file_id: site.source_file_id,
        range: path_string_range(db, site, path)?,
        new_path,
    })
}

/// `path`, which names `target` from `source`, rewritten to name the file
/// from where the renames leave both, in the same style.
///
/// A path relative to the including file stays relative to it when it can,
/// and otherwise is written from the `lua/` or `gamemodes/` root.
fn rewritten_load_path(
    renames: &FileRenames,
    kind: LuaDependencyKind,
    path: &str,
    source: &Path,
    target: &Path,
) -> Option<String> {
    let is_require = kind == LuaDependencyKind::Require;
    let with_extension = path.ends_with(".lua");
    let named = path.strip_suffix(".lua").unwrap_or(path);
    let named = named.trim_start_matches("./");
    let separators: &[char] = if is_require {
        &['.', '/', '\\']
    } else {
        &['/', '\\']
    };
    let named_parts = named.split(separators).collect::<Vec<_>>();
    let base = named_base(target, &named_parts)?;

    let new_base = if !is_require && Some(base) == source.parent() {
        renames.after(source).parent()?.to_path_buf()
    } else {
        renames.after(base)
    };
    let new_target = renames.after(target);
    let relative = match new_target.strip_prefix(&new_base) {
        Ok(relative) => relative,
        Err(_) if !is_require => lua_root_relative(&new_target)?,
        Err(_) => return None,
    };

    let mut parts = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    if !with_extension {
        let file_name = parts.pop()?;
        parts.push(Path::new(file_name).file_stem()?.to_str()?);
    }
    let separator = if is_require && !path.contains('/') {
        "."
    } else {
        "/"
    };
    Some(parts.join(separator))
}

/// The folder `named_parts` name `target` from: `target`'s folder with as
/// many trailing folders taken off as the path names.
fn named_base<'a>(target: &'a Path, named_parts: &[&str]) -> Option<&'a Path> {
    let (file, folders) = named_parts.split_last()?;
    if !file.eq_ignore_ascii_case(target.file_stem()?.to_str()?) {
        return None;
    }
    let mut base = target.parent()?;
    for folder in folders.iter().rev() {
        if !folder.eq_ignore_ascii_case(base.file_name()?.to_str()?) {
            return None;
        }
        base = base.parent()?;
    }
    Some(base)
}

/// `path` from the nearest `lua/` or `gamemodes/` folder above it, the roots
/// Garry's Mod looks load paths up under.
fn lua_root_relative(path: &Path) -> Option<&Path> {
    let root = path.ancestors().skip(1).find(|folder| {
        folder
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("lua") || name.eq_ignore_ascii_case("gamemodes")
            })
    })?;
    path.strip_prefix(root).ok()
}

/// The contents of the string literal holding `path` in `site`'s call.
fn path_string_range(db: &DbIndex, site: &LuaDependencySite, path: &str) -> Option<TextRange> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&site.source_file_id)?
        .get_red_root();
    let call = match root.covering_element(site.range) {
        NodeOrToken::Node(node) => node,
        NodeOrToken::Token(token) => token.parent()?,
    };
    call.descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| site.range.contains_range(token.text_range()))
        .find_map(|token| string_contents_range(LuaStringToken::cast(token)?.syntax(), path))
}

/// The range of `token`'s contents when they are exactly `value`, so escapes
/// and long-string newlines are never rewritten.
fn string_contents_range(token: &LuaSyntaxToken, value: &str) -> Option<TextRange> {
    let text = token.text();
    let delimiter = match text.as_bytes().first()? {
        b'"' | b'\'' => 1,
        b'[' => 2 + text[1..].bytes().take_while(|byte| *byte == b'=').count(),
        _ => return None,
    };
    let contents = text.get(delimiter..text.len().checked_sub(delimiter)?)?;
    (contents == value).then(|| {
        TextRange::at(
            token.text_range().start() + TextSize::of(&text[..delimiter]),
            TextSize::of(contents),
        )
    })
}
