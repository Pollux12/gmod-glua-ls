use std::collections::HashMap;

use glua_parser::{
    LuaAstNode, LuaAstToken, LuaChunk, LuaExpr, LuaForRangeStat, LuaLocalName, LuaLocalStat,
};
use rowan::TextRange;

use crate::{DiagnosticCode, LuaDecl, LuaReferenceIndex, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct UnusedChecker;

impl Checker for UnusedChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::Unused, DiagnosticCode::UnusedSelf];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let Some(decl_tree) = semantic_model
            .get_db()
            .get_decl_index()
            .get_decl_tree(&file_id)
        else {
            return;
        };

        let root = semantic_model.get_root();
        let ref_index = semantic_model.get_db().get_reference_index();
        let decls_by_range = decl_tree
            .get_decls()
            .values()
            .map(|decl| (decl.get_range(), decl))
            .collect::<HashMap<_, _>>();
        for (_, decl) in decl_tree.get_decls().iter() {
            if decl.is_global() || decl.is_param() && decl.get_name() == "..." {
                continue;
            }
            if decl.is_seeded_class_local() {
                continue;
            }
            if semantic_model.get_emmyrc().gmod.enabled && decl.is_param() {
                continue;
            }

            if let Err(result) = get_unused_check_result(ref_index, decl, root) {
                let name = decl.get_name();
                if name.starts_with('_') {
                    continue;
                }
                if should_ignore_positional_placeholder(ref_index, &decls_by_range, decl, root) {
                    continue;
                }
                match result {
                    UnusedCheckResult::Unused(range) => {
                        context.add_diagnostic(
                        DiagnosticCode::Unused,
                        range,
                        format!(
                            "{name} is never used, if this is intentional, prefix it with an underscore: _{name}",
                            name = name
                        ).to_string(),
                        None)
                    }
                    // UnusedCheckResult::AssignedButNotRead(range) => {
                    //     context.add_diagnostic(
                    //         DiagnosticCode::Unused,
                    //         range,
                    //         t!(
                    //             "Variable '%{name}' is assigned a value but this value is never read, use _%{name} to indicate this is intentional",
                    //             name = name
                    //         ).to_string(),
                    //         None)
                    // }
                    UnusedCheckResult::UnusedSelf(range) => {
                        context.add_diagnostic(
                            DiagnosticCode::UnusedSelf,
                            range,
                            "Implicit self is never used, if this is intentional, please use '.' instead of ':' to define the method".to_string(),
                            None,
                        );
                    }
                }
            }
        }
    }
}

enum UnusedCheckResult {
    Unused(TextRange),
    // AssignedButNotRead(TextRange),
    UnusedSelf(TextRange),
}

fn get_unused_check_result(
    ref_index: &LuaReferenceIndex,
    decl: &LuaDecl,
    _root: &LuaChunk,
) -> Result<(), UnusedCheckResult> {
    let decl_range = decl.get_range();
    let file_id = decl.get_file_id();
    let decl_ref = match ref_index.get_decl_references(&file_id, &decl.get_id()) {
        Some(decl_ref) => decl_ref,
        None => {
            if decl.is_implicit_self() {
                return Err(UnusedCheckResult::UnusedSelf(decl_range));
            }
            return Err(UnusedCheckResult::Unused(decl_range));
        }
    };

    if decl_ref.cells.is_empty() {
        return Err(UnusedCheckResult::Unused(decl_range));
    }

    // if decl_ref.mutable {
    //     let last_ref_cell = decl_ref
    //         .cells
    //         .last()
    //         .ok_or(UnusedCheckResult::Unused(decl_range))?;

    //     if last_ref_cell.is_write
    //         && let Some(result) =
    //             check_last_mutable_is_read(decl_range.start(), decl_ref, last_ref_cell.range, root)
    //     {
    //         return Err(result);
    //     }
    // }

    Ok(())
}

fn should_ignore_positional_placeholder(
    ref_index: &LuaReferenceIndex,
    decls_by_range: &HashMap<TextRange, &LuaDecl>,
    decl: &LuaDecl,
    root: &LuaChunk,
) -> bool {
    is_generic_for_placeholder(ref_index, decls_by_range, decl, root)
        || is_local_multireturn_placeholder(ref_index, decls_by_range, decl, root)
}

fn is_generic_for_placeholder(
    ref_index: &LuaReferenceIndex,
    decls_by_range: &HashMap<TextRange, &LuaDecl>,
    decl: &LuaDecl,
    root: &LuaChunk,
) -> bool {
    let Some(token) = decl.get_syntax_id().to_token_from_root(root.syntax()) else {
        return false;
    };
    let Some(for_range_stat) = token.parent_ancestors().find_map(LuaForRangeStat::cast) else {
        return false;
    };

    let vars = for_range_stat.get_var_name_list().collect::<Vec<_>>();
    let Some(index) = vars
        .iter()
        .position(|var| var.get_range() == decl.get_range())
    else {
        return false;
    };

    vars.iter().enumerate().any(|(other_index, var)| {
        other_index != index
            && decls_by_range
                .get(&var.get_range())
                .is_some_and(|other_decl| decl_has_references(ref_index, other_decl))
    })
}

fn is_local_multireturn_placeholder(
    ref_index: &LuaReferenceIndex,
    decls_by_range: &HashMap<TextRange, &LuaDecl>,
    decl: &LuaDecl,
    root: &LuaChunk,
) -> bool {
    let Some(initializer) = decl.get_initializer() else {
        return false;
    };
    if !initializer_is_call(root, initializer.get_expr_syntax_id()) {
        return false;
    }

    let Some(node) = decl.get_syntax_id().to_node_from_root(root.syntax()) else {
        return false;
    };
    let Some(local_name) = LuaLocalName::cast(node) else {
        return false;
    };
    let Some(local_stat) = local_name.get_parent::<LuaLocalStat>() else {
        return false;
    };

    let local_names = local_stat.get_local_name_list().collect::<Vec<_>>();
    let Some(index) = local_names
        .iter()
        .position(|local| local.get_range() == decl.get_range())
    else {
        return false;
    };

    local_names.iter().enumerate().any(|(other_index, local)| {
        if other_index == index {
            return false;
        }

        let Some(other_decl) = decls_by_range.get(&local.get_range()) else {
            return false;
        };
        let Some(other_initializer) = other_decl.get_initializer() else {
            return false;
        };
        other_initializer.get_expr_syntax_id() == initializer.get_expr_syntax_id()
            && decl_has_references(ref_index, other_decl)
    })
}

fn initializer_is_call(root: &LuaChunk, syntax_id: glua_parser::LuaSyntaxId) -> bool {
    syntax_id
        .to_node_from_root(root.syntax())
        .and_then(LuaExpr::cast)
        .is_some_and(|expr| matches!(expr, LuaExpr::CallExpr(_)))
}

fn decl_has_references(ref_index: &LuaReferenceIndex, decl: &LuaDecl) -> bool {
    ref_index
        .get_decl_references(&decl.get_file_id(), &decl.get_id())
        .is_some_and(|decl_ref| !decl_ref.cells.is_empty())
}

// remove for future implement
// fn check_last_mutable_is_read(
//     decl_position: TextSize,
//     decl_ref: &DeclReference,
//     range: TextRange,
//     root: &LuaChunk,
// ) -> Option<UnusedCheckResult> {
//     let syntax_id = LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range);
//     let node = LuaNameExpr::cast(syntax_id.to_node_from_root(root.syntax())?)?;

//     for ancestor_node in node.ancestors::<LuaAst>() {
//         // decl's parent
//         if ancestor_node.syntax().text_range().contains(decl_position) {
//             return Some(UnusedCheckResult::AssignedButNotRead(range));
//         }

//         if let Some(loop_stat) = LuaLoopStat::cast(ancestor_node.syntax().clone()) {
//             // in a loop stat
//             let loop_range = loop_stat.syntax().text_range();
//             for ref_cell in decl_ref.cells.iter() {
//                 if !ref_cell.is_write && loop_range.contains(ref_cell.range.start()) {
//                     return None;
//                 }
//             }
//         } else if ancestor_node.syntax().kind() == LuaSyntaxKind::ClosureExpr.into() {
//             return None;
//         }
//     }

//     // not in a loop stat
//     Some(UnusedCheckResult::AssignedButNotRead(range))
// }
