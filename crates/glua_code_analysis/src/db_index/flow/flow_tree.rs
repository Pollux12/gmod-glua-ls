use std::collections::{HashMap, HashSet};

use glua_parser::{LuaAstPtr, LuaExpr, LuaSyntaxId};
use internment::ArcIntern;
use smol_str::SmolStr;

use crate::{FlowId, FlowNode, FlowNodeKind, LuaDeclId, LuaDefinitionId};

/// File-wide summary of which variables/paths can possibly be narrowed by the
/// backward flow walk. Used to skip the (expensive) walk entirely for variable
/// references that provably reach no narrowing site — measured at ~95% of
/// top-level narrow queries on real GMod codebases.
///
/// Soundness: every set here is a SUPERSET of what could actually narrow a
/// reference. `referenced_names` / `referenced_index_paths` collect every name
/// and access path appearing in an assignment target, `---@cast`, or condition
/// expression (and special-call effect sites). If a reference's name/path is in
/// none of these sets — and there are no "unknown"/opaque narrowing sources —
/// the walk cannot change its type, so we return the declared type directly.
#[derive(Debug, Clone, Default)]
pub struct FileNarrowingCapability {
    /// Names (bare identifiers) appearing in any assignment target, cast, or
    /// condition expression. Covers `VarRef`/`SelfRef`/`GlobalName` references.
    pub referenced_names: HashSet<ArcIntern<SmolStr>>,
    /// Access paths (e.g. `self.foo`, `tbl.a.b`) appearing in any assignment
    /// target, cast, or condition expression. Covers `IndexRef` references.
    pub referenced_index_paths: HashSet<ArcIntern<SmolStr>>,
    /// When true, a narrowing site referenced a name/index we could not reduce
    /// to a stable key (e.g. computed index). Disables name/index skipping
    /// respectively to stay sound.
    pub has_opaque_name_target: bool,
    pub has_opaque_index_target: bool,
    /// Condition flow nodes keyed by stable index path.
    pub condition_flows_by_path: HashMap<ArcIntern<SmolStr>, HashSet<FlowId>>,
}

impl FileNarrowingCapability {
    /// Whether a bare-name reference (`VarRef`/`SelfRef`/`GlobalName`) named
    /// `name` could be narrowed somewhere in the file.
    pub fn name_can_be_narrowed(&self, name: &ArcIntern<SmolStr>) -> bool {
        self.has_opaque_name_target || self.referenced_names.contains(name)
    }

    /// Whether an index reference with access `path` could be narrowed.
    pub fn index_path_can_be_narrowed(&self, path: &ArcIntern<SmolStr>) -> bool {
        self.has_opaque_index_target || self.referenced_index_paths.contains(path)
    }

    fn condition_flows(&self, path: &ArcIntern<SmolStr>) -> Option<&HashSet<FlowId>> {
        self.condition_flows_by_path.get(path)
    }
}

/// Metadata for BranchLabel nodes that enables the merge-skip optimisation.
///
/// When the backward flow walk hits a BranchLabel, it normally merges the types
/// from every antecedent branch.  For variables NOT modified in any branch (and
/// all branches are alive), the merge is guaranteed to produce the same type as
/// the node before the branch (`common_predecessor`).  The walk can skip
/// directly to that predecessor, turning an O(branches × depth) merge into O(1).
#[derive(Debug, Clone)]
pub struct BranchLabelInfo {
    /// FlowId of the node immediately before the if/elseif/else split.
    pub common_predecessor: FlowId,
    /// `true` when any `Assignment(_, NameOnly|Mixed)` node was created inside
    /// the branches — meaning a local/global name may have been reassigned.
    pub has_name_assigns: bool,
    /// `true` when any `Assignment(_, IndexOnly|Mixed)` node was created inside
    /// the branches — meaning a field/index may have been reassigned.
    pub has_index_assigns: bool,
    /// `true` when any `ImplFunc` or `TagCast` node was created inside
    /// the branches — these can modify the type of a named or indexed variable.
    pub has_casts_or_implfunc: bool,
    /// `true` when any `TrueCondition` or `FalseCondition` node was created
    /// inside the branch *blocks* (not the outer if's condition).  Assert-like
    /// patterns create inner conditions that can narrow variables beyond what
    /// the outer condition/merge would cancel out.
    pub has_inner_conditions: bool,
    /// Branch-local names and index paths that can change/narrow a variable.
    /// This lets the flow walk skip branch merges for variables unrelated to
    /// assignments and inner conditions in the branch.
    pub narrowing_capability: FileNarrowingCapability,
}

#[derive(Debug, Clone, Default)]
pub struct AssignmentFlowInfo {
    pub name_targets: Vec<AssignmentNameTarget>,
    pub index_paths: Vec<ArcIntern<SmolStr>>,
    pub has_unknown_index_target: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssignVarHint, FileId, FlowAntecedent, FlowNodeKind};
    use glua_parser::{LuaAssignStat, LuaAstNode, LuaParser, ParserConfig};
    use rowan::TextSize;

    fn assignment_ptr(source: &str) -> LuaAstPtr<LuaAssignStat> {
        let tree = LuaParser::parse(source, ParserConfig::default());
        tree.get_chunk_node()
            .descendants::<LuaAssignStat>()
            .next()
            .expect("assignment")
            .to_ptr()
    }

    fn node(id: u32, kind: FlowNodeKind, antecedent: Option<FlowAntecedent>) -> FlowNode {
        FlowNode {
            id: FlowId(id),
            kind,
            antecedent,
        }
    }

    fn tree(
        nodes: Vec<FlowNode>,
        branches: Vec<Vec<FlowId>>,
        infos: Vec<AssignmentFlowInfo>,
    ) -> FlowTree {
        FlowTree::new(
            HashMap::new(),
            nodes,
            branches,
            HashMap::new(),
            HashMap::new(),
            infos,
            FileNarrowingCapability::default(),
        )
    }

    #[test]
    fn reaching_definitions_stops_at_nearest_assignment() {
        let file_id = FileId::new(1);
        let decl_id = LuaDeclId::new(file_id, TextSize::new(4));
        let first = assignment_ptr("value = 1");
        let second = assignment_ptr("value = 2");
        let nodes = vec![
            node(0, FlowNodeKind::Start, None),
            node(
                1,
                FlowNodeKind::DeclPosition(decl_id.position),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                2,
                FlowNodeKind::Assignment(first, AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(1))),
            ),
            node(
                3,
                FlowNodeKind::Assignment(second.clone(), AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(2))),
            ),
        ];
        let target = |target_idx| AssignmentFlowInfo {
            name_targets: vec![AssignmentNameTarget {
                decl_id,
                target_idx,
            }],
            ..AssignmentFlowInfo::default()
        };
        let tree = tree(
            nodes,
            vec![],
            vec![
                AssignmentFlowInfo::default(),
                AssignmentFlowInfo::default(),
                target(0),
                target(0),
            ],
        );

        assert_eq!(
            tree.reaching_definitions(decl_id, FlowId(3)).as_ref(),
            &[LuaDefinitionId::Assignment {
                file_id,
                assignment: second.get_syntax_id(),
                target_idx: 0
            }]
        );
    }

    #[test]
    fn reaching_definitions_unions_and_sorts_branch_assignments() {
        let file_id = FileId::new(2);
        let decl_id = LuaDeclId::new(file_id, TextSize::new(4));
        let left = assignment_ptr("value = 1");
        let right = assignment_ptr("value = 22");
        let nodes = vec![
            node(0, FlowNodeKind::Start, None),
            node(
                1,
                FlowNodeKind::Assignment(right, AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                2,
                FlowNodeKind::Assignment(left, AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                3,
                FlowNodeKind::BranchLabel,
                Some(FlowAntecedent::Multiple(0)),
            ),
        ];
        let target = || AssignmentFlowInfo {
            name_targets: vec![AssignmentNameTarget {
                decl_id,
                target_idx: 0,
            }],
            ..AssignmentFlowInfo::default()
        };
        let tree = tree(
            nodes,
            vec![vec![FlowId(1), FlowId(2)]],
            vec![
                AssignmentFlowInfo::default(),
                target(),
                target(),
                AssignmentFlowInfo::default(),
            ],
        );
        let result = tree.reaching_definitions(decl_id, FlowId(3));

        assert_eq!(result.len(), 2);
        assert!(result[0].stable_cmp(&result[1]).is_lt());
    }

    #[test]
    fn reaching_definitions_falls_back_to_declaration() {
        let file_id = FileId::new(3);
        let decl_id = LuaDeclId::new(file_id, TextSize::new(4));
        let nodes = vec![
            node(0, FlowNodeKind::Start, None),
            node(
                1,
                FlowNodeKind::DeclPosition(decl_id.position),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
        ];
        let tree = tree(
            nodes,
            vec![],
            vec![AssignmentFlowInfo::default(), AssignmentFlowInfo::default()],
        );

        assert_eq!(
            tree.reaching_definitions(decl_id, FlowId(1)).as_ref(),
            &[LuaDefinitionId::Declaration(decl_id)]
        );
    }

    #[test]
    fn reaching_definitions_walks_through_closure_entry_to_assignment() {
        let file_id = FileId::new(4);
        let decl_id = LuaDeclId::new(file_id, TextSize::new(4));
        let assignment = assignment_ptr("value = 1");
        let nodes = vec![
            node(0, FlowNodeKind::Start, None),
            node(
                1,
                FlowNodeKind::Assignment(assignment.clone(), AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                2,
                FlowNodeKind::ClosureEntry(TextSize::new(12)),
                Some(FlowAntecedent::Single(FlowId(1))),
            ),
        ];
        let tree = tree(
            nodes,
            vec![],
            vec![
                AssignmentFlowInfo::default(),
                AssignmentFlowInfo {
                    name_targets: vec![AssignmentNameTarget {
                        decl_id,
                        target_idx: 0,
                    }],
                    ..AssignmentFlowInfo::default()
                },
                AssignmentFlowInfo::default(),
            ],
        );

        assert_eq!(
            tree.reaching_definitions(decl_id, FlowId(2)).as_ref(),
            &[LuaDefinitionId::Assignment {
                file_id,
                assignment: assignment.get_syntax_id(),
                target_idx: 0,
            }]
        );
    }

    #[test]
    fn reaching_definitions_walks_through_nested_closure_entries_to_assignment() {
        let file_id = FileId::new(5);
        let decl_id = LuaDeclId::new(file_id, TextSize::new(4));
        let assignment = assignment_ptr("value = 1");
        let nodes = vec![
            node(0, FlowNodeKind::Start, None),
            node(
                1,
                FlowNodeKind::Assignment(assignment.clone(), AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                2,
                FlowNodeKind::ClosureEntry(TextSize::new(12)),
                Some(FlowAntecedent::Single(FlowId(1))),
            ),
            node(
                3,
                FlowNodeKind::ClosureEntry(TextSize::new(24)),
                Some(FlowAntecedent::Single(FlowId(2))),
            ),
        ];
        let tree = tree(
            nodes,
            vec![],
            vec![
                AssignmentFlowInfo::default(),
                AssignmentFlowInfo {
                    name_targets: vec![AssignmentNameTarget {
                        decl_id,
                        target_idx: 0,
                    }],
                    ..AssignmentFlowInfo::default()
                },
                AssignmentFlowInfo::default(),
                AssignmentFlowInfo::default(),
            ],
        );

        assert_eq!(
            tree.reaching_definitions(decl_id, FlowId(3)).as_ref(),
            &[LuaDefinitionId::Assignment {
                file_id,
                assignment: assignment.get_syntax_id(),
                target_idx: 0,
            }]
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignmentNameTarget {
    pub decl_id: LuaDeclId,
    pub target_idx: u16,
}

impl AssignmentFlowInfo {
    pub fn is_empty(&self) -> bool {
        self.name_targets.is_empty()
            && self.index_paths.is_empty()
            && !self.has_unknown_index_target
    }
}

#[derive(Debug)]
pub struct FlowTree {
    decl_bind_expr_ref: HashMap<LuaDeclId, LuaAstPtr<LuaExpr>>,
    flow_nodes: Vec<FlowNode>,
    multiple_antecedents: Vec<Vec<FlowId>>,
    // labels: HashMap<LuaClosureId, HashMap<SmolStr, FlowId>>,
    bindings: HashMap<LuaSyntaxId, FlowId>,
    /// Per-BranchLabel metadata used to skip redundant merges.
    branch_label_info: HashMap<FlowId, BranchLabelInfo>,
    assignment_flow_info: Vec<AssignmentFlowInfo>,
    narrowing_capability: FileNarrowingCapability,
}

impl FlowTree {
    pub fn new(
        decl_bind_expr_ref: HashMap<LuaDeclId, LuaAstPtr<LuaExpr>>,
        flow_nodes: Vec<FlowNode>,
        multiple_antecedents: Vec<Vec<FlowId>>,
        // labels: HashMap<LuaClosureId, HashMap<SmolStr, FlowId>>,
        bindings: HashMap<LuaSyntaxId, FlowId>,
        branch_label_info: HashMap<FlowId, BranchLabelInfo>,
        assignment_flow_info: Vec<AssignmentFlowInfo>,
        mut narrowing_capability: FileNarrowingCapability,
    ) -> Self {
        let mut successors = vec![Vec::new(); flow_nodes.len()];
        for node in &flow_nodes {
            let Some(antecedent) = &node.antecedent else {
                continue;
            };
            match antecedent {
                crate::FlowAntecedent::Single(antecedent) => {
                    if let Some(flow_successors) = successors.get_mut(antecedent.0 as usize) {
                        flow_successors.push(node.id);
                    }
                }
                crate::FlowAntecedent::Multiple(id) => {
                    if let Some(antecedents) = multiple_antecedents.get(*id as usize) {
                        for antecedent in antecedents {
                            if let Some(flow_successors) = successors.get_mut(antecedent.0 as usize)
                            {
                                flow_successors.push(node.id);
                            }
                        }
                    }
                }
            }
        }
        for reachable in narrowing_capability.condition_flows_by_path.values_mut() {
            let mut pending = reachable.iter().copied().collect::<Vec<_>>();
            while let Some(flow_id) = pending.pop() {
                let Some(flow_successors) = successors.get(flow_id.0 as usize) else {
                    continue;
                };
                for successor in flow_successors {
                    if reachable.insert(*successor) {
                        pending.push(*successor);
                    }
                }
            }
        }
        Self {
            decl_bind_expr_ref,
            flow_nodes,
            multiple_antecedents,
            bindings,
            branch_label_info,
            assignment_flow_info,
            narrowing_capability,
        }
    }

    pub fn get_narrowing_capability(&self) -> &FileNarrowingCapability {
        &self.narrowing_capability
    }

    pub fn get_flow_id(&self, syntax_id: LuaSyntaxId) -> Option<FlowId> {
        self.bindings.get(&syntax_id).cloned()
    }

    pub fn get_flow_node(&self, flow_id: FlowId) -> Option<&FlowNode> {
        self.flow_nodes.get(flow_id.0 as usize)
    }

    pub fn has_condition_path_antecedent(
        &self,
        flow_id: FlowId,
        path: &ArcIntern<SmolStr>,
    ) -> bool {
        self.narrowing_capability
            .condition_flows(path)
            .is_some_and(|flows| flows.contains(&flow_id))
    }

    pub fn get_multi_antecedents(&self, id: u32) -> Option<&[FlowId]> {
        self.multiple_antecedents
            .get(id as usize)
            .map(|v| v.as_slice())
    }

    pub fn get_decl_ref_expr(&self, decl_id: &LuaDeclId) -> Option<LuaAstPtr<LuaExpr>> {
        self.decl_bind_expr_ref.get(decl_id).cloned()
    }

    pub fn get_branch_label_info(&self, flow_id: FlowId) -> Option<&BranchLabelInfo> {
        self.branch_label_info.get(&flow_id)
    }

    pub fn get_assignment_flow_info(&self, flow_id: FlowId) -> Option<&AssignmentFlowInfo> {
        let info = self.assignment_flow_info.get(flow_id.0 as usize)?;
        (!info.is_empty()).then_some(info)
    }

    pub fn reaching_definitions(
        &self,
        decl_id: LuaDeclId,
        flow_id: FlowId,
    ) -> std::sync::Arc<[LuaDefinitionId]> {
        let mut definitions = Vec::new();
        let mut pending = vec![flow_id];
        let mut visited = HashSet::new();

        while let Some(current) = pending.pop() {
            if !visited.insert(current) {
                continue;
            }
            let Some(node) = self.get_flow_node(current) else {
                continue;
            };

            if let FlowNodeKind::Assignment(assignment, _) = &node.kind
                && let Some(info) = self.get_assignment_flow_info(current)
                && let Some(target) = info
                    .name_targets
                    .iter()
                    .find(|target| target.decl_id == decl_id)
            {
                definitions.push(LuaDefinitionId::Assignment {
                    file_id: decl_id.file_id,
                    assignment: assignment.get_syntax_id(),
                    target_idx: target.target_idx,
                });
                continue;
            }

            if matches!(&node.kind, FlowNodeKind::DeclPosition(position) if *position == decl_id.position)
                || matches!(&node.kind, FlowNodeKind::Start)
            {
                definitions.push(LuaDefinitionId::Declaration(decl_id));
                continue;
            }

            match node.antecedent {
                Some(crate::FlowAntecedent::Single(antecedent)) => pending.push(antecedent),
                Some(crate::FlowAntecedent::Multiple(index)) => {
                    if let Some(antecedents) = self.get_multi_antecedents(index) {
                        pending.extend(antecedents.iter().copied());
                    }
                }
                None => definitions.push(LuaDefinitionId::Declaration(decl_id)),
            }
        }

        definitions.sort_by(LuaDefinitionId::stable_cmp);
        definitions.dedup();
        definitions.into()
    }
}
