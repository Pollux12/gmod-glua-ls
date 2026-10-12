use crate::{LuaAstNode, LuaSyntaxNode};
use smol_str::SmolStr;

use super::{LuaExpr, LuaIndexKey};

/// Join path segments with `.` into a single `SmolStr`, sizing the buffer once.
fn join_path(paths: &[SmolStr]) -> SmolStr {
    let width = paths.iter().map(|part| part.len() + 1).sum::<usize>();
    let mut joined = String::with_capacity(width);
    for (index, part) in paths.iter().enumerate() {
        if index > 0 {
            joined.push('.');
        }
        joined.push_str(part);
    }
    SmolStr::new(joined)
}

/// The access-path segment of the string key `key`: bare where it reads as a
/// name would, else `["key"]` with `"` and `\` escaped. No bare segment holds
/// a `.`, opens a bracket or starts as a number does, so `t["a.b"]` and
/// `t.a.b`, or `t["1"]` and `t[1]`, never share a path.
pub fn path_segment(key: &str) -> SmolStr {
    let bare = key
        .chars()
        .next()
        .is_some_and(|first| !first.is_ascii_digit() && !matches!(first, '-' | '['))
        && !key.contains('.');
    if bare {
        return SmolStr::new(key);
    }
    let mut segment = String::with_capacity(key.len() + 4);
    segment.push_str("[\"");
    for ch in key.chars() {
        if matches!(ch, '"' | '\\') {
            segment.push('\\');
        }
        segment.push(ch);
    }
    segment.push_str("\"]");
    SmolStr::new(segment)
}

/// The string key a segment [`path_segment`] bracketed names.
pub fn path_segment_string_key(segment: &str) -> Option<String> {
    let escaped = segment.strip_prefix("[\"")?.strip_suffix("\"]")?;
    let mut key = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(ch) = chars.next() {
        key.push(if ch == '\\' { chars.next()? } else { ch });
    }
    Some(key)
}

/// Where the `.`s separating `path`'s segments are: none inside a bracketed
/// segment, string key or computed key.
fn path_separators(path: &str) -> impl Iterator<Item = usize> + '_ {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    path.char_indices().filter_map(move |(index, ch)| {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote = None;
            }
            return None;
        }
        match ch {
            '"' | '\'' if depth > 0 => quote = Some(ch),
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => return Some(index),
            _ => {}
        }
        None
    })
}

/// `path`'s segments in order.
pub fn path_segments(path: &str) -> impl Iterator<Item = &str> + '_ {
    let mut start = 0;
    path_separators(path)
        .chain(std::iter::once(path.len()))
        .map(move |end| {
            let segment = &path[start..end];
            start = end + 1;
            segment
        })
}

/// `path` split before its last segment, if it has more than one.
pub fn split_last_path_segment(path: &str) -> Option<(&str, &str)> {
    let at = path_separators(path).last()?;
    Some((&path[..at], &path[at + 1..]))
}

/// How a computed index key is written into an access path.
#[derive(Clone, Copy)]
enum ComputedKey {
    /// Spelled out, so `t[a]` and `t[b]` are distinct paths.
    Spelled,
    /// Collapsed to `[]`, so every spelling of one runtime slot agrees.
    Collapsed,
}

/// Walk an expression's prefix chain into a dotted path, innermost name first.
fn access_path(node: &LuaSyntaxNode, computed: ComputedKey) -> Option<SmolStr> {
    let mut paths: Vec<SmolStr> = Vec::new();
    let mut current_node = node.clone();
    loop {
        match LuaExpr::cast(current_node)? {
            LuaExpr::NameExpr(name_expr) => {
                let name = name_expr.get_name_text()?;
                if paths.is_empty() {
                    return Some(name);
                }
                paths.push(name);
                paths.reverse();
                return Some(join_path(&paths));
            }
            LuaExpr::CallExpr(call_expr) => {
                current_node = call_expr.get_prefix_expr()?.syntax().clone();
            }
            LuaExpr::IndexExpr(index_expr) => {
                match index_expr.get_index_key()? {
                    LuaIndexKey::String(s) => paths.push(path_segment(&s.get_value())),
                    LuaIndexKey::Name(name) => paths.push(SmolStr::new(name.get_name_text())),
                    LuaIndexKey::Integer(i) => {
                        paths.push(SmolStr::new(i.get_number_value().to_string()))
                    }
                    LuaIndexKey::Expr(expr) => paths.push(match computed {
                        ComputedKey::Spelled => SmolStr::new(format!("[{}]", expr.syntax().text())),
                        ComputedKey::Collapsed => SmolStr::new_static("[]"),
                    }),
                    LuaIndexKey::Idx(idx) => paths.push(match computed {
                        ComputedKey::Spelled => SmolStr::new(format!("[{idx}]")),
                        ComputedKey::Collapsed => SmolStr::new_static("[]"),
                    }),
                }
                current_node = index_expr.get_prefix_expr()?.syntax().clone();
            }
            _ => return None,
        }
    }
}

pub trait PathTrait: LuaAstNode {
    /// The dotted access path of this expression, e.g. `foo.bar.baz`.
    ///
    /// Returns `SmolStr` because paths are short and this is one of the hottest
    /// allocation sites in analysis. A bare name — by far the common case —
    /// returns without allocating at all: a name of 22 bytes or fewer lives
    /// inline and the segment buffer is never allocated.
    fn get_access_path(&self) -> Option<SmolStr> {
        access_path(self.syntax(), ComputedKey::Spelled)
    }

    /// The access path used for *member-owner identity*, where a computed key
    /// collapses to `[]`.
    ///
    /// [`get_access_path`](Self::get_access_path) spells a computed key out, so
    /// `t[a]` and `t[b]` are distinct there, which is what flow narrowing needs
    /// since those are different values. An owner is the other question: both
    /// index the same table, so a field written through one has to be visible to
    /// the other.
    fn get_owner_access_path(&self) -> Option<SmolStr> {
        access_path(self.syntax(), ComputedKey::Collapsed)
    }

    fn get_member_path(&self) -> Option<String> {
        let mut paths = Vec::new();
        let mut current_node = self.syntax().clone();
        loop {
            match LuaExpr::cast(current_node)? {
                LuaExpr::NameExpr(_) => {
                    if paths.is_empty() {
                        return None;
                    } else {
                        paths.reverse();
                        return Some(paths.join("."));
                    }
                }
                LuaExpr::CallExpr(call_expr) => {
                    let prefix_expr = call_expr.get_prefix_expr()?;
                    current_node = prefix_expr.syntax().clone();
                }
                LuaExpr::IndexExpr(index_expr) => {
                    let path_parts = index_expr.get_index_key()?.get_path_part();
                    paths.push(path_parts);

                    current_node = index_expr.get_prefix_expr()?.syntax().clone();
                }
                _ => return None,
            }
        }
    }
}
