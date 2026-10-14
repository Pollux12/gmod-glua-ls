#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use crate::{DiagnosticCode, Emmyrc, SemanticInfoOrigin, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaLocalName, LuaNameExpr};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn enable_gmod(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    fn last_name_type(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> crate::LuaType {
        last_name_info(ws, file_id, name).display_typ().clone()
    }

    fn nth_name_type_from_end(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
        nth_from_end: usize,
    ) -> crate::LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let name_exprs = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .collect::<Vec<_>>();
        let name_expr = name_exprs
            .into_iter()
            .rev()
            .nth(nth_from_end)
            .expect("name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("semantic info")
            .display_typ()
            .clone()
    }

    fn last_name_info(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> crate::SemanticInfo {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let name_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .last()
            .expect("name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("semantic info")
    }

    fn diagnostic_count(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> usize {
        diagnostics_for(ws, file_id, code).len()
    }

    fn diagnostics_for(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> Vec<lsp_types::Diagnostic> {
        ws.analysis.diagnostic.enable_only(code);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String(code.get_name().to_string()))
            })
            .cloned()
            .collect()
    }

    fn member_path_receiver_types(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        path_text: &str,
    ) -> Vec<String> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver))
                    if receiver.syntax().text().to_string().trim() == path_text =>
                {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct NestedCallbackState {
        receiver_types: Vec<String>,
        undefined_methods: usize,
        unguarded_children: usize,
    }

    fn nested_callback_state(
        ws: &mut VirtualWorkspace,
        consumer_file_id: crate::FileId,
    ) -> NestedCallbackState {
        let undefined_methods =
            diagnostic_count(ws, consumer_file_id, DiagnosticCode::UndefinedMethod);
        let unguarded_children =
            diagnostic_count(ws, consumer_file_id, DiagnosticCode::InferUnguardedChild);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file_id)
            .expect("consumer semantic model");
        let receiver_types = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver))
                    if receiver.syntax().text().to_string().trim() == "data.proc" =>
                {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect();
        NestedCallbackState {
            receiver_types,
            undefined_methods,
            unguarded_children,
        }
    }

    fn fresh_nested_callback_state(producer: Option<&str>, consumer: &str) -> NestedCallbackState {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let mut files = Vec::new();
        if let Some(producer) = producer {
            files.push(("lua/autorun/shared/transfer.lua", producer));
        }
        files.push(("lua/autorun/client/consumer.lua", consumer));
        let consumer_file_id = ws
            .def_files(files)
            .into_iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_semantic_model(*file_id)
                    .is_some_and(|model| {
                        model
                            .get_root()
                            .syntax()
                            .text()
                            .to_string()
                            .contains("Transfer.Read(function")
                    })
            })
            .expect("consumer file");
        nested_callback_state(&mut ws, consumer_file_id)
    }

    #[test]
    fn unguarded_child_member_evidence_selects_player_and_resolves_member_result() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Vector

            ---@class Entity
            ---@class Player: Entity
            ---@field GetShootPos fun(self: Player): Vector
            ---@field IsActiveCannibal fun(self: Player): boolean
            ---@field IsRoleAbilityDisabled fun(self: Player): boolean
            ---@class Cannibal: Entity
            ---@field IsActiveCannibal fun(self: Cannibal): boolean

            ---@type Entity
            local owner
            local spos = owner:GetShootPos()
            local active = owner:IsActiveCannibal()
            local disabled = owner:IsRoleAbilityDisabled()
            print(owner)
            print(spos)
            "#,
        );

        let owner_type = last_name_type(&ws, file_id, "owner");
        let spos_type = last_name_type(&ws, file_id, "spos");
        let owner_info = last_name_info(&ws, file_id, "owner");
        assert_eq!(ws.humanize_type(owner_type), "Player");
        assert_eq!(owner_info.origin, SemanticInfoOrigin::ContextualExpected);
        assert_eq!(ws.humanize_type(spos_type), "Vector");
    }

    #[test]
    fn guarded_nested_member_receiver_selects_scripted_entity_child() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@attribute self_guard(member: string)

            ---@class Entity
            ---@class starfall_processor: Entity
            ---@field Compile fun(self: starfall_processor, data: table)
            ---@field Error fun(self: starfall_processor, err: table)

            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local Ent_IsValid = FindMetaTable("Entity").IsValid
            local function Read(callback)
                ---@type unknown
                local proc
                callback({ proc = proc })
                callback({ owner = proc })
            end

            Read(function(sfdata)
                if not Ent_IsValid(sfdata.proc) then return end
                sfdata.proc:Compile(sfdata)
                sfdata.proc:Error({ message = "failed" })
            end)
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0,
            "nested Entity receiver should use child-only methods as scripted-class evidence"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1,
            "nested Entity receiver should report one heuristic child inference"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let receiver_types = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver)) if receiver.syntax().text() == "sfdata.proc" => {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect::<Vec<_>>();
        assert_eq!(
            receiver_types,
            vec!["starfall_processor", "starfall_processor"]
        );
    }

    #[test]
    fn cross_file_callback_preserves_entity_guard_for_nested_child_inference() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/shared/transfer.lua",
                r#"
                ---@attribute self_guard(member: string)
                ---@class Entity
                ---@class starfall_processor: Entity
                ---@field Compile fun(self: starfall_processor, data: table)
                ---@field Error fun(self: starfall_processor, err: table)

                Entity = {}
                ---@return boolean
                ---@return_cast self Entity
                ---@[self_guard("gmod.entity")]
                function Entity:IsValid() end

                ---@param value any
                ---@return TypeGuard<Entity>
                function isentity(value) end

                Transfer = {}
                function Transfer.Read(callback)
                    local data = {}
                    ---@type unknown
                    local proc
                    if isentity(proc) then data.proc = proc end
                    callback(true, data)
                end
                "#,
            ),
            (
                "lua/autorun/client/consumer.lua",
                r#"
                ---@generic T
                ---@param name `T`
                ---@return T
                function FindMetaTable(name) end

                local Ent_IsValid = FindMetaTable("Entity").IsValid
                Transfer.Read(function(ok, data)
                    if ok and Ent_IsValid(data.proc) then
                        data.proc:Compile(data)
                        data.proc:Error({ message = "failed" })
                    end
                end)
                "#,
            ),
        ]);
        let consumer_file_id = file_ids
            .into_iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_semantic_model(*file_id)
                    .is_some_and(|model| {
                        model
                            .get_root()
                            .syntax()
                            .text()
                            .to_string()
                            .contains("Transfer.Read")
                    })
            })
            .expect("consumer file");

        assert_eq!(
            diagnostic_count(&mut ws, consumer_file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file_id)
            .expect("consumer semantic model");
        let receiver_types = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver))
                    if receiver.syntax().text().to_string().trim() == "data.proc" =>
                {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect::<Vec<_>>();
        assert_eq!(
            receiver_types,
            vec!["starfall_processor", "starfall_processor"]
        );
        assert_eq!(
            diagnostic_count(
                &mut ws,
                consumer_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            1
        );
    }

    #[test]
    fn nested_callback_inference_matches_fresh_analysis_across_edit_delete_and_reopen() {
        const STRUCTURAL_PRODUCER: &str = r#"
            ---@attribute self_guard(member: string)
            ---@class Entity
            ---@class starfall_processor: Entity
            ---@field Compile fun(self: starfall_processor, data: table)
            ---@field Error fun(self: starfall_processor, err: table)

            Entity = {}
            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@param value any
            ---@return TypeGuard<Entity>
            function isentity(value) end

            Transfer = {}
            function Transfer.Read(callback)
                local data = {}
                ---@type unknown
                local proc
                if isentity(proc) then data.proc = proc end
                callback(false, { owner = proc })
                callback(true, data)
            end
        "#;
        const NON_STRUCTURAL_PRODUCER: &str = r#"
            ---@attribute self_guard(member: string)
            ---@class Entity
            ---@class starfall_processor: Entity
            ---@field Compile fun(self: starfall_processor, data: table)
            ---@field Error fun(self: starfall_processor, err: table)

            Entity = {}
            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            Transfer = {}
            function Transfer.Read(callback)
                callback(true, nil)
            end
        "#;
        const CONSUMER: &str = r#"
            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local Ent_IsValid = FindMetaTable("Entity").IsValid
            Transfer.Read(function(ok, data)
                if ok and Ent_IsValid(data.proc) then
                    data.proc:Compile(data)
                    data.proc:Error({ message = "failed" })
                end
            end)
        "#;
        const EDITED_CONSUMER: &str = r#"
            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local unused = true
            local Ent_IsValid = FindMetaTable("Entity").IsValid
            Transfer.Read(function(ok, data)
                if ok and Ent_IsValid(data.proc) then
                    data.proc:Compile(data)
                    data.proc:Error({ message = "failed" })
                end
            end)
        "#;

        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let producer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/transfer.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/consumer.lua");
        ws.analysis
            .update_file_by_uri(&producer_uri, Some(STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("initial producer");
        let mut consumer_file_id = ws
            .analysis
            .update_file_by_uri(&consumer_uri, Some(CONSUMER.to_string()))
            .map(|(id, _)| id)
            .expect("initial consumer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), CONSUMER)
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(NON_STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("producer without structural callback data");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(NON_STRUCTURAL_PRODUCER), CONSUMER)
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("restored structural producer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), CONSUMER)
        );

        consumer_file_id = ws
            .analysis
            .update_file_by_uri(&consumer_uri, Some(EDITED_CONSUMER.to_string()))
            .map(|(id, _)| id)
            .expect("edited consumer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), EDITED_CONSUMER)
        );

        ws.analysis
            .remove_file_by_uri(&producer_uri)
            .0
            .expect("removed producer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(None, EDITED_CONSUMER)
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("reopened producer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), EDITED_CONSUMER)
        );

        ws.analysis
            .remove_file_by_uri(&consumer_uri)
            .0
            .expect("removed consumer");
        consumer_file_id = ws
            .analysis
            .update_file_by_uri(&consumer_uri, Some(CONSUMER.to_string()))
            .map(|(id, _)| id)
            .expect("reopened consumer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), CONSUMER)
        );
    }

    #[test]
    fn nullable_parent_member_evidence_selects_child_and_resolves_member_result() {
        for parent_type in ["Entity|NULL", "Entity|nil"] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);

            let file_id = ws.def(&format!(
                r#"
                ---@class TraceResult

                ---@class Entity
                ---@class NULL: Entity
                ---@class Player: Entity
                ---@field GetEyeTrace fun(self: Player): TraceResult

                ---@type {parent_type}
                local owner
                local trace = owner:GetEyeTrace()
                print(owner)
                print(trace)
                "#,
            ));

            assert_eq!(
                ws.humanize_type(last_name_type(&ws, file_id, "owner")),
                "Player",
                "nullable parent {parent_type} should retain child inference"
            );
            assert_eq!(
                ws.humanize_type(last_name_type(&ws, file_id, "trace")),
                "TraceResult",
                "nullable parent {parent_type} should resolve the child method return"
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
                0,
                "nullable parent {parent_type} should resolve the child method"
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
                1,
                "nullable parent {parent_type} should keep the unguarded-child warning"
            );
        }
    }

    #[test]
    fn null_union_resolves_member_owned_by_parent_without_child_inference() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Vector

            ---@class Entity
            ---@field EyePos fun(self: Entity): Vector
            ---@class NULL: Entity

            ---@type Entity|NULL
            local owner
            local position = owner:EyePos()
            print(owner)
            print(position)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "owner")),
            "(Entity|NULL)"
        );
        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "position")),
            "Vector"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn nullable_concrete_class_resolves_its_member_without_child_inference() {
        for receiver_type in ["Player|NULL", "Player|nil"] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);

            let file_id = ws.def(&format!(
                r#"
                ---@class TraceResult

                ---@class Entity
                ---@class NULL: Entity
                ---@class Player: Entity
                ---@field GetEyeTrace fun(self: Player): TraceResult

                ---@type {receiver_type}
                local owner
                local trace = owner:GetEyeTrace()
                print(owner)
                print(trace)
                "#,
            ));

            assert_eq!(last_name_type(&ws, file_id, "owner"), ws.ty(receiver_type));
            assert_eq!(
                ws.humanize_type(last_name_type(&ws, file_id, "trace")),
                "TraceResult"
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
                0
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
                0
            );
        }
    }

    #[test]
    fn generated_finite_player_method_names_drive_child_inference() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        ws.def_file(
            "gamemodes/terrortown/gamemode/shared.lua",
            r#"
            ---@class Entity
            ---@class Player: Entity

            ROLE_MAX = 2
            ROLE_STRINGS = {
                [0] = "Innocent",
                [1] = "Killer",
                [2] = "Old Man",
            }
            "#,
        );

        ws.def_file(
            "gamemodes/terrortown/gamemode/player_ext_shd.lua",
            r#"
            ---@type Player
            local plymeta = {}

            for role = 0, ROLE_MAX do
                local name = string.gsub(ROLE_STRINGS[role], "%s+", "")
                plymeta["Get" .. name] = function(self)
                    return true
                end
                plymeta["Is" .. name] = plymeta["Get" .. name]
                plymeta["IsActive" .. name] = function(self)
                    return true
                end
            end
            "#,
        );

        let file_id = ws.def_file(
            "gamemodes/terrortown/entities/entities/ttt_crowbar/shared.lua",
            r#"
            ---@type Entity
            local activator
            local is_killer = activator:IsKiller()
            print(activator)
            print(is_killer)
            "#,
        );

        let player_id = ws
            .analysis
            .compilation
            .get_db()
            .get_type_index()
            .find_type_decl(file_id, "Player")
            .expect("Player type")
            .get_id();
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_dynamic_field_index()
                .has_field(
                    &crate::DynamicFieldOwner::Type(player_id.clone()),
                    "IsKiller"
                ),
            "generated IsKiller dynamic field"
        );
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_dynamic_field_index()
                .has_field(
                    &crate::DynamicFieldOwner::Type(player_id.clone()),
                    "IsOldMan"
                ),
            "generated names apply their constant string transform"
        );
        let mut cache = crate::LuaInferCache::new(file_id, Default::default());
        assert!(
            crate::semantic::resolve_dynamic_field_member(
                ws.analysis.compilation.get_db(),
                &mut cache,
                &crate::LuaType::Ref(player_id.clone()),
                &crate::LuaMemberKey::Name("IsKiller".into()),
                None,
            )
            .is_ok_and(|resolution| resolution.is_some()),
            "generated IsKiller dynamic field resolves"
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "activator")),
            "Player"
        );
        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "is_killer")),
            "true"
        );
    }

    #[test]
    fn unguarded_child_fact_refines_a_declared_parameter_for_hover_and_members() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetShootPos fun(self: Player): Vector
            ---@class Vector

            ---@param owner Entity
            local function use_owner(owner)
                local pos = owner:GetShootPos()
                print(owner)
                print(pos)
            end
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "owner")),
            "Player"
        );
        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "pos")),
            "Vector"
        );
    }

    #[test]
    fn unguarded_child_fact_rebinds_an_earlier_cross_file_local_type_cache_in_the_same_batch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (
                "definitions.lua",
                r#"
                ---@class Vector
                ---@class Entity
                ---@class Player: Entity
                ---@field GetShootPos fun(self: Player): Vector

                SharedTable = {}
                "#,
            ),
            (
                "consumer.lua",
                r#"
                local cross_file_anchor = SharedTable

                ---@type Entity
                local owner
                local method = owner.GetShootPos
                local position = owner:GetShootPos()
                "#,
            ),
        ]);

        let definitions_file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri("definitions.lua"))
            .expect("expected definitions file id");
        let consumer_file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri("consumer.lua"))
            .expect("expected consumer file id");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file_id)
            .expect("expected consumer semantic model");
        let method_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "method")
            })
            .expect("expected local method name");
        let method_decl_id =
            crate::LuaDeclId::new(consumer_file_id, method_name.get_range().start());

        let db = ws.analysis.compilation.get_db();
        let cross_file_dependents = db
            .get_type_index()
            .files_with_cross_file_type_caches_referencing_files(&HashSet::from([
                definitions_file_id,
            ]));
        assert!(
            cross_file_dependents.contains(&consumer_file_id),
            "consumer must depend on a cross-file type cache for this stabilization regression"
        );

        let method_type = db
            .get_type_index()
            .get_type_cache(&method_decl_id.into())
            .expect("expected persistent type cache for local method")
            .as_type();
        assert!(
            method_type.is_function(),
            "late unguarded-child inference must rebind the earlier local method cache, got {method_type:?}"
        );
    }

    #[test]
    fn unguarded_child_ties_produce_a_deterministic_union() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Bravo: Base
            ---@field Shared fun(self: Bravo)
            ---@class Alpha: Base
            ---@field Shared fun(self: Alpha)
            ---@type Base
            local value
            value:Shared()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "(Alpha|Bravo)"
        );
    }

    #[test]
    fn unguarded_child_does_not_refine_parent_member_override() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@field Shared fun(self: Base)
            ---@class Child: Base
            ---@field Shared fun(self: Child)
            ---@type Base
            local value
            value:Shared()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
    }

    #[test]
    fn unguarded_child_uses_a_grandchild() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Middle: Base
            ---@class Leaf: Middle
            ---@field LeafOnly fun(self: Leaf)
            ---@type Base
            local value
            value:LeafOnly()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Leaf"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
    }

    #[test]
    fn truly_absent_child_member_stays_an_undefined_method() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field Present fun(self: Child)
            ---@type Base
            local value
            value:Absent()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            1
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn unguarded_child_respects_member_realm_at_the_use_site() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def_file(
            "lua/autorun/server/sv_child.lua",
            r#"
            ---@class Base
            ---@class ClientChild: Base
            ---@realm client
            function ClientChild:ClientOnly() end
            ---@type Base
            local value
            value:ClientOnly()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            1
        );
    }

    #[test]
    fn unguarded_child_evidence_does_not_cross_assignments() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class First: Base
            ---@field FirstOnly fun(self: First)
            ---@class Second: Base
            ---@field SecondOnly fun(self: Second)
            ---@type Base
            local value
            value:FirstOnly()
            value = value
            value:SecondOnly()
            print(value)
            "#,
        );

        // The self-assignment makes the second use a distinct reaching definition;
        // both regions receive independent child facts rather than one mixed score.
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            2
        );
    }

    #[test]
    fn later_concrete_assignment_overrides_declaration_child_fact_for_semantic_info() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@return Base
            local function make_base() end
            ---@type Base
            local value
            value:ChildOnly()
            value = make_base()
            print(value)
            "#,
        );

        let info = last_name_info(&ws, file_id, "value");
        assert_eq!(
            (ws.humanize_type(info.display_typ().clone()), info.origin),
            ("Base".to_string(), SemanticInfoOrigin::Actual)
        );
    }

    #[test]
    fn explicit_field_guard_suppresses_unguarded_child_provenance() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field OnlyChild fun(self: Child)
            ---@type Base
            local value
            if value.OnlyChild then
                value:OnlyChild()
                print(value)
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn member_assignment_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            value.ChildOnly = function() end
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn matching_short_circuit_member_guard_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = value.ChildOnly and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn optional_member_fallback_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly string
            ---@type Base
            local value
            local result = value.ChildOnly or "fallback"
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn guarded_method_and_optional_field_fallbacks_are_not_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@field GetClass fun(self: Base): string
            ---@class Child: Base
            ---@field GetPrintName fun(self: Child): string
            ---@field PrintName string
            ---@type Base
            local value
            local result = value.GetPrintName and value:GetPrintName() or value.PrintName or value:GetClass() or "..."
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn child_method_call_controlling_fallback_is_still_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child): string?
            ---@type Base
            local value
            local result = value:ChildOnly() or "fallback"
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn child_member_used_before_fallback_is_still_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly number
            ---@type Base
            local value
            local result = value.ChildOnly + 1 or 0
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn outer_fallback_does_not_suppress_deferred_closure_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly string
            ---@type Base
            local value
            local callback = (function()
                return value.ChildOnly
            end) or function() end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn parenthesized_positive_short_circuit_guard_suppresses_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = (value.ChildOnly) and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn nested_positive_short_circuit_guard_suppresses_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local enabled = true
            local result = enabled and value.ChildOnly and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn deferred_closure_call_does_not_inherit_short_circuit_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local callback = value.ChildOnly and function()
                value:ChildOnly()
            end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn deferred_named_function_does_not_inherit_outer_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            if value.ChildOnly then
                function callback()
                    value:ChildOnly()
                end
            end
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn deferred_local_function_does_not_inherit_outer_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            if value.ChildOnly then
                local function callback()
                    value:ChildOnly()
                end
            end
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn deferred_closure_local_member_guard_remains_guarded() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local callback = function()
                if value.ChildOnly then
                    value:ChildOnly()
                end
            end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn deferred_closure_keeps_concrete_assignment_before_creation() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            ---@type Child
            local child
            value = child
            local callback = function()
                value:ChildOnly()
            end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(nth_name_type_from_end(&ws, file_id, "value", 1)),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn deferred_closure_does_not_capture_assignment_after_creation() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local callback = function()
                print(value)
            end
            ---@type Child
            local child
            value = child
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(nth_name_type_from_end(&ws, file_id, "value", 2)),
            "Base"
        );
    }

    #[test]
    fn nested_deferred_closure_ignores_each_outer_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local outer = value.ChildOnly and function()
                local inner = value.ChildOnly and function()
                    value:ChildOnly()
                end
                print(inner)
            end
            print(outer)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn negated_short_circuit_member_check_does_not_guard_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = not value.ChildOnly and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn comparison_short_circuit_member_check_does_not_guard_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = value.ChildOnly == nil and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn table_short_circuit_member_check_does_not_guard_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = { value.ChildOnly } and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn child_method_call_used_as_a_condition_is_still_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field IsReady fun(self: Child): boolean
            ---@type Base
            local value
            if value:IsReady() then
                print(value)
            end
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn immutable_return_cast_guard_suppresses_closure_child_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            local function make(ent)
                if ent:IsPlayer() then
                    local direct = ent:GetPlayerColor()
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    return direct, callback
                end
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn a_member_read_a_guard_on_the_same_path_proves_present_is_not_an_unguarded_child() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class prop_effect: Entity
            ---@field AttachedEntity Entity

            ---@param ent Entity
            local function target(ent)
                if IsValid(ent.AttachedEntity) then ent = ent.AttachedEntity end
                local other = IsValid(ent.AttachedEntity) and ent.AttachedEntity or ent
                return ent, other
            end

            ---@param ent Entity
            local function unguarded(ent)
                return ent.AttachedEntity
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range.start.line, 14);
    }

    #[test]
    fn a_guard_does_not_cover_a_read_after_its_root_is_replaced_or_shadowed() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class prop_effect: Entity
            ---@field AttachedEntity Entity

            ---@param ent Entity
            ---@param other Entity
            local function replaced(ent, other, flag)
                if IsValid(ent.AttachedEntity) then
                    if flag then ent = other end
                    return ent.AttachedEntity
                end
            end

            ---@param ent Entity
            ---@param other Entity
            local function shadowed(ent, other)
                if IsValid(ent.AttachedEntity) then
                    local ent = other
                    return ent.AttachedEntity
                end
            end
            "#,
        );

        let lines = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, vec![10, 19]);
    }

    fn guarded_lines(ws: &mut VirtualWorkspace, code: &str) -> Vec<u32> {
        enable_gmod(ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(code);
        diagnostics_for(ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect()
    }

    const ATTACHED: &str = r#"
            ---@class Entity
            ---@class prop_effect: Entity
            ---@field AttachedEntity Entity
            ---@class Panel
            ---@class DVScrollBar: Panel
            ---@field btnUp Panel
            ---@field btnDown Panel
    "#;

    #[test]
    fn a_guard_is_not_undone_by_a_write_in_a_closure_it_never_calls() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param ent Entity
            ---@param other Entity
            local function target(ent, other)
                if IsValid(ent.AttachedEntity) then
                    local reset = function() ent = other end
                    return ent.AttachedEntity, reset
                end
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_guard_through_a_constant_key_covers_the_named_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param ent Entity
            local function target(ent)
                local key = \"AttachedEntity\"
                if IsValid(ent[key]) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn sibling_reads_inside_a_guard_are_covered_in_either_order() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param parent Panel
            local function paint(parent)
                if IsValid(parent.btnUp) then
                    local down = parent.btnDown
                    local up = parent.btnUp
                    return up, down
                end
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn an_unguarded_read_through_a_nested_receiver_is_reported_after_a_guarded_one() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@class Holder
            ---@field panel Panel

            ---@param holder Holder
            local function paint(holder)
                if IsValid(holder.panel.btnUp) then
                    local up = holder.panel.btnUp
                end
                return holder.panel.btnDown
            end
            "
            ),
        );
        assert_eq!(lines, vec![17]);
    }

    #[test]
    fn a_type_guard_whose_alias_allows_nil_does_not_cover_its_reads() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@alias MaybeEntity Entity|nil
            ---@param value any
            ---@return TypeGuard<MaybeEntity>
            function isMaybeEntity(value) end

            ---@param ent Entity
            local function target(ent)
                if isMaybeEntity(ent.AttachedEntity) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert_eq!(lines, vec![17]);
    }

    #[test]
    fn a_colon_presence_predicate_tests_its_receiver() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Probe
            ---@return TypeGuard<Probe>
            function Probe:Holds(other) end

            ---@param probe Probe
            local function check(probe, other)
                return probe:Holds(other.Child)
            end
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let call = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaCallExpr>()
            .next()
            .expect("call");
        let target = crate::semantic::presence_guard_target(
            semantic_model.get_db(),
            &mut semantic_model.get_cache().borrow_mut(),
            &call,
        )
        .expect("a presence guard");
        assert_eq!(target.syntax().text().to_string(), "probe");
    }

    #[test]
    fn a_guard_on_the_read_path_covers_it_for_a_local_the_guard_reassigns() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@class Trace
            ---@field Entity Entity

            ---@param trace Trace
            local function click(trace)
                local ent = trace.Entity
                if IsValid(ent.AttachedEntity) then ent = ent.AttachedEntity end
                return ent
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_guard_on_a_member_the_base_type_has_does_not_cover_a_child_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            r#"
            ---@class Entity
            ---@field Enabled boolean
            ---@class Player: Entity
            ---@field PlayerOnly number

            ---@param ent Entity
            local function read(ent)
                if ent.Enabled then
                    return ent.PlayerOnly
                end
            end
            "#,
        );
        assert_eq!(lines, vec![9]);
    }

    #[test]
    fn a_short_circuit_guard_through_a_constant_key_covers_the_named_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param ent Entity
            local function target(ent)
                local key = \"AttachedEntity\"
                return IsValid(ent[key]) and ent.AttachedEntity or ent
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_type_guard_whose_union_hides_a_nullable_alias_does_not_cover_its_reads() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@alias MaybeEntity Entity|nil
            ---@alias GuardedValue MaybeEntity|string
            ---@param value any
            ---@return TypeGuard<GuardedValue>
            function isMaybeEntity(value) end

            ---@param ent Entity
            local function target(ent)
                if isMaybeEntity(ent.AttachedEntity) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    #[test]
    fn a_nested_inference_is_reported_at_a_read_of_the_child_it_names() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            r#"
            ---@class Panel
            ---@class Scroll: Panel
            ---@field up Panel
            ---@field down Panel
            ---@class Label: Panel
            ---@field text string
            ---@class Holder
            ---@field panel Panel

            ---@param holder Holder
            local function paint(holder)
                if IsValid(holder.panel.up) then
                    local up = holder.panel.up
                    local down = holder.panel.down
                end
                return holder.panel.text
            end
            "#,
        );
        assert!(!lines.contains(&16), "{lines:?}");
    }

    #[test]
    fn a_presence_guard_still_lets_its_reads_infer_the_child_for_sibling_members() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DVScrollBar: Panel
            ---@field btnUp Panel
            ---@field btnDown Panel

            ---@param parent Panel
            local function paint(parent)
                if IsValid(parent.btnUp) then
                    local up = parent.btnUp
                    local down = parent.btnDown
                    return up, down
                end
            end
            "#,
        );

        for code in [
            DiagnosticCode::InferUnguardedChild,
            DiagnosticCode::UndefinedField,
        ] {
            let diagnostics = diagnostics_for(&mut ws, file_id, code);
            assert!(diagnostics.is_empty(), "{code:?}: {diagnostics:?}");
        }
    }

    #[test]
    fn an_unguarded_read_is_reported_even_when_a_guarded_read_comes_first() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DVScrollBar: Panel
            ---@field btnUp Panel
            ---@field btnDown Panel

            ---@param parent Panel
            local function paint(parent)
                if IsValid(parent.btnUp) then
                    local up = parent.btnUp
                end
                return parent.btnDown
            end
            "#,
        );

        let lines = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, vec![11]);
    }

    #[test]
    fn mutable_return_cast_guard_reports_only_closure_child_access() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local direct = ent:GetPlayerColor()
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    ent = replacement
                    return direct, callback
                end
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range.start.line, 15);
        assert_eq!(
            diagnostics[0].message,
            "expected `Player` but found `Entity`. Add a guard to narrow the parent to `Player`."
        );
    }

    #[test]
    fn unguarded_child_diagnostic_uses_receiver_type_after_return_propagation() {
        for _ in 0..32 {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            let file_id = ws.def(
                r#"
                ---@class Entity
                ---@class Player: Entity
                ---@field SteamID fun(self: Player): string

                ---@param user Entity
                local function read_steam_id(user)
                    local steam_id = user:SteamID()
                    return steam_id
                end
                "#,
            );

            let diagnostics =
                diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(
                diagnostics[0].message,
                "expected `Player` but found `Entity`. Add a guard to narrow the parent to `Player`."
            );
        }
    }

    #[test]
    fn return_cast_closure_capture_tracks_incremental_mutability_and_reopen() {
        const IMMUTABLE_SOURCE: &str = r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    return callback
                end
            end
        "#;
        const MUTABLE_SOURCE: &str = r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    ent = replacement
                    return callback
                end
            end
        "#;

        let fresh_diagnostic_count = |source: &str| {
            let mut fresh = VirtualWorkspace::new();
            enable_gmod(&mut fresh);
            let file_id = fresh.def(source);
            diagnostic_count(&mut fresh, file_id, DiagnosticCode::InferUnguardedChild)
        };

        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/issue_49_incremental.lua");
        let file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(IMMUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("initial immutable file");
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            fresh_diagnostic_count(IMMUTABLE_SOURCE)
        );

        let updated_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(MUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("mutable update");
        assert_eq!(updated_file_id, file_id);
        assert_eq!(
            diagnostic_count(
                &mut ws,
                updated_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            fresh_diagnostic_count(MUTABLE_SOURCE)
        );

        let restored_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(IMMUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("restored immutable update");
        assert_eq!(
            diagnostic_count(
                &mut ws,
                restored_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            fresh_diagnostic_count(IMMUTABLE_SOURCE)
        );

        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("removed incremental file");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(IMMUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("reopened immutable file");
        assert_eq!(
            diagnostic_count(
                &mut ws,
                reopened_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            fresh_diagnostic_count(IMMUTABLE_SOURCE)
        );
    }

    #[test]
    fn unguarded_child_reports_once_and_keeps_unknown_diagnostic_separate() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field One fun(self: Child)
            ---@field Two fun(self: Child)
            ---@type Base
            local value
            value:One()
            value:Two()
            "#,
        );

        let child_diagnostics =
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(child_diagnostics.len(), 1, "{child_diagnostics:?}");
        assert_eq!(child_diagnostics[0].range.start.line, 7);
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnknown),
            0
        );
    }

    #[test]
    fn declared_field_narrows_to_the_only_child_defining_the_member() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            h.Owner:ConCommand("kill")
            h.Owner:ConCommand("say")
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Player` but found `Entity`. Add a guard to narrow the parent to `Player`."
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["Player", "Player"]
        );
    }

    #[test]
    fn declared_field_narrows_from_a_single_use() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            h.Owner:ConCommand("kill")
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["Player"]
        );
    }

    #[test]
    fn declared_field_narrowing_follows_a_deeper_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)
            ---@class Inner
            ---@field Owner Entity
            ---@class Outer
            ---@field data Inner
            ---@type Outer
            local o
            o.data.Owner:ConCommand("kill")
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "o.data.Owner"),
            vec!["Player"]
        );
    }

    #[test]
    fn declared_field_member_owned_by_the_base_is_not_narrowed() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@field GetClass fun(self: Entity): string
            ---@class Player: Entity
            ---@field GetClass fun(self: Player): string
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            local class = h.Owner:GetClass()
            print(class)
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["Entity"]
        );
    }

    #[test]
    fn guarded_declared_field_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            if h.Owner:IsPlayer() then
                h.Owner:ConCommand("kill")
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn declared_field_tie_names_the_member_instead_of_an_unwritable_union() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Bravo: Entity
            ---@field Shared fun(self: Bravo)
            ---@class Alpha: Entity
            ---@field Shared fun(self: Alpha)
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            h.Owner:Shared()
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "`Shared` is not defined on `Entity`. Add a guard that narrows the parent to one of \
             `Alpha`, `Bravo`."
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["(Alpha|Bravo)"]
        );
    }

    #[test]
    fn unguarded_child_tie_caps_the_listed_candidate_types() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DAlpha: Panel
            ---@field Shared fun(self: DAlpha)
            ---@class DBravo: Panel
            ---@field Shared fun(self: DBravo)
            ---@class DCharlie: Panel
            ---@field Shared fun(self: DCharlie)
            ---@class DDelta: Panel
            ---@field Shared fun(self: DDelta)
            ---@class DEcho: Panel
            ---@field Shared fun(self: DEcho)
            ---@type Panel
            local value
            value:Shared()
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "`Shared` is not defined on `Panel`. Add a guard that narrows the parent to one of \
             `DAlpha`, `DBravo`, `DCharlie` and 2 more."
        );
    }

    /// `Alpha` and `Bravo` sit inside a `return`, so the early evidence pass
    /// narrows their receiver to `Middle` and publishes it. The late pass then
    /// sees that narrowed receiver again at the `Shared` use, and `Shared` is
    /// defined on `Leaf`, a child of `Middle`. Scoring against the published
    /// type would descend one more level of the class tree per pass, so every
    /// use has to keep scoring against `Entity`.
    #[test]
    fn unguarded_child_path_does_not_rescore_against_its_own_narrowing() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Middle: Entity
            ---@field Alpha fun(self: Middle)
            ---@field Bravo fun(self: Middle)
            ---@class Other: Entity
            ---@field Shared fun(self: Other)
            ---@class Leaf: Middle
            ---@field Shared fun(self: Leaf)

            ---@param value any
            ---@return TypeGuard<Entity>
            function isentity(value) end

            ---@class Holder
            ---@field proc unknown

            ---@type Holder
            local h

            local function run(a, b)
                if not isentity(h.proc) then return end
                if a then return h.proc:Alpha() end
                if b then return h.proc:Bravo() end
                return h.proc:Shared()
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Leaf` but found `Entity`. Add a guard to narrow the parent to `Leaf`."
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.proc"),
            vec!["Leaf", "Leaf", "Leaf"]
        );
    }

    #[test]
    fn table_literal_through_declared_field_does_not_report_unguarded_child() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity

            ---@class ContextChildA: Entity
            local A = {}
            function A:special() return true end

            ---@class ContextChildB: Entity
            local B = {}
            function B:special() return true end

            ---@class ContextHolder
            ---@field condition (fun(id: string, ent: ContextChildA|ContextChildB): boolean?)?

            ---@class ContextMeta
            ---@field templates table<string, ContextHolder>?

            ---@type ContextMeta
            local meta = {}
            meta.templates = {
                example = {
                    condition = function(id, ent)
                        return ent:special()
                    end,
                },
            }
            "#,
        );

        let child_diagnostics =
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(
            child_diagnostics.len(),
            0,
            "declared field contract should type `ent` as ContextChildA|ContextChildB: \
             {child_diagnostics:?}"
        );
    }

    #[test]
    fn local_fallback_reads_a_class_binding_another_files_local_fallback_writes() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_files(vec![
            (
                "lua/autorun/radio_tuner.lua",
                r#"
                ---@class Entity
                ---@class Radio: Entity
                ---@field Tune fun(self: Radio)

                ---@type Entity
                local tuned
                tuned:Tune()
                "#,
            ),
            (
                "lua/autorun/radio_muter.lua",
                r#"
                ---@class Radio

                ---@type Entity
                local muted
                muted:Mute()
                "#,
            ),
        ]);
        let tuner_file_id = ws
            .analysis
            .get_file_id(
                &ws.virtual_url_generator
                    .new_uri("lua/autorun/radio_tuner.lua"),
            )
            .expect("tuner file id");

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, tuner_file_id, "tuned")),
            "Radio"
        );
    }

    const TYPES: &str = "lua/autorun/sh_types.lua";
    const LIB: &str = "lua/autorun/sh_lib.lua";
    const USER: &str = "lua/autorun/sh_user.lua";
    const TYPES_TEXT: &str = "---@class Player
---@class Vehicle
---@class Dummy1
---@param ply Player
function Kick(ply) end
---@param car Vehicle
function Tow(car) end
";
    const LIB_TEXT: &str = "Lib = {}
function Lib.Get(a) return Mystery end
";
    const USER_TEXT: &str = "local target = Lib.Get(1)
Kick(target)
local towed = Lib.Get(2)
Tow(towed)
";

    fn retyped_types_text() -> String {
        TYPES_TEXT.replace("---@param ply Player", "---@param ply Dummy1")
    }

    fn file_diagnostics(ws: &VirtualWorkspace, file_id: crate::FileId) -> Vec<String> {
        let mut diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .map(|diagnostic| {
                format!(
                    "{:?} {:?} {}",
                    diagnostic.range, diagnostic.code, diagnostic.message
                )
            })
            .collect::<Vec<_>>();
        diagnostics.sort();
        diagnostics
    }

    /// An edit that retypes one local's fallback leaves the types and
    /// diagnostics a cold build of the edited text has.
    #[test]
    fn an_edit_leaves_the_local_fallbacks_and_diagnostics_a_cold_build_has() {
        let edited = retyped_types_text();
        let mut warm = VirtualWorkspace::new();
        let file_ids = warm.def_files(vec![
            (TYPES, TYPES_TEXT),
            (LIB, LIB_TEXT),
            (USER, USER_TEXT),
        ]);
        warm.def_file(TYPES, &edited);
        let mut cold = VirtualWorkspace::new();
        cold.def_files(vec![
            (TYPES, edited.as_str()),
            (LIB, LIB_TEXT),
            (USER, USER_TEXT),
        ]);

        let settled = warm.settled_digest();
        assert!(settled.iter().any(|line| line.ends_with("target = Dummy1")));
        assert!(settled.iter().any(|line| line.ends_with("towed = Vehicle")));
        assert_eq!(settled, cold.settled_digest());
        assert_eq!(
            file_diagnostics(&warm, file_ids[2]),
            file_diagnostics(&cold, file_ids[2])
        );
        assert!(warm.stale_derivations().is_empty());
        assert!(warm.phantom_derivations().is_empty());
    }

    /// Editing what types one local re-runs the fallbacks of that local's
    /// file alone.
    #[test]
    fn editing_what_types_one_local_re_runs_only_its_files_fallbacks() {
        let fallbacks_after_the_edit = |files: Vec<(&str, &str)>| {
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files);
            ws.def_file(TYPES, &retyped_types_text());
            ws.analysis
                .compilation
                .last_stats()
                .evaluations(crate::db_index::derivation::DerivationKind::LocalFallbacks)
        };

        let with_user = fallbacks_after_the_edit(vec![
            (TYPES, TYPES_TEXT),
            (LIB, LIB_TEXT),
            (USER, USER_TEXT),
        ]);
        let without_user = fallbacks_after_the_edit(vec![(TYPES, TYPES_TEXT), (LIB, LIB_TEXT)]);

        assert_eq!(with_user, without_user + 1);
    }

    const BLAST: &str = "lua/autorun/sh_blast.lua";
    const FIRE: &str = "lua/autorun/sh_fire.lua";
    const BLAST_TEXT: &str = "---@class Entity
---@class Player: Entity
---@param inflictor Entity
---@param attacker Player
function Blast(inflictor, attacker) end
";
    const FIRE_TEXT: &str = "local attacker, inflictor
function Fire(params)
    attacker = params.attacker
    inflictor = params.inflictor or attacker
    Blast(inflictor, attacker)
end
";

    /// Locals whose guesses feed each other, one assigned from the other, are
    /// guessed together: a cold build settles them, and an edit of what types
    /// them leaves what a cold build of the edited text has.
    #[test]
    fn locals_whose_guesses_feed_each_other_settle_as_a_cold_build_has_them() {
        let edited = BLAST_TEXT.replace("---@param attacker Player", "---@param attacker Entity");
        let mut warm = VirtualWorkspace::new();
        warm.def_files(vec![(BLAST, BLAST_TEXT), (FIRE, FIRE_TEXT)]);
        assert!(warm.stale_derivations().is_empty());
        warm.def_file(BLAST, &edited);
        let mut cold = VirtualWorkspace::new();
        cold.def_files(vec![(BLAST, edited.as_str()), (FIRE, FIRE_TEXT)]);

        assert_eq!(warm.settled_digest(), cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());
        assert!(warm.phantom_derivations().is_empty());
    }

    const PANEL_TREE: &str = r#"
        ---@class Panel
        ---@class TextEntry: Panel
        ---@class DTextEntry: TextEntry
        ---@field SetUpdateOnType fun(self: DTextEntry, update: boolean)
        ---@class DPanel: Panel
        ---@class DNumPad: DPanel
        ---@field SetUpdateOnType fun(self: DNumPad, update: boolean)
        ---@class DNumPadSub: DNumPad
        "#;

    fn panel_tree_diagnostics(consumer: &str) -> (usize, usize, Vec<String>) {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def(PANEL_TREE);
        let file_id = ws.def(consumer);
        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        (
            unguarded.len(),
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            unguarded.into_iter().map(|d| d.message).collect(),
        )
    }

    #[test]
    fn unguarded_child_finds_a_grandchild_that_declares_the_member() {
        let (unguarded, undefined, messages) = panel_tree_diagnostics(
            r#"
            ---@type Panel
            local filter
            filter:SetUpdateOnType(true)
            "#,
        );

        assert_eq!(undefined, 0);
        assert_eq!(unguarded, 1);
        assert!(messages[0].contains("`DTextEntry`"), "{messages:?}");
    }

    #[test]
    fn unguarded_child_lists_the_nearest_declaring_class_only() {
        let (_, _, messages) = panel_tree_diagnostics(
            r#"
            ---@type Panel
            local filter
            filter:SetUpdateOnType(true)
            "#,
        );

        assert!(
            messages[0].contains("one of `DNumPad`, `DTextEntry`."),
            "{messages:?}"
        );
    }

    /// A subclass of the parent that does not declare the used member leaves the
    /// unguarded use alone; one that declares it re-evaluates the use.
    #[test]
    fn adding_a_subclass_re_runs_unguarded_uses_only_when_it_declares_the_member() {
        const CONSUMER: &str = "lua/autorun/client/consumer.lua";
        const CONSUMER_TEXT: &str = "---@type Panel
local filter
filter:SetUpdateOnType(true)
";
        let fallbacks_after_adding = |file: &str, text: &str| {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            ws.def_files(vec![
                ("lua/autorun/sh_types.lua", PANEL_TREE),
                (CONSUMER, CONSUMER_TEXT),
            ]);
            ws.def_file(file, text);
            ws.analysis
                .compilation
                .last_stats()
                .evaluations(crate::db_index::derivation::DerivationKind::LocalFallbacks)
        };

        let unrelated_class =
            fallbacks_after_adding("lua/autorun/sh_other.lua", "---@class Unrelated");
        let unrelated_subclass =
            fallbacks_after_adding("lua/autorun/sh_other.lua", "---@class DLabel: Panel");
        let declaring_subclass = fallbacks_after_adding(
            "lua/autorun/sh_other.lua",
            "---@class DSlider: Panel
---@field SetUpdateOnType fun(self: DSlider, update: boolean)",
        );

        assert_eq!(unrelated_subclass, unrelated_class);
        assert!(declaring_subclass > unrelated_class);
    }

    #[test]
    fn unguarded_child_picks_the_class_that_has_every_used_member() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Weapon
            ---@class SWEP: Weapon
            ---@field Clip fun(self: SWEP): number
            ---@class SubSWEP: SWEP
            ---@field Reload fun(self: SubSWEP)

            ---@param wep Weapon
            local function use(wep)
                wep:Reload()
                return wep:Clip()
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert!(
            unguarded
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("`SWEP`")),
            "{unguarded:?}"
        );
    }

    #[test]
    fn child_method_is_not_a_candidate_for_a_field_indexed_into() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class D: Base
            ---@field Thing fun(self: D)

            ---@param base Base
            local function use(base)
                base.Thing:Other()
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn child_method_is_a_candidate_when_only_the_receiver_is_a_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Weapon: Entity
            ---@field IsCarried fun(self: Weapon): boolean
            ---@class Holder
            ---@field WeaponEnt Entity

            ---@param holder Holder
            local function use(holder)
                holder.WeaponEnt:IsCarried()
            end
            "#,
        );

        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(unguarded.len(), 1, "{unguarded:?}");
        assert!(unguarded[0].message.contains("`Weapon`"), "{unguarded:?}");
    }

    #[test]
    fn child_method_is_a_candidate_for_a_dotted_call_on_a_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetActive fun(self: Player): Entity
            ---@class NPC: Entity
            ---@field GetActive fun(self: NPC): Entity
            ---@class Effect
            ---@field Player Entity

            ---@param x Effect
            local function use(x)
                x.Player:GetActive()
            end
            "#,
        );

        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(unguarded.len(), 1, "{unguarded:?}");
        assert!(
            unguarded[0].message.contains("`NPC`") && unguarded[0].message.contains("`Player`"),
            "{unguarded:?}"
        );
    }

    #[test]
    fn child_method_is_a_candidate_on_an_assigned_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Weapon: Entity
            ---@field IsCarried fun(self: Weapon): boolean
            ---@class CEffectData
            ---@field GetEntity fun(self: CEffectData): Entity
            ---@class Effect

            ---@param effect Effect
            ---@param data CEffectData
            local function init(effect, data)
                effect.WeaponEnt = data:GetEntity()
                effect.WeaponEnt:IsCarried()
            end
            "#,
        );

        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(unguarded.len(), 1, "{unguarded:?}");
        assert!(unguarded[0].message.contains("`Weapon`"), "{unguarded:?}");
    }

    #[test]
    fn child_method_is_still_a_candidate_for_a_method_call() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class D: Base
            ---@field Thing fun(self: D)

            ---@param base Base
            local function use(base)
                base:Thing()
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn assigned_member_path_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class AvatarImage: Panel
            ---@field SetPaintedManually fun(self: AvatarImage, painted: boolean)
            ---@class DFrame: Panel
            ---@field Avatar AvatarImage

            vgui = vgui or {}

            ---@generic T : Panel
            ---@param classname `T`
            ---@param parent? Panel
            ---@return T
            function vgui.Create(classname, parent) end

            ---@param pnl Panel
            local function build(pnl)
                pnl.Avatar = vgui.Create("AvatarImage", pnl)
                pnl.Avatar:SetPaintedManually(true)
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    fn guard_form_diagnostic_count(use_site: &str) -> usize {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let source = format!(
            r#"
            ---@class Entity
            ---@class Car: Entity
            ---@field Honk fun(self: Car)
            ---@param ent Entity
            local function use(ent)
                {use_site}
            end
            "#
        );
        let file_id = ws.def(&source);
        diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
    }

    /// The unguarded-child and usage-inference hints on a callback's read of
    /// a child-only member off its `panel` param.
    fn callback_panel_hints(param_doc: &str) -> (usize, usize) {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let source = format!(
            r#"
            ---@class Panel
            ---@class FacePanel: Panel
            ---@field charData table
            local data = {{
                {param_doc}
                Think = function(animation, panel)
                    return panel.charData
                end,
            }}
            return data
            "#
        );
        let file_id = ws.def(&source);
        (
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnknown),
        )
    }

    #[test]
    fn a_child_read_off_a_param_only_its_name_types_is_inferred_from_usage() {
        assert_eq!(callback_panel_hints(""), (0, 1));
    }

    #[test]
    fn a_child_read_off_a_param_declared_as_the_parent_needs_a_guard() {
        assert_eq!(callback_panel_hints("---@param panel Panel"), (1, 0));
    }

    #[test]
    fn a_member_an_anonymous_panel_class_declares_suggests_no_other_class() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/menu/annotations.lua",
            r#"
            ---@class Panel
            ---@class DPanel: Panel
            ---@class PublishWindow: Panel
            ---@field UpdateProgress Panel

            ---@generic T: table
            ---@[call_arg("gmod.load", "include")]
            ---@[call_arg("gmod.vgui_panel", "register_file")]
            ---@param file string
            ---@return T
            function vgui.RegisterFile(file) end
            "#,
        );
        ws.def_file(
            "lua/menu/vgui/workshop.lua",
            r#"
            PANEL.Base = "DPanel"

            function PANEL:UpdateProgress(done) end
            "#,
        );
        let file_id = ws.def_file(
            "lua/menu/mount.lua",
            r#"
            local pnlWorkshop = vgui.RegisterFile("vgui/workshop.lua")

            ---@param pnl Panel
            local function show(pnl)
                pnl:UpdateProgress(1)
            end
            return pnlWorkshop, show
            "#,
        );

        assert_eq!(
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            Vec::new()
        );
    }

    /// A child-only member read straight off a call returning the parent,
    /// `self:GetOwner():GetAimVector()`, needs a guard as the same read off a
    /// local does; a member no child defines stays undefined.
    #[test]
    fn a_child_read_off_a_call_needs_a_guard_as_off_a_local() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetAimVector fun(self: Player): number
            ---@class NPC: Entity
            ---@field GetAimVector fun(self: NPC): number

            ---@return Entity
            local function owner() end

            local through_local = owner()
            local a = through_local:GetAimVector()
            local b = owner():GetAimVector()
            local c = owner():NoSuchMethod()
            "#,
        );

        let messages = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        let undefined = diagnostics_for(&mut ws, file_id, DiagnosticCode::UndefinedMethod)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();

        let advice = "`GetAimVector` is not defined on `Entity`. Add a guard that narrows the parent to one of `NPC`, `Player`.";
        assert_eq!(messages, vec![advice.to_string(), advice.to_string()]);
        assert_eq!(
            undefined,
            vec!["Undefined method `NoSuchMethod`. ".to_string()]
        );
    }

    #[test]
    fn unguarded_use_fires_without_any_guard() {
        assert_eq!(guard_form_diagnostic_count("ent:Honk()"), 1);
    }

    #[test]
    fn member_presence_if_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count("if ent.Honk then ent:Honk() end"),
            0
        );
    }

    #[test]
    fn member_presence_and_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count("local _ = ent.Honk and ent:Honk()"),
            0
        );
    }

    #[test]
    fn member_presence_early_return_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count(
                "if not ent.Honk then return end
                ent:Honk()"
            ),
            0
        );
    }

    #[test]
    fn isfunction_member_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count("if isfunction(ent.Honk) then ent:Honk() end"),
            0
        );
    }

    #[test]
    fn class_name_comparison_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count(r#"if ent:GetClass() == "Car" then ent:Honk() end"#),
            0
        );
    }

    #[test]
    fn class_name_early_return_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count(
                r#"if ent:GetClass() ~= "Car" then return end
                ent:Honk()"#
            ),
            0
        );
    }

    #[test]
    fn a_field_written_through_a_maybe_null_entity_is_not_declared_on_null() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class NULL: Entity
            ---@class Car: Entity

            ---@return Car|NULL
            local function make() end
            local car = make()
            car.Driver = 1

            ---@param ent Entity
            local function use(ent)
                return ent.Driver
            end
            return use
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    fn entity_table_guard_hint_count(use_site: &str) -> usize {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let source = format!(
            r#"
            ---@class Entity
            Entity = {{}}

            ---@return tableof<self>
            function Entity:GetTable() end

            ---@class Car: Entity
            ---@field Honk fun(self: Car)

            local GetTable = Entity.GetTable

            ---@param ent Entity
            ---@param other Entity
            local function use(ent, other)
                {use_site}
            end
            return use
            "#
        );
        let file_id = ws.def(&source);
        diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
    }

    #[test]
    fn a_member_check_on_the_entity_table_guards_the_entity() {
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = ent:GetTable()
                if tbl and tbl.Honk then ent:Honk() end"
            ),
            0
        );
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = GetTable(ent)
                if not tbl or not tbl.Honk then return end
                ent:Honk()"
            ),
            0
        );
        assert_eq!(
            entity_table_guard_hint_count(
                "ent = other
                local tbl = GetTable(ent)
                if tbl.Honk then ent:Honk() end"
            ),
            0
        );
    }

    /// A field the variable is given through a cast is evidence of what the
    /// variable may be: an unguarded read of it before the cast is reported.
    #[test]
    fn a_field_written_under_a_cast_names_the_child_for_an_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class ReadCar: Entity

            ---@param ent Entity
            local function stamp(ent)
                local before = ent.Boost
                ---@cast ent ReadCar
                ent.Boost = 1
                return before
            end
            return stamp
            "#,
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn a_member_check_on_a_replaced_entity_table_does_not_guard_the_entity() {
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = GetTable(ent)
                ent = other
                if tbl.Honk then ent:Honk() end"
            ),
            1
        );
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = ent:GetTable()
                for _ = 1, 2 do
                    if tbl.Honk then ent:Honk() end
                    ent = other
                end"
            ),
            1
        );
    }

    #[test]
    fn is_valid_guard_alone_does_not_narrow_to_child() {
        assert_eq!(
            guard_form_diagnostic_count("if IsValid(ent) then ent:Honk() end"),
            1
        );
    }

    #[test]
    fn unguarded_child_message_names_the_flow_narrowed_receiver() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class NULL
            ---@class Entity
            ---@class Car: Entity
            ---@field GetFuelCapacity fun(self: Car): number
            ---@param veh Entity|NULL
            local function use(veh)
                if IsValid(veh) then
                    return veh:GetFuelCapacity()
                end
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    #[test]
    fn unguarded_child_message_names_the_receiver_an_early_return_narrowed() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class NULL
            ---@class Entity
            ---@class Car: Entity
            ---@field GetFuelCapacity fun(self: Car): number
            ---@return Entity|NULL
            local function find() end
            local function use()
                local veh = find()
                if not IsValid(veh) then return end
                return veh:GetFuelCapacity()
            end
            return use
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    #[test]
    fn the_runtime_isvalid_in_util_lua_narrows_like_the_annotated_one() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_file(
            "lua/includes/util.lua",
            r#"
            function IsValid(object)
                if ( !object ) then return false end
                local isvalid = object.IsValid
                if ( !isvalid ) then return false end
                return isvalid( object )
            end
            "#,
        );
        let file_id = ws.def_file(
            "lua/autorun/fuel.lua",
            r#"
            ---@class NULL
            ---@class Entity
            ---@class Car: Entity
            ---@field GetFuelCapacity fun(self: Car): number
            ---@return Entity|NULL
            local function find() end
            local function use()
                local veh = find()
                if not IsValid(veh) then return end
                return veh:GetFuelCapacity()
            end
            return use
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    fn closure_receiver_hint_count(write_avatar: bool) -> usize {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let avatar_write = if write_avatar {
            r#"pnl.Avatar = vgui.Create("AvatarImage", pnl)"#
        } else {
            ""
        };
        let file_id = ws.def(&format!(
            r#"
            ---@class Panel
            ---@field Paint fun(self: Panel, w: number, h: number)
            ---@field PerformLayout fun(self: Panel)
            ---@field SetPlayer fun(self: Panel, ply: any, size: number)
            ---@class AvatarImage: Panel
            ---@field SetPlayer fun(self: AvatarImage, ply: any, size: number)
            ---@class DFrame: Panel
            ---@field Avatar AvatarImage

            vgui = vgui or {{}}

            ---@generic T : Panel
            ---@param classname `T`
            ---@param parent? Panel
            ---@return T
            function vgui.Create(classname, parent) end

            ---@param pnl Panel
            local function build(pnl)
                {avatar_write}
                pnl.Paint = function(s, w, h)
                    s.Avatar:SetPlayer(nil, 1)
                end
                pnl.PerformLayout = function(s)
                    s.Avatar:SetPlayer(nil, 2)
                end
                pnl.SetPlayer = function(s, ply, size) s.Avatar:SetPlayer(ply, size) end
            end
            "#
        ));
        diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
    }

    #[test]
    fn closure_assigned_to_member_sees_receiver_member_writes() {
        assert_eq!(closure_receiver_hint_count(true), 0);
    }

    #[test]
    fn closure_assigned_to_member_without_receiver_write_still_hints() {
        assert!(closure_receiver_hint_count(false) > 0);
    }
}
