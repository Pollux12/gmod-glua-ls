use glua_parser::{LuaAstNode, LuaIndexExpr, LuaIndexKey, LuaSyntaxId};

use crate::{
    DiagnosticCode, FileId, InFiled, LuaInferenceProvenanceKind, LuaType, RenderLevel,
    SemanticModel, humanize_type,
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
        events.extend_from_slice(
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
            context.add_diagnostic(
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
                None,
            );
        }
    }
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
        return format!(
            "expected `{inferred_text}` but found `{found}`. Add a guard to narrow the parent to `{inferred_text}`."
        );
    };
    // A union orders its arms by a content hash, so the names are sorted before
    // the cap decides which of them the message keeps.
    let mut children = union
        .types()
        .map(|child| humanize_type(semantic_model.get_db(), child, RenderLevel::Simple))
        .collect::<Vec<_>>();
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
    match used_member_name(semantic_model, file_id, source) {
        Some(member) => format!(
            "`{member}` is not defined on `{found}`. Add a guard that narrows the parent to one of {candidates}."
        ),
        None => format!(
            "this member is not defined on `{found}`. Add a guard that narrows the parent to one of {candidates}."
        ),
    }
}

fn used_member_name(
    semantic_model: &SemanticModel,
    file_id: FileId,
    source: &InFiled<LuaSyntaxId>,
) -> Option<String> {
    if source.file_id != file_id {
        return None;
    }
    let root = semantic_model.get_root().syntax().clone();
    let index_expr = LuaIndexExpr::cast(source.value.to_node_from_root(&root)?)?;
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
        LuaIndexKey::String(string) => Some(string.get_value().to_string()),
        _ => None,
    }
}
