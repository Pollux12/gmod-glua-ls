use rustc_hash::FxHashSet;

use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaIndexKey, LuaSyntaxId};

use crate::{
    DiagnosticCode, FileId, FlowAntecedent, FlowId, FlowNodeKind, FlowTree, InFiled, LuaDecl,
    LuaDeclId, LuaDefinitionId, LuaInferenceDiagnosticEvent, LuaInferenceProvenanceKind, LuaType,
    RenderLevel, SemanticModel, humanize_type,
    semantic::{
        ParamInferenceSource, condition_has_concrete_type_evidence, condition_nominal_type,
        infer_param_source, unwrap_paren_to_name_expr,
    },
};

use super::{Checker, DiagnosticContext};

/// A tie lists this many candidate children before it falls back to a count.
const MAX_LISTED_CHILDREN: usize = 3;

pub struct InferenceTrustChecker;

impl Checker for InferenceTrustChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::InferUnknown,
        DiagnosticCode::InferUnguardedChild,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let mut events = semantic_model
            .get_db()
            .get_type_index()
            .get_inference_events_for_file(context.get_file_id())
            .to_vec();
        events.extend(
            semantic_model
                .get_db()
                .get_call_site_param_index()
                .get_inference_events_for_file(context.get_file_id()),
        );
        events.sort_by(|left, right| left.event.stable_cmp(&right.event));
        events.dedup_by(|left, right| left.event == right.event);
        for inference in events {
            let (code, is_unguarded_child) = match inference.event.kind {
                LuaInferenceProvenanceKind::ContextualUnknown => {
                    (DiagnosticCode::InferUnknown, false)
                }
                // A child read off a param only its name types was inferred
                // from usage: there is no declared parent a guard could narrow.
                LuaInferenceProvenanceKind::UnguardedChild
                    if reads_guessed_declaration(
                        semantic_model,
                        context.get_file_id(),
                        &inference.event.source,
                    ) =>
                {
                    (DiagnosticCode::InferUnknown, false)
                }
                // A read inside a guard proving the same member path present is
                // where that guard already stands, so there is nothing to add.
                LuaInferenceProvenanceKind::UnguardedChild
                    if source_is_presence_guarded(semantic_model, &inference.event.source) =>
                {
                    continue;
                }
                LuaInferenceProvenanceKind::UnguardedChild => {
                    (DiagnosticCode::InferUnguardedChild, true)
                }
                _ => continue,
            };
            let step = inference
                .fact
                .provenance()
                .iter()
                .find(|step| step.event == inference.event);
            let inferred_type = step
                .and_then(|step| step.inferred_type.as_deref())
                .unwrap_or_else(|| inference.fact.typ());
            let typ = humanize_type(semantic_model.get_db(), inferred_type, RenderLevel::Simple);
            let severity = if is_unguarded_child {
                step.and_then(|step| step.found_type.as_deref())
                    .and_then(|found| {
                        let index = use_index_expr(
                            semantic_model,
                            context.get_file_id(),
                            &inference.event.source,
                        )?;
                        unguarded_child_severity(context, semantic_model, &index, found)
                    })
            } else {
                None
            };
            context.add_diagnostic_with_severity(
                code,
                inference.event.source.value.get_range(),
                if is_unguarded_child {
                    let found = step
                        .and_then(|step| step.found_type.as_deref())
                        .map_or_else(
                            || "unknown".to_string(),
                            |typ| humanize_type(semantic_model.get_db(), typ, RenderLevel::Simple),
                        );
                    unguarded_child_message(
                        semantic_model,
                        context.get_file_id(),
                        &inference.event.source,
                        inferred_type,
                        &typ,
                        &found,
                    )
                } else {
                    format!("Type `{typ}` was inferred from usage context and may be incorrect.")
                },
                severity,
                None,
            );
        }
    }
}

pub(super) fn unguarded_child_severity(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    found_type: &LuaType,
) -> Option<lsp_types::DiagnosticSeverity> {
    if let Some(severity) = context
        .config
        .severity
        .get(&DiagnosticCode::InferUnguardedChild)
    {
        return Some(*severity);
    }
    (is_broad_panel_contract(found_type)
        && index_expr.get_prefix_expr().is_some_and(|receiver| {
            receiver_has_broad_panel_contract(semantic_model, receiver, &mut FxHashSet::default())
        }))
    .then_some(lsp_types::DiagnosticSeverity::HINT)
}

fn is_broad_panel_contract(typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => id.get_name() == "Panel",
        LuaType::Union(union) => {
            let mut types = union.types().filter(|typ| !typ.is_nil());
            types.next().is_some_and(is_broad_panel_contract) && types.all(is_broad_panel_contract)
        }
        _ => false,
    }
}

fn receiver_has_broad_panel_contract(
    semantic_model: &SemanticModel,
    receiver: LuaExpr,
    visited: &mut FxHashSet<LuaDeclId>,
) -> bool {
    let db = semantic_model.get_db();
    match receiver {
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|expr| receiver_has_broad_panel_contract(semantic_model, expr, visited)),
        LuaExpr::NameExpr(name) => {
            let receiver = LuaExpr::NameExpr(name);
            let Some(decl) = local_read_declaration(semantic_model, &receiver) else {
                return false;
            };
            if !visited.insert(decl.get_id())
                || decl.is_param()
                || db
                    .get_type_index()
                    .get_type_cache(&decl.get_id().into())
                    .is_some_and(|cache| cache.is_doc())
                || !param_read_rests_on_declaration(semantic_model, &receiver, decl)
            {
                return false;
            }
            decl.get_initializer()
                .filter(|initializer| initializer.get_ret_idx() == 0)
                .and_then(|initializer| {
                    initializer
                        .get_expr_syntax_id()
                        .to_node_from_root(semantic_model.get_root().syntax())
                })
                .and_then(LuaExpr::cast)
                .is_some_and(|expr| {
                    receiver_has_broad_panel_contract(semantic_model, expr, visited)
                })
        }
        LuaExpr::CallExpr(call) => {
            let Some(prefix) = call.get_prefix_expr() else {
                return false;
            };
            match semantic_model.infer_expr(prefix) {
                Ok(LuaType::DocFunction(func)) => is_broad_panel_contract(func.get_ret()),
                Ok(LuaType::Signature(id)) => {
                    db.get_signature_index().get(&id).is_some_and(|signature| {
                        signature.resolve_return == crate::SignatureReturnStatus::DocResolve
                            && signature
                                .return_docs
                                .first()
                                .is_some_and(|ret| is_broad_panel_contract(&ret.type_ref))
                    })
                }
                _ => false,
            }
        }
        LuaExpr::IndexExpr(index) => {
            let receiver = LuaExpr::IndexExpr(index.clone());
            let var_ref = crate::get_var_expr_var_ref_id(
                db,
                &mut semantic_model.get_cache().borrow_mut(),
                receiver.clone(),
            );
            if let Some(tree) = db
                .get_flow_index()
                .get_flow_tree(&semantic_model.get_file_id())
                && let Some(flow_id) = tree.get_flow_id(receiver.get_syntax_id())
                && let Some(var_ref) = var_ref
                && local_flow_evidence_reaches(semantic_model, tree, flow_id, &var_ref)
            {
                return false;
            }
            let member = crate::semantic::infer_expr_semantic_decl(
                db,
                &mut semantic_model.get_cache().borrow_mut(),
                LuaExpr::IndexExpr(index),
                crate::semantic::SemanticDeclGuard::default(),
                crate::semantic::SemanticDeclLevel::NoTrace,
            );
            let Some(crate::LuaSemanticDeclId::Member(id)) = member else {
                return false;
            };
            db.get_type_index()
                .get_type_cache(&id.into())
                .is_some_and(|cache| cache.is_doc() && is_broad_panel_contract(cache.as_type()))
        }
        _ => false,
    }
}

fn file_inference_events(
    semantic_model: &SemanticModel,
    file_id: FileId,
) -> Vec<LuaInferenceDiagnosticEvent> {
    let db = semantic_model.get_db();
    let mut events = db
        .get_type_index()
        .get_inference_events_for_file(file_id)
        .to_vec();
    events.extend(
        db.get_call_site_param_index()
            .get_inference_events_for_file(file_id),
    );
    events.sort_by(|left, right| left.event.stable_cmp(&right.event));
    events.dedup_by(|left, right| left.event == right.event);
    events
}

/// The reads the file's unguarded-child inference events report at.
pub(super) fn unguarded_child_event_sources(
    semantic_model: &SemanticModel,
    file_id: FileId,
) -> FxHashSet<InFiled<LuaSyntaxId>> {
    file_inference_events(semantic_model, file_id)
        .into_iter()
        .filter(|inference| inference.event.kind == LuaInferenceProvenanceKind::UnguardedChild)
        .map(|inference| inference.event.source)
        .collect()
}

fn source_is_presence_guarded(
    semantic_model: &SemanticModel,
    source: &InFiled<LuaSyntaxId>,
) -> bool {
    let Some(index_expr) = source
        .value
        .to_node_from_root(semantic_model.get_root().syntax())
        .and_then(LuaIndexExpr::cast)
    else {
        return false;
    };
    crate::compilation::analyzer::is_guarded_by_enclosing_condition(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        &index_expr,
    )
}

/// A single winning child can be written into a guard, so it is named directly.
/// A tie cannot: its union is not a type the user can narrow to, so the message
/// names the member that drove the inference and the children that define it.
fn unguarded_child_message(
    semantic_model: &SemanticModel,
    file_id: FileId,
    source: &InFiled<LuaSyntaxId>,
    inferred_type: &LuaType,
    inferred_text: &str,
    found: &str,
) -> String {
    let LuaType::Union(union) = inferred_type else {
        return unguarded_children_message(None, found, &[inferred_text.to_string()]);
    };
    let children = union
        .types()
        .map(|child| humanize_type(semantic_model.get_db(), child, RenderLevel::Simple))
        .collect::<Vec<_>>();
    let member = used_member_name(semantic_model, file_id, source);
    unguarded_children_message(member.as_deref(), found, &children)
}

/// The guard advice for a member `found` lacks that its `children` define:
/// one child is named as the type to narrow to, several are listed.
pub(super) fn unguarded_children_message(
    member: Option<&str>,
    found: &str,
    children: &[String],
) -> String {
    if let [child] = children {
        return format!(
            "expected `{child}` but found `{found}`. Add a guard to narrow the parent to `{child}`."
        );
    }
    // A union orders its arms by a content hash, so the names are sorted before
    // the cap decides which of them the message keeps.
    let mut children = children.to_vec();
    children.sort();
    let listed = children
        .iter()
        .take(MAX_LISTED_CHILDREN)
        .map(|child| format!("`{child}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = children.len().saturating_sub(MAX_LISTED_CHILDREN);
    let candidates = if remaining == 0 {
        listed
    } else {
        format!("{listed} and {remaining} more")
    };
    match member {
        Some(member) => format!(
            "`{member}` is not defined on `{found}`. Add a guard that narrows the parent to one of {candidates}."
        ),
        None => format!(
            "this member is not defined on `{found}`. Add a guard that narrows the parent to one of {candidates}."
        ),
    }
}

fn reads_guessed_declaration(
    semantic_model: &SemanticModel,
    file_id: FileId,
    source: &InFiled<LuaSyntaxId>,
) -> bool {
    use_index_expr(semantic_model, file_id, source)
        .is_some_and(|index_expr| is_guessed_declaration_read(semantic_model, &index_expr))
}

pub(super) fn is_guessed_declaration_read(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(prefix) = index_expr.get_prefix_expr() else {
        return false;
    };
    let db = semantic_model.get_db();
    let Some(decl) = local_read_declaration(semantic_model, &prefix) else {
        return false;
    };
    param_read_rests_on_declaration(semantic_model, &prefix, decl)
        && if decl.is_param() {
            infer_param_source(db, &mut semantic_model.get_cache().borrow_mut(), decl)
                .is_some_and(ParamInferenceSource::is_weak)
        } else {
            db.get_type_index()
                .holds_contextual_fallback(&decl.get_id().into())
        }
}

pub(super) fn local_read_declaration<'a>(
    semantic_model: &'a SemanticModel,
    expr: &LuaExpr,
) -> Option<&'a LuaDecl> {
    let name = unwrap_paren_to_name_expr(expr)?;
    let db = semantic_model.get_db();
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&semantic_model.get_file_id())?
        .get_decl_id(&name.get_range())?;
    db.get_decl_index()
        .get_decl(&decl_id)
        .filter(|decl| decl.is_local())
}

/// Whether the read of `prefix`, which names the param `decl`, still has the declaration as its
/// only definition and no type test of the param on a flow path to it. Truthiness and nil tests
/// leave the declaration's evidence as it was.
pub(super) fn param_read_rests_on_declaration(
    semantic_model: &SemanticModel,
    prefix: &LuaExpr,
    decl: &LuaDecl,
) -> bool {
    let Some(flow_tree) = semantic_model
        .get_db()
        .get_flow_index()
        .get_flow_tree(&semantic_model.get_file_id())
    else {
        return true;
    };
    let Some(flow_id) = unwrap_paren_to_name_expr(prefix)
        .and_then(|name_expr| flow_tree.get_flow_id(name_expr.get_syntax_id()))
    else {
        return true;
    };
    let decl_id = decl.get_id();
    matches!(
        flow_tree.reaching_definitions(decl_id, flow_id).as_ref(),
        [LuaDefinitionId::Declaration(_)]
    ) && !local_flow_evidence_reaches(
        semantic_model,
        flow_tree,
        flow_id,
        &crate::VarRefId::VarRef(decl_id),
    )
}

fn local_flow_evidence_reaches(
    semantic_model: &SemanticModel,
    flow_tree: &FlowTree,
    flow_id: FlowId,
    var_ref: &crate::VarRefId,
) -> bool {
    let root = semantic_model.get_root();
    let mut pending = vec![flow_id];
    let mut visited = FxHashSet::default();
    while let Some(current) = pending.pop() {
        if !visited.insert(current) {
            continue;
        }
        let Some(node) = flow_tree.get_flow_node(current) else {
            continue;
        };
        match &node.kind {
            FlowNodeKind::Assignment(ptr, _) => {
                if let Some(assignment) = ptr.to_node(root)
                    && assignment
                        .get_var_and_expr_list()
                        .0
                        .into_iter()
                        .any(|target| {
                            crate::get_var_expr_var_ref_id(
                                semantic_model.get_db(),
                                &mut semantic_model.get_cache().borrow_mut(),
                                target.into(),
                            )
                            .as_ref()
                                == Some(var_ref)
                        })
                {
                    return true;
                }
            }
            FlowNodeKind::TagCast(ptr) => {
                let Some(tag) = ptr.to_node(root) else {
                    continue;
                };
                if let Some(target) = tag.get_key_expr()
                    && crate::get_var_expr_var_ref_id(
                        semantic_model.get_db(),
                        &mut semantic_model.get_cache().borrow_mut(),
                        target,
                    ).as_ref() == Some(var_ref)
                    && tag.get_op_types().any(|op| {
                        let typ = op.get_type().and_then(|typ| {
                            semantic_model.get_db().get_type_index()
                                .get_type_cache(&crate::LuaTypeOwner::SyntaxId(InFiled::new(
                                    semantic_model.get_file_id(), typ.get_syntax_id(),
                                )))
                                .map(|cache| cache.as_type().clone())
                        });
                        let cast = crate::LuaCastOp::new(&op, typ);
                        matches!((cast.action, cast.target),
                            (crate::LuaCastAction::Force | crate::LuaCastAction::Add,
                                Some(crate::LuaCastTarget::Type(typ)))
                                if !typ.is_unknown() && !typ.is_any()
                                    && !condition_nominal_type(semantic_model.get_db(), typ.clone()).is_never())
                    })
                {
                    return true;
                }
            }
            FlowNodeKind::TrueCondition(_) | FlowNodeKind::FalseCondition(_) => {
                if condition_has_concrete_type_evidence(
                    semantic_model.get_db(),
                    flow_tree,
                    &mut semantic_model.get_cache().borrow_mut(),
                    root,
                    var_ref,
                    node,
                ) {
                    return true;
                }
            }
            FlowNodeKind::Start | FlowNodeKind::ClosureEntry(_) => continue,
            _ => {}
        }
        match node.antecedent {
            Some(FlowAntecedent::Single(antecedent)) => pending.push(antecedent),
            Some(FlowAntecedent::Multiple(index)) => {
                if let Some(antecedents) = flow_tree.get_multi_antecedents(index) {
                    pending.extend(antecedents.iter().copied());
                }
            }
            None => {}
        }
    }
    false
}

fn use_index_expr(
    semantic_model: &SemanticModel,
    file_id: FileId,
    source: &InFiled<LuaSyntaxId>,
) -> Option<LuaIndexExpr> {
    if source.file_id != file_id {
        return None;
    }
    let root = semantic_model.get_root().syntax().clone();
    LuaIndexExpr::cast(source.value.to_node_from_root(&root)?)
}

fn used_member_name(
    semantic_model: &SemanticModel,
    file_id: FileId,
    source: &InFiled<LuaSyntaxId>,
) -> Option<String> {
    let index_expr = use_index_expr(semantic_model, file_id, source)?;
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
        LuaIndexKey::String(string) => Some(string.get_value().to_string()),
        _ => None,
    }
}
