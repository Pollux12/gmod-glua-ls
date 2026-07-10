#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use glua_parser::{LuaAstNode, LuaLocalName};

    use crate::{
        DiagnosticCode, Emmyrc, InFiled, LuaDefinitionId, LuaInferenceConfidence,
        LuaInferenceEventId, LuaInferenceNodeId, LuaInferenceProvenanceKind, LuaInferenceStep,
        LuaType, LuaTypeFact, VirtualWorkspace,
    };

    fn workspace() -> (VirtualWorkspace, crate::FileId) {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "annotations/infer-unknown.lua",
            r#"
            ---@meta
            ---@class InferredVector
            ---@class InferredTrace
            ---@field start InferredVector
            infer_util = {}
            ---@param trace InferredTrace
            function infer_util.Trace(trace) end
            ---@return unknown
            function unknown_source() end
            "#,
        );
        let file_id = ws.def_file(
            "lua/autorun/infer-unknown.lua",
            "local value = unknown_source()\ninfer_util.Trace({ start = value })\nlocal again = value",
        );
        (ws, file_id)
    }

    fn diagnostics_for(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> Vec<lsp_types::Diagnostic> {
        let shared = ws.analysis.precompute_diagnostic_shared_data();
        ws.analysis
            .diagnose_file_with_shared(file_id, CancellationToken::new(), shared)
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String(code.get_name().to_string()))
            })
            .collect()
    }

    #[test]
    fn infer_unknown_is_one_indexed_hint_at_the_anchor() {
        let (mut ws, file_id) = workspace();
        let found = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Some(lsp_types::DiagnosticSeverity::HINT));
        assert_eq!(
            found[0].message,
            "Type `InferredVector` was inferred from usage context and may be incorrect."
        );
        assert_eq!(found[0].range.start.line, 1);
    }

    #[test]
    fn infer_unknown_can_be_disabled_independently() {
        let (mut ws, file_id) = workspace();
        let mut config = Emmyrc::default();
        config
            .diagnostics
            .disable
            .push(DiagnosticCode::InferUnknown);
        ws.analysis.update_config(Arc::new(config));

        assert!(diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown).is_empty());
    }

    #[test]
    fn unguarded_child_is_configured_separately_from_generic_unknown_inference() {
        let (mut ws, file_id) = workspace();
        let local = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node()
            .descendants::<LuaLocalName>()
            .find(|local| local.get_text() == "again")
            .expect("separate child-inference local");
        let decl_id = crate::LuaDeclId::new(file_id, local.get_position());
        let node = LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(decl_id));
        let event = LuaInferenceEventId {
            node: node.clone(),
            kind: LuaInferenceProvenanceKind::UnguardedChild,
            source: InFiled::new(file_id, local.get_syntax_id()),
        };
        ws.get_db_mut().publish_inference_facts(vec![(
            node,
            LuaTypeFact::new(
                LuaType::String,
                LuaInferenceConfidence::Heuristic,
                vec![LuaInferenceStep {
                    event,
                    support: vec![].into(),
                }]
                .into(),
            ),
        )]);

        let found = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].severity,
            Some(lsp_types::DiagnosticSeverity::WARNING)
        );

        let mut config = Emmyrc::default();
        config
            .diagnostics
            .disable
            .push(DiagnosticCode::InferUnguardedChild);
        ws.analysis.update_config(Arc::new(config));
        assert!(diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild).is_empty());
        assert_eq!(
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown).len(),
            1,
            "disabling child inference must not disable generic unknown inference"
        );
    }
}
