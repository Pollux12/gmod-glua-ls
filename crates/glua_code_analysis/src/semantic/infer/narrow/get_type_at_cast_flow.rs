use glua_parser::{LuaAstNode, LuaCallExpr, LuaChunk, LuaDocOpType, LuaDocTagCast, LuaExpr};

use crate::{
    DbIndex, FileId, FlowId, FlowNode, FlowNodeKind, FlowTree, InFiled, InferFailReason,
    LuaCastAction, LuaCastOp, LuaCastTarget, LuaInferCache, LuaType, LuaTypeOwner, TypeOps,
    semantic::infer::{
        VarRefId,
        narrow::{
            ResultTypeOrContinue,
            condition_flow::InferConditionFlow,
            get_single_antecedent,
            get_type_at_flow::{FlowWalkPolicy, get_type_at_flow_in_mode},
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

pub fn get_type_at_cast_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    tag_cast: LuaDocTagCast,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    match tag_cast.get_key_expr() {
        Some(expr) => get_type_at_cast_expr(
            db, tree, cache, root, var_ref_id, flow_node, tag_cast, expr, policy,
        ),
        None => get_type_at_inline_cast(
            db, tree, cache, root, var_ref_id, flow_node, tag_cast, policy,
        ),
    }
}

fn get_type_at_cast_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    tag_cast: LuaDocTagCast,
    key_expr: LuaExpr,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, key_expr) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if maybe_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let mut antecedent_type = get_type_at_flow_in_mode(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        antecedent_flow_id,
        policy,
    )?;
    for cast_op_type in tag_cast.get_op_types() {
        antecedent_type = apply_cast(
            db,
            &doc_cast_op(db, cache.get_file_id(), &cast_op_type),
            antecedent_type,
            InferConditionFlow::TrueCondition,
        );
    }
    Ok(ResultTypeOrContinue::Result(antecedent_type))
}

fn get_type_at_inline_cast(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    tag_cast: LuaDocTagCast,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let mut antecedent_type = get_type_at_flow_in_mode(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        antecedent_flow_id,
        policy,
    )?;
    for cast_op_type in tag_cast.get_op_types() {
        antecedent_type = apply_cast(
            db,
            &doc_cast_op(db, cache.get_file_id(), &cast_op_type),
            antecedent_type,
            InferConditionFlow::TrueCondition,
        );
    }
    Ok(ResultTypeOrContinue::Result(antecedent_type))
}

pub fn get_type_at_call_expr_inline_cast(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    tree: &FlowTree,
    call_expr: LuaCallExpr,
    flow_id: FlowId,
    mut return_type: LuaType,
) -> Option<LuaType> {
    let flow_node = tree.get_flow_node(flow_id)?;
    let FlowNodeKind::TagCast(tag_cast_ptr) = &flow_node.kind else {
        return None;
    };

    let root = LuaChunk::cast(call_expr.get_root())?;
    let tag_cast = tag_cast_ptr.to_node(&root)?;
    if tag_cast.get_key_expr().is_some() {
        return None;
    }

    for cast_op_type in tag_cast.get_op_types() {
        return_type = apply_cast(
            db,
            &doc_cast_op(db, cache.get_file_id(), &cast_op_type),
            return_type,
            InferConditionFlow::TrueCondition,
        );
    }

    Some(return_type)
}

/// Applies `op` to `source_type`; a false condition applies it negated.
pub fn apply_cast(
    db: &DbIndex,
    op: &LuaCastOp,
    source_type: LuaType,
    condition_flow: InferConditionFlow,
) -> LuaType {
    let action = if condition_flow.is_false() {
        op.action.negated()
    } else {
        op.action
    };
    let target = match &op.target {
        Some(LuaCastTarget::Nil) => {
            return match action {
                LuaCastAction::Add => TypeOps::Union.apply(db, &source_type, &LuaType::Nil),
                LuaCastAction::Remove => TypeOps::Remove.apply(db, &source_type, &LuaType::Nil),
                LuaCastAction::Force => source_type,
            };
        }
        Some(LuaCastTarget::Type(typ)) => typ,
        None => return source_type,
    };
    match action {
        // A predicate that removes `target` when true leaves possible when false only
        // the parts of it the value could have held.
        LuaCastAction::Add if condition_flow.is_false() => {
            let held = arms(target)
                .filter(|part| arms(&source_type).any(|arm| arm_can_hold(db, arm, part)))
                .cloned()
                .collect::<Vec<_>>();
            if held.is_empty() {
                source_type
            } else {
                TypeOps::Union.apply(db, &source_type, &LuaType::from_vec(held))
            }
        }
        LuaCastAction::Add => TypeOps::Union.apply(db, &source_type, target),
        LuaCastAction::Remove => {
            let removed = TypeOps::Remove.apply(db, &source_type, target);
            if removed == source_type && source_type == *target {
                LuaType::Never
            } else {
                removed
            }
        }
        LuaCastAction::Force => target.clone(),
    }
}

fn arms(typ: &LuaType) -> Box<dyn Iterator<Item = &LuaType> + '_> {
    match typ {
        LuaType::Union(union) => Box::new(union.types()),
        _ => Box::new(std::iter::once(typ)),
    }
}

fn arm_can_hold(db: &DbIndex, arm: &LuaType, part: &LuaType) -> bool {
    !arm.is_unknown()
        && !arm.is_any()
        && crate::semantic::type_check::check_type_compact(db, arm, part).is_ok()
}

/// The operation a `---@cast` in `file_id` writes, with the type the doc
/// analyzer bound to its doc type.
pub fn doc_cast_op(db: &DbIndex, file_id: FileId, op_type: &LuaDocOpType) -> LuaCastOp {
    let typ = op_type.get_type().and_then(|doc_type| {
        db.get_type_index()
            .get_type_cache(&LuaTypeOwner::SyntaxId(InFiled::new(
                file_id,
                doc_type.get_syntax_id(),
            )))
            .map(|cache| cache.as_type().clone())
    });
    LuaCastOp::new(op_type, typ)
}
