mod binary_flow;
mod call_flow;
mod index_flow;

pub(crate) use call_flow::presence_guard_target;

use std::sync::Arc;

use glua_parser::{LuaAstNode, LuaChunk, LuaExpr, LuaNameExpr, LuaUnaryExpr, UnaryOperator};

use crate::{
    DbIndex, FlowNode, FlowTree, InferFailReason, LuaInferCache,
    semantic::infer::{
        InferResult, VarRefId,
        narrow::{
            ResultTypeOrContinue,
            condition_flow::{
                binary_flow::{call_alias_holds, get_type_at_binary_expr},
                call_flow::get_type_at_call_expr,
                index_flow::get_type_at_index_expr,
            },
            get_single_antecedent,
            get_type_at_flow::{FlowWalkPolicy, get_type_at_flow_in_mode},
            narrow_direct_name_false_or_nil, remove_false_or_nil,
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferConditionFlow {
    TrueCondition,
    FalseCondition,
}

impl InferConditionFlow {
    pub fn get_negated(&self) -> Self {
        match self {
            InferConditionFlow::TrueCondition => InferConditionFlow::FalseCondition,
            InferConditionFlow::FalseCondition => InferConditionFlow::TrueCondition,
        }
    }

    #[allow(unused)]
    pub fn is_true(&self) -> bool {
        matches!(self, InferConditionFlow::TrueCondition)
    }

    pub fn is_false(&self) -> bool {
        matches!(self, InferConditionFlow::FalseCondition)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn get_type_at_condition_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    condition: LuaExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    use crate::db_index::derivation;
    use crate::semantic::cache::{FlowConditionKey, VarRefCacheKey};

    let var_key = VarRefCacheKey::from(var_ref_id);
    let (origin, baseline) = policy.memo_policy();
    let config = cache.get_config();
    let key = FlowConditionKey {
        var_ref: var_ref_id.clone(),
        flow: flow_node.id,
        antecedent: flow_node.antecedent.clone(),
        view: (
            config.analysis_phase as u8,
            config.dynamic_fields_visible,
            config.call_site_params_visible,
        ),
        syntax: condition.get_syntax_id(),
        outcome: condition_flow.is_true(),
        realm: cache.query_realm(),
        origin,
        baseline,
        keyed_origin: cache.keyed_origin(&var_key).cloned(),
    };
    cache.watch_reuse();
    if let Some(cached) = cache.tracked_get(|cache| &mut cache.condition_flow_cache, &key) {
        let result = cached.as_ref().clone();
        cache.check_reuse(&key, &result, |fresh| {
            get_type_at_condition_flow_uncached(
                db,
                tree,
                fresh,
                root,
                var_ref_id,
                flow_node,
                condition.clone(),
                condition_flow,
                policy,
            )
            .ok()
        });
        return Ok(result);
    }
    let frame = derivation::open_frame_absorbing(LuaInferCache::flow_context_key(&var_key));
    db.get_flow_index().get_flow_tree(&cache.get_file_id());
    let result = get_type_at_condition_flow_uncached(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        condition,
        condition_flow,
        policy,
    );
    let provenance = cache.close_frame(frame);
    if let (Ok(result), Some(provenance)) = (&result, provenance) {
        cache
            .condition_flow_cache
            .insert(key, Arc::new(result.clone()), Some(provenance));
    }
    result
}

#[cfg(test)]
thread_local! {
    static CONDITION_COMPUTATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_condition_flow_uncached(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    condition: LuaExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    #[cfg(test)]
    CONDITION_COMPUTATIONS.with(|count| count.set(count.get() + 1));
    match condition {
        LuaExpr::NameExpr(name_expr) => get_type_at_name_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            name_expr,
            condition_flow,
            policy,
        ),
        LuaExpr::CallExpr(call_expr) => get_type_at_call_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            call_expr,
            condition_flow,
            policy,
        ),
        LuaExpr::IndexExpr(index_expr) => get_type_at_index_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            index_expr,
            condition_flow,
            policy,
        ),
        LuaExpr::TableExpr(_) | LuaExpr::LiteralExpr(_) | LuaExpr::ClosureExpr(_) => {
            Ok(ResultTypeOrContinue::Continue)
        }
        LuaExpr::BinaryExpr(binary_expr) => get_type_at_binary_expr(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            binary_expr,
            condition_flow,
            policy,
        ),
        LuaExpr::UnaryExpr(unary_expr) => get_type_at_unary_flow(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            unary_expr,
            condition_flow,
            policy,
        ),
        LuaExpr::ParenExpr(paren_expr) => {
            let Some(inner_expr) = paren_expr.get_expr() else {
                return Ok(ResultTypeOrContinue::Continue);
            };

            get_type_at_condition_flow(
                db,
                tree,
                cache,
                root,
                var_ref_id,
                flow_node,
                inner_expr,
                condition_flow,
                policy,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_name_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    name_expr: LuaNameExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(name_var_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(name_expr.clone()))
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if name_var_ref_id != *var_ref_id {
        return get_type_at_name_ref(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            name_expr,
            condition_flow,
            policy,
        );
    }

    let antecedent_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;

    let result_type = match condition_flow {
        InferConditionFlow::FalseCondition => narrow_direct_name_false_or_nil(db, antecedent_type),
        InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_name_ref(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    name_expr: LuaNameExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(expr) = tree
        .get_decl_ref_expr(&decl_id)
        .and_then(|expr_ptr| expr_ptr.to_node(root))
    else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let result = get_type_at_condition_flow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        expr.clone(),
        condition_flow,
        policy,
    )?;
    if matches!(result, ResultTypeOrContinue::Result(_))
        && let LuaExpr::CallExpr(call_expr) = &expr
        && !call_alias_holds(
            db,
            cache,
            root,
            var_ref_id,
            call_expr,
            decl_id.position,
            &name_expr,
        )
    {
        return Ok(ResultTypeOrContinue::Continue);
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_unary_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    unary_expr: LuaUnaryExpr,
    condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(inner_expr) = unary_expr.get_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let Some(op) = unary_expr.get_op_token() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    match op.get_op() {
        UnaryOperator::OpNot => {}
        _ => {
            return Ok(ResultTypeOrContinue::Continue);
        }
    }

    get_type_at_condition_flow(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        flow_node,
        inner_expr,
        condition_flow.get_negated(),
        policy,
    )
}

pub(super) fn get_condition_antecedent_type(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    policy: FlowWalkPolicy,
) -> InferResult {
    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    get_type_at_flow_in_mode(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        antecedent_flow_id,
        policy,
    )
}

#[cfg(test)]
mod memo_tests {
    use super::*;
    use crate::db_index::derivation::{self, DerivationId, DerivationKind};
    use crate::semantic::cache::FlowOrigin;
    use crate::{FlowAntecedent, FlowId, FlowNodeKind, LuaType, VirtualWorkspace};
    use glua_parser::LuaNameExpr;

    fn fixture() -> (
        VirtualWorkspace,
        crate::FileId,
        LuaChunk,
        LuaExpr,
        VarRefId,
        FlowNode,
    ) {
        let mut ws = VirtualWorkspace::new();
        let file = ws.def_file(
            "lua/autorun/client/condition_memo.lua",
            r#"
            ---@class Receiver
            local Receiver = {}
            function Receiver:Read()
                if self then return self end
            end
        "#,
        );
        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file)
            .unwrap()
            .get_chunk_node();
        let condition = LuaExpr::NameExpr(
            root.descendants::<LuaNameExpr>()
                .find(|name| name.get_name_text().as_deref() == Some("self"))
                .unwrap(),
        );
        let mut identity = LuaInferCache::new(file, Default::default());
        let var = get_var_expr_var_ref_id(db, &mut identity, condition.clone()).unwrap();
        let antecedent = db
            .get_flow_index()
            .get_flow_tree(&file)
            .unwrap()
            .get_flow_id(condition.get_syntax_id())
            .unwrap();
        let node = FlowNode {
            id: FlowId(999_999),
            kind: FlowNodeKind::TrueCondition(condition.to_ptr()),
            antecedent: Some(FlowAntecedent::Single(antecedent)),
        };
        (ws, file, root, condition, var, node)
    }

    #[allow(clippy::too_many_arguments)]
    fn seeded_condition(
        db: &DbIndex,
        tree: &FlowTree,
        cache: &mut LuaInferCache,
        root: &LuaChunk,
        var: &VarRefId,
        node: &FlowNode,
        condition: &LuaExpr,
        seed: LuaType,
        outcome: InferConditionFlow,
    ) -> Result<ResultTypeOrContinue, InferFailReason> {
        let previous_entries = cache.key_flow_cache(var, &seed);
        let previous_seed = cache.seed_self(var.clone(), seed.clone());
        let result = get_type_at_condition_flow(
            db,
            tree,
            cache,
            root,
            var,
            node,
            condition.clone(),
            outcome,
            FlowWalkPolicy::normal(FlowOrigin::Real),
        );
        cache.unkey_flow_cache(var, seed, previous_entries);
        cache.restore_self_seed(previous_seed);
        result
    }

    #[test]
    fn condition_memo_reuses_successes_and_separates_seeded_origins_and_outcomes() {
        let (mut ws, file, root, condition, var, node) = fixture();
        let db = ws.get_db_mut();
        let mut cache = LuaInferCache::new(file, Default::default());
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::Diagnostics));
        CONDITION_COMPUTATIONS.with(|count| count.set(0));
        let tree = db.get_flow_index().get_flow_tree(&file).unwrap();
        for (seed, outcome, expected) in [
            (
                LuaType::String,
                InferConditionFlow::TrueCondition,
                LuaType::String,
            ),
            (
                LuaType::String,
                InferConditionFlow::TrueCondition,
                LuaType::String,
            ),
            (
                LuaType::Boolean,
                InferConditionFlow::TrueCondition,
                LuaType::BooleanConst(true),
            ),
            (
                LuaType::Boolean,
                InferConditionFlow::FalseCondition,
                LuaType::BooleanConst(false),
            ),
            (
                LuaType::Boolean,
                InferConditionFlow::FalseCondition,
                LuaType::BooleanConst(false),
            ),
        ] {
            assert_eq!(
                seeded_condition(
                    db, tree, &mut cache, &root, &var, &node, &condition, seed, outcome
                ),
                Ok(ResultTypeOrContinue::Result(expected))
            );
        }
        let _ = derivation::end();
        cache.clear();
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::VguiChains));
        assert_eq!(
            seeded_condition(
                db,
                tree,
                &mut cache,
                &root,
                &var,
                &node,
                &condition,
                LuaType::String,
                InferConditionFlow::TrueCondition
            ),
            Ok(ResultTypeOrContinue::Result(LuaType::String))
        );
        let _ = derivation::end();
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        assert_eq!(CONDITION_COMPUTATIONS.with(|count| count.get()), 3);
    }

    #[test]
    fn condition_memo_does_not_keep_a_result_without_tracked_context() {
        let (mut ws, file, root, condition, var, node) = fixture();
        let db = ws.get_db_mut();
        let mut cache = LuaInferCache::new(file, Default::default());
        cache
            .expr_var_ref_id_cache
            .insert(condition.get_syntax_id(), var.clone(), None);
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::Diagnostics));
        CONDITION_COMPUTATIONS.with(|count| count.set(0));
        let tree = db.get_flow_index().get_flow_tree(&file).unwrap();
        for _ in 0..2 {
            assert_eq!(
                seeded_condition(
                    db,
                    tree,
                    &mut cache,
                    &root,
                    &var,
                    &node,
                    &condition,
                    LuaType::String,
                    InferConditionFlow::TrueCondition
                ),
                Ok(ResultTypeOrContinue::Result(LuaType::String))
            );
        }
        let _ = derivation::end();
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        assert!(cache.condition_flow_cache.is_empty());
        assert_eq!(CONDITION_COMPUTATIONS.with(|count| count.get()), 2);
    }

    #[test]
    fn condition_memo_rechecks_written_flow_dependencies() {
        let (mut ws, file, root, condition, var, node) = fixture();
        let db = ws.get_db_mut();
        let tree = db.get_flow_index().get_flow_tree(&file).unwrap().clone();
        let mut cache = LuaInferCache::new(file, Default::default());
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::Diagnostics));
        CONDITION_COMPUTATIONS.with(|count| count.set(0));
        let first = seeded_condition(
            db,
            &tree,
            &mut cache,
            &root,
            &var,
            &node,
            &condition,
            LuaType::Boolean,
            InferConditionFlow::TrueCondition,
        );
        db.get_flow_index_mut().add_flow_tree(file, tree.clone());
        let second = seeded_condition(
            db,
            &tree,
            &mut cache,
            &root,
            &var,
            &node,
            &condition,
            LuaType::Boolean,
            InferConditionFlow::TrueCondition,
        );
        let _ = derivation::end();
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        assert_eq!(
            first,
            Ok(ResultTypeOrContinue::Result(LuaType::BooleanConst(true)))
        );
        assert_eq!(second, first);
        assert_eq!(CONDITION_COMPUTATIONS.with(|count| count.get()), 2);
    }

    #[test]
    fn condition_memo_does_not_keep_errors() {
        let (mut ws, file, root, condition, var, mut node) = fixture();
        node.antecedent = None;
        let db = ws.get_db_mut();
        let mut cache = LuaInferCache::new(file, Default::default());
        derivation::lend_keys(db.reader_graph_mut().lend_keys());
        let _recording = derivation::begin(DerivationId::workspace(DerivationKind::Diagnostics));
        CONDITION_COMPUTATIONS.with(|count| count.set(0));
        let tree = db.get_flow_index().get_flow_tree(&file).unwrap();
        for _ in 0..2 {
            assert!(
                seeded_condition(
                    db,
                    tree,
                    &mut cache,
                    &root,
                    &var,
                    &node,
                    &condition,
                    LuaType::String,
                    InferConditionFlow::TrueCondition
                )
                .is_err()
            );
        }
        let _ = derivation::end();
        db.reader_graph_mut().restore_keys(derivation::take_keys());
        assert!(cache.condition_flow_cache.is_empty());
        assert_eq!(CONDITION_COMPUTATIONS.with(|count| count.get()), 2);
    }
}
