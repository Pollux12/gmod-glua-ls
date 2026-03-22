use std::collections::HashMap;

use glua_parser::{LuaAstPtr, LuaExpr, LuaSyntaxId};

use crate::{FlowId, FlowNode, LuaDeclId};

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
}

impl FlowTree {
    pub fn new(
        decl_bind_expr_ref: HashMap<LuaDeclId, LuaAstPtr<LuaExpr>>,
        flow_nodes: Vec<FlowNode>,
        multiple_antecedents: Vec<Vec<FlowId>>,
        // labels: HashMap<LuaClosureId, HashMap<SmolStr, FlowId>>,
        bindings: HashMap<LuaSyntaxId, FlowId>,
        branch_label_info: HashMap<FlowId, BranchLabelInfo>,
    ) -> Self {
        Self {
            decl_bind_expr_ref,
            flow_nodes,
            multiple_antecedents,
            bindings,
            branch_label_info,
        }
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
}
