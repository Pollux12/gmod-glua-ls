#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use crate::{DiagnosticCode, Emmyrc, SemanticInfoOrigin, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaLocalName, LuaNameExpr};
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
            .is_some(),
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
    fn unguarded_child_never_uses_a_grandchild() {
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
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            1
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
}
