#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use glua_parser::{LuaAstNode, LuaFuncStat, LuaLocalName, PathTrait};

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

    fn signature_for_path(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        path: &str,
    ) -> crate::LuaSignatureId {
        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_red_root();
        root.descendants()
            .filter_map(LuaFuncStat::cast)
            .find_map(|func_stat| {
                (func_stat
                    .get_func_name()
                    .and_then(|name| name.get_access_path())
                    .is_some_and(|access_path| access_path.as_str() == path))
                .then(|| func_stat.get_closure())
                .flatten()
                .map(|closure| crate::LuaSignatureId::from_closure(file_id, &closure))
            })
            .unwrap_or_else(|| panic!("expected signature for {path}"))
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
    fn declared_colon_receiver_is_not_reinferred_from_explicit_subclass_call() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/issue-51.lua",
            r#"
            ---@class base_thing
            ---@field BaseClass base_thing
            local BASE = {}

            ---@param name string
            function BASE:M(name)
                BASE_SELF = self
                return name
            end

            ---@class derived_thing : base_thing
            local DERIVED = {}

            ---@param name string
            function DERIVED:M(name)
                return self.BaseClass.M(self, name)
            end

            ---@class base_ok
            ---@field BaseClass base_ok
            local BASE2 = {}

            ---@param self base_ok
            ---@param name string
            function BASE2.M(self, name) return name end

            ---@class derived_ok : base_ok
            local DERIVED2 = {}

            ---@param name string
            function DERIVED2:M(name)
                return self.BaseClass.M(self, name)
            end
            "#,
        );

        assert!(diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown).is_empty());

        let signature_id = signature_for_path(&ws, file_id, "BASE.M");
        let db = ws.analysis.compilation.get_db();
        let signature = db
            .get_signature_index()
            .get(&signature_id)
            .expect("base method signature");
        assert!(
            db.get_call_site_param_index()
                .get_inferred_param(&signature_id, signature.params.len())
                .is_none()
        );

        let self_type = ws.expr_ty("BASE_SELF");
        assert_eq!(ws.humanize_type(self_type), "base_thing");
    }

    #[test]
    fn raw_colon_receiver_keeps_contextual_inference_and_hint() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/raw-receiver.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end

            ---@class receiver_host
            local HOST = {}
            function HOST:OnlyOnReceiver() end
            function HOST:Dispatch()
                local callback = MIXIN.Run
                callback(self)
            end
            "#,
        );

        assert_eq!(
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown).len(),
            1
        );

        let signature_id = signature_for_path(&ws, file_id, "MIXIN.Run");
        let db = ws.analysis.compilation.get_db();
        let signature = db
            .get_signature_index()
            .get(&signature_id)
            .expect("raw mixin signature");
        assert!(
            db.get_call_site_param_index()
                .get_inferred_param(&signature_id, signature.params.len())
                .is_some()
        );
        assert!(diagnostics_for(&mut ws, file_id, DiagnosticCode::UndefinedMethod).is_empty());
    }

    #[test]
    fn receiver_contract_edits_refresh_contextual_inference() {
        fn source(base_annotation: &str) -> String {
            format!(
                r#"
                {base_annotation}
                local BASE = {{}}
                function BASE:Run() end

                ---@class incremental_host
                local HOST = {{}}
                function HOST:Dispatch()
                    local callback = BASE.Run
                    callback(self)
                end
                "#
            )
        }

        let mut ws = VirtualWorkspace::new();
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/incremental-receiver.lua");
        let file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(source("")))
            .expect("raw receiver file");
        assert_eq!(
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown).len(),
            1
        );

        let annotated = source("---@class incremental_base");
        let annotated_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(annotated.clone()))
            .expect("annotated receiver file");
        assert!(
            diagnostics_for(&mut ws, annotated_file_id, DiagnosticCode::InferUnknown).is_empty()
        );

        let raw_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(source("")))
            .expect("raw receiver file after edit");
        assert_eq!(
            diagnostics_for(&mut ws, raw_file_id, DiagnosticCode::InferUnknown).len(),
            1
        );

        ws.analysis
            .update_file_by_uri(&uri, None)
            .expect("receiver file removal");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(annotated))
            .expect("reopened annotated receiver file");
        assert!(
            diagnostics_for(&mut ws, reopened_file_id, DiagnosticCode::InferUnknown).is_empty()
        );
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
                    found_type: None,
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
