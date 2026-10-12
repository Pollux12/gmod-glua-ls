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
    /// The text to replace: the string's contents, without its delimiters, or
    /// the whole literal where they cannot hold the new path.
    pub range: TextRange,
    /// The new path, written for the literal it goes into.
    pub new_text: String,
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
    let target_file_id = site.target_file_id?;
    let target = vfs.get_file_path(&target_file_id)?;
    let new_path = match site.kind {
        LuaDependencyKind::Require => {
            rewritten_require_path(db, renames, path, target_file_id, target)?
        }
        _ => rewritten_load_path(renames, path, source, target)?,
    };
    if new_path == path {
        return None;
    }
    let (token, contents) = path_string(db, site, path)?;
    let (range, new_text) = written_for_literal(&token, contents, &new_path);
    Some(LoadPathEdit {
        file_id: site.source_file_id,
        range,
        new_text,
    })
}

/// `new_path` as it replaces the `contents` of the string literal `token`:
/// escaped for a quoted string, as is in a long string that can hold it, and
/// otherwise as a quoted string replacing the whole literal.
fn written_for_literal(
    token: &LuaSyntaxToken,
    contents: TextRange,
    new_path: &str,
) -> (TextRange, String) {
    let text = token.text();
    if let Some(quote @ (b'"' | b'\'')) = text.bytes().next() {
        return (contents, quoted_contents(new_path, char::from(quote)));
    }
    let level = text[1..].bytes().take_while(|byte| *byte == b'=').count();
    let closing = format!("]{}]", "=".repeat(level));
    // A long string drops a newline right after its opening bracket.
    if new_path.contains(&closing) || new_path.starts_with(['\n', '\r']) {
        (
            token.text_range(),
            format!("\"{}\"", quoted_contents(new_path, '"')),
        )
    } else {
        (contents, new_path.to_string())
    }
}

/// `text` escaped to sit between two `quote`s.
fn quoted_contents(text: &str, quote: char) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            ch if ch == quote => {
                escaped.push('\\');
                escaped.push(ch);
            }
            ch if ch.is_ascii_control() => escaped.push_str(&format!("\\{:03}", u32::from(ch))),
            ch => escaped.push(ch),
        }
    }
    escaped
}

/// `path`, a `require` that names the module `target_file_id` registers
/// as, rewritten to name the module it registers as from where the renames
/// leave it: as many trailing segments of it as `path` names, which is what
/// the lookup matches, in the same style.
fn rewritten_require_path(
    db: &DbIndex,
    renames: &FileRenames,
    path: &str,
    target_file_id: FileId,
    target: &Path,
) -> Option<String> {
    let module_index = db.get_module_index();
    let module = &module_index.get_module(target_file_id)?.full_module_name;
    let module_parts = module.split('.').collect::<Vec<_>>();
    let named_parts = path.split(['.', '/', '\\']).collect::<Vec<_>>();
    let unnamed = module_parts.len().checked_sub(named_parts.len())?;
    if !named_parts
        .iter()
        .zip(&module_parts[unnamed..])
        .all(|(named, part)| named.eq_ignore_ascii_case(part))
    {
        return None;
    }

    let (new_module, _) = module_index.module_path_at(renames.after(target).to_str()?)?;
    let new_parts = new_module.split('.').collect::<Vec<_>>();
    let kept = &new_parts[new_parts.len().saturating_sub(named_parts.len())..];
    let separator = if path.contains('/') { "/" } else { "." };
    Some(kept.join(separator))
}

/// `path`, which names `target` from `source`, rewritten to name the file
/// from where the renames leave both, in the same style.
///
/// A path relative to the including file stays relative to it when it can,
/// and otherwise is written from the `lua/` or `gamemodes/` root.
fn rewritten_load_path(
    renames: &FileRenames,
    path: &str,
    source: &Path,
    target: &Path,
) -> Option<String> {
    let with_extension = path.ends_with(".lua");
    let named = path.strip_suffix(".lua").unwrap_or(path);
    let named = named.trim_start_matches("./");
    let named_parts = named.split(['/', '\\']).collect::<Vec<_>>();
    let base = named_base(target, &named_parts)?;

    let new_base = if Some(base) == source.parent() {
        renames.after(source).parent()?.to_path_buf()
    } else {
        renames.after(base)
    };
    let new_target = renames.after(target);
    let relative = match new_target.strip_prefix(&new_base) {
        Ok(relative) => relative,
        Err(_) => lua_root_relative(&new_target)?,
    };

    let mut parts = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    if !with_extension {
        let file_name = parts.pop()?;
        parts.push(Path::new(file_name).file_stem()?.to_str()?);
    }
    Some(parts.join("/"))
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

/// The string literal holding `path` in `site`'s call, and its contents.
fn path_string(
    db: &DbIndex,
    site: &LuaDependencySite,
    path: &str,
) -> Option<(LuaSyntaxToken, TextRange)> {
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
        .find_map(|token| {
            let token = LuaStringToken::cast(token)?.syntax().clone();
            let contents = string_contents_range(&token, path)?;
            Some((token, contents))
        })
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

#[cfg(test)]
mod tests {
    use super::quoted_contents;

    #[test]
    fn a_quoted_path_escapes_its_quote_backslashes_and_control_characters() {
        assert_eq!(quoted_contents("a\\b'c\"d\te", '\''), r#"a\\b\'c"d\009e"#);
    }
}
