#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use glua_parser::{
        LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaFuncStat, LuaLocalName, PathTrait,
    };

    use crate::{
        DiagnosticCode, Emmyrc, InFiled, LuaDefinitionId, LuaInferCache, LuaInferenceConfidence,
        LuaInferenceEventId, LuaInferenceNodeId, LuaInferenceProvenanceKind, LuaInferenceStep,
        LuaType, LuaTypeFact, LuaTypeOwner, VirtualWorkspace,
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

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut config = Emmyrc::default();
        config.gmod.enabled = true;
        ws.analysis.update_config(Arc::new(config));
        ws.def_gmod_call_arg_builtins();
        ws
    }

    fn file_id(ws: &VirtualWorkspace, file_name: &str) -> crate::FileId {
        let uri = ws.virtual_url_generator.new_uri(file_name);
        ws.analysis.get_file_id(&uri).expect("file id")
    }

    fn local_type(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaType {
        let local = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node()
            .descendants::<LuaLocalName>()
            .find(|local| local.get_text() == name)
            .expect("local");
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let token = local.get_name_token().expect("local name token");
        model
            .get_semantic_info(token.syntax().clone().into())
            .expect("semantic info")
            .display_typ()
            .clone()
    }

    fn local_fact(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaTypeFact {
        let local = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node()
            .descendants::<LuaLocalName>()
            .find(|local| local.get_text() == name)
            .expect("local");
        let owner = LuaTypeOwner::Decl(crate::LuaDeclId::new(file_id, local.get_position()));
        ws.analysis
            .compilation
            .get_db()
            .get_type_index()
            .get_type_fact(&owner)
            .expect("local fact")
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
    fn declared_cross_file_network_var_result_does_not_infer_from_usage_context() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![
            (
                "lua/entities/probe_sent/shared.lua",
                r#"
                ---@class probe_sent : Entity
                ENT.Type = "anim"
                ENT.Base = "base_entity"

                function ENT:SetupDataTables()
                    self:NetworkVar("Float", 0, "ZFar")
                end

                ---@param zfar number
                local function takesNumberSameFile(zfar) return zfar end

                function ENT:ReadHere()
                    local zfar = self:GetZFar()
                    return takesNumberSameFile(zfar)
                end
                "#,
            ),
            (
                "consumer.lua",
                r#"
                ---@param zfar number
                local function takesNumber(zfar) return zfar end

                ---@param ent probe_sent
                local function read(ent)
                    local zfar = ent:GetZFar()
                    return takesNumber(zfar)
                end

                return read
                "#,
            ),
        ]);
        let provider_file_id = file_id(&ws, "lua/entities/probe_sent/shared.lua");
        let consumer_file_id = file_id(&ws, "consumer.lua");

        let consumer_found =
            diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown);
        let provider_found =
            diagnostics_for(&mut ws, provider_file_id, DiagnosticCode::InferUnknown);

        assert_eq!(
            (
                local_type(&ws, consumer_file_id, "zfar"),
                consumer_found,
                provider_found,
            ),
            (LuaType::Number, Vec::new(), Vec::new())
        );
    }

    #[test]
    fn declared_cross_file_return_does_not_infer_from_usage_context() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                "annotations/producer.lua",
                r#"
                ---@class DeclaredProducer
                local DeclaredProducer = {}

                ---@return number
                function DeclaredProducer:GetNumber() end
                "#,
            ),
            (
                "consumer.lua",
                r#"
                ---@param value number
                local function takesNumber(value) return value end

                ---@param producer DeclaredProducer
                local function read(producer)
                    local value = producer:GetNumber()
                    return takesNumber(value)
                end

                return read
                "#,
            ),
        ]);
        let consumer_uri = ws.virtual_url_generator.new_uri("consumer.lua");
        let consumer_file_id = ws
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");

        let found = diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn declared_overload_return_does_not_infer_from_usage_context() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "overload_return.lua",
            r#"
            ---@overload fun(name: "Count"): number
            ---@param name string
            local function resolve(name) end

            ---@param value number
            local function takesNumber(value) return value end

            local value = resolve("Count")
            return takesNumber(value)
            "#,
        );

        let found = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown);

        assert_eq!(
            (
                local_type(&ws, file_id, "value"),
                local_fact(&ws, file_id, "value").base_provenance_kind(),
                found,
            ),
            (
                LuaType::Number,
                Some(LuaInferenceProvenanceKind::ExplicitAnnotation),
                Vec::new(),
            )
        );
    }

    #[test]
    fn declared_element_member_retains_contextual_receiver_provenance() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "element_receiver.lua",
            r#"
            local declared = {}

            ---@return number
            function declared:GetNumber() end

            local receiver = declared
            local value = receiver:GetNumber()
            "#,
        );
        let root = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node();
        let receiver = root
            .descendants::<LuaLocalName>()
            .find(|local| local.get_text() == "receiver")
            .expect("receiver local");
        let receiver_node = LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Decl(
            crate::LuaDeclId::new(file_id, receiver.get_position()),
        ));
        let receiver_fact = local_fact(&ws, file_id, "receiver");
        let receiver_event = LuaInferenceEventId {
            node: receiver_node.clone(),
            kind: LuaInferenceProvenanceKind::ContextualUnknown,
            source: InFiled::new(file_id, receiver.get_syntax_id()),
        };
        ws.get_db_mut().publish_inference_facts(vec![(
            receiver_node,
            LuaTypeFact::new(
                receiver_fact.typ().clone(),
                LuaInferenceConfidence::Anchored,
                vec![LuaInferenceStep {
                    event: receiver_event.clone(),
                    support: Vec::new().into(),
                    inferred_type: Some(Arc::new(receiver_fact.typ().clone())),
                    found_type: None,
                }]
                .into(),
            ),
        )]);
        let call = root
            .descendants::<LuaCallExpr>()
            .find(|call| {
                call.syntax()
                    .text()
                    .to_string()
                    .contains("receiver:GetNumber")
            })
            .expect("declared element-member call");
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let fact = crate::semantic::infer_expr_fact_with_cache(
            ws.analysis.compilation.get_db(),
            &mut cache,
            LuaExpr::CallExpr(call),
        );

        assert_eq!(
            (
                fact.typ(),
                fact.confidence(),
                fact.base_provenance_kind(),
                fact.provenance()
                    .iter()
                    .map(|step| &step.event)
                    .collect::<Vec<_>>(),
            ),
            (
                &LuaType::Number,
                LuaInferenceConfidence::Anchored,
                None,
                vec![&receiver_event],
            )
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "Type `{ GetNumber = fun() -> number }` was inferred from usage context and may be incorrect."
        );
    }

    #[test]
    fn inferred_return_does_not_gain_declared_authority() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "annotations/inferred-return.lua",
            r#"
            ---@return unknown
            function unknown_source() end
            "#,
        );
        let file_id = ws.def_file(
            "inferred-return.lua",
            r#"
            local function inferredSource()
                return unknown_source()
            end

            ---@param value number
            local function takesNumber(value) return value end

            local value = inferredSource()
            return takesNumber(value)
            "#,
        );

        let found = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown);

        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn unmatched_overload_base_return_does_not_gain_declared_authority() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "annotations/unmatched-overload.lua",
            r#"
            ---@return unknown
            function unknown_source() end
            "#,
        );
        let file_id = ws.def_file(
            "unmatched-overload.lua",
            r#"
            ---@overload fun(name: "Count"): number
            ---@param name string
            local function resolve(name)
                return unknown_source()
            end

            ---@param value number
            local function takesNumber(value) return value end

            local value = resolve("Other")
            return takesNumber(value)
            "#,
        );

        let found = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown);

        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn declared_cross_file_field_does_not_infer_from_usage_context() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                "annotations/holder.lua",
                r#"
                ---@class DeclaredHolder
                ---@field count number
                local DeclaredHolder = {}
                "#,
            ),
            (
                "consumer.lua",
                r#"
                ---@param value number
                local function takesNumber(value) return value end

                ---@param holder DeclaredHolder
                local function read(holder)
                    local value = holder.count
                    return takesNumber(value)
                end

                return read
                "#,
            ),
        ]);
        let consumer_file_id = file_id(&ws, "consumer.lua");

        let found = diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown);

        assert_eq!(
            (local_type(&ws, consumer_file_id, "value"), found),
            (LuaType::Number, Vec::new())
        );
    }

    #[test]
    fn declared_cross_file_accessor_func_result_does_not_infer_from_usage_context() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![
            (
                "lua/entities/accessor_probe/shared.lua",
                r#"
                ---@class accessor_probe : Entity
                ENT.Type = "anim"
                ENT.Base = "base_entity"
                AccessorFunc(ENT, "m_bEnabled", "Enabled", true)
                "#,
            ),
            (
                "consumer.lua",
                r#"
                ---@param value boolean
                local function takesBoolean(value) return value end

                ---@param ent accessor_probe
                local function read(ent)
                    local value = ent:GetEnabled()
                    return takesBoolean(value)
                end

                return read
                "#,
            ),
        ]);
        let consumer_uri = ws.virtual_url_generator.new_uri("consumer.lua");
        let consumer_file_id = ws
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");

        let found = diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn declared_cross_file_network_var_element_result_does_not_infer_from_usage_context() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![
            (
                "lua/entities/element_probe/shared.lua",
                r#"
                ---@class element_probe : Entity
                ENT.Type = "anim"
                ENT.Base = "base_entity"

                function ENT:SetupDataTables()
                    self:NetworkVarElement("Vector", 0, "x", "OffsetX")
                end
                "#,
            ),
            (
                "consumer.lua",
                r#"
                ---@param value number
                local function takesNumber(value) return value end

                ---@param ent element_probe
                local function read(ent)
                    local value = ent:GetOffsetX()
                    return takesNumber(value)
                end

                return read
                "#,
            ),
        ]);
        let consumer_uri = ws.virtual_url_generator.new_uri("consumer.lua");
        let consumer_file_id = ws
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");

        let found = diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn declared_hook_callback_parameter_does_not_infer_from_usage_context() {
        let mut ws = gmod_workspace();
        let callback_file_id = ws.def_file(
            "lua/autorun/server/hook_callback.lua",
            r#"
            ---@class GM
            GM = {}

            ---@hook AcceptInput
            ---@param ent Entity
            function GM:AcceptInput(ent) end

            ---@param ent Entity
            local function takesEntity(ent) return ent end

            hook.Add("AcceptInput", "provenance", function(ent)
                return takesEntity(ent)
            end)
            "#,
        );

        let found = diagnostics_for(&mut ws, callback_file_id, DiagnosticCode::InferUnknown);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn declared_overload_callback_parameter_does_not_infer_from_usage_context() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "callback.lua",
            r#"
            ---@overload fun(name: "Count", callback: fun(value: number))
            ---@param name string
            ---@param callback function
            local function addHook(name, callback) end

            ---@param value number
            local function takesNumber(value) return value end

            addHook("Count", function(value)
                return takesNumber(value)
            end)
            "#,
        );

        let found = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnknown);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn cross_file_declared_result_reconciles_after_incremental_changes() {
        const WITHOUT_NETWORK_VAR: &str = r#"
            ---@class incremental_probe : Entity
            ENT.Type = "anim"
            ENT.Base = "base_entity"
        "#;
        const WITH_FLOAT_NETWORK_VAR: &str = r#"
            ---@class incremental_probe : Entity
            ENT.Type = "anim"
            ENT.Base = "base_entity"

            function ENT:SetupDataTables()
                self:NetworkVar("Float", 0, "Value")
            end
        "#;
        const WITH_BOOL_NETWORK_VAR: &str = r#"
            ---@class incremental_probe : Entity
            ENT.Type = "anim"
            ENT.Base = "base_entity"

            function ENT:SetupDataTables()
                self:NetworkVar("Bool", 0, "Value")
            end
        "#;
        const CONSUMER: &str = r#"
            ---@param value number
            local function takesNumber(value) return value end

            ---@param ent incremental_probe
            local function read(ent)
                local value = ent:GetValue()
                return takesNumber(value)
            end

            return read
        "#;

        let mut ws = gmod_workspace();
        ws.def_files(vec![
            (
                "lua/entities/incremental_probe/shared.lua",
                WITHOUT_NETWORK_VAR,
            ),
            ("consumer.lua", CONSUMER),
        ]);
        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/incremental_probe/shared.lua");
        let consumer_file_id = file_id(&ws, "consumer.lua");

        assert_eq!(
            diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown).len(),
            1
        );

        ws.analysis
            .update_file_by_uri(&provider_uri, Some(WITH_FLOAT_NETWORK_VAR.to_string()));
        assert_eq!(
            (
                local_type(&ws, consumer_file_id, "value"),
                diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown),
            ),
            (LuaType::Number, Vec::new())
        );

        ws.analysis
            .update_file_by_uri(&provider_uri, Some(WITH_BOOL_NETWORK_VAR.to_string()));
        assert_eq!(local_type(&ws, consumer_file_id, "value"), LuaType::Boolean);

        ws.analysis.update_file_by_uri(&provider_uri, None);
        assert_eq!(
            diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown).len(),
            1
        );

        ws.analysis
            .update_file_by_uri(&provider_uri, Some(WITH_FLOAT_NETWORK_VAR.to_string()));
        assert_eq!(
            (
                local_type(&ws, consumer_file_id, "value"),
                diagnostics_for(&mut ws, consumer_file_id, DiagnosticCode::InferUnknown),
            ),
            (LuaType::Number, Vec::new())
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
                    inferred_type: Some(Arc::new(LuaType::String)),
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

    fn gmod_workspace_with_std_lib() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut config = Emmyrc::default();
        config.gmod.enabled = true;
        ws.analysis.update_config(Arc::new(config));
        ws.def_gmod_call_arg_builtins();
        ws
    }

    /// A `NetworkVar`-synthesized accessor carries the type written in its
    /// `"Int"` literal, so a value derived from it is not a guess. Narrowing the
    /// result through a branch that can assign `nil`, then storing it in a table
    /// field, must not turn the declared type into an inferred one.
    #[test]
    fn network_var_result_through_nilable_branch_does_not_infer_from_usage_context() {
        let mut ws = gmod_workspace_with_std_lib();
        ws.def_files(vec![
            (
                "lua/entities/thing/shared.lua",
                r#"
                ---@class thing : Entity
                ENT.Type = "anim"

                function ENT:SetupDataTables()
                    self:NetworkVar("Int", "ZFar")
                end

                ---@return integer
                function ENT:GetHand() end
                "#,
            ),
            (
                "lua/use.lua",
                r#"
                local t1, t2 = { zfar = nil }, { zfar = nil }

                ---@param p thing
                ---@param d number
                local function subject(p, d)
                    local z = p:GetZFar()
                    if z > 0 then z = math.max(d, z) else z = nil end
                    t1.zfar = z
                end

                ---@param p thing
                ---@param d number
                local function control(p, d)
                    local hand = p:GetHand()
                    if hand > 0 then hand = math.max(d, hand) else hand = nil end
                    t2.zfar = hand
                end

                return subject, control
                "#,
            ),
        ]);
        let use_file_id = file_id(&ws, "lua/use.lua");

        let found = diagnostics_for(&mut ws, use_file_id, DiagnosticCode::InferUnknown);

        // The synthesized accessor and the hand-declared control must both still
        // resolve, so silence above cannot come from nothing having attached.
        assert_eq!(
            (
                local_type(&ws, use_file_id, "z"),
                local_type(&ws, use_file_id, "hand"),
                found,
            ),
            (LuaType::Integer, LuaType::Integer, Vec::new())
        );
    }

    /// The using file sorts before the file declaring the entity, so the
    /// receiver is still unresolved when the call is analysed. Past an
    /// `IsValid` guard, a stock method's declared `---@return number` used to
    /// come back flagged as inferred purely because the receiver had been —
    /// which made the answer depend on what the files were named.
    #[test]
    fn declared_return_past_valid_guard_does_not_infer_when_use_file_sorts_first() {
        let mut ws = gmod_workspace_with_std_lib();
        ws.def_files(vec![
            (
                "annotations/gmod.lua",
                r#"
                ---@meta
                ---@attribute valid_guard()

                ---@class Entity
                local Entity = {}

                ---@param name string
                ---@return number
                function Entity:FindBodygroupByName(name) end

                ---@param bodyGroupId number
                ---@param subModelId number
                function Entity:SetBodygroup(bodyGroupId, subModelId) end

                ---@class NULL : Entity

                ---@param object any
                ---@return TypeGuard<any>
                ---@return_cast object -NULL
                ---@[valid_guard]
                function _G.IsValid(object) end
                "#,
            ),
            (
                "lua/entities/my_int/aa_use.lua",
                r#"
                ---@param int my_int
                ---@param name string
                local function use(int, name)
                    local p = int:GetPart("door")
                    if not IsValid(p) then return end

                    local bg = p:FindBodygroupByName(name)
                    p:SetBodygroup(bg, 0)
                end
                return use
                "#,
            ),
            (
                "lua/entities/my_int/zz_ent.lua",
                r#"
                ---@class my_int : Entity
                ENT.Type = "anim"

                ---@param id string
                ---@return Entity
                function ENT:GetPart(id) end
                "#,
            ),
        ]);
        let use_file_id = file_id(&ws, "lua/entities/my_int/aa_use.lua");

        let found = diagnostics_for(&mut ws, use_file_id, DiagnosticCode::InferUnknown);

        // Both must still resolve past the guard, so silence above cannot come
        // from a workspace where the receiver never attached.
        assert_eq!(
            (
                local_type(&ws, use_file_id, "p"),
                local_type(&ws, use_file_id, "bg"),
                found,
            ),
            (ws.ty("Entity"), LuaType::Number, Vec::new())
        );
    }
}
