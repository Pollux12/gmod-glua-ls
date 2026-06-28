use std::collections::HashMap;

use glua_parser::{LuaAstPtr, LuaExpr, LuaNameToken, LuaSyntaxId};
use internment::ArcIntern;
use rowan::TextSize;
use smol_str::SmolStr;

use crate::{
    AnalyzeError, AssignVarHint, AssignmentFlowInfo, BranchLabelInfo, DbIndex, FileId,
    FileNarrowingCapability, FlowAntecedent, FlowId, FlowNode, FlowNodeKind, FlowTree,
    LuaClosureId, LuaDeclId,
};

/// Snapshot of the modification counters, used to detect what was created
/// between two points during flow-graph construction.
#[derive(Debug, Clone, Copy)]
pub struct ModificationSnapshot {
    pub name_assign_count: u32,
    pub index_assign_count: u32,
    pub cast_or_implfunc_count: u32,
    pub condition_count: u32,
    pub narrowing_event_count: usize,
}

#[derive(Debug, Clone)]
enum NarrowingEvent {
    Name(ArcIntern<SmolStr>),
    IndexPath(ArcIntern<SmolStr>),
    OpaqueName,
    OpaqueIndex,
}

#[derive(Debug)]
pub struct FlowBinder<'a> {
    pub db: &'a DbIndex,
    pub file_id: FileId,
    /// Errors accumulated during binding. Collected here (rather than written
    /// straight to the diagnostic index) so flow binding can run with only an
    /// immutable `&DbIndex`, enabling parallel per-file binding. The pipeline
    /// drains these into the diagnostic index sequentially afterward.
    pub errors: Vec<AnalyzeError>,
    pub decl_bind_expr_ref: HashMap<LuaDeclId, LuaAstPtr<LuaExpr>>,
    pub start: FlowId,
    pub unreachable: FlowId,
    pub loop_label: FlowId,
    pub break_target_label: FlowId,
    pub true_target: FlowId,
    pub false_target: FlowId,
    flow_nodes: Vec<FlowNode>,
    multiple_antecedents: Vec<Vec<FlowId>>,
    labels: HashMap<LuaClosureId, HashMap<SmolStr, FlowId>>,
    goto_stats: Vec<GotoCache>,
    bindings: HashMap<LuaSyntaxId, FlowId>,
    branch_label_info: HashMap<FlowId, BranchLabelInfo>,
    assignment_flow_info: Vec<AssignmentFlowInfo>,
    // Counters for tracking modifications inside branch blocks.
    name_assign_count: u32,
    index_assign_count: u32,
    cast_or_implfunc_count: u32,
    condition_count: u32,
    // File-wide narrowing capability (which names/paths can be narrowed).
    narrowing_capability: FileNarrowingCapability,
    narrowing_events: Vec<NarrowingEvent>,
}

impl<'a> FlowBinder<'a> {
    pub fn new(db: &'a DbIndex, file_id: FileId) -> Self {
        let mut binder = FlowBinder {
            db,
            file_id,
            errors: Vec::new(),
            flow_nodes: Vec::new(),
            multiple_antecedents: Vec::new(),
            decl_bind_expr_ref: HashMap::new(),
            labels: HashMap::new(),
            start: FlowId::default(),
            unreachable: FlowId::default(),
            break_target_label: FlowId::default(),
            bindings: HashMap::new(),
            goto_stats: Vec::new(),
            loop_label: FlowId::default(),
            true_target: FlowId::default(),
            false_target: FlowId::default(),
            branch_label_info: HashMap::new(),
            assignment_flow_info: Vec::new(),
            name_assign_count: 0,
            index_assign_count: 0,
            cast_or_implfunc_count: 0,
            condition_count: 0,
            narrowing_capability: FileNarrowingCapability::default(),
            narrowing_events: Vec::new(),
        };

        binder.start = binder.create_start();
        binder.unreachable = binder.create_unreachable();
        binder.break_target_label = binder.unreachable;
        binder.loop_label = binder.unreachable;
        binder.true_target = binder.unreachable;
        binder.false_target = binder.unreachable;

        binder
    }

    pub fn create_node(&mut self, kind: FlowNodeKind) -> FlowId {
        // Track modifications for the BranchLabel merge-skip optimisation.
        match &kind {
            FlowNodeKind::Assignment(_, hint) => match hint {
                AssignVarHint::NameOnly => self.name_assign_count += 1,
                AssignVarHint::IndexOnly => self.index_assign_count += 1,
                AssignVarHint::Mixed => {
                    self.name_assign_count += 1;
                    self.index_assign_count += 1;
                }
            },
            FlowNodeKind::ImplFunc(_) | FlowNodeKind::TagCast(_) => {
                self.cast_or_implfunc_count += 1;
            }
            FlowNodeKind::TrueCondition(_) | FlowNodeKind::FalseCondition(_) => {
                self.condition_count += 1;
            }
            _ => {}
        }

        let id = FlowId(self.flow_nodes.len() as u32);
        let flow_node = FlowNode {
            id,
            kind,
            antecedent: None,
        };
        self.flow_nodes.push(flow_node);
        self.assignment_flow_info
            .push(AssignmentFlowInfo::default());
        id
    }

    pub fn create_branch_label(&mut self) -> FlowId {
        self.create_node(FlowNodeKind::BranchLabel)
    }

    pub fn create_loop_label(&mut self) -> FlowId {
        self.create_node(FlowNodeKind::LoopLabel)
    }

    pub fn create_name_label(&mut self, name: &str, closure_id: LuaClosureId) -> FlowId {
        let label_id = self.create_node(FlowNodeKind::NamedLabel(ArcIntern::from(SmolStr::new(
            name,
        ))));
        self.labels
            .entry(closure_id)
            .or_default()
            .insert(SmolStr::new(name), label_id);

        label_id
    }

    pub fn get_label(&self, closure_id: LuaClosureId, name: &str) -> Option<FlowId> {
        self.labels
            .get(&closure_id)
            .and_then(|labels| labels.get(name).copied())
    }

    pub fn create_start(&mut self) -> FlowId {
        self.create_node(FlowNodeKind::Start)
    }

    pub fn create_unreachable(&mut self) -> FlowId {
        self.create_node(FlowNodeKind::Unreachable)
    }

    pub fn create_break(&mut self) -> FlowId {
        self.create_node(FlowNodeKind::Break)
    }

    pub fn create_return(&mut self) -> FlowId {
        self.create_node(FlowNodeKind::Return)
    }

    pub fn create_decl(&mut self, position: TextSize) -> FlowId {
        self.create_node(FlowNodeKind::DeclPosition(position))
    }

    pub fn add_antecedent(&mut self, node_id: FlowId, antecedent: FlowId) {
        if antecedent == self.unreachable || node_id == self.unreachable {
            // If the antecedent is the unreachable node, we don't need to add it
            return;
        }

        if let Some(existing) = self.flow_nodes.get_mut(node_id.0 as usize) {
            match existing.antecedent {
                Some(FlowAntecedent::Single(existing_id)) => {
                    // If the existing antecedent is a single node, convert it to multiple
                    if existing_id == antecedent {
                        return; // No change needed if it's the same antecedent
                    }
                    existing.antecedent = Some(FlowAntecedent::Multiple(
                        self.multiple_antecedents.len() as u32,
                    ));
                    self.multiple_antecedents
                        .push(vec![existing_id, antecedent]);
                }
                Some(FlowAntecedent::Multiple(index)) => {
                    // Add to multiple antecedents
                    if let Some(multiple) = self.multiple_antecedents.get_mut(index as usize) {
                        multiple.push(antecedent);
                    } else {
                        self.multiple_antecedents.push(vec![antecedent]);
                    }
                }
                _ => {
                    // Set new antecedent
                    existing.antecedent = Some(FlowAntecedent::Single(antecedent));
                }
            };
        }
    }

    pub fn bind_syntax_node(&mut self, syntax_id: LuaSyntaxId, flow_id: FlowId) {
        self.bindings.insert(syntax_id, flow_id);
    }

    pub fn get_bind_flow(&self, syntax_id: LuaSyntaxId) -> Option<FlowId> {
        self.bindings.get(&syntax_id).copied()
    }

    pub fn cache_goto_flow(
        &mut self,
        closure_id: LuaClosureId,
        label_token: LuaNameToken,
        label: &str,
        flow_id: FlowId,
    ) {
        self.goto_stats.push(GotoCache {
            closure_id,
            label_token,
            label: SmolStr::new(label),
            flow_id,
        });
    }

    pub fn get_goto_caches(&mut self) -> Vec<GotoCache> {
        self.goto_stats.drain(..).collect()
    }

    pub fn get_flow(&self, flow_id: FlowId) -> Option<&FlowNode> {
        self.flow_nodes.get(flow_id.0 as usize)
    }

    /// Snapshot the current modification counters so we can later detect what
    /// Assignment / ImplFunc / TagCast nodes were added during a block binding.
    pub fn save_modification_counts(&self) -> ModificationSnapshot {
        ModificationSnapshot {
            name_assign_count: self.name_assign_count,
            index_assign_count: self.index_assign_count,
            cast_or_implfunc_count: self.cast_or_implfunc_count,
            condition_count: self.condition_count,
            narrowing_event_count: self.narrowing_events.len(),
        }
    }

    /// Compare the current counters against a previous snapshot and return
    /// (has_name_assigns, has_index_assigns, has_casts_or_implfunc, has_conditions).
    pub fn check_new_modifications(&self, snap: ModificationSnapshot) -> (bool, bool, bool, bool) {
        (
            self.name_assign_count > snap.name_assign_count,
            self.index_assign_count > snap.index_assign_count,
            self.cast_or_implfunc_count > snap.cast_or_implfunc_count,
            self.condition_count > snap.condition_count,
        )
    }

    pub fn narrowing_capability_since(
        &self,
        snap: ModificationSnapshot,
    ) -> FileNarrowingCapability {
        let mut capability = FileNarrowingCapability::default();
        for event in self
            .narrowing_events
            .iter()
            .skip(snap.narrowing_event_count)
        {
            match event {
                NarrowingEvent::Name(name) => {
                    capability.referenced_names.insert(name.clone());
                }
                NarrowingEvent::IndexPath(path) => {
                    capability.referenced_index_paths.insert(path.clone());
                }
                NarrowingEvent::OpaqueName => {
                    capability.has_opaque_name_target = true;
                }
                NarrowingEvent::OpaqueIndex => {
                    capability.has_opaque_index_target = true;
                }
            }
        }
        capability
    }

    /// Record merge-skip metadata for a BranchLabel created by an if/elseif/else.
    pub fn set_branch_label_info(&mut self, label_id: FlowId, info: BranchLabelInfo) {
        self.branch_label_info.insert(label_id, info);
    }

    pub fn set_assignment_flow_info(&mut self, flow_id: FlowId, info: AssignmentFlowInfo) {
        if !info.is_empty()
            && let Some(slot) = self.assignment_flow_info.get_mut(flow_id.0 as usize)
        {
            *slot = info;
        }
    }

    pub fn report_error(&mut self, error: AnalyzeError) {
        self.errors.push(error);
    }

    /// Record a bare name that can be narrowed at some site (assignment target,
    /// cast, or condition expression).
    pub fn record_narrowable_name(&mut self, name: &str) {
        let name = ArcIntern::from(SmolStr::new(name));
        self.narrowing_capability
            .referenced_names
            .insert(name.clone());
        self.narrowing_events.push(NarrowingEvent::Name(name));
    }

    /// Record an index access path that can be narrowed.
    pub fn record_narrowable_index_path(&mut self, path: &str) {
        let path = ArcIntern::from(SmolStr::new(path));
        self.narrowing_capability
            .referenced_index_paths
            .insert(path.clone());
        self.narrowing_events.push(NarrowingEvent::IndexPath(path));
    }

    pub fn mark_opaque_name_target(&mut self) {
        self.narrowing_capability.has_opaque_name_target = true;
        self.narrowing_events.push(NarrowingEvent::OpaqueName);
    }

    pub fn mark_opaque_index_target(&mut self) {
        self.narrowing_capability.has_opaque_index_target = true;
        self.narrowing_events.push(NarrowingEvent::OpaqueIndex);
    }

    /// Walk an expression subtree and record every name / index access path it
    /// references as narrowable. Used for condition and cast expressions, which
    /// can narrow any variable they mention.
    pub fn record_narrowable_refs_in_expr(&mut self, expr: &LuaExpr) {
        use glua_parser::{LuaAstNode, LuaIndexExpr, LuaNameExpr, PathTrait};
        // Record the expr itself if it is a name or index, then recurse into
        // descendants to cover nested references (e.g. `a.b and c(d.e)`).
        for node in expr.syntax().descendants() {
            if let Some(name_expr) = LuaNameExpr::cast(node.clone()) {
                if let Some(name) = name_expr.get_name_text() {
                    self.record_narrowable_name(&name);
                } else {
                    self.mark_opaque_name_target();
                }
            } else if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
                let dynamic = matches!(
                    index_expr.get_index_key(),
                    Some(glua_parser::LuaIndexKey::Expr(_))
                );
                match index_expr.get_access_path() {
                    Some(path) if !dynamic => self.record_narrowable_index_path(&path),
                    _ => self.mark_opaque_index_target(),
                }
            }
        }
    }

    pub fn finish(self) -> (FlowTree, Vec<AnalyzeError>) {
        let flow_tree = FlowTree::new(
            self.decl_bind_expr_ref,
            self.flow_nodes,
            self.multiple_antecedents,
            // self.labels,
            self.bindings,
            self.branch_label_info,
            self.assignment_flow_info,
            self.narrowing_capability,
        );
        (flow_tree, self.errors)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GotoCache {
    pub closure_id: LuaClosureId,
    pub label_token: LuaNameToken,
    pub label: SmolStr,
    pub flow_id: FlowId,
}
