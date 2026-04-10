use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, PathTrait};
use smol_str::SmolStr;

use crate::{GlobalId, LuaMemberOwner};

use super::DeclAnalyzer;

pub fn find_index_owner(
    analyzer: &mut DeclAnalyzer,
    index_expr: LuaIndexExpr,
) -> (LuaMemberOwner, Option<GlobalId>) {
    if is_in_global_member(analyzer, &index_expr).unwrap_or(false) {
        if let Some(prefix_expr) = index_expr.get_prefix_expr() {
            match prefix_expr {
                LuaExpr::IndexExpr(parent_index_expr) => {
                    if let Some(parent_access_path) = parent_index_expr.get_access_path() {
                        if let Some(module_path) = rewrite_legacy_module_member_path(
                            analyzer,
                            &parent_access_path,
                            index_expr.get_position(),
                        ) {
                            if let Some(access_path) = index_expr.get_access_path()
                                && let Some(global_path) = rewrite_legacy_module_member_path(
                                    analyzer,
                                    &access_path,
                                    index_expr.get_position(),
                                )
                            {
                                return (
                                    LuaMemberOwner::GlobalPath(GlobalId(
                                        SmolStr::new(module_path).into(),
                                    )),
                                    Some(GlobalId(SmolStr::new(global_path).into())),
                                );
                            }

                            return (
                                LuaMemberOwner::GlobalPath(GlobalId(
                                    SmolStr::new(module_path).into(),
                                )),
                                None,
                            );
                        }

                        if let Some(access_path) = index_expr.get_access_path() {
                            return (
                                LuaMemberOwner::GlobalPath(GlobalId(
                                    SmolStr::new(parent_access_path).into(),
                                )),
                                Some(GlobalId(SmolStr::new(access_path).into())),
                            );
                        }

                        return (
                            LuaMemberOwner::GlobalPath(GlobalId(
                                SmolStr::new(parent_access_path).into(),
                            )),
                            None,
                        );
                    }
                }
                LuaExpr::NameExpr(name) => {
                    if let Some(parent_path) = name.get_name_text() {
                        if parent_path == "self" {
                            return (LuaMemberOwner::LocalUnresolve, None);
                        }

                        if let Some(module_path) = legacy_module_global_path(
                            analyzer,
                            parent_path.as_str(),
                            index_expr.get_position(),
                        ) {
                            if let Some(access_path) = index_expr.get_access_path()
                                && let Some(global_path) = rewrite_legacy_module_member_path(
                                    analyzer,
                                    &access_path,
                                    index_expr.get_position(),
                                )
                            {
                                return (
                                    LuaMemberOwner::GlobalPath(GlobalId(
                                        SmolStr::new(module_path).into(),
                                    )),
                                    Some(GlobalId(SmolStr::new(global_path).into())),
                                );
                            }

                            return (
                                LuaMemberOwner::GlobalPath(GlobalId(
                                    SmolStr::new(module_path).into(),
                                )),
                                None,
                            );
                        }

                        if let Some(access_path) = index_expr.get_access_path() {
                            return (
                                LuaMemberOwner::GlobalPath(GlobalId(
                                    SmolStr::new(parent_path).into(),
                                )),
                                Some(GlobalId(SmolStr::new(access_path).into())),
                            );
                        }

                        return (
                            LuaMemberOwner::GlobalPath(GlobalId(SmolStr::new(parent_path).into())),
                            None,
                        );
                    }
                }
                _ => {}
            }
        } else if let Some(access_path) = index_expr.get_access_path() {
            return (
                LuaMemberOwner::LocalUnresolve,
                Some(GlobalId(SmolStr::new(access_path).into())),
            );
        }
    }

    (LuaMemberOwner::LocalUnresolve, None)
}

fn is_in_global_member(analyzer: &DeclAnalyzer, index_expr: &LuaIndexExpr) -> Option<bool> {
    let prefix = index_expr.get_prefix_expr()?;
    match prefix {
        LuaExpr::IndexExpr(index_expr) => {
            return is_in_global_member(analyzer, &index_expr);
        }
        LuaExpr::NameExpr(name) => {
            let name_text = name.get_name_text()?;
            if name_text == "self" {
                return Some(false);
            }

            // The scoped class global (e.g. SWEP, ENT) is not a real global in this file:
            // its members belong to the per-entity class type resolved in the Lua phase.
            if analyzer.is_scoped_class_global_name(&name_text) {
                return Some(false);
            }

            if legacy_module_global_path(analyzer, name_text.as_str(), name.get_position())
                .is_some()
            {
                return Some(true);
            }

            let decl = analyzer.find_decl(&name_text, name.get_position());
            return Some(decl.is_none_or(|decl| decl.is_global() || decl.is_module_scoped()));
        }
        _ => {}
    }
    None
}

fn legacy_module_global_path(
    analyzer: &DeclAnalyzer,
    name: &str,
    position: rowan::TextSize,
) -> Option<String> {
    let env = analyzer.get_legacy_module_env_at(position)?;
    match name {
        "_M" => Some(env.module_path.clone()),
        _ => None,
    }
}

fn rewrite_legacy_module_member_path(
    analyzer: &DeclAnalyzer,
    access_path: &str,
    position: rowan::TextSize,
) -> Option<String> {
    let module_path = legacy_module_global_path(analyzer, "_M", position)?;
    if access_path == "_M" {
        return Some(module_path);
    }

    access_path
        .strip_prefix("_M.")
        .map(|suffix| format!("{}.{}", module_path, suffix))
}
