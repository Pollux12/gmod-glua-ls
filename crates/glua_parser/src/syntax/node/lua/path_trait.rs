use crate::LuaAstNode;
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

pub trait PathTrait: LuaAstNode {
    /// The dotted access path of this expression, e.g. `foo.bar.baz`.
    fn get_access_path(&self) -> Option<SmolStr> {
        let mut paths: Vec<SmolStr> = Vec::new();
        let mut current_node = self.syntax().clone();
        loop {
            match LuaExpr::cast(current_node)? {
                LuaExpr::NameExpr(name_expr) => {
                    let name = name_expr.get_name_text()?;
                    if paths.is_empty() {
                        return Some(name);
                    } else {
                        paths.push(name);
                        paths.reverse();
                        return Some(join_path(&paths));
                    }
                }
                LuaExpr::CallExpr(call_expr) => {
                    let prefix_expr = call_expr.get_prefix_expr()?;
                    current_node = prefix_expr.syntax().clone();
                }
                LuaExpr::IndexExpr(index_expr) => {
                    match index_expr.get_index_key()? {
                        LuaIndexKey::String(s) => {
                            paths.push(SmolStr::new(s.get_value()));
                        }
                        LuaIndexKey::Name(name) => {
                            paths.push(SmolStr::new(name.get_name_text()));
                        }
                        LuaIndexKey::Integer(i) => {
                            paths.push(SmolStr::new(i.get_number_value().to_string()));
                        }
                        LuaIndexKey::Expr(expr) => {
                            paths.push(SmolStr::new(format!("[{}]", expr.syntax().text())));
                        }
                        LuaIndexKey::Idx(idx) => {
                            paths.push(SmolStr::new(format!("[{}]", idx)));
                        }
                    }

                    current_node = index_expr.get_prefix_expr()?.syntax().clone();
                }
                _ => return None,
            }
        }
    }

    /// The access path used for *member-owner identity*, where a computed key
    fn get_owner_access_path(&self) -> Option<SmolStr> {
        let mut paths: Vec<SmolStr> = Vec::new();
        let mut current_node = self.syntax().clone();
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
                        LuaIndexKey::String(s) => paths.push(SmolStr::new(s.get_value())),
                        LuaIndexKey::Name(name) => paths.push(SmolStr::new(name.get_name_text())),
                        LuaIndexKey::Integer(i) => {
                            paths.push(SmolStr::new(i.get_number_value().to_string()))
                        }
                        LuaIndexKey::Expr(_) | LuaIndexKey::Idx(_) => {
                            paths.push(SmolStr::new_static("[]"))
                        }
                    }
                    current_node = index_expr.get_prefix_expr()?.syntax().clone();
                }
                _ => return None,
            }
        }
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
