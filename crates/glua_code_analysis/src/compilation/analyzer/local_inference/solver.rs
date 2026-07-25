use std::{collections::HashMap, sync::Arc};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{LuaInferenceNodeId, LuaInferenceStep, LuaType, LuaTypeFact};

use super::evidence::ContextualTypeEvidence;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LocalInferenceSolveStats {
    pub nodes: usize,
    pub sccs: usize,
    pub resolved: usize,
    pub unresolved: usize,
}

pub(crate) struct LocalInferenceSolveResult {
    pub facts: Vec<(LuaInferenceNodeId, LuaTypeFact)>,
    pub stats: LocalInferenceSolveStats,
}

pub(crate) fn solve_local_inference_graph(
    nodes: &FxHashMap<LuaInferenceNodeId, Vec<ContextualTypeEvidence>>,
) -> LocalInferenceSolveResult {
    let components = strongly_connected_components(nodes);
    let mut resolved = HashMap::<LuaInferenceNodeId, LuaTypeFact>::new();

    loop {
        let mut progress = false;
        for component in &components {
            if component.iter().all(|node| resolved.contains_key(node)) {
                continue;
            }
            let members = component.iter().cloned().collect::<FxHashSet<_>>();
            let mut anchors = Vec::new();
            for node in component {
                let Some(evidence) = nodes.get(node) else {
                    continue;
                };
                for item in evidence {
                    if type_is_uninformative(&item.candidate) {
                        continue;
                    }
                    let external_ready = item
                        .support
                        .iter()
                        .all(|support| members.contains(support) || resolved.contains_key(support));
                    let independently_anchored = item.confidence
                        >= crate::LuaInferenceConfidence::Anchored
                        || item.support.is_empty()
                        || (external_ready && item.support.iter().all(|s| !members.contains(s)));
                    if independently_anchored {
                        anchors.push(item.candidate.clone());
                    }
                }
            }
            anchors.dedup();
            if anchors.len() != 1 {
                continue;
            }
            let candidate = anchors.pop().expect("one anchor");
            for node in component {
                let Some(evidence) = nodes.get(node) else {
                    continue;
                };
                let mut selected = evidence
                    .iter()
                    .filter(|item| item.candidate == candidate)
                    .collect::<Vec<_>>();
                selected.sort_by(|left, right| left.event.stable_cmp(&right.event));
                if selected.is_empty() {
                    continue;
                }
                let confidence = selected
                    .iter()
                    .map(|item| item.confidence)
                    .max()
                    .unwrap_or(crate::LuaInferenceConfidence::Unknown);
                let provenance = selected
                    .into_iter()
                    .map(|item| LuaInferenceStep {
                        event: item.event.clone(),
                        support: item.support.clone(),
                        found_type: None,
                    })
                    .collect::<Vec<_>>();
                resolved.insert(
                    node.clone(),
                    LuaTypeFact::new(candidate.clone(), confidence, Arc::from(provenance)),
                );
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    let mut facts = resolved.into_iter().collect::<Vec<_>>();
    facts.sort_by(|(left, _), (right, _)| left.stable_cmp(right));
    LocalInferenceSolveResult {
        stats: LocalInferenceSolveStats {
            nodes: nodes.len(),
            sccs: components.len(),
            resolved: facts.len(),
            unresolved: nodes.len().saturating_sub(facts.len()),
        },
        facts,
    }
}

fn type_is_uninformative(typ: &LuaType) -> bool {
    matches!(typ, LuaType::Any | LuaType::Unknown | LuaType::Never)
}

fn strongly_connected_components(
    nodes: &FxHashMap<LuaInferenceNodeId, Vec<ContextualTypeEvidence>>,
) -> Vec<Vec<LuaInferenceNodeId>> {
    struct Tarjan<'a> {
        nodes: &'a FxHashMap<LuaInferenceNodeId, Vec<ContextualTypeEvidence>>,
        next: usize,
        indices: FxHashMap<LuaInferenceNodeId, usize>,
        low: FxHashMap<LuaInferenceNodeId, usize>,
        stack: Vec<LuaInferenceNodeId>,
        on_stack: FxHashSet<LuaInferenceNodeId>,
        components: Vec<Vec<LuaInferenceNodeId>>,
    }

    fn visit(node: LuaInferenceNodeId, state: &mut Tarjan<'_>) {
        let index = state.next;
        state.next += 1;
        state.indices.insert(node.clone(), index);
        state.low.insert(node.clone(), index);
        state.stack.push(node.clone());
        state.on_stack.insert(node.clone());

        let mut edges = state
            .nodes
            .get(&node)
            .into_iter()
            .flatten()
            .flat_map(|evidence| evidence.support.iter())
            .filter(|support| state.nodes.contains_key(*support))
            .cloned()
            .collect::<Vec<_>>();
        edges.sort_by(LuaInferenceNodeId::stable_cmp);
        edges.dedup();
        for edge in edges {
            if !state.indices.contains_key(&edge) {
                visit(edge.clone(), state);
                let edge_low = state.low[&edge];
                state
                    .low
                    .entry(node.clone())
                    .and_modify(|low| *low = (*low).min(edge_low));
            } else if state.on_stack.contains(&edge) {
                let edge_index = state.indices[&edge];
                state
                    .low
                    .entry(node.clone())
                    .and_modify(|low| *low = (*low).min(edge_index));
            }
        }

        if state.low[&node] == state.indices[&node] {
            let mut component = Vec::new();
            while let Some(member) = state.stack.pop() {
                state.on_stack.remove(&member);
                component.push(member.clone());
                if member == node {
                    break;
                }
            }
            component.sort_by(LuaInferenceNodeId::stable_cmp);
            state.components.push(component);
        }
    }

    let mut ordered = nodes.keys().cloned().collect::<Vec<_>>();
    ordered.sort_by(LuaInferenceNodeId::stable_cmp);
    let mut state = Tarjan {
        nodes,
        next: 0,
        indices: FxHashMap::default(),
        low: FxHashMap::default(),
        stack: Vec::new(),
        on_stack: FxHashSet::default(),
        components: Vec::new(),
    };
    for node in ordered {
        if !state.indices.contains_key(&node) {
            visit(node, &mut state);
        }
    }
    state
        .components
        .sort_by(|left, right| left[0].stable_cmp(&right[0]));
    state.components
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileId, InFiled, LuaDeclId, LuaDefinitionId, LuaInferenceConfidence, LuaInferenceEventId,
        LuaInferenceProvenanceKind,
    };
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    fn node(position: u32) -> LuaInferenceNodeId {
        LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(LuaDeclId::new(
            FileId::new(1),
            TextSize::new(position),
        )))
    }

    fn evidence(
        target: LuaInferenceNodeId,
        candidate: LuaType,
        support: Vec<LuaInferenceNodeId>,
        position: u32,
    ) -> ContextualTypeEvidence {
        let confidence = if support.is_empty() {
            LuaInferenceConfidence::Anchored
        } else {
            LuaInferenceConfidence::Heuristic
        };
        ContextualTypeEvidence {
            candidate,
            confidence,
            event: LuaInferenceEventId {
                node: target,
                kind: LuaInferenceProvenanceKind::ContextualUnknown,
                source: InFiled::new(
                    FileId::new(1),
                    LuaSyntaxId::new(
                        LuaSyntaxKind::NameExpr.into(),
                        TextRange::new(TextSize::new(position), TextSize::new(position + 1)),
                    ),
                ),
            },
            support: support.into(),
        }
    }

    #[test]
    fn unanchored_cycle_remains_unknown() {
        let a = node(1);
        let b = node(2);
        let mut graph = FxHashMap::default();
        graph.insert(
            a.clone(),
            vec![evidence(a.clone(), LuaType::String, vec![b.clone()], 10)],
        );
        graph.insert(
            b.clone(),
            vec![evidence(b.clone(), LuaType::String, vec![a.clone()], 20)],
        );

        let result = solve_local_inference_graph(&graph);

        assert!(result.facts.is_empty());
        assert_eq!(result.stats.unresolved, 2);
        assert_eq!(result.stats.sccs, 1);
    }

    #[test]
    fn independent_anchor_resolves_cycle_deterministically() {
        let a = node(1);
        let b = node(2);
        let mut graph = FxHashMap::default();
        graph.insert(
            b.clone(),
            vec![evidence(b.clone(), LuaType::String, vec![a.clone()], 20)],
        );
        graph.insert(
            a.clone(),
            vec![
                evidence(a.clone(), LuaType::String, vec![b.clone()], 10),
                evidence(a.clone(), LuaType::String, vec![], 11),
            ],
        );

        let result = solve_local_inference_graph(&graph);

        assert_eq!(result.facts.len(), 2);
        assert!(
            result
                .facts
                .iter()
                .all(|(_, fact)| fact.typ() == &LuaType::String)
        );
        assert!(result.facts[0].0.stable_cmp(&result.facts[1].0).is_lt());
    }

    #[test]
    fn incompatible_anchors_do_not_select_a_fact() {
        let a = node(1);
        let mut graph = FxHashMap::default();
        graph.insert(
            a.clone(),
            vec![
                evidence(a.clone(), LuaType::String, vec![], 10),
                evidence(a, LuaType::Number, vec![], 20),
            ],
        );

        assert!(solve_local_inference_graph(&graph).facts.is_empty());
    }
}
