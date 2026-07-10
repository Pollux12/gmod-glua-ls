mod evidence;
mod solver;

use std::sync::Arc;

use rustc_hash::FxHashMap;

use glua_parser::{LuaAstNode, LuaExpr, LuaNameExpr, LuaVarExpr};

use crate::{
    InFiled, LuaInferenceNodeId, compilation::analyzer::AnalyzeContext,
    semantic::infer_bind_value_type,
};

use self::evidence::ContextualTypeEvidence;

pub(super) fn stabilize_unknown_locals(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
) -> std::collections::HashSet<crate::FileId> {
    let mut candidates = context
        .tree_list
        .iter()
        .filter_map(|tree| {
            db.get_reference_index()
                .get_decl_references_map(&tree.file_id)
                .map(|references| (tree.file_id, references.clone()))
        })
        .flat_map(|(file_id, references)| {
            references
                .into_iter()
                .map(move |(decl_id, references)| (file_id, decl_id, references))
        })
        .filter(|(_, decl_id, _)| {
            db.get_type_index()
                .get_type_cache(&(*decl_id).into())
                .is_none_or(|cache| {
                    cache.is_infer() && super::type_is_uninformative(cache.as_type())
                })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(_, decl_id, _)| (decl_id.file_id, decl_id.position));

    let mut evidence_by_node =
        FxHashMap::<LuaInferenceNodeId, Vec<ContextualTypeEvidence>>::default();
    for (file_id, decl_id, references) in candidates {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let flow_tree = db.get_flow_index().get_flow_tree(&file_id);
        let mut cells = references.cells;
        cells.sort_by_key(|cell| cell.range.start());
        for cell in cells.into_iter().filter(|cell| !cell.is_write) {
            let Some(name_expr) = root
                .covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .filter(|name| name.get_range() == cell.range)
            else {
                continue;
            };
            let expr: LuaExpr = LuaVarExpr::NameExpr(name_expr.clone()).into();
            let Some(candidate) =
                infer_bind_value_type(db, context.infer_manager.get_infer_cache(file_id), expr)
            else {
                continue;
            };
            if super::type_is_uninformative(&candidate) {
                continue;
            }
            let definitions = flow_tree
                .and_then(|tree| {
                    tree.get_flow_id(name_expr.get_syntax_id())
                        .map(|flow| (tree, flow))
                })
                .map(|(tree, flow)| tree.reaching_definitions(decl_id, flow))
                .unwrap_or_else(|| Arc::from([crate::LuaDefinitionId::Declaration(decl_id)]));
            for definition in definitions.iter().cloned() {
                let target = LuaInferenceNodeId::Definition(definition);
                evidence_by_node.entry(target.clone()).or_default().push(
                    ContextualTypeEvidence::anchored(
                        target,
                        candidate.clone(),
                        InFiled::new(file_id, name_expr.get_syntax_id()),
                    ),
                );
            }
        }
    }

    let solved = solver::solve_local_inference_graph(&evidence_by_node);
    log::debug!(
        "local inference: candidates={} sccs={} resolved={} unresolved={}",
        solved.stats.nodes,
        solved.stats.sccs,
        solved.stats.resolved,
        solved.stats.unresolved
    );
    let changed = db.publish_inference_facts(solved.facts);
    if !changed.is_empty() {
        context.infer_manager.clear();
    }
    changed
}
