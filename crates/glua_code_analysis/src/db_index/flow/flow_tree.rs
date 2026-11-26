use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::hash::{Hash, Hasher};

use glua_parser::{LuaAstPtr, LuaExpr, LuaSyntaxId};
use internment::ArcIntern;
use smol_str::SmolStr;

use crate::db_index::derivation::{self, Fingerprint, Local};
use crate::{FlowId, FlowNode, FlowNodeKind, LuaDeclId, LuaDefinitionId};

/// Order-independent hash of a set's elements.
fn unordered_fingerprint<T: Hash>(items: impl IntoIterator<Item = T>) -> Fingerprint {
    items
        .into_iter()
        .map(|item| derivation::fingerprint_of(&item))
        .fold(0, u64::wrapping_add)
}

/// Compact sorted slice of interned names. Empty sets hold zero heap memory
/// (16 bytes inline vs 48 bytes + table allocation for `FxHashSet`).
#[derive(Debug, Clone, Default)]
pub struct CompactNameSet(Box<[ArcIntern<SmolStr>]>);

impl CompactNameSet {
    pub fn from_vec(mut items: Vec<ArcIntern<SmolStr>>) -> Self {
        if items.is_empty() {
            return Self(Box::new([]));
        }
        items.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        items.dedup();
        Self(items.into_boxed_slice())
    }

    pub fn from_iter(items: impl IntoIterator<Item = ArcIntern<SmolStr>>) -> Self {
        Self::from_vec(items.into_iter().collect())
    }

    pub fn contains(&self, name: &ArcIntern<SmolStr>) -> bool {
        if self.0.len() <= 8 {
            self.0.iter().any(|item| item == name)
        } else {
            self.0
                .binary_search_by(|item| item.as_str().cmp(name.as_str()))
                .is_ok()
        }
    }

    pub fn contains_str(&self, name: &str) -> bool {
        if self.0.len() <= 8 {
            self.0.iter().any(|item| item.as_str() == name)
        } else {
            self.0
                .binary_search_by(|item| item.as_str().cmp(name))
                .is_ok()
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ArcIntern<SmolStr>> {
        self.0.iter()
    }
}

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
    pub referenced_names: CompactNameSet,
    /// Access paths (e.g. `self.foo`, `tbl.a.b`) appearing in any assignment
    /// target, cast, or condition expression. Covers `IndexRef` references.
    pub referenced_index_paths: CompactNameSet,
    /// When true, a narrowing site referenced a name/index we could not reduce
    /// to a stable key (e.g. computed index). Disables name/index skipping
    /// respectively to stay sound.
    pub has_opaque_name_target: bool,
    pub has_opaque_index_target: bool,
}

/// Names hash by their text: an interned name hashes its pointer, which a name
/// freed and interned again does not keep.
impl Hash for FileNarrowingCapability {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unordered_fingerprint(self.referenced_names.iter().map(|name| name.as_str())).hash(state);
        unordered_fingerprint(self.referenced_index_paths.iter().map(|path| path.as_str()))
            .hash(state);
        self.has_opaque_name_target.hash(state);
        self.has_opaque_index_target.hash(state);
    }
}

impl FileNarrowingCapability {
    pub fn from_parts(
        referenced_names: CompactNameSet,
        referenced_index_paths: CompactNameSet,
        has_opaque_name_target: bool,
        has_opaque_index_target: bool,
    ) -> Self {
        Self {
            referenced_names,
            referenced_index_paths,
            has_opaque_name_target,
            has_opaque_index_target,
        }
    }

    /// Whether a bare-name reference (`VarRef`/`SelfRef`/`GlobalName`) named
    /// `name` could be narrowed somewhere in the file.
    pub fn name_can_be_narrowed(&self, name: &ArcIntern<SmolStr>) -> bool {
        self.has_opaque_name_target || self.referenced_names.contains(name)
    }

    /// Borrowed `&str` counterpart of [`Self::name_can_be_narrowed`].
    pub fn name_str_can_be_narrowed(&self, name: &str) -> bool {
        self.has_opaque_name_target || self.referenced_names.contains_str(name)
    }

    /// Whether an index reference with access `path` could be narrowed.
    pub fn index_path_can_be_narrowed(&self, path: &ArcIntern<SmolStr>) -> bool {
        self.has_opaque_index_target || self.referenced_index_paths.contains(path)
    }
}

/// Metadata for BranchLabel nodes that enables the merge-skip optimisation.
///
/// When the backward flow walk hits a BranchLabel, it normally merges the types
/// from every antecedent branch.  For variables NOT modified in any branch (and
/// all branches are alive), the merge is guaranteed to produce the same type as
/// the node before the branch (`common_predecessor`).  The walk can skip
/// directly to that predecessor, turning an O(branches × depth) merge into O(1).
#[derive(Debug, Clone, Hash)]
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
    /// The part of `narrowing_capability` the branches write rather than only
    /// test: a variable outside it keeps the value it had at
    /// `common_predecessor` on every branch, narrowed at most.
    pub written_capability: FileNarrowingCapability,
}

#[derive(Debug, Clone, Default)]
pub struct AssignmentFlowInfo {
    pub name_targets: Vec<AssignmentNameTarget>,
    pub index_paths: Vec<ArcIntern<SmolStr>>,
    pub has_unknown_index_target: bool,
}

/// Paths hash by their text, as in [`FileNarrowingCapability`].
impl Hash for AssignmentFlowInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name_targets.hash(state);
        self.index_paths.len().hash(state);
        for path in &self.index_paths {
            path.as_str().hash(state);
        }
        self.has_unknown_index_target.hash(state);
    }
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

    fn node(kind: FlowNodeKind, antecedent: Option<FlowAntecedent>) -> FlowNode {
        FlowNode { kind, antecedent }
    }

    fn tree(
        nodes: Vec<FlowNode>,
        branches: Vec<Vec<FlowId>>,
        infos: Vec<AssignmentFlowInfo>,
    ) -> FlowTree {
        FlowTree::new(
            HashMap::default(),
            nodes,
            branches,
            HashMap::default(),
            HashMap::default(),
            HashMap::default(),
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
            node(FlowNodeKind::Start, None),
            node(
                FlowNodeKind::DeclPosition(decl_id.position),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                FlowNodeKind::Assignment(first, AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(1))),
            ),
            node(
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
            node(FlowNodeKind::Start, None),
            node(
                FlowNodeKind::Assignment(right, AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                FlowNodeKind::Assignment(left, AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(FlowNodeKind::BranchLabel, Some(FlowAntecedent::Multiple(0))),
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
            node(FlowNodeKind::Start, None),
            node(
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
            node(FlowNodeKind::Start, None),
            node(
                FlowNodeKind::Assignment(assignment.clone(), AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
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
            node(FlowNodeKind::Start, None),
            node(
                FlowNodeKind::Assignment(assignment.clone(), AssignVarHint::NameOnly),
                Some(FlowAntecedent::Single(FlowId(0))),
            ),
            node(
                FlowNodeKind::ClosureEntry(TextSize::new(12)),
                Some(FlowAntecedent::Single(FlowId(1))),
            ),
            node(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone)]
pub struct FlowTree {
    decl_bind_expr_ref: HashMap<LuaDeclId, LuaAstPtr<LuaExpr>>,
    flow_nodes: Box<[FlowNode]>,
    multiple_antecedents: Vec<Box<[FlowId]>>,
    // labels: HashMap<LuaClosureId, HashMap<SmolStr, FlowId>>,
    bindings: HashMap<LuaSyntaxId, FlowId>,
    /// Per-BranchLabel metadata used to skip redundant merges.
    branch_label_info: HashMap<FlowId, BranchLabelInfo>,
    /// Branch labels reached only by one condition's outcomes, mapped to the
    /// node the condition is evaluated from.
    condition_label_entries: HashMap<FlowId, FlowId>,
    /// Sparse sorted `(FlowId, AssignmentFlowInfo)` entries (non-empty only).
    assignment_flow_info: Box<[(FlowId, AssignmentFlowInfo)]>,
    narrowing_capability: FileNarrowingCapability,
}

struct IndexedFlowNodes<'a>(&'a [FlowNode]);

impl Hash for IndexedFlowNodes<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        for (index, node) in self.0.iter().enumerate() {
            FlowId(index as u32).hash(state);
            node.hash(state);
        }
    }
}

impl FlowTree {
    pub fn new(
        mut decl_bind_expr_ref: HashMap<LuaDeclId, LuaAstPtr<LuaExpr>>,
        flow_nodes: Vec<FlowNode>,
        multiple_antecedents: Vec<Vec<FlowId>>,
        // labels: HashMap<LuaClosureId, HashMap<SmolStr, FlowId>>,
        mut bindings: HashMap<LuaSyntaxId, FlowId>,
        mut branch_label_info: HashMap<FlowId, BranchLabelInfo>,
        mut condition_label_entries: HashMap<FlowId, FlowId>,
        assignment_flow_info: Vec<AssignmentFlowInfo>,
        narrowing_capability: FileNarrowingCapability,
    ) -> Self {
        decl_bind_expr_ref.shrink_to_fit();
        bindings.shrink_to_fit();
        branch_label_info.shrink_to_fit();
        condition_label_entries.shrink_to_fit();
        let mut multiple_antecedents = multiple_antecedents
            .into_iter()
            .map(Vec::into_boxed_slice)
            .collect::<Vec<_>>();
        multiple_antecedents.shrink_to_fit();
        let assignment_flow_info = assignment_flow_info
            .into_iter()
            .enumerate()
            .filter_map(|(idx, info)| (!info.is_empty()).then_some((FlowId(idx as u32), info)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            decl_bind_expr_ref,
            flow_nodes: flow_nodes.into_boxed_slice(),
            multiple_antecedents,
            bindings,
            branch_label_info,
            condition_label_entries,
            assignment_flow_info,
            narrowing_capability,
        }
    }

    /// Content hash of everything the tree's accessors can observe, with its
    /// positions hashed as positions of the file being fingerprinted.
    pub fn content_fingerprint(&self) -> Fingerprint {
        derivation::fingerprint_of(&(
            unordered_fingerprint(
                self.decl_bind_expr_ref
                    .iter()
                    .map(|(decl_id, ptr)| (decl_id, Local(&ptr.syntax_id))),
            ),
            IndexedFlowNodes(&self.flow_nodes),
            &self.multiple_antecedents,
            unordered_fingerprint(
                self.bindings
                    .iter()
                    .map(|(syntax_id, flow_id)| (Local(syntax_id), flow_id)),
            ),
            unordered_fingerprint(&self.branch_label_info),
            unordered_fingerprint(&self.condition_label_entries),
            &self.assignment_flow_info,
            &self.narrowing_capability,
        ))
    }

    /// How many name and index expressions the file's flow analysis bound.
    pub fn binding_count(&self) -> usize {
        self.bindings.len()
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

    pub fn get_multi_antecedents(&self, id: u32) -> Option<&[FlowId]> {
        self.multiple_antecedents
            .get(id as usize)
            .map(|antecedents| &**antecedents)
    }

    pub fn get_decl_ref_expr(&self, decl_id: &LuaDeclId) -> Option<LuaAstPtr<LuaExpr>> {
        self.decl_bind_expr_ref.get(decl_id).cloned()
    }

    pub fn get_branch_label_info(&self, flow_id: FlowId) -> Option<&BranchLabelInfo> {
        self.branch_label_info.get(&flow_id)
    }

    pub fn get_condition_label_entry(&self, flow_id: FlowId) -> Option<FlowId> {
        self.condition_label_entries.get(&flow_id).copied()
    }

    pub fn get_assignment_flow_info(&self, flow_id: FlowId) -> Option<&AssignmentFlowInfo> {
        let idx = self
            .assignment_flow_info
            .binary_search_by_key(&flow_id.0, |(id, _)| id.0)
            .ok()?;
        Some(&self.assignment_flow_info[idx].1)
    }

    /// Whether every live path back from `flow_id` meets a node that `settle`
    /// answers `Some(true)` for before it meets one answering `Some(false)`, the
    /// start of the flow, or a deferred closure's entry.
    pub fn every_path_settles(
        &self,
        flow_id: FlowId,
        mut settle: impl FnMut(&FlowNode) -> Option<bool>,
    ) -> bool {
        let mut pending = vec![flow_id];
        let mut visited = HashSet::default();

        while let Some(current) = pending.pop() {
            if !visited.insert(current) {
                continue;
            }
            let Some(node) = self.get_flow_node(current) else {
                return false;
            };
            if let Some(settled) = settle(node) {
                if settled {
                    continue;
                }
                return false;
            }

            match node.kind {
                FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break => continue,
                FlowNodeKind::Start | FlowNodeKind::ClosureEntry(_) => return false,
                _ => {}
            }
            match node.antecedent {
                Some(crate::FlowAntecedent::Single(antecedent)) => pending.push(antecedent),
                Some(crate::FlowAntecedent::Multiple(index)) => {
                    let Some(antecedents) = self.get_multi_antecedents(index) else {
                        return false;
                    };
                    pending.extend(antecedents.iter().copied());
                }
                None => return false,
            }
        }
        true
    }

    pub fn reaching_definitions(
        &self,
        decl_id: LuaDeclId,
        flow_id: FlowId,
    ) -> std::sync::Arc<[LuaDefinitionId]> {
        let mut definitions = Vec::new();
        let mut pending = vec![flow_id];
        let mut visited = HashSet::default();

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
