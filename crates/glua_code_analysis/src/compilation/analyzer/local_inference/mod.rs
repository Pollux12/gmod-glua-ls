mod evidence;
mod solver;

use std::{collections::HashMap, sync::Arc};

use rustc_hash::FxHashMap;

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaClosureExpr,
    LuaElseIfClauseStat, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexExpr, LuaNameExpr, LuaRepeatStat,
    LuaReturnStat, LuaVarExpr, LuaWhileStat,
};
use rustc_hash::FxHashSet;

use crate::{
    InFiled, LuaDefinitionId, LuaInferenceConfidence, LuaInferenceEventId, LuaInferenceNodeId,
    LuaInferenceProvenanceKind, LuaInferenceStep, LuaInferredGuardOwner, LuaInferredPositiveGuard,
    LuaMemberKey, LuaMemberOwner, LuaSignatureId, LuaType, LuaTypeDeclId, LuaTypeFact,
    SignatureReturnStatus,
    compilation::analyzer::AnalyzeContext,
    semantic::{
        infer_bind_value_type, infer_expr, infer_true_condition_narrowing,
        resolve_dynamic_field_member,
    },
};

use self::evidence::ContextualTypeEvidence;

pub(super) fn stabilize_unknown_locals(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
) -> bool {
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
    let changed_any = !changed.is_empty();
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
    if changed_any {
        context.infer_manager.clear();
    }
    changed_any
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
#[derive(Default)]
struct UnguardedChildProfile {
    subtype_index: std::time::Duration,
    reference_scan: std::time::Duration,
    publish: std::time::Duration,
    references_scanned: usize,
    assignment_targets_skipped: usize,
    conditions_skipped: usize,
    short_circuit_guards_skipped: usize,
    evidence_sites: usize,
}

struct UnguardedChildCandidates {
    parent_type: LuaType,
    children: HashMap<crate::LuaTypeDeclId, FxHashSet<LuaMemberKey>>,
}

pub(super) fn stabilize_unguarded_children(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
    only_return_evidence: bool,
) -> Vec<InFiled<glua_parser::LuaSyntaxId>> {
    let _profile =
        crate::profile::Profile::cond_new("unguarded child inference", context.tree_list.len() > 1);
    let mut profile = std::env::var_os("GLUALS_PROFILE")
        .is_some()
        .then(UnguardedChildProfile::default);
    if !db.get_emmyrc().gmod.enabled {
        return Vec::new();
    }

    let mut scores = HashMap::<LuaDefinitionId, UnguardedChildCandidates>::new();
    let mut sources =
        HashMap::<(LuaDefinitionId, crate::LuaTypeDeclId), InFiled<glua_parser::LuaSyntaxId>>::new(
        );
    let subtype_index_start = profile.as_ref().map(|_| std::time::Instant::now());
    let direct_subtype_members = precompute_direct_subtype_members(db);
    if let (Some(profile), Some(start)) = (&mut profile, subtype_index_start) {
        profile.subtype_index = start.elapsed();
    }

    let reference_scan_start = profile.as_ref().map(|_| std::time::Instant::now());
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
            let Some(base_id) = unguarded_child_base_id(&base_type) else {
                continue;
            };
            let Some(members) = direct_subtype_members.get(&base_id) else {
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
                if only_return_evidence
                    && !index_expr
                        .syntax()
                        .ancestors()
                        .any(|node| LuaReturnStat::cast(node).is_some())
                {
                    continue;
                }
                if let Some(profile) = &mut profile {
                    profile.references_scanned += 1;
                }
                if is_assignment_target(&index_expr) {
                    if let Some(profile) = &mut profile {
                        profile.assignment_targets_skipped += 1;
                    }
                    continue;
                }
                let Some(index_key) = index_expr.get_index_key() else {
                    continue;
                };
                if is_condition_evidence(&index_expr) {
                    if let Some(profile) = &mut profile {
                        profile.conditions_skipped += 1;
                    }
                    continue;
                }
                let cache = context.infer_manager.get_infer_cache(file_id);
                if is_matching_short_circuit_guard(db, cache, &index_expr) {
                    if let Some(profile) = &mut profile {
                        profile.short_circuit_guards_skipped += 1;
                    }
                    continue;
                }
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
                if !unguarded_child_current_matches_base(&current, &base_type, &base_id) {
                    continue;
                }
                if type_has_visible_member_at_use(
                    db,
                    context,
                    &LuaType::Ref(base_id.clone()),
                    &member_key,
                    file_id,
                    index_expr.get_position(),
                ) {
                    continue;
                }
                let cache = context.infer_manager.get_infer_cache(file_id);

                let definitions = flow_tree
                    .and_then(|tree| {
                        tree.get_flow_id(name_expr.get_syntax_id())
                            .map(|flow| tree.reaching_definitions(decl_id, flow))
                    })
                    .unwrap_or_else(|| Arc::from([LuaDefinitionId::Declaration(decl_id)]));
                let source = InFiled::new(file_id, index_expr.get_syntax_id());
                let mut evidence_recorded = false;
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
                        })
                        || resolve_dynamic_field_member(
                            db,
                            cache,
                            &LuaType::Ref(child_id.clone()),
                            &member_key,
                            None,
                        )
                        .is_some();
                    if !visible {
                        continue;
                    }
                    if !evidence_recorded {
                        if let Some(profile) = &mut profile {
                            profile.evidence_sites += 1;
                        }
                        evidence_recorded = true;
                    }
                    for definition in definitions.iter().cloned() {
                        scores
                            .entry(definition.clone())
                            .or_insert_with(|| UnguardedChildCandidates {
                                parent_type: base_type.clone(),
                                children: HashMap::new(),
                            })
                            .children
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
    if let (Some(profile), Some(start)) = (&mut profile, reference_scan_start) {
        profile.reference_scan = start.elapsed();
    }

    let mut updates = Vec::new();
    let mut update_sources = Vec::new();
    for (definition, candidates) in scores {
        let found_type = candidates.parent_type;
        let candidates = candidates.children;
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
        if only_return_evidence
            && !db
                .get_vfs()
                .get_syntax_tree(&source.file_id)
                .and_then(|tree| source.value.to_node_from_root(&tree.get_red_root()))
                .is_some_and(|node| {
                    node.ancestors()
                        .any(|node| LuaReturnStat::cast(node).is_some())
                })
        {
            continue;
        }
        let node = LuaInferenceNodeId::Definition(definition);
        let event = LuaInferenceEventId {
            node: node.clone(),
            kind: LuaInferenceProvenanceKind::UnguardedChild,
            source: source.clone(),
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
        update_sources.push(source.clone());
        updates.push((
            node,
            LuaTypeFact::new(
                typ,
                LuaInferenceConfidence::Heuristic,
                Arc::from([LuaInferenceStep {
                    event,
                    support: support.into(),
                    found_type: Some(Arc::new(found_type)),
                }]),
            ),
        ));
    }

    let updates_len = updates.len();
    let publish_start = profile.as_ref().map(|_| std::time::Instant::now());
    let changed = db.publish_inference_facts(updates);
    if let (Some(profile), Some(start)) = (&mut profile, publish_start) {
        profile.publish = start.elapsed();
    }
    let changed_any = !changed.is_empty();
    if changed_any {
        context.infer_manager.clear();
    }
    if let Some(profile) = profile {
        eprintln!(
            "[profile] unguarded_child subtype_index={:?} reference_scan={:?} publish={:?} references={} assignment_targets_skipped={} conditions_skipped={} short_circuit_guards_skipped={} evidence={} facts={} changed_files={}",
            profile.subtype_index,
            profile.reference_scan,
            profile.publish,
            profile.references_scanned,
            profile.assignment_targets_skipped,
            profile.conditions_skipped,
            profile.short_circuit_guards_skipped,
            profile.evidence_sites,
            updates_len,
            changed.len(),
        );
    }
    if changed_any {
        update_sources.retain(|source| changed.contains(&source.file_id));
        update_sources
    } else {
        Vec::new()
    }
}

pub(super) fn prepare_inferred_positive_guards(db: &mut crate::DbIndex, context: &AnalyzeContext) {
    let file_ids = context
        .tree_list
        .iter()
        .map(|tree| tree.file_id)
        .collect::<Vec<_>>();
    for file_id in &file_ids {
        db.get_signature_index_mut()
            .clear_inferred_positive_guards_for_file(*file_id);
    }
    db.get_signature_index_mut()
        .take_inferred_positive_guards_changed();
}

pub(super) fn publish_inferred_positive_guards(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
) -> usize {
    let mut published = 0;
    let mut pending = Vec::new();
    for candidate in std::mem::take(&mut context.inferred_guard_candidates) {
        let file_id = candidate.file_id;
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let Some(node) = candidate.value.to_node_from_root(&root) else {
            continue;
        };
        let Some(closure) = LuaClosureExpr::cast(node) else {
            continue;
        };
        let Some((guard, returns)) =
            inferred_positive_guard_for_closure(db, context, file_id, &closure)
        else {
            pending.push(candidate);
            continue;
        };
        let signature_id = LuaSignatureId::from_closure(file_id, &closure);
        let owner = inferred_guard_owner(db, signature_id, &closure);
        if let Some(signature) = db.get_signature_index_mut().get_mut(&signature_id)
            && signature.resolve_return == SignatureReturnStatus::UnResolve
        {
            signature.return_docs = returns;
            signature.resolve_return = SignatureReturnStatus::InferResolve;
        }
        if let Some(owner) = owner {
            db.get_signature_index_mut()
                .set_owned_inferred_positive_guard(signature_id, owner, guard);
        } else {
            db.get_signature_index_mut()
                .set_inferred_positive_guard(signature_id, guard);
        }
        published += 1;
    }
    context.inferred_guard_candidates = pending;
    published
}

fn inferred_guard_owner(
    db: &crate::DbIndex,
    signature_id: LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<LuaInferredGuardOwner> {
    let var = if let Some(func_stat) = closure.get_parent::<LuaFuncStat>() {
        func_stat.get_func_name()?
    } else {
        let assign_stat = closure.get_parent::<LuaAssignStat>()?;
        let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
        let value_idx = value_exprs
            .iter()
            .position(|expr| expr.get_position() == closure.get_position())?;
        vars.get(value_idx)?.clone()
    };
    let path = match var {
        LuaVarExpr::NameExpr(name_expr) => {
            vec![name_expr.get_name_token()?.get_name_text().into()]
        }
        LuaVarExpr::IndexExpr(index_expr) => global_path_from_index_expr(index_expr)?,
    };
    Some(LuaInferredGuardOwner::GlobalPath {
        signature_id,
        state_mask: db
            .get_gmod_infer_index()
            .get_state_mask_at_offset(&signature_id.get_file_id(), closure.get_range().start()),
        path: path.into_boxed_slice(),
    })
}

fn global_path_from_index_expr(
    index_expr: glua_parser::LuaIndexExpr,
) -> Option<Vec<smol_str::SmolStr>> {
    use glua_parser::LuaIndexKey;

    if index_expr.get_index_token()?.is_colon() {
        return None;
    }
    let mut path = match index_expr.get_prefix_expr()? {
        LuaExpr::NameExpr(name_expr) => {
            vec![name_expr.get_name_token()?.get_name_text().into()]
        }
        LuaExpr::IndexExpr(prefix) => global_path_from_index_expr(prefix)?,
        _ => return None,
    };
    let member = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => name.get_name_text().into(),
        LuaIndexKey::String(string) => string.get_value().into(),
        _ => return None,
    };
    path.push(member);
    Some(path)
}

fn inferred_positive_guard_for_closure(
    db: &crate::DbIndex,
    context: &mut AnalyzeContext,
    file_id: crate::FileId,
    closure: &LuaClosureExpr,
) -> Option<(LuaInferredPositiveGuard, Vec<crate::LuaDocReturnInfo>)> {
    let signature_id = LuaSignatureId::from_closure(file_id, closure);
    let signature = db.get_signature_index().get(&signature_id)?;
    if signature.resolve_return == SignatureReturnStatus::DocResolve
        || signature.is_colon_define
        || signature.is_generic()
        || signature.is_vararg
        || !signature.param_docs.is_empty()
        || !signature.overloads.is_empty()
    {
        return None;
    }

    let return_points = super::lua::func_body::analyze_func_body_returns(closure.get_block()?);
    let [super::lua::LuaReturnPoint::Expr(return_expr)] = return_points.as_slice() else {
        return None;
    };
    let return_type = if signature.resolve_return == SignatureReturnStatus::InferResolve {
        signature.get_return_type()
    } else {
        crate::infer_expr(
            db,
            context.infer_manager.get_infer_cache(file_id),
            return_expr.clone(),
        )
        .ok()?
    };
    if !type_is_boolean(&return_type) {
        return None;
    }
    let returns = super::lua::analyze_return_point(
        db,
        context.infer_manager.get_infer_cache(file_id),
        &return_points,
    )
    .ok()?;
    let mut candidates = closure
        .get_params_list()?
        .get_params()
        .enumerate()
        .filter_map(|(param_idx, param)| {
            let decl_id = crate::LuaDeclId::new(file_id, param.get_position());
            if db
                .get_reference_index()
                .get_decl_references(&file_id, &decl_id)
                .is_some_and(|references| references.mutable)
            {
                return None;
            }
            let target_expr = return_expr
                .descendants::<LuaNameExpr>()
                .find(|name_expr| {
                    db.get_reference_index()
                        .get_var_reference_decl(&file_id, name_expr.get_range())
                        == Some(decl_id)
                })
                .map(LuaExpr::NameExpr)?;
            let (antecedent, narrowed_type) = infer_true_condition_narrowing(
                db,
                context.infer_manager.get_infer_cache(file_id),
                target_expr,
                return_expr.clone(),
            )?;
            if antecedent == narrowed_type
                || antecedent.is_any()
                || antecedent.is_unknown()
                || narrowed_type.is_any()
                || narrowed_type.is_unknown()
                || narrowed_type.is_never()
                || narrowed_type.is_nil()
            {
                return None;
            }
            Some(LuaInferredPositiveGuard {
                param_idx,
                narrowed_type,
            })
        });
    let guard = candidates.next()?;
    candidates.next().is_none().then_some((guard, returns))
}

fn type_is_boolean(typ: &LuaType) -> bool {
    match typ {
        LuaType::Union(union) => union.types().all(type_is_boolean),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(typ, _)| type_is_boolean(typ)),
        _ => typ.is_boolean(),
    }
}

fn unguarded_child_base_id(typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => Some(type_id.clone()),
        LuaType::Union(union) => {
            let mut base_id = None;
            let mut saw_nullable_arm = false;
            for component in union.types() {
                match component {
                    LuaType::Nil => saw_nullable_arm = true,
                    LuaType::Ref(type_id) | LuaType::Def(type_id)
                        if type_id == &LuaTypeDeclId::global("NULL") =>
                    {
                        saw_nullable_arm = true;
                    }
                    LuaType::Ref(type_id) | LuaType::Def(type_id) if base_id.is_none() => {
                        base_id = Some(type_id.clone());
                    }
                    _ => return None,
                }
            }
            saw_nullable_arm.then_some(base_id).flatten()
        }
        _ => None,
    }
}

fn unguarded_child_current_matches_base(
    current: &LuaType,
    declared: &LuaType,
    base_id: &LuaTypeDeclId,
) -> bool {
    current == declared
        || matches!(current, LuaType::Ref(current_id) | LuaType::Def(current_id) if current_id == base_id)
}

fn type_has_visible_member_at_use(
    db: &crate::DbIndex,
    context: &AnalyzeContext,
    typ: &LuaType,
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> bool {
    let visible_in_workspace = context
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
        .is_some_and(|members| !members.is_empty());
    visible_in_workspace
        && type_has_visible_static_member_at_use(db, typ, member_key, file_id, position)
}

fn type_has_visible_static_member_at_use(
    db: &crate::DbIndex,
    typ: &LuaType,
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> bool {
    let (LuaType::Ref(type_id) | LuaType::Def(type_id)) = typ else {
        return false;
    };
    let mut owners = vec![type_id.clone()];
    let mut super_types = Vec::new();
    type_id.collect_super_types(db, &mut super_types);
    owners.extend(
        super_types
            .into_iter()
            .filter_map(|super_type| match super_type {
                LuaType::Ref(super_id) | LuaType::Def(super_id) => Some(super_id),
                _ => None,
            }),
    );
    owners.into_iter().any(|owner_id| {
        db.get_member_index()
            .get_member_item(&LuaMemberOwner::Type(owner_id), member_key)
            .is_some_and(|item| {
                !item
                    .visible_member_ids_with_realm_at_offset(db, &file_id, position)
                    .is_empty()
            })
    })
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
        let members = member_index.get_members(&owner);
        let dynamic_owner = crate::DynamicFieldOwner::Type(child_id.clone());
        let dynamic_fields = db
            .get_dynamic_field_index()
            .get_direct_fields(&dynamic_owner);
        if members.is_none() && dynamic_fields.is_none() {
            continue;
        }
        let Some(super_types) = type_index.get_super_types_iter(&child_id) else {
            continue;
        };

        for super_type in super_types {
            let (LuaType::Ref(base_id) | LuaType::Def(base_id)) = super_type else {
                continue;
            };
            let members_for_base = candidates.entry(base_id.clone()).or_default();
            if let Some(members) = &members {
                for member in members {
                    members_for_base
                        .entry(member.get_key().clone())
                        .or_default()
                        .insert(child_id.clone());
                }
            }
            if let Some(dynamic_fields) = dynamic_fields {
                for field_name in dynamic_fields.keys() {
                    members_for_base
                        .entry(LuaMemberKey::Name(field_name.clone()))
                        .or_default()
                        .insert(child_id.clone());
                }
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
    if is_call_prefix(index_expr) {
        return false;
    }

    let index_range = index_expr.syntax().text_range();
    for node in index_expr.syntax().ancestors() {
        if LuaClosureExpr::cast(node.clone()).is_some() {
            break;
        }

        if let Some(binary) = LuaBinaryExpr::cast(node.clone())
            && binary.get_op_token().is_some_and(|token| {
                matches!(token.get_op(), BinaryOperator::OpAnd | BinaryOperator::OpOr)
            })
            && binary
                .get_exprs()
                .is_some_and(|(left, _)| logical_condition_contains_index(&left, index_expr))
        {
            return true;
        }

        let condition = LuaIfStat::cast(node.clone())
            .and_then(|stat| stat.get_condition_expr())
            .or_else(|| {
                LuaElseIfClauseStat::cast(node.clone()).and_then(|stat| stat.get_condition_expr())
            })
            .or_else(|| LuaWhileStat::cast(node.clone()).and_then(|stat| stat.get_condition_expr()))
            .or_else(|| LuaRepeatStat::cast(node).and_then(|stat| stat.get_condition_expr()));
        if condition
            .is_some_and(|condition| condition.syntax().text_range().contains_range(index_range))
        {
            return true;
        }
    }

    false
}

fn logical_condition_contains_index(expr: &LuaExpr, index_expr: &LuaIndexExpr) -> bool {
    match expr {
        LuaExpr::IndexExpr(index) => index.syntax() == index_expr.syntax(),
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|inner| logical_condition_contains_index(&inner, index_expr)),
        LuaExpr::BinaryExpr(binary)
            if binary.get_op_token().is_some_and(|token| {
                matches!(token.get_op(), BinaryOperator::OpAnd | BinaryOperator::OpOr)
            }) =>
        {
            binary.get_exprs().is_some_and(|(left, right)| {
                logical_condition_contains_index(&left, index_expr)
                    || logical_condition_contains_index(&right, index_expr)
            })
        }
        _ => false,
    }
}

fn is_assignment_target(index_expr: &LuaIndexExpr) -> bool {
    let Some(assign_stat) = index_expr
        .syntax()
        .ancestors()
        .find_map(LuaAssignStat::cast)
    else {
        return false;
    };
    let index_range = index_expr.syntax().text_range();
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter()
        .any(|var| var.syntax().text_range().contains_range(index_range))
}

fn is_call_prefix(index_expr: &LuaIndexExpr) -> bool {
    index_expr
        .syntax()
        .ancestors()
        .find_map(LuaCallExpr::cast)
        .and_then(|call| call.get_prefix_expr())
        .is_some_and(|prefix| prefix.syntax() == index_expr.syntax())
}

fn is_matching_short_circuit_guard(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    let index_range = index_expr.syntax().text_range();
    for ancestor in index_expr.syntax().ancestors() {
        if LuaClosureExpr::cast(ancestor.clone()).is_some() {
            break;
        }
        let Some(binary) = LuaBinaryExpr::cast(ancestor) else {
            continue;
        };
        if binary.get_op_token().map(|token| token.get_op()) != Some(BinaryOperator::OpAnd) {
            continue;
        }
        let Some((left, right)) = binary.get_exprs() else {
            continue;
        };
        if left.syntax().text_range().contains_range(index_range) {
            if positive_guard_contains_index(&left, &mut |guard| {
                guard.syntax() == index_expr.syntax()
            }) {
                return true;
            }
            continue;
        }
        if !right.syntax().text_range().contains_range(index_range) {
            continue;
        }
        if matching_guard_in_expr(db, cache, &left, index_expr) {
            return true;
        }
    }
    false
}

fn matching_guard_in_expr(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    guard_expr: &LuaExpr,
    guarded: &LuaIndexExpr,
) -> bool {
    let Some(guarded_prefix) = guarded.get_prefix_expr() else {
        return false;
    };
    let Some(guarded_key) = guarded.get_index_key() else {
        return false;
    };
    let Ok(guarded_key) = LuaMemberKey::from_index_key(db, cache, &guarded_key) else {
        return false;
    };

    positive_guard_contains_index(guard_expr, &mut |guard| {
        guard
            .get_prefix_expr()
            .is_some_and(|prefix| prefix.syntax().text() == guarded_prefix.syntax().text())
            && guard.get_index_key().is_some_and(|key| {
                LuaMemberKey::from_index_key(db, cache, &key)
                    .is_ok_and(|guard_key| guard_key == guarded_key)
            })
    })
}

fn positive_guard_contains_index(
    expr: &LuaExpr,
    predicate: &mut impl FnMut(&LuaIndexExpr) -> bool,
) -> bool {
    match expr {
        LuaExpr::IndexExpr(index) => predicate(index),
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|inner| positive_guard_contains_index(&inner, predicate)),
        LuaExpr::BinaryExpr(binary)
            if binary.get_op_token().map(|token| token.get_op()) == Some(BinaryOperator::OpAnd) =>
        {
            binary.get_exprs().is_some_and(|(left, right)| {
                positive_guard_contains_index(&left, predicate)
                    || positive_guard_contains_index(&right, predicate)
            })
        }
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use glua_parser::{LuaAstNode, LuaIndexExpr, LuaParser, ParserConfig};

    use super::is_assignment_target;

    fn parse_index_expr(code: &str) -> LuaIndexExpr {
        LuaParser::parse(code, ParserConfig::default())
            .get_chunk_node()
            .descendants::<LuaIndexExpr>()
            .next()
            .expect("expected index expression")
    }

    #[test]
    fn assignment_target_classifier_accepts_index_on_left_hand_side() {
        let index_expr = parse_index_expr("value.ChildOnly = function() end");

        assert!(is_assignment_target(&index_expr));
    }

    #[test]
    fn assignment_target_classifier_rejects_index_on_right_hand_side() {
        let index_expr = parse_index_expr("result = value.ChildOnly");

        assert!(!is_assignment_target(&index_expr));
    }
}
