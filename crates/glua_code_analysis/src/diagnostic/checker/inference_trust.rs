use crate::{
    DiagnosticCode, LuaInferenceProvenanceKind, RenderLevel, SemanticModel, humanize_type,
};

use super::{Checker, DiagnosticContext};

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
            let typ = humanize_type(
                semantic_model.get_db(),
                inference.fact.typ(),
                RenderLevel::Simple,
            );
            context.add_diagnostic(
                code,
                inference.event.source.value.get_range(),
                if is_unguarded_child {
                    let found = inference
                        .fact
                        .provenance()
                        .iter()
                        .find(|step| step.event == inference.event)
                        .and_then(|step| step.found_type.as_deref())
                        .map_or_else(
                            || "unknown".to_string(),
                            |typ| {
                                humanize_type(semantic_model.get_db(), typ, RenderLevel::Simple)
                            },
                        );
                    format!(
                        "expected `{typ}` but found `{found}`. Add a guard to narrow the parent to `{typ}`."
                    )
                } else {
                    format!("Type `{typ}` was inferred from usage context and may be incorrect.")
                },
                None,
            );
        }
    }
}
