use std::collections::{HashMap, HashSet};

use glua_parser::{LuaAstPtr, LuaExpr, LuaSyntaxId};
use internment::ArcIntern;
use smol_str::SmolStr;

use crate::{FlowId, FlowNode, LuaDeclId};

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
    pub index_paths: Vec<ArcIntern<SmolStr>>,
    pub has_unknown_index_target: bool,
}

impl AssignmentFlowInfo {
    pub fn is_empty(&self) -> bool {
        self.index_paths.is_empty() && !self.has_unknown_index_target
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
        narrowing_capability: FileNarrowingCapability,
    ) -> Self {
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
}
