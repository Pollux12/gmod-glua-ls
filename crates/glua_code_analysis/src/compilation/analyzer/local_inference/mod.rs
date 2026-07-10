mod evidence;
mod solver;

use std::{collections::HashMap, sync::Arc};

use rustc_hash::FxHashMap;

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaElseIfClauseStat, LuaExpr, LuaIfStat, LuaIndexExpr, LuaNameExpr,
    LuaRepeatStat, LuaVarExpr, LuaWhileStat,
};
use rustc_hash::FxHashSet;

use crate::{
    InFiled, LuaDefinitionId, LuaInferenceConfidence, LuaInferenceEventId, LuaInferenceNodeId,
    LuaInferenceProvenanceKind, LuaInferenceStep, LuaMemberKey, LuaMemberOwner, LuaType,
    LuaTypeDeclId, LuaTypeFact,
    compilation::analyzer::AnalyzeContext,
    semantic::{infer_bind_value_type, infer_expr},
};

use self::evidence::ContextualTypeEvidence;

pub(super) fn stabilize_unknown_locals(db: &mut crate::DbIndex, context: &mut AnalyzeContext) {
    let _profile =
        crate::profile::Profile::cond_new("local inference stabilize", context.tree_list.len() > 1);
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
            db.get_decl_index()
                .get_decl(decl_id)
                .is_some_and(|decl| matches!(decl.extra, crate::LuaDeclExtra::Local { .. }))
                && db
                    .get_type_index()
                    .get_type_cache(&(*decl_id).into())
                    .is_none_or(|cache| cache.is_infer() && cache.as_type().is_unknown())
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
        for cell in cells {
            let Some(name_expr) = root
                .covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .filter(|name| name.get_range() == cell.range)
            else {
                continue;
            };
            let is_function_definition = cell.is_write
                && name_expr
                    .ancestors::<glua_parser::LuaFuncStat>()
                    .next()
                    .is_some_and(|function| {
                        function
                            .get_func_name()
                            .is_some_and(|name| name.syntax() == name_expr.syntax())
                    });
            if cell.is_write && !is_function_definition {
                continue;
            }
            let expr: LuaExpr = LuaVarExpr::NameExpr(name_expr.clone()).into();
            let Some(candidate) =
                infer_bind_value_type(db, context.infer_manager.get_infer_cache(file_id), expr)
            else {
                continue;
            };
            if super::type_is_uninformative(&candidate) {
                continue;
            }
            let definitions = if is_function_definition {
                Arc::from([crate::LuaDefinitionId::Declaration(decl_id)])
            } else {
                flow_tree
                    .and_then(|tree| {
                        tree.get_flow_id(name_expr.get_syntax_id())
                            .map(|flow| (tree, flow))
                    })
                    .map(|(tree, flow)| tree.reaching_definitions(decl_id, flow))
                    .unwrap_or_else(|| Arc::from([crate::LuaDefinitionId::Declaration(decl_id)]))
            };
            for definition in definitions.iter().cloned() {
                let target = LuaInferenceNodeId::Definition(definition);
                evidence_by_node.entry(target.clone()).or_default().push(
                    ContextualTypeEvidence::anchored(
                        target,
                        candidate.clone(),
                        InFiled::new(file_id, name_expr.get_syntax_id()),
                        contextual_type_support(db, &candidate),
                    ),
                );
            }
        }
    }

    let evidence_count = evidence_by_node.values().map(Vec::len).sum::<usize>();
    let solved = solver::solve_local_inference_graph(&evidence_by_node);
    log::debug!(
        "local inference: candidates={} sccs={} resolved={} unresolved={}",
        solved.stats.nodes,
        solved.stats.sccs,
        solved.stats.resolved,
        solved.stats.unresolved
    );
    let changed = db.publish_inference_facts(solved.facts);
    if std::env::var_os("GLUALS_PROFILE").is_some() {
        eprintln!(
            "[profile] local_inference candidates={} evidence={} sccs={} resolved={} unresolved={} changed_files={}",
            solved.stats.nodes,
            evidence_count,
            solved.stats.sccs,
            solved.stats.resolved,
            solved.stats.unresolved,
            changed.len()
        );
    }
    if !changed.is_empty() {
        context.infer_manager.clear();
    }
}

fn contextual_type_support(
    db: &crate::DbIndex,
    candidate: &crate::LuaType,
) -> Arc<[LuaInferenceNodeId]> {
    let type_decl_id = match candidate {
        crate::LuaType::Ref(id) | crate::LuaType::Def(id) => id,
        _ => return Arc::from([]),
    };
    let Some(type_decl) = db.get_type_index().get_type_decl(type_decl_id) else {
        return Arc::from([]);
    };
    let mut support = type_decl
        .get_locations()
        .iter()
        .map(|location| {
            LuaInferenceNodeId::TypeOwner(crate::LuaTypeOwner::SyntaxId(InFiled::new(
                location.file_id,
                glua_parser::LuaSyntaxId::new(
                    glua_parser::LuaSyntaxKind::DocTagClass.into(),
                    location.range,
                ),
            )))
        })
        .collect::<Vec<_>>();
    support.sort_by(LuaInferenceNodeId::stable_cmp);
    support.dedup();
    support.into()
}

/// Infers a known direct child from several otherwise-invalid member uses of the
/// same reaching definition. This is deliberately distinct from generic unknown
/// stabilization: it only starts from a concrete parent reference type.
pub(super) fn stabilize_unguarded_children(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
) -> std::collections::HashSet<crate::FileId> {
    if !db.get_emmyrc().gmod.enabled {
        return Default::default();
    }

    let mut scores =
        HashMap::<LuaDefinitionId, HashMap<crate::LuaTypeDeclId, FxHashSet<LuaMemberKey>>>::new();
    let mut sources =
        HashMap::<(LuaDefinitionId, crate::LuaTypeDeclId), InFiled<glua_parser::LuaSyntaxId>>::new(
        );
    let direct_subtype_members = precompute_direct_subtype_members(db);

    let file_ids = context
        .tree_list
        .iter()
        .map(|tree| tree.file_id)
        .collect::<Vec<_>>();
    for file_id in file_ids {
        let Some(references) = db
            .get_reference_index()
            .get_decl_references_map(&file_id)
            .cloned()
        else {
            continue;
        };
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let flow_tree = db.get_flow_index().get_flow_tree(&file_id);

        for (decl_id, references) in references {
            let Some(base_type) = declaration_base_type(db, context, decl_id) else {
                continue;
            };
            let (LuaType::Ref(base_id) | LuaType::Def(base_id)) = &base_type else {
                continue;
            };
            let Some(members) = direct_subtype_members.get(base_id) else {
                continue;
            };

            for cell in references.cells.into_iter().filter(|cell| !cell.is_write) {
                let Some(name_expr) = root
                    .covering_element(cell.range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
                    .filter(|name| name.get_range() == cell.range)
                else {
                    continue;
                };
                let Some(index_expr) = name_expr
                    .syntax()
                    .ancestors()
                    .find_map(LuaIndexExpr::cast)
                    .filter(|index| {
                        index
                            .get_prefix_expr()
                            .is_some_and(|prefix| prefix.syntax() == name_expr.syntax())
                    })
                else {
                    continue;
                };
                let Some(index_key) = index_expr.get_index_key() else {
                    continue;
                };
                if is_condition_evidence(&index_expr) {
                    continue;
                }
                let cache = context.infer_manager.get_infer_cache(file_id);
                if LuaMemberKey::index_key_is_dynamic(db, cache, &index_key) {
                    continue;
                }
                let Ok(member_key) = LuaMemberKey::from_index_key(db, cache, &index_key) else {
                    continue;
                };
                let Some(children) = members.get(&member_key) else {
                    continue;
                };

                // A real flow guard wins. Its narrowed use is neither heuristic
                // evidence nor an unguarded-child diagnostic site.
                let current = infer_expr(db, cache, LuaExpr::NameExpr(name_expr.clone()))
                    .unwrap_or(LuaType::Unknown);
                if !matches!(
                    &current,
                    LuaType::Ref(current_id) | LuaType::Def(current_id) if current_id == base_id
                ) {
                    continue;
                }
                if type_has_visible_member_at_use(
                    db,
                    context,
                    &base_type,
                    &member_key,
                    file_id,
                    index_expr.get_position(),
                ) {
                    continue;
                }

                let definitions = flow_tree
                    .and_then(|tree| {
                        tree.get_flow_id(name_expr.get_syntax_id())
                            .map(|flow| tree.reaching_definitions(decl_id, flow))
                    })
                    .unwrap_or_else(|| Arc::from([LuaDefinitionId::Declaration(decl_id)]));
                let source = InFiled::new(file_id, index_expr.get_syntax_id());
                for child_id in children.iter() {
                    let owner = LuaMemberOwner::Type(child_id.clone());
                    let visible = db
                        .get_member_index()
                        .get_member_item(&owner, &member_key)
                        .is_some_and(|item| {
                            !item
                                .visible_member_ids_with_realm_at_offset(
                                    db,
                                    &file_id,
                                    index_expr.get_position(),
                                )
                                .is_empty()
                        });
                    if !visible {
                        continue;
                    }
                    for definition in definitions.iter().cloned() {
                        scores
                            .entry(definition.clone())
                            .or_default()
                            .entry(child_id.clone())
                            .or_default()
                            .insert(member_key.clone());
                        let source_key = (definition, child_id.clone());
                        sources
                            .entry(source_key)
                            .and_modify(|existing| {
                                if source.value.get_range().start()
                                    < existing.value.get_range().start()
                                {
                                    *existing = source.clone();
                                }
                            })
                            .or_insert_with(|| source.clone());
                    }
                }
            }
        }
    }

    let mut updates = Vec::new();
    for (definition, candidates) in scores {
        let Some(max_score) = candidates.values().map(FxHashSet::len).max() else {
            continue;
        };
        let mut winners = candidates
            .into_iter()
            .filter_map(|(child_id, keys)| (keys.len() == max_score).then_some(child_id))
            .collect::<Vec<_>>();
        winners.sort_by(|left, right| {
            db.get_type_index()
                .get_type_decl(left)
                .map(|decl| decl.get_name())
                .unwrap_or_else(|| left.get_name())
                .cmp(
                    db.get_type_index()
                        .get_type_decl(right)
                        .map(|decl| decl.get_name())
                        .unwrap_or_else(|| right.get_name()),
                )
                .then_with(|| left.get_name().cmp(right.get_name()))
        });
        let Some(source) = winners
            .iter()
            .filter_map(|child| sources.get(&(definition.clone(), child.clone())))
            .min_by_key(|source| source.value.get_range().start())
            .cloned()
        else {
            continue;
        };
        let node = LuaInferenceNodeId::Definition(definition);
        let event = LuaInferenceEventId {
            node: node.clone(),
            kind: LuaInferenceProvenanceKind::UnguardedChild,
            source,
        };
        let typ = LuaType::from_vec(winners.iter().cloned().map(LuaType::Ref).collect());
        let mut support = Vec::new();
        for child in &winners {
            support.extend(
                contextual_type_support(db, &LuaType::Ref(child.clone()))
                    .iter()
                    .cloned(),
            );
        }
        support.sort_by(LuaInferenceNodeId::stable_cmp);
        support.dedup();
        updates.push((
            node,
            LuaTypeFact::new(
                typ,
                LuaInferenceConfidence::Heuristic,
                Arc::from([LuaInferenceStep {
                    event,
                    support: support.into(),
                }]),
            ),
        ));
    }

    let changed = db.publish_inference_facts(updates);
    if !changed.is_empty() {
        context.infer_manager.clear();
    }
    changed
}

fn type_has_visible_member_at_use(
    db: &crate::DbIndex,
    context: &AnalyzeContext,
    typ: &LuaType,
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> bool {
    context
        .workspace_id
        .and_then(|workspace_id| {
            crate::semantic::find_members_with_key_in_workspace_for_file_at_offset(
                db,
                typ,
                member_key.clone(),
                false,
                workspace_id,
                file_id,
                position,
            )
        })
        .is_some_and(|members| !members.is_empty())
}

type DirectSubtypeMembers = FxHashMap<LuaTypeDeclId, FxHashMap<LuaMemberKey, Vec<LuaTypeDeclId>>>;

fn precompute_direct_subtype_members(db: &crate::DbIndex) -> DirectSubtypeMembers {
    let type_index = db.get_type_index();
    let member_index = db.get_member_index();
    let mut candidates =
        FxHashMap::<LuaTypeDeclId, FxHashMap<LuaMemberKey, FxHashSet<LuaTypeDeclId>>>::default();

    for child in type_index.get_all_types() {
        let child_id = child.get_id();
        let owner = LuaMemberOwner::Type(child_id.clone());
        let Some(members) = member_index.get_members(&owner) else {
            continue;
        };
        let Some(super_types) = type_index.get_super_types_iter(&child_id) else {
            continue;
        };

        for super_type in super_types {
            let (LuaType::Ref(base_id) | LuaType::Def(base_id)) = super_type else {
                continue;
            };
            let members_for_base = candidates.entry(base_id.clone()).or_default();
            for member in &members {
                members_for_base
                    .entry(member.get_key().clone())
                    .or_default()
                    .insert(child_id.clone());
            }
        }
    }

    candidates
        .into_iter()
        .map(|(base_id, members)| {
            let members = members
                .into_iter()
                .map(|(member_key, child_ids)| {
                    let mut child_ids = child_ids.into_iter().collect::<Vec<_>>();
                    child_ids.sort_by(|left, right| {
                        type_index
                            .get_type_decl(left)
                            .map(|decl| decl.get_name())
                            .unwrap_or_else(|| left.get_name())
                            .cmp(
                                type_index
                                    .get_type_decl(right)
                                    .map(|decl| decl.get_name())
                                    .unwrap_or_else(|| right.get_name()),
                            )
                            .then_with(|| left.get_name().cmp(right.get_name()))
                    });
                    (member_key, child_ids)
                })
                .collect();
            (base_id, members)
        })
        .collect()
}

fn is_condition_evidence(index_expr: &LuaIndexExpr) -> bool {
    if index_expr
        .syntax()
        .ancestors()
        .find_map(LuaCallExpr::cast)
        .and_then(|call| call.get_prefix_expr())
        .is_some_and(|prefix| prefix.syntax() == index_expr.syntax())
    {
        return false;
    }

    let index_range = index_expr.syntax().text_range();
    index_expr.syntax().ancestors().any(|node| {
        let condition = LuaIfStat::cast(node.clone())
            .and_then(|stat| stat.get_condition_expr())
            .or_else(|| {
                LuaElseIfClauseStat::cast(node.clone()).and_then(|stat| stat.get_condition_expr())
            })
            .or_else(|| LuaWhileStat::cast(node.clone()).and_then(|stat| stat.get_condition_expr()))
            .or_else(|| LuaRepeatStat::cast(node).and_then(|stat| stat.get_condition_expr()));
        condition
            .is_some_and(|condition| condition.syntax().text_range().contains_range(index_range))
    })
}

fn declaration_base_type(
    db: &crate::DbIndex,
    context: &mut AnalyzeContext,
    decl_id: crate::LuaDeclId,
) -> Option<LuaType> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if decl.is_param() {
        return crate::infer_param_with_cache(
            db,
            context.infer_manager.get_infer_cache(decl_id.file_id),
            decl,
        )
        .ok();
    }
    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .map(|cache| cache.as_type().clone())
}
