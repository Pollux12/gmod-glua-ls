#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaAstToken, LuaLocalName, LuaNameExpr};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, Emmyrc, EmmyrcGmodScriptedClassScopeEntry, GlobalId, GmodClassCallLiteral,
        LuaMemberKey, LuaMemberOwner, LuaType, LuaTypeDeclId, VirtualWorkspace,
    };

    fn legacy_scope(pattern: &str) -> EmmyrcGmodScriptedClassScopeEntry {
        EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(pattern.to_string())
    }

    fn legacy_scopes(patterns: &[&str]) -> Vec<EmmyrcGmodScriptedClassScopeEntry> {
        patterns
            .iter()
            .map(|pattern| legacy_scope(pattern))
            .collect()
    }

    #[gtest]
    fn test_extracts_scripted_class_call_literals() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "lua/entities/entities_test.lua",
            r#"
            DEFINE_BASECLASS("base_anim")
            AccessorFunc(ENT, "m_iHealth", "Health", true)
            ENT:NetworkVar("Float", 0, "MoveSpeed", nil)
        "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
            .cloned()
            .expect("expected scripted class metadata");

        assert_eq!(metadata.define_baseclass_calls.len(), 1);
        assert_eq!(
            metadata.define_baseclass_calls[0].literal_args,
            vec![Some(GmodClassCallLiteral::String("base_anim".to_string()))]
        );

        assert_eq!(metadata.accessor_func_calls.len(), 1);
        assert_eq!(
            metadata.accessor_func_calls[0].literal_args,
            vec![
                Some(GmodClassCallLiteral::NameRef("ENT".to_string())),
                Some(GmodClassCallLiteral::String("m_iHealth".to_string())),
                Some(GmodClassCallLiteral::String("Health".to_string())),
                Some(GmodClassCallLiteral::Boolean(true))
            ]
        );

        assert_eq!(metadata.network_var_calls.len(), 1);
        assert_eq!(
            metadata.network_var_calls[0].literal_args,
            vec![
                Some(GmodClassCallLiteral::String("Float".to_string())),
                Some(GmodClassCallLiteral::Integer(0)),
                Some(GmodClassCallLiteral::String("MoveSpeed".to_string())),
                Some(GmodClassCallLiteral::Nil)
            ]
        );
    }

    #[gtest]
    fn test_scripted_class_index_clears_on_reparse_without_patterns() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_name = "lua/entities/scripted.lua";

        let file_id = ws.def_file(file_name, r#"DEFINE_BASECLASS("base_anim")"#);
        assert!(
            ws.get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&file_id)
                .is_some()
        );

        ws.def_file(file_name, "local value = 1");
        assert!(
            ws.get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&file_id)
                .is_none()
        );
    }

    #[gtest]
    fn test_scripted_class_metadata_disabled_when_gmod_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(r#"DEFINE_BASECLASS("base_anim")"#);
        assert!(
            ws.get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&file_id)
                .is_none()
        );
    }

    #[gtest]
    fn test_scripted_class_scope_filters_metadata_collection() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("lua/entities/**")];
        ws.update_emmyrc(emmyrc);

        let allowed_file_id = ws.def_file(
            "lua/entities/entities_test.lua",
            r#"
            DEFINE_BASECLASS("base_anim")
            AccessorFunc(ENT, "m_iHealth", "Health", true)
        "#,
        );
        let denied_file_id = ws.def_file(
            "lua/autorun/ignored.lua",
            r#"
            DEFINE_BASECLASS("base_anim")
            AccessorFunc(ENT, "m_iHealth", "Health", true)
        "#,
        );

        let allowed_metadata = ws
            .get_db_mut()
            .get_gmod_class_metadata_index()
            .get_file_metadata(&allowed_file_id)
            .cloned()
            .expect("expected scoped metadata for allowed scripted-class file");
        assert_eq!(allowed_metadata.define_baseclass_calls.len(), 1);
        assert_eq!(allowed_metadata.accessor_func_calls.len(), 1);

        let denied_metadata = ws
            .get_db_mut()
            .get_gmod_class_metadata_index()
            .get_file_metadata(&denied_file_id)
            .cloned()
            .expect("expected DEFINE_BASECLASS metadata for out-of-scope file");
        assert_eq!(denied_metadata.define_baseclass_calls.len(), 1);
        // AccessorFunc is always collected regardless of scripted class scope,
        // since it's used by VGUI panels and other non-entity code.
        assert_eq!(denied_metadata.accessor_func_calls.len(), 1);
    }

    #[gtest]
    fn test_scripted_class_scope_matches_nested_entities_folder_anywhere() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/mygamemode/gamemode/entities/entities_test/shared.lua",
            r#"DEFINE_BASECLASS("base_anim")"#,
        );

        assert!(
            ws.get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&file_id)
                .is_some()
        );
    }

    #[gtest]
    fn test_plugin_scope_binds_plugin_decl_to_scoped_class() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("plugins/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "plugins/vehicles/sh_plugin.lua",
            r#"
            local PLUGIN = {}

            function PLUGIN:PlayerSpawn(client)
            end
        "#,
        );

        let plugin_decl_id = {
            let db = ws.get_db_mut();
            let decl_tree = db
                .get_decl_index()
                .get_decl_tree(&file_id)
                .expect("expected decl tree");
            decl_tree
                .get_decls()
                .values()
                .find(|decl| decl.get_name() == "PLUGIN" && decl.is_local())
                .expect("expected local PLUGIN declaration")
                .get_id()
        };

        {
            let db = ws.get_db_mut();
            let type_cache = db
                .get_type_index()
                .get_type_cache(&plugin_decl_id.into())
                .expect("expected PLUGIN declaration type cache");
            assert_eq!(
                type_cache.as_type(),
                &LuaType::Def(LuaTypeDeclId::global("vehicles"))
            );

            let plugin_class_id = LuaTypeDeclId::global("vehicles");
            let plugin_class_decl = db
                .get_type_index()
                .get_type_decl(&plugin_class_id)
                .expect("expected inferred plugin class declaration");
            assert!(plugin_class_decl.is_class());

            let super_types = db
                .get_type_index()
                .get_super_types(&plugin_class_id)
                .expect("expected inferred plugin super types");
            assert!(
                super_types
                    .iter()
                    .any(|ty| ty == &LuaType::Ref(LuaTypeDeclId::global("PLUGIN")))
            );
            assert!(
                super_types
                    .iter()
                    .any(|ty| ty == &LuaType::Ref(LuaTypeDeclId::global("GM")))
            );
        }
    }

    #[gtest]
    fn test_plugin_scope_binds_plugin_decl_with_self_reference_initializer() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("plugins/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "plugins/vehicles/sh_plugin.lua",
            r#"
            local PLUGIN = PLUGIN ---@diagnostic disable-line: undefined-global

            function PLUGIN:PlayerSpawn(client)
            end
        "#,
        );

        let plugin_decl_id = {
            let db = ws.get_db_mut();
            let decl_tree = db
                .get_decl_index()
                .get_decl_tree(&file_id)
                .expect("expected decl tree");
            decl_tree
                .get_decls()
                .values()
                .find(|decl| decl.get_name() == "PLUGIN" && decl.is_local())
                .expect("expected local PLUGIN declaration")
                .get_id()
        };

        let db = ws.get_db_mut();
        let type_cache = db
            .get_type_index()
            .get_type_cache(&plugin_decl_id.into())
            .expect("expected PLUGIN declaration type cache");
        assert_eq!(
            type_cache.as_type(),
            &LuaType::Def(LuaTypeDeclId::global("vehicles"))
        );

        let local_name = ws.get_node::<LuaLocalName>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let token = local_name
            .get_name_token()
            .expect("expected local PLUGIN name token");
        let semantic_info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info for local PLUGIN name");
        assert_eq!(
            semantic_info.typ,
            LuaType::Def(LuaTypeDeclId::global("vehicles"))
        );
    }

    #[gtest]
    fn test_plugin_scope_binding_respects_scope_filters() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "plugins/vehicles/sh_plugin.lua",
            r#"
            local PLUGIN = {}
        "#,
        );

        let plugin_decl_id = {
            let db = ws.get_db_mut();
            let decl_tree = db
                .get_decl_index()
                .get_decl_tree(&file_id)
                .expect("expected decl tree");
            decl_tree
                .get_decls()
                .values()
                .find(|decl| decl.get_name() == "PLUGIN" && decl.is_local())
                .expect("expected local PLUGIN declaration")
                .get_id()
        };

        let db = ws.get_db_mut();
        let type_cache = db
            .get_type_index()
            .get_type_cache(&plugin_decl_id.into())
            .expect("expected PLUGIN declaration type cache");
        assert_ne!(
            type_cache.as_type(),
            &LuaType::Def(LuaTypeDeclId::global("vehicles"))
        );
    }

    #[gtest]
    fn test_entity_scope_infers_ent_reference_to_scoped_class_without_local_decl() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "plugins/vehicles/entities/entities/vehicles_money/sh_init.lua",
            r#"
            ENT.Type = "anim"
            ENT.Base = "base_gmodentity"
        "#,
        );

        let name_expr = ws.get_node::<LuaNameExpr>(file_id);
        let token = name_expr.get_name_token().expect("expected ENT name token");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let semantic_info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info for ENT");

        assert_eq!(
            semantic_info.typ,
            LuaType::Def(LuaTypeDeclId::global("vehicles_money"))
        );
    }

    #[gtest]
    fn test_entity_scope_ent_decl_is_local_and_not_indexed_globally() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "entities/entities/cityrp_money/sh_init.lua",
            r#"
            ENT = ENT or {}
            ENT.Type = "anim"
            ENT.Base = "base_gmodentity"
        "#,
        );

        let db = ws.get_db_mut();
        let (ent_decl_id, ent_decl_is_local) = {
            let decl_tree = db
                .get_decl_index()
                .get_decl_tree(&file_id)
                .expect("expected decl tree");
            let ent_decl = decl_tree
                .get_decls()
                .values()
                .find(|decl| decl.get_name() == "ENT")
                .expect("expected ENT declaration");
            (ent_decl.get_id(), ent_decl.is_local())
        };

        assert!(ent_decl_is_local, "expected ENT declaration to be local");

        let has_ent_global_decl = db
            .get_global_index()
            .get_global_decl_ids("ENT")
            .is_some_and(|decl_ids| decl_ids.contains(&ent_decl_id));
        assert!(
            !has_ent_global_decl,
            "expected ENT declaration to be excluded from global index"
        );

        let has_ent_global_refs_in_file = db
            .get_reference_index()
            .get_global_file_references("ENT", file_id)
            .is_some_and(|refs| !refs.is_empty());
        assert!(
            !has_ent_global_refs_in_file,
            "expected no global ENT references for scripted-scope file"
        );
    }

    #[gtest]
    fn test_entity_scope_creates_class_decl_without_ent_base_assignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "entities/entities/cityrp_inventory/init.lua",
            r#"
            function ENT:Initialize()
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("cityrp_inventory");
        let class_decl = db.get_type_index().get_type_decl(&class_id);
        assert!(class_decl.is_some(), "expected inferred class declaration");

        let super_types: Vec<_> = db
            .get_type_index()
            .get_super_types_iter(&class_id)
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("ENT"))),
            "expected ENT as super type, got {super_types:?}"
        );

        let name_expr = ws.get_node::<LuaNameExpr>(file_id);
        let token = name_expr.get_name_token().expect("expected ENT name token");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let semantic_info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info for ENT");

        assert_eq!(
            semantic_info.typ,
            LuaType::Def(LuaTypeDeclId::global("cityrp_inventory"))
        );
    }

    #[gtest]
    fn test_entity_method_self_infers_scoped_class_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "entities/entities/cityrp_inventory/init.lua",
            r#"
            function ENT:Initialize()
                local self_copy = self
            end
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let self_name_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|name_expr| {
                name_expr
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "self")
            })
            .expect("expected self name expr in method body");
        let self_token = self_name_expr
            .get_name_token()
            .expect("expected self token");
        let semantic_info = semantic_model
            .get_semantic_info(self_token.syntax().clone().into())
            .expect("expected semantic info for self");

        assert_eq!(
            semantic_info.typ,
            LuaType::Def(LuaTypeDeclId::global("cityrp_inventory"))
        );
    }

    #[gtest]
    fn test_accessor_func_synthesizes_get_set_members() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/my_entity/init.lua",
            r#"
            AccessorFunc(ENT, "m_iHealth", "Health", FORCE_NUMBER)
            AccessorFunc(ENT, "m_sName", "Name", FORCE_STRING)
            AccessorFunc(ENT, "m_bActive", "Active", true)
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("my_entity");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on my_entity class");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        // Should have Get/Set for Health, Name, Active + backing fields
        assert!(
            member_names.contains(&"GetHealth".to_string()),
            "missing GetHealth in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetHealth".to_string()),
            "missing SetHealth in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetName".to_string()),
            "missing GetName in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetName".to_string()),
            "missing SetName in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetActive".to_string()),
            "missing GetActive in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetActive".to_string()),
            "missing SetActive in {member_names:?}"
        );
        assert!(
            member_names.contains(&"m_iHealth".to_string()),
            "missing m_iHealth backing field in {member_names:?}"
        );
    }

    #[gtest]
    fn test_accessor_func_force_type_resolves_correctly() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/typed_ent/init.lua",
            r#"
            AccessorFunc(ENT, "m_vPos", "Position", FORCE_VECTOR)
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("typed_ent");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on typed_ent class");

        let getter = members
            .iter()
            .find(|m| m.get_key().get_name() == Some("GetPosition"))
            .expect("expected GetPosition member");

        let getter_type = db.get_type_index().get_type_cache(&getter.get_id().into());
        assert!(
            getter_type.is_some(),
            "GetPosition should have a type bound"
        );
    }

    #[gtest]
    fn test_network_var_synthesizes_get_set_members() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/nw_entity/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:NetworkVar("Float", 0, "Speed")
                self:NetworkVar("String", 0, "Label")
                self:NetworkVar("Entity", 0, "Owner")
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("nw_entity");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on nw_entity class");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            member_names.contains(&"GetSpeed".to_string()),
            "missing GetSpeed in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetSpeed".to_string()),
            "missing SetSpeed in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetLabel".to_string()),
            "missing GetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetLabel".to_string()),
            "missing SetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetOwner".to_string()),
            "missing GetOwner in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetOwner".to_string()),
            "missing SetOwner in {member_names:?}"
        );
    }

    #[gtest]
    fn test_define_baseclass_sets_super_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/derived_ent/init.lua",
            r#"
            DEFINE_BASECLASS("base_anim")
            ENT.Type = "anim"
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("derived_ent");
        let super_types: Vec<_> = db
            .get_type_index()
            .get_super_types_iter(&class_id)
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();

        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("base_anim"))),
            "expected base_anim super type, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_define_baseclass_infers_baseclass_local_and_no_undefined_global() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedGlobal);

        let file_id = ws.def_file(
            "lua/entities/fl_dodge_charger/shared.lua",
            r#"
            DEFINE_BASECLASS("base_glide")
            local _ = BaseClass
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let baseclass_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|name_expr| name_expr.get_name_text().as_deref() == Some("BaseClass"))
            .expect("expected BaseClass name expression");
        let baseclass_token = baseclass_expr
            .get_name_token()
            .expect("expected BaseClass token");
        let semantic_info = semantic_model
            .get_semantic_info(baseclass_token.syntax().clone().into())
            .expect("expected semantic info for BaseClass");

        assert_eq!(
            semantic_info.typ,
            LuaType::Ref(LuaTypeDeclId::global("base_glide")),
            "expected BaseClass to resolve to base_glide"
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_global_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobal.get_name().to_string(),
        ));

        assert!(
            diagnostics.iter().all(|diag| {
                diag.code != undefined_global_code || !diag.message.contains("BaseClass")
            }),
            "unexpected undefined-global diagnostic for BaseClass: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_define_baseclass_infers_baseclass_outside_scripted_scope_and_no_undefined_global() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedGlobal);

        let file_id = ws.def_file(
            "lua/vgui/test_panel.lua",
            r#"
            DEFINE_BASECLASS("base_panel")
            local _ = BaseClass
        "#,
        );

        {
            let metadata = ws
                .get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&file_id)
                .cloned()
                .expect("expected scripted class metadata for out-of-scope DEFINE_BASECLASS");
            assert_eq!(metadata.define_baseclass_calls.len(), 1);
            assert_eq!(metadata.accessor_func_calls.len(), 0);
        }

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let baseclass_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|name_expr| name_expr.get_name_text().as_deref() == Some("BaseClass"))
            .expect("expected BaseClass name expression");
        let baseclass_token = baseclass_expr
            .get_name_token()
            .expect("expected BaseClass token");
        let semantic_info = semantic_model
            .get_semantic_info(baseclass_token.syntax().clone().into())
            .expect("expected semantic info for BaseClass");

        assert_eq!(
            semantic_info.typ,
            LuaType::Ref(LuaTypeDeclId::global("base_panel")),
            "expected BaseClass to resolve to base_panel"
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_global_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobal.get_name().to_string(),
        ));

        assert!(
            diagnostics.iter().all(|diag| {
                diag.code != undefined_global_code || !diag.message.contains("BaseClass")
            }),
            "unexpected undefined-global diagnostic for BaseClass: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_ent_base_from_shared_file_sets_folder_class_super_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "entities/entities/cityrp_money/sh_init.lua",
            r#"
            ENT.Base = "cityrp_base"
        "#,
        );

        let file_id = ws.def_file(
            "entities/entities/cityrp_money/init.lua",
            r#"
            function ENT:Initialize()
            end
        "#,
        );

        {
            let db = ws.get_db_mut();
            let class_id = LuaTypeDeclId::global("cityrp_money");
            let super_types: Vec<_> = db
                .get_type_index()
                .get_super_types_iter(&class_id)
                .map(|iter| iter.cloned().collect())
                .unwrap_or_default();

            assert!(
                super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("cityrp_base"))),
                "expected cityrp_base super type, got {super_types:?}"
            );
        }

        let name_expr = ws.get_node::<LuaNameExpr>(file_id);
        let token = name_expr.get_name_token().expect("expected ENT name token");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let semantic_info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info for ENT");

        assert_eq!(
            semantic_info.typ,
            LuaType::Def(LuaTypeDeclId::global("cityrp_money"))
        );
    }

    #[gtest]
    fn test_ent_base_known_gmod_base_maps_to_ent_super_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/mapped_ent/shared.lua",
            r#"
            ENT.Base = "base_gmodentity"
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("mapped_ent");
        let super_types: Vec<_> = db
            .get_type_index()
            .get_super_types_iter(&class_id)
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();

        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("ENT"))),
            "expected ENT super type, got {super_types:?}"
        );
        assert!(
            !super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("base_gmodentity"))),
            "base_gmodentity should map to ENT super type"
        );
    }

    #[gtest]
    fn test_doc_param_resolves_scoped_entity_type_without_type_not_found() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::TypeNotFound);

        ws.def_files(vec![
            (
                "lua/entities/base_glide_car/shared.lua",
                r#"
                function ENT:Initialize()
                end
            "#,
            ),
            (
                "lua/autorun/use_base.lua",
                r#"
                ---@param ent base_glide_car
                local function consume(ent)
                end
            "#,
            ),
        ]);

        let param_uri = ws.virtual_url_generator.new_uri("lua/autorun/use_base.lua");
        let param_file_id = ws
            .analysis
            .get_file_id(&param_uri)
            .expect("expected test file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(param_file_id, CancellationToken::new())
            .unwrap_or_default();

        let type_not_found_code = Some(NumberOrString::String(
            DiagnosticCode::TypeNotFound.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != type_not_found_code),
            "unexpected type-not-found diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_creates_panel_class() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/my_panel.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
            end

            function PANEL:Paint(w, h)
            end

            vgui.Register("MyPanel", PANEL, "DPanel")
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("MyPanel");

        // Class should exist
        let decl = db.get_type_index().get_type_decl(&class_id);
        assert!(decl.is_some(), "MyPanel class should be created");

        // Should have DPanel as super type
        let super_types: Vec<_> = db
            .get_type_index()
            .get_super_types_iter(&class_id)
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();

        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("DPanel"))),
            "expected DPanel super type, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_multiple_panels_bind_nearest_panel_decl() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/multi_panel.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
            end

            vgui.Register("MyPanelOne", PANEL, "DFrame")

            local PANEL = {}

            function PANEL:Paint(w, h)
            end

            vgui.Register("MyPanelTwo", PANEL, "EditablePanel")
        "#,
        );

        let db = ws.get_db_mut();

        let first_class_id = LuaTypeDeclId::global("MyPanelOne");
        let second_class_id = LuaTypeDeclId::global("MyPanelTwo");

        assert!(
            db.get_type_index().get_type_decl(&first_class_id).is_some(),
            "MyPanelOne class should be created"
        );
        assert!(
            db.get_type_index()
                .get_type_decl(&second_class_id)
                .is_some(),
            "MyPanelTwo class should be created"
        );

        let first_supers: Vec<_> = db
            .get_type_index()
            .get_super_types_iter(&first_class_id)
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            first_supers.contains(&LuaType::Ref(LuaTypeDeclId::global("DFrame"))),
            "expected DFrame super type for MyPanelOne, got {first_supers:?}"
        );

        let second_supers: Vec<_> = db
            .get_type_index()
            .get_super_types_iter(&second_class_id)
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            second_supers.contains(&LuaType::Ref(LuaTypeDeclId::global("EditablePanel"))),
            "expected EditablePanel super type for MyPanelTwo, got {second_supers:?}"
        );

        let first_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(first_class_id.clone()))
            .expect("expected members on MyPanelOne");
        let first_member_names: Vec<_> = first_members
            .iter()
            .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
            .collect();

        assert!(
            first_member_names.contains(&"Init".to_string()),
            "expected Init on MyPanelOne, got {first_member_names:?}"
        );
        assert!(
            !first_member_names.contains(&"Paint".to_string()),
            "MyPanelOne should not inherit PANEL:Paint from second panel, got {first_member_names:?}"
        );

        let second_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(second_class_id.clone()))
            .expect("expected members on MyPanelTwo");
        let second_member_names: Vec<_> = second_members
            .iter()
            .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
            .collect();

        assert!(
            second_member_names.contains(&"Paint".to_string()),
            "expected Paint on MyPanelTwo, got {second_member_names:?}"
        );
        assert!(
            !second_member_names.contains(&"Init".to_string()),
            "MyPanelTwo should not inherit PANEL:Init from first panel, got {second_member_names:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_reassigned_local_panel_transfers_members_per_register() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/reassigned_panel.lua",
            r#"
            local PANEL = {}
            function PANEL:Init() end
            vgui.Register("PanelA", PANEL, "DFrame")

            PANEL = {}
            function PANEL:Paint(w, h) end
            vgui.Register("PanelB", PANEL, "EditablePanel")
        "#,
        );

        let db = ws.get_db_mut();
        let panel_a_id = LuaTypeDeclId::global("PanelA");
        let panel_b_id = LuaTypeDeclId::global("PanelB");

        let panel_a_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(panel_a_id.clone()))
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert!(
            panel_a_members.contains(&"Init".to_string()),
            "expected Init on PanelA, got {panel_a_members:?}"
        );
        assert!(
            !panel_a_members.contains(&"Paint".to_string()),
            "PanelA should not inherit Paint from reassigned PANEL, got {panel_a_members:?}"
        );

        let panel_b_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(panel_b_id.clone()))
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert!(
            panel_b_members.contains(&"Paint".to_string()),
            "expected Paint on PanelB, got {panel_b_members:?}"
        );
        assert!(
            !panel_b_members.contains(&"Init".to_string()),
            "PanelB should not inherit Init from first PANEL table, got {panel_b_members:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_reassigned_local_panel_stress_three_panels() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/reassigned_panel_stress.lua",
            r#"
            local PANEL = {}
            function PANEL:Alpha() end
            vgui.Register("PA", PANEL, "Panel")

            PANEL = {}
            function PANEL:Beta() end
            vgui.Register("PB", PANEL, "DFrame")

            PANEL = {}
            function PANEL:Gamma() end
            vgui.Register("PC", PANEL, "EditablePanel")
        "#,
        );

        let db = ws.get_db_mut();
        let pa_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(LuaTypeDeclId::global("PA")))
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let pb_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(LuaTypeDeclId::global("PB")))
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let pc_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(LuaTypeDeclId::global("PC")))
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert!(
            pa_members.contains(&"Alpha".to_string()),
            "expected Alpha on PA, got {pa_members:?}"
        );
        assert!(
            !pa_members.contains(&"Beta".to_string()) && !pa_members.contains(&"Gamma".to_string()),
            "PA should only contain Alpha, got {pa_members:?}"
        );

        assert!(
            pb_members.contains(&"Beta".to_string()),
            "expected Beta on PB, got {pb_members:?}"
        );
        assert!(
            !pb_members.contains(&"Alpha".to_string())
                && !pb_members.contains(&"Gamma".to_string()),
            "PB should only contain Beta, got {pb_members:?}"
        );

        assert!(
            pc_members.contains(&"Gamma".to_string()),
            "expected Gamma on PC, got {pc_members:?}"
        );
        assert!(
            !pc_members.contains(&"Alpha".to_string()) && !pc_members.contains(&"Beta".to_string()),
            "PC should only contain Gamma, got {pc_members:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_panel_decl_inside_closed_do_block_does_not_leak_to_outer_register() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/panel_do_scope.lua",
            r#"
            do
                local PANEL = {}
                function PANEL:Init() end
                vgui.Register("InnerPanel", PANEL, "DFrame")
            end

            local PANEL = {}
            function PANEL:Paint(w, h) end
            vgui.Register("OuterPanel", PANEL, "EditablePanel")
        "#,
        );

        let db = ws.get_db_mut();
        let inner_class_id = LuaTypeDeclId::global("InnerPanel");
        let outer_class_id = LuaTypeDeclId::global("OuterPanel");

        assert!(
            db.get_type_index().get_type_decl(&inner_class_id).is_some(),
            "InnerPanel class should be created"
        );
        assert!(
            db.get_type_index().get_type_decl(&outer_class_id).is_some(),
            "OuterPanel class should be created"
        );

        let inner_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(inner_class_id.clone()))
            .expect("expected members on InnerPanel");
        let inner_member_names: Vec<_> = inner_members
            .iter()
            .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
            .collect();

        assert!(
            inner_member_names.contains(&"Init".to_string()),
            "expected Init on InnerPanel, got {inner_member_names:?}"
        );
        assert!(
            !inner_member_names.contains(&"Paint".to_string()),
            "InnerPanel should not inherit outer PANEL:Paint, got {inner_member_names:?}"
        );

        let outer_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(outer_class_id.clone()))
            .expect("expected members on OuterPanel");
        let outer_member_names: Vec<_> = outer_members
            .iter()
            .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
            .collect();

        assert!(
            outer_member_names.contains(&"Paint".to_string()),
            "expected Paint on OuterPanel, got {outer_member_names:?}"
        );
        assert!(
            !outer_member_names.contains(&"Init".to_string()),
            "OuterPanel should not inherit inner PANEL:Init, got {outer_member_names:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_outside_closed_do_block_ignores_inner_panel_decl() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/panel_orphan_scope.lua",
            r#"
            do
                local PANEL = {}
                function PANEL:Init() end
            end

            vgui.Register("OrphanPanel", PANEL, "DFrame")
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("OrphanPanel");

        assert!(
            db.get_type_index().get_type_decl(&class_id).is_some(),
            "OrphanPanel class should be created"
        );

        let member_names: Vec<_> = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(class_id))
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert!(
            !member_names.contains(&"Init".to_string()),
            "OrphanPanel should not inherit members from out-of-scope PANEL, got {member_names:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_panel_local_name_resolves_to_correct_panel_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "lua/vgui/panel_hover_type.lua",
            r#"
            local PANEL = {}
            vgui.Register("HoverPanelOne", PANEL, "DPanel")

            local PANEL = {}
            vgui.Register("HoverPanelTwo", PANEL, "EditablePanel")
        "#,
        );

        let first_class_id = LuaTypeDeclId::global("HoverPanelOne");
        let second_class_id = LuaTypeDeclId::global("HoverPanelTwo");

        {
            let db = ws.get_db_mut();
            let decl_tree = db
                .get_decl_index()
                .get_decl_tree(&file_id)
                .expect("expected decl tree");
            let mut panel_decl_ids = decl_tree
                .get_decls()
                .values()
                .filter(|decl| decl.get_name() == "PANEL" && decl.is_local())
                .map(|decl| (decl.get_position(), decl.get_id()))
                .collect::<Vec<_>>();
            panel_decl_ids.sort_by_key(|(position, _)| *position);

            assert_eq!(
                panel_decl_ids.len(),
                2,
                "expected two local PANEL declarations, got {panel_decl_ids:?}"
            );

            let first_panel_type = db
                .get_type_index()
                .get_type_cache(&panel_decl_ids[0].1.into())
                .expect("expected first PANEL type cache")
                .as_type()
                .clone();
            assert_eq!(first_panel_type, LuaType::Def(first_class_id.clone()));

            let second_panel_type = db
                .get_type_index()
                .get_type_cache(&panel_decl_ids[1].1.into())
                .expect("expected second PANEL type cache")
                .as_type()
                .clone();
            assert_eq!(second_panel_type, LuaType::Def(second_class_id.clone()));
        }

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let mut panel_local_types = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .filter_map(|local_name| {
                let token = local_name.get_name_token()?;
                if token.get_name_text() != "PANEL" {
                    return None;
                }

                let semantic_info =
                    semantic_model.get_semantic_info(token.syntax().clone().into())?;
                Some((token.get_position(), semantic_info.typ))
            })
            .collect::<Vec<_>>();
        panel_local_types.sort_by_key(|(position, _)| *position);

        assert_eq!(
            panel_local_types.len(),
            2,
            "expected semantic info for two local PANEL names, got {panel_local_types:?}"
        );
        assert_eq!(panel_local_types[0].1, LuaType::Def(first_class_id));
        assert_eq!(panel_local_types[1].1, LuaType::Def(second_class_id));
    }

    #[gtest]
    fn test_vgui_panel_self_dynamic_field_no_undefined_field_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "lua/vgui/self_field_panel.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                self.buttons = {}
                local _ = self.buttons
            end

            vgui.Register("SelfFieldPanel", PANEL, "EditablePanel")
        "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_vgui_panel_dynamic_field_method_call_no_undefined_field_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def(
            r#"
            ---@class Panel
            local Panel = {}

            ---@param name string
            ---@return Panel
            function Panel:Add(name)
            end

            ---@param dir number
            function Panel:Dock(dir)
            end

            ---@class EditablePanel : Panel
            "#,
        );

        let file_id = ws.def_file(
            "lua/vgui/test_dynamic_field_panel.lua",
            r#"
            local TOP = 1
            local PANEL = {}

            function PANEL:Init()
                self.buttons = self:Add("Panel")
                self.buttons:Dock(TOP)
            end

            vgui.Register("TestDynamicFieldPanel", PANEL, "EditablePanel")
        "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_vgui_panel_cross_method_dynamic_field_no_undefined_field_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def(
            r#"
            ---@class EditablePanel
            "#,
        );

        let file_id = ws.def_file(
            "lua/vgui/cross_method_panel.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                self.header = {}
            end

            function PANEL:PerformLayout()
                local _ = self.header
            end

            vgui.Register("CrossMethodPanel", PANEL, "EditablePanel")
        "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_vgui_register_not_captured_when_gmod_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/disabled_panel.lua",
            r#"
            local PANEL = {}
            vgui.Register("DisabledPanel", PANEL, "DPanel")
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("DisabledPanel");
        let decl = db.get_type_index().get_type_decl(&class_id);
        assert!(
            decl.is_none(),
            "Panel class should not be created when gmod disabled"
        );
    }

    #[gtest]
    fn test_network_var_two_arg_form_synthesizes_get_set() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/two_arg_nw/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:NetworkVar("String", "Label")
                self:NetworkVar("Bool", "Active")
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("two_arg_nw");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on two_arg_nw class");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            member_names.contains(&"GetLabel".to_string()),
            "missing GetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetLabel".to_string()),
            "missing SetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetActive".to_string()),
            "missing GetActive in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetActive".to_string()),
            "missing SetActive in {member_names:?}"
        );
    }

    #[gtest]
    fn test_network_var_element_collected_and_synthesized() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "lua/entities/nve_entity/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:NetworkVarElement("Float", 0, "x", "VehiclePos")
                self:NetworkVarElement("Float", 1, "y", "VehicleAng")
            end
        "#,
        );

        // Verify collection
        let metadata = ws
            .get_db_mut()
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
            .cloned()
            .expect("expected scripted class metadata");
        assert_eq!(
            metadata.network_var_element_calls.len(),
            2,
            "expected 2 NetworkVarElement calls, got {}",
            metadata.network_var_element_calls.len()
        );

        // Verify synthesis
        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("nve_entity");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on nve_entity class");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            member_names.contains(&"GetVehiclePos".to_string()),
            "missing GetVehiclePos in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetVehiclePos".to_string()),
            "missing SetVehiclePos in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetVehicleAng".to_string()),
            "missing GetVehicleAng in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetVehicleAng".to_string()),
            "missing SetVehicleAng in {member_names:?}"
        );
    }

    #[gtest]
    fn test_network_var_element_three_arg_form() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/nve_short/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:NetworkVarElement("Float", 0, "Offset")
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("nve_short");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on nve_short class");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            member_names.contains(&"GetOffset".to_string()),
            "missing GetOffset in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetOffset".to_string()),
            "missing SetOffset in {member_names:?}"
        );
    }

    #[gtest]
    fn test_inherited_network_vars_accessible_on_derived_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_files(vec![
            (
                "lua/entities/base_glide_car/shared.lua",
                r#"
                function ENT:SetupDataTables()
                    self:NetworkVar("Vector", 0, "HeadlightColor")
                    self:NetworkVar("Float", 0, "SteerConeChangeRate")
                end
            "#,
            ),
            (
                "lua/entities/fl_dodge_charger/shared.lua",
                r#"
                ENT.Base = "base_glide_car"
            "#,
            ),
        ]);

        // Verify base entity has the NetworkVar members
        {
            let db = ws.get_db_mut();
            let base_class_id = LuaTypeDeclId::global("base_glide_car");
            let base_owner = LuaMemberOwner::Type(base_class_id);
            let base_members = db
                .get_member_index()
                .get_members(&base_owner)
                .expect("expected members on base_glide_car");
            let base_member_names: Vec<_> = base_members
                .iter()
                .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                .collect();
            assert!(
                base_member_names.contains(&"GetHeadlightColor".to_string()),
                "missing GetHeadlightColor on base: {base_member_names:?}"
            );
            assert!(
                base_member_names.contains(&"SetSteerConeChangeRate".to_string()),
                "missing SetSteerConeChangeRate on base: {base_member_names:?}"
            );
        }

        // Verify derived entity inherits from base
        {
            let db = ws.get_db_mut();
            let derived_class_id = LuaTypeDeclId::global("fl_dodge_charger");
            let super_types: Vec<_> = db
                .get_type_index()
                .get_super_types_iter(&derived_class_id)
                .map(|iter| iter.cloned().collect())
                .unwrap_or_default();
            assert!(
                super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("base_glide_car"))),
                "expected base_glide_car super type on derived, \
                 got {super_types:?}"
            );
        }
    }

    #[gtest]
    fn test_network_var_mixed_forms_in_same_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/mixed_nw/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:NetworkVar("Float", 0, "Speed")
                self:NetworkVar("String", "Label")
                self:NetworkVarElement("Float", 0, "x", "Position")
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("mixed_nw");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members on mixed_nw class");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        // 3-arg NetworkVar
        assert!(
            member_names.contains(&"GetSpeed".to_string()),
            "missing GetSpeed in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetSpeed".to_string()),
            "missing SetSpeed in {member_names:?}"
        );
        // 2-arg NetworkVar
        assert!(
            member_names.contains(&"GetLabel".to_string()),
            "missing GetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetLabel".to_string()),
            "missing SetLabel in {member_names:?}"
        );
        // NetworkVarElement
        assert!(
            member_names.contains(&"GetPosition".to_string()),
            "missing GetPosition in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetPosition".to_string()),
            "missing SetPosition in {member_names:?}"
        );
    }

    #[gtest]
    fn test_network_var_element_metadata_collection() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "lua/entities/nve_meta_test.lua",
            r#"
            self:NetworkVarElement("Float", 0, "x", "Position")
        "#,
        );

        let metadata = ws
            .get_db_mut()
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
            .cloned()
            .expect("expected metadata for NetworkVarElement");

        assert_eq!(metadata.network_var_element_calls.len(), 1);
        assert_eq!(
            metadata.network_var_element_calls[0].literal_args,
            vec![
                Some(GmodClassCallLiteral::String("Float".to_string())),
                Some(GmodClassCallLiteral::Integer(0)),
                Some(GmodClassCallLiteral::String("x".to_string())),
                Some(GmodClassCallLiteral::String("Position".to_string())),
            ]
        );
    }

    #[gtest]
    fn test_network_var_wrapper_function_synthesizes_get_set() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/wrapper_ent/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:RegisterNW("String", "Label")
                self:RegisterNW("Float", "Health")
            end

            function ENT:RegisterNW(type, name)
                self:NetworkVar(type, 0, name)
            end
        "#,
        );

        let class_id = LuaTypeDeclId::global("wrapper_ent");
        let owner = LuaMemberOwner::Type(class_id);
        let member_names: Vec<String> = ws
            .get_db_mut()
            .get_member_index()
            .get_members(&owner)
            .map(|members| {
                members
                    .iter()
                    .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        assert!(
            member_names.contains(&"GetLabel".to_string()),
            "missing GetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetLabel".to_string()),
            "missing SetLabel in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetHealth".to_string()),
            "missing GetHealth in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetHealth".to_string()),
            "missing SetHealth in {member_names:?}"
        );
    }

    #[gtest]
    fn test_network_var_wrapper_with_fixed_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/wrapper_fixed/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:AddBoolNW("Active")
                self:AddBoolNW("Visible")
            end

            function ENT:AddBoolNW(name)
                self:NetworkVar("Bool", 0, name)
            end
        "#,
        );

        let class_id = LuaTypeDeclId::global("wrapper_fixed");
        let owner = LuaMemberOwner::Type(class_id);
        let member_names: Vec<String> = ws
            .get_db_mut()
            .get_member_index()
            .get_members(&owner)
            .map(|members| {
                members
                    .iter()
                    .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        assert!(
            member_names.contains(&"GetActive".to_string()),
            "missing GetActive in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetActive".to_string()),
            "missing SetActive in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetVisible".to_string()),
            "missing GetVisible in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetVisible".to_string()),
            "missing SetVisible in {member_names:?}"
        );
    }

    #[gtest]
    fn test_network_var_element_wrapper_synthesizes_number_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/elem_wrap/init.lua",
            r#"
            function ENT:SetupDataTables()
                self:AddPosElement("PosX")
            end

            function ENT:AddPosElement(name)
                self:NetworkVarElement("Float", 0, "x", name)
            end
        "#,
        );

        let class_id = LuaTypeDeclId::global("elem_wrap");
        let owner = LuaMemberOwner::Type(class_id);
        let member_names: Vec<String> = ws
            .get_db_mut()
            .get_member_index()
            .get_members(&owner)
            .map(|members| {
                members
                    .iter()
                    .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        assert!(
            member_names.contains(&"GetPosX".to_string()),
            "missing GetPosX in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetPosX".to_string()),
            "missing SetPosX in {member_names:?}"
        );
    }

    #[gtest]
    fn test_network_var_no_undefined_field_across_entity_files() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_files(vec![
            (
                "lua/entities/base_glide_car/shared.lua",
                r#"
                function ENT:SetupDataTables()
                    self:NetworkVar("Bool", "IsRedlining")
                    self:NetworkVar("Float", 0, "Speed")
                end
            "#,
            ),
            (
                "lua/entities/base_glide_car/cl_init.lua",
                r#"
                function ENT:Think()
                    local x = self:GetIsRedlining()
                    local y = self:GetSpeed()
                end
            "#,
            ),
        ]);

        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/base_glide_car/cl_init.lua");
        let target_file_id = ws
            .analysis
            .get_file_id(&target_uri)
            .expect("expected file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(target_file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_self_resolves_to_scoped_class_in_secondary_entity_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_files(vec![
            (
                "lua/entities/multi_file_ent/shared.lua",
                r#"
                ENT.Type = "anim"
            "#,
            ),
            (
                "lua/entities/multi_file_ent/cl_init.lua",
                r#"
                function ENT:Draw()
                    local self_copy = self
                end
            "#,
            ),
        ]);

        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/multi_file_ent/cl_init.lua");
        let file_id = ws
            .analysis
            .get_file_id(&target_uri)
            .expect("expected file id");

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let self_name_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|name_expr| {
                name_expr
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "self")
            })
            .expect("expected self name expr");
        let self_token = self_name_expr
            .get_name_token()
            .expect("expected self token");
        let semantic_info = semantic_model
            .get_semantic_info(self_token.syntax().clone().into())
            .expect("expected semantic info for self");

        assert_eq!(
            semantic_info.typ,
            LuaType::Def(LuaTypeDeclId::global("multi_file_ent"))
        );
    }

    #[gtest]
    fn test_inherited_network_var_no_undefined_field_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_files(vec![
            (
                "lua/entities/base_vehicle/shared.lua",
                r#"
                function ENT:SetupDataTables()
                    self:NetworkVar("Vector", 0, "HeadlightColor")
                    self:NetworkVar("Float", 0, "SteerRate")
                end
            "#,
            ),
            (
                "lua/entities/derived_vehicle/shared.lua",
                r#"
                ENT.Base = "base_vehicle"
            "#,
            ),
            (
                "lua/entities/derived_vehicle/cl_init.lua",
                r#"
                function ENT:Think()
                    self:SetHeadlightColor(Vector(0, 0, 0))
                    local rate = self:GetSteerRate()
                end
            "#,
            ),
        ]);

        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/derived_vehicle/cl_init.lua");
        let target_file_id = ws
            .analysis
            .get_file_id(&target_uri)
            .expect("expected file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(target_file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_local_function_wrapper_inside_setup_data_tables() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/local_wrap_ent/init.lua",
            r#"
            function ENT:SetupDataTables()
                local function AddFloatVar(key, min, max)
                    self:NetworkVar("Float", key)
                end

                AddFloatVar("Speed", 0, 100)
                AddFloatVar("Health", 0, 500)
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("local_wrap_ent");
        let owner = LuaMemberOwner::Type(class_id);
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("expected members");
        let member_names: Vec<_> = members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            member_names.contains(&"GetSpeed".to_string()),
            "missing GetSpeed in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetSpeed".to_string()),
            "missing SetSpeed in {member_names:?}"
        );
        assert!(
            member_names.contains(&"GetHealth".to_string()),
            "missing GetHealth in {member_names:?}"
        );
        assert!(
            member_names.contains(&"SetHealth".to_string()),
            "missing SetHealth in {member_names:?}"
        );
    }

    #[gtest]
    fn test_two_arg_network_var_no_undefined_field_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_files(vec![
            (
                "lua/entities/two_arg_diag/shared.lua",
                r#"
                function ENT:SetupDataTables()
                    self:NetworkVar("Bool", "IsRedlining")
                    self:NetworkVar("String", "Label")
                end
            "#,
            ),
            (
                "lua/entities/two_arg_diag/cl_init.lua",
                r#"
                function ENT:Think()
                    local x = self:GetIsRedlining()
                    local y = self:GetLabel()
                end
            "#,
            ),
        ]);

        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/two_arg_diag/cl_init.lua");
        let target_file_id = ws
            .analysis
            .get_file_id(&target_uri)
            .expect("expected file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(target_file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_realistic_mixed_networkvar_and_wrapper_calls_in_same_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_files(vec![
            (
                "lua/entities/base_glide_car_realistic/shared.lua",
                r#"
                function ENT:SetupDataTables()
                    self:NetworkVar("Bool", "IsRedlining")
                    self:NetworkVar("Int", "Gear")
                    self:NetworkVar("Float", "Steering")

                    self:NetworkVar("Vector", "TireSmokeColor",
                        { KeyName = "TireSmokeColor" })
                    self:NetworkVar("Vector", "HeadlightColor",
                        { KeyName = "HeadlightColor" })

                    local order = 0
                    local function AddFloatVar(key, min, max, category)
                        order = order + 1
                        self:NetworkVar("Float", key)
                    end

                    local function AddIntVar(key, min, max, category)
                        order = order + 1
                        self:NetworkVar("Int", key)
                    end

                    local function AddBoolVar(key, category)
                        order = order + 1
                        self:NetworkVar("Bool", key)
                    end

                    AddFloatVar("MaxSteerAngle", 10, 80, "steering")
                    AddFloatVar("SteerConeChangeRate", 0.1, 10, "steering")
                    AddBoolVar("HasHeadlights", "lights")
                    AddIntVar("MaxHealth", 100, 10000, "health")
                end
            "#,
            ),
            (
                "lua/entities/base_glide_car_realistic/cl_init.lua",
                r#"
                function ENT:OnUpdateSounds()
                    local redlining = self:GetIsRedlining()
                    local gear = self:GetGear()
                    local steer = self:GetSteering()
                end
            "#,
            ),
        ]);

        {
            let db = ws.get_db_mut();
            let class_id = LuaTypeDeclId::global("base_glide_car_realistic");
            let owner = LuaMemberOwner::Type(class_id);
            let members = db
                .get_member_index()
                .get_members(&owner)
                .expect("expected members on base_glide_car_realistic");
            let member_names: Vec<_> = members
                .iter()
                .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                .collect();

            assert!(
                member_names.contains(&"GetIsRedlining".to_string()),
                "missing GetIsRedlining in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetIsRedlining".to_string()),
                "missing SetIsRedlining in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetGear".to_string()),
                "missing GetGear in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetGear".to_string()),
                "missing SetGear in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetSteering".to_string()),
                "missing GetSteering in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetSteering".to_string()),
                "missing SetSteering in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetTireSmokeColor".to_string()),
                "missing GetTireSmokeColor in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetTireSmokeColor".to_string()),
                "missing SetTireSmokeColor in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetHeadlightColor".to_string()),
                "missing GetHeadlightColor in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetHeadlightColor".to_string()),
                "missing SetHeadlightColor in {member_names:?}"
            );
        }

        let cl_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/base_glide_car_realistic/cl_init.lua");
        let cl_file_id = ws
            .analysis
            .get_file_id(&cl_uri)
            .expect("expected cl_init file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(cl_file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics on cl_init.lua accessing direct NetworkVar getters: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_network_var_sequential_file_analysis() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        // First batch: shared.lua analyzed alone
        ws.def_file(
            "lua/entities/seq_entity/shared.lua",
            r#"
        function ENT:SetupDataTables()
            self:NetworkVar("Bool", "IsRedlining")
            self:NetworkVar("Float", 0, "Speed")
        end
    "#,
        );

        // Second batch: cl_init.lua analyzed separately (incremental)
        let cl_file_id = ws.def_file(
            "lua/entities/seq_entity/cl_init.lua",
            r#"
        function ENT:Think()
            local x = self:GetIsRedlining()
            local y = self:GetSpeed()
        end
    "#,
        );

        // Check: cl_init.lua should have no UndefinedField diagnostics
        let diagnostics = ws
            .analysis
            .diagnose_file(cl_file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics when files analyzed sequentially: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_network_var_with_std_lib_initialized() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_files(vec![
            (
                "lua/entities/stdlib_ent/shared.lua",
                r#"
            function ENT:SetupDataTables()
                self:NetworkVar("Bool", "IsRedlining")
                self:NetworkVar("Float", 0, "Speed")
            end
        "#,
            ),
            (
                "lua/entities/stdlib_ent/cl_init.lua",
                r#"
            function ENT:Think()
                local x = self:GetIsRedlining()
                local y = self:GetSpeed()
            end
        "#,
            ),
        ]);

        let cl_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/stdlib_ent/cl_init.lua");
        let cl_file_id = ws
            .analysis
            .get_file_id(&cl_uri)
            .expect("expected cl_init file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(cl_file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_field_code),
            "unexpected undefined-field diagnostics with std lib loaded: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_network_var_same_function_usage_with_real_config() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = legacy_scopes(&[
            "entities/**",
            "weapons/**",
            "effects/**",
            "weapons/gmod_tool/stools/**",
            "plugins/**",
        ]);
        emmyrc.gmod.scripted_class_scopes.legacy_exclude = vec![
            "**/tests/**".to_string(),
            "**/test/**".to_string(),
            "**/docs/**".to_string(),
        ];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        // This exactly mirrors the real base_glide/shared.lua:
        // - ENT.Type and ENT.Base at top level
        // - Many ENT field assignments
        // - SetupDataTables with NetworkVar calls followed by setter usage
        // - CLIENT/SERVER conditional blocks
        // - NetworkVarNotify calls
        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/entities/base_glide/shared.lua",
            r##"
        ENT.Type = "anim"
        ENT.Base = "base_anim"

        ENT.PrintName = "Glide Base Vehicle"
        ENT.IsGlideVehicle = true
        ENT.MaxChassisHealth = 1000
        ENT.CanSwitchHeadlights = false

        function ENT:SetupDataTables()
            self:NetworkVar("Entity", "Driver")
            self:NetworkVar("Int", "EngineState")
            self:NetworkVar("Bool", "IsEngineOnFire")
            self:NetworkVar("Bool", "IsLocked")
            self:NetworkVar("Int", "LockOnState")
            self:NetworkVar("Entity", "LockOnTarget")
            self:NetworkVar("Float", "ChassisHealth")
            self:NetworkVar("Float", "EngineHealth")
            self:NetworkVar("Float", "BrakeValue")
            self:NetworkVar("Bool", "HandbrakeActive")
            self:NetworkVar("Int", "HeadlightState")
            self:NetworkVar("Int", "TurnSignalState")
            self:NetworkVar("Int", "ConnectedReceptacleCount")
            self:NetworkVar("Bool", "ReducedThrottle")
            self:NetworkVar("Int", "WaterState")

            if CLIENT then
                self:NetworkVarNotify("WaterState", self.OnWaterStateChange)
            end

            local editData = nil
            if self.CanSwitchHeadlights then
                editData = {
                    KeyName = "HeadlightColor",
                    Edit = { type = "VectorColor", order = 0, category = "#glide.settings" },
                }
            end
            self:NetworkVar("Vector", "HeadlightColor", editData)

            self:SetDriver(NULL)
            self:SetEngineState(0)
            self:SetIsEngineOnFire(false)
            self:SetLockOnState(0)
            self:SetLockOnTarget(NULL)
            self:SetBrakeValue(0)
            self:SetHandbrakeActive(false)
            self:SetHeadlightState(0)
            self:SetTurnSignalState(0)

            local maxChassis = self.MaxChassisHealth or 1000
            if maxChassis < 1 then maxChassis = 1 end
            self:SetChassisHealth(maxChassis)
            self:SetEngineHealth(1.0)

            self:NetworkVarNotify("EngineState", self.OnEngineStateChange)

            if SERVER then
                self:NetworkVarNotify("EngineState", self.OnEngineStateChangePhoton)
                self:NetworkVarNotify("HeadlightState", self.OnHeadlightStateChangePhoton)
                self:NetworkVarNotify("TurnSignalState", self.OnTurnSignalStateChangePhoton)
            end
        end

        function ENT:IsEngineOn(selfTbl)
            selfTbl = selfTbl or self:GetTable()
            local cached = selfTbl._cachedEngineState
            if cached ~= nil then
                return cached > 1
            end
            return self:GetEngineState() > 1
        end

        function ENT:IsBraking(selfTbl)
            selfTbl = selfTbl or self:GetTable()
            local cached = selfTbl._cachedBrakeValue
            if cached ~= nil then
                return cached > 0.1
            end
            return self:GetBrakeValue() > 0.1
        end

        function ENT:OnPostInitialize() end
        function ENT:OnTurnOn() end
        function ENT:OnTurnOff() end

        if CLIENT then
            ENT.Spawnable = false
            ENT.CameraOffset = Vector(-200, 0, 50)

            function ENT:OnActivateSounds() end
            function ENT:OnDeactivateSounds() end
            function ENT:OnUpdateSounds() end
        end

        if SERVER then
            ENT.Spawnable = true
            ENT.ChassisMass = 700

            function ENT:CreateFeatures() end
            function ENT:OnDriverEnter() end
            function ENT:OnDriverExit() end
        end
    "##,
        );

        // First check: members should exist on base_glide class
        let member_names: Vec<String> = {
            let db = ws.get_db_mut();
            let class_id = LuaTypeDeclId::global("base_glide");
            let owner = LuaMemberOwner::Type(class_id);
            let members = db
                .get_member_index()
                .get_members(&owner)
                .expect("expected members on base_glide class");
            let member_names: Vec<String> = members
                .iter()
                .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                .collect();

            assert!(
                member_names.contains(&"GetDriver".to_string()),
                "missing GetDriver in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetDriver".to_string()),
                "missing SetDriver in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetEngineState".to_string()),
                "missing GetEngineState in {member_names:?}"
            );
            assert!(
                member_names.contains(&"SetEngineState".to_string()),
                "missing SetEngineState in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetIsEngineOnFire".to_string()),
                "missing GetIsEngineOnFire in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetHeadlightColor".to_string()),
                "missing GetHeadlightColor in {member_names:?}"
            );

            member_names
        };

        // Second check: no UndefinedField diagnostics within the same file
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let param_type_mismatch_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        let missing_parameter_code = Some(NumberOrString::String(
            DiagnosticCode::MissingParameter.get_name().to_string(),
        ));
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        let param_diagnostics: Vec<_> = diagnostics
            .iter()
            .filter(|diag| {
                diag.code == param_type_mismatch_code || diag.code == missing_parameter_code
            })
            .collect();

        assert!(
            param_diagnostics.is_empty(),
            "unexpected param diagnostics for synthesized NetworkVar accessors: {param_diagnostics:?}"
        );

        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        let extract_undefined_field_name = |message: &str| {
            let prefix = "Undefined field `";
            message
                .strip_prefix(prefix)
                .and_then(|rest| rest.split_once('`'))
                .map(|(name, _)| name.to_string())
        };

        let mut undefined_field_names: Vec<String> = undefined_field_diags
            .iter()
            .filter_map(|diag| extract_undefined_field_name(&diag.message))
            .collect();
        undefined_field_names.sort();

        let allowed_undefined_field_names = [
            "NetworkVar",
            "NetworkVarNotify",
            "OnWaterStateChange",
            "OnEngineStateChange",
            "OnEngineStateChangePhoton",
            "OnHeadlightStateChangePhoton",
            "OnTurnSignalStateChangePhoton",
            "GetTable",
        ];

        assert!(
            undefined_field_names
                .iter()
                .all(|name| allowed_undefined_field_names.contains(&name.as_str())),
            "unexpected undefined-field diagnostics in same file: {undefined_field_diags:?}; undefined_field_names={undefined_field_names:?}; member_names={member_names:?}"
        );

        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "NetworkVar")
                .count(),
            16,
            "expected 16 undefined NetworkVar members"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "NetworkVarNotify")
                .count(),
            5,
            "expected 5 undefined NetworkVarNotify members"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "GetTable")
                .count(),
            2,
            "expected 2 undefined GetTable members"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "OnWaterStateChange")
                .count(),
            1,
            "expected one undefined callback member for OnWaterStateChange"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "OnEngineStateChange")
                .count(),
            1,
            "expected one undefined callback member for OnEngineStateChange"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "OnEngineStateChangePhoton")
                .count(),
            1,
            "expected one undefined callback member for OnEngineStateChangePhoton"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "OnHeadlightStateChangePhoton")
                .count(),
            1,
            "expected one undefined callback member for OnHeadlightStateChangePhoton"
        );
        assert_eq!(
            undefined_field_names
                .iter()
                .filter(|name| name.as_str() == "OnTurnSignalStateChangePhoton")
                .count(),
            1,
            "expected one undefined callback member for OnTurnSignalStateChangePhoton"
        );
    }

    #[gtest]
    fn test_network_var_with_entity_type_definitions() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_files(vec![
            (
                "lua/includes/entity_defs.lua",
                r#"
            ---@class Entity
            ---@field NetworkVar fun(self: Entity, type: string, name: string)
            ---@field NetworkVarNotify fun(self: Entity, name: string, callback: function)
            ---@field GetTable fun(self: Entity): table
            local Entity = {}

            ---@class ENT : Entity
            local ENT = {}
        "#,
            ),
            (
                "lua/entities/base_glide/shared.lua",
                r#"
            ENT.Type = "anim"
            ENT.Base = "base_anim"
            ENT.PrintName = "Glide Base Vehicle"
            ENT.MaxChassisHealth = 1000

            function ENT:SetupDataTables()
                self:NetworkVar("Entity", "Driver")
                self:NetworkVar("Int", "EngineState")
                self:NetworkVar("Bool", "IsEngineOnFire")
                self:NetworkVar("Bool", "IsLocked")
                self:NetworkVar("Float", "ChassisHealth")
                self:NetworkVar("Float", "EngineHealth")
                self:NetworkVar("Float", "BrakeValue")
                self:NetworkVar("Bool", "HandbrakeActive")
                self:NetworkVar("Int", "HeadlightState")

                if CLIENT then
                    self:NetworkVarNotify("HeadlightState", self.OnHeadlightStateChange)
                end

                self:SetDriver(NULL)
                self:SetEngineState(0)
                self:SetIsEngineOnFire(false)
                self:SetBrakeValue(0)
                self:SetHandbrakeActive(false)
                self:SetHeadlightState(0)

                local maxChassis = self.MaxChassisHealth or 1000
                self:SetChassisHealth(maxChassis)
                self:SetEngineHealth(1.0)

                self:NetworkVarNotify("EngineState", self.OnEngineStateChange)
            end

            function ENT:IsEngineOn()
                return self:GetEngineState() > 1
            end

            function ENT:IsBraking()
                return self:GetBrakeValue() > 0.1
            end
        "#,
            ),
        ]);

        let member_names: Vec<String> = {
            let db = ws.get_db_mut();
            let class_id = LuaTypeDeclId::global("base_glide");
            let owner = LuaMemberOwner::Type(class_id);
            let members = db
                .get_member_index()
                .get_members(&owner)
                .expect("expected members on base_glide");
            let member_names: Vec<_> = members
                .iter()
                .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                .collect();

            assert!(
                member_names.contains(&"SetDriver".to_string()),
                "missing SetDriver in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetDriver".to_string()),
                "missing GetDriver in {member_names:?}"
            );
            assert!(
                member_names.contains(&"GetEngineState".to_string()),
                "missing GetEngineState in {member_names:?}"
            );

            member_names
        };

        let shared_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/base_glide/shared.lua");
        let shared_file_id = ws
            .analysis
            .get_file_id(&shared_uri)
            .expect("expected shared file id");
        let diagnostics = ws
            .analysis
            .diagnose_file(shared_file_id, CancellationToken::new())
            .unwrap_or_default();

        let param_type_mismatch_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        let missing_parameter_code = Some(NumberOrString::String(
            DiagnosticCode::MissingParameter.get_name().to_string(),
        ));
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        let param_diagnostics: Vec<_> = diagnostics
            .iter()
            .filter(|diag| {
                diag.code == param_type_mismatch_code || diag.code == missing_parameter_code
            })
            .collect();

        assert!(
            param_diagnostics.is_empty(),
            "unexpected param diagnostics for synthesized NetworkVar accessors: {param_diagnostics:?}"
        );

        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        let extract_undefined_field_name = |message: &str| {
            let prefix = "Undefined field `";
            message
                .strip_prefix(prefix)
                .and_then(|rest| rest.split_once('`'))
                .map(|(name, _)| name.to_string())
        };

        let mut undefined_field_names: Vec<String> = undefined_field_diags
            .iter()
            .filter_map(|diag| extract_undefined_field_name(&diag.message))
            .collect();
        undefined_field_names.sort();

        let mut expected_undefined_field_names = vec![
            "OnEngineStateChange".to_string(),
            "OnHeadlightStateChange".to_string(),
        ];
        expected_undefined_field_names.sort();

        assert!(
            undefined_field_names == expected_undefined_field_names,
            "unexpected undefined-field diagnostics with Entity type defined: {undefined_field_diags:?}; undefined_field_names={undefined_field_names:?}; member_names={member_names:?}"
        );
    }

    #[gtest]
    fn test_stool_methods_resolve_from_tool_super_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file(
            "lua/includes/custom_classes.lua",
            r#"
                ---@class Tool
                ---@class TOOL : Tool
            "#,
        );

        ws.def_file(
            "lua/includes/tool_meta.lua",
            r#"
                ---@class Player
                ---@param command string
                function Player:ConCommand(command) end

                ---@return Player
                function Tool:GetOwner() end

                ---@param name string
                ---@return number
                function Tool:GetClientNumber(name) end

                ---@return table
                function Tool:BuildConVarList() end

                ---@class DNumSlider
                ---@param value number
                function DNumSlider:SetValue(value) end

                ---@class ControlPanel
                ---@param text string
                function ControlPanel:Help(text) end

                ---@param controlType string
                ---@param info table
                function ControlPanel:AddControl(controlType, info) end

                ---@return DNumSlider
                function ControlPanel:NumSlider(...) end

                ---@param panel ControlPanel
                function TOOL.BuildCPanel(panel) end
            "#,
        );

        let file_id = ws.def_file(
            "lua/weapons/gmod_tool/stools/my_tool.lua",
            r#"
                TOOL.Category = "Test"
                TOOL.Name = "My Tool"

                function TOOL:LeftClick(trace)
                    local owner = self:GetOwner()
                    local speed = self:GetClientNumber("my_speed")
                    local defaults = self:BuildConVarList()
                    owner:ConCommand("say test")
                    return IsValid(owner) and speed >= 0 and defaults ~= nil
                end

                function TOOL.BuildCPanel(panel)
                    panel:Help("help")
                    panel:AddControl("slider", {})
                    local slider = panel:NumSlider("Label", "tool_speed", 0, 10, 0)
                    slider:SetValue(5)
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        let extract_name = |message: &str| {
            message
                .strip_prefix("Undefined field `")
                .and_then(|rest| rest.split_once('`'))
                .map(|(name, _)| name.to_string())
        };

        let undefined_field_names: Vec<String> = undefined_field_diags
            .iter()
            .filter_map(|diag| extract_name(&diag.message))
            .collect();

        assert!(
            !undefined_field_names.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "GetOwner"
                        | "GetClientNumber"
                        | "BuildConVarList"
                        | "ConCommand"
                        | "Help"
                        | "AddControl"
                        | "NumSlider"
                        | "SetValue"
                )
            }),
            "unexpected undefined-field diagnostics for TOOL base methods: {undefined_field_diags:?}"
        );
    }

    #[gtest]
    fn test_stool_buildcpanel_from_field_annotation_only() {
        // Reproduces the real scenario with exact annotation structure from glua-api-snippets.
        // @field BuildCPanel fun(panel: ControlPanel) is on Tool class (not TOOL).
        // ControlPanel : DForm, and DForm:Help is inherited.
        // Methods are split across separate files.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        // class.TOOL.lua - exact real annotation structure
        ws.def_file(
            "lua/annotations/class.TOOL.lua",
            r#"
                ---@class Tool
                ---@field BuildCPanel fun(panel: ControlPanel) Called to populate the tool's control panel.
                Tool = Tool or {}

                ---@class TOOL : Tool
                TOOL = {}
            "#,
        );

        // tool.lua - Tool methods
        ws.def_file(
            "lua/annotations/tool.lua",
            r#"
                ---@return Player
                function Tool:GetOwner() end
            "#,
        );

        // player.lua
        ws.def_file(
            "lua/annotations/player.lua",
            r#"
                ---@class Player
                ---@param command string
                function Player:ConCommand(command) end
            "#,
        );

        // class.ControlPanel.lua (separate from controlpanel.lua methods)
        ws.def_file(
            "lua/annotations/class.ControlPanel.lua",
            r#"
                ---@class ControlPanel : DForm
                local ControlPanel = {}
            "#,
        );

        // controlpanel.lua (methods, separate from class declaration)
        ws.def_file(
            "lua/annotations/controlpanel.lua",
            r#"
                ---@param type string
                ---@param controlinfo table
                ---@return Panel
                function ControlPanel:AddControl(type, controlinfo) end

                ---@return DNumSlider
                function ControlPanel:NumSlider(...) end
            "#,
        );

        // dform.lua (DForm methods, inherited by ControlPanel)
        ws.def_file(
            "lua/annotations/dform.lua",
            r#"
                ---@class DForm : DCollapsibleCategory
                local DForm = {}

                ---@param help string
                ---@return DLabel
                function DForm:Help(help) end

                ---@param label string
                ---@param convar string
                ---@param min number
                ---@param max number
                ---@param decimals? number
                ---@return DNumSlider
                function DForm:NumSlider(label, convar, min, max, decimals) end
            "#,
        );

        // dcollapsiblecategory.lua
        ws.def_file(
            "lua/annotations/dcollapsiblecategory.lua",
            r#"
                ---@class DCollapsibleCategory
                ---@class DNumSlider
                ---@class DLabel
                ---@class Panel
            "#,
        );

        // Stool file: NO @param annotations, types come from @field on Tool
        let file_id = ws.def_file(
            "lua/weapons/gmod_tool/stools/glide_test.lua",
            r#"
                TOOL.Category = "Glide"
                TOOL.Name = "Projectile Launcher"

                function TOOL:RightClick(trace)
                    local ply = self:GetOwner()
                    ply:ConCommand("say test")
                end

                function TOOL.BuildCPanel(panel)
                    panel:Help("help text")
                    panel:AddControl("slider", {})
                    panel:NumSlider("Speed", "tool_speed", 0, 100)
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        let extract_name = |message: &str| {
            message
                .strip_prefix("Undefined field `")
                .and_then(|rest| rest.split_once('`'))
                .map(|(name, _)| name.to_string())
        };
        let undefined_field_names: Vec<String> = undefined_field_diags
            .iter()
            .filter_map(|diag| extract_name(&diag.message))
            .collect();

        assert!(
            !undefined_field_names.iter().any(|name| matches!(
                name.as_str(),
                "Help" | "AddControl" | "NumSlider" | "ConCommand" | "GetOwner"
            )),
            "unexpected undefined-field diagnostics for TOOL methods (type from @field): {undefined_field_diags:?}"
        );
    }

    #[gtest]
    fn test_vector_member_access_after_arithmetic_chain() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "lua/glide/server/vector_chain.lua",
            r#"
                local function compute(steerDir, fixedLen)
                    local dirLen = steerDir:Length()
                    local dirNorm = steerDir / dirLen
                    local dirVec = dirNorm * fixedLen
                    dirVec:Normalize()
                    local len = dirVec:Length()
                    local cross = dirVec:Cross(Vector(1, 0, 0))
                    return len + cross.x + dirVec.y
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        let extract_name = |message: &str| {
            message
                .strip_prefix("Undefined field `")
                .and_then(|rest| rest.split_once('`'))
                .map(|(name, _)| name.to_string())
        };
        let undefined_field_names: Vec<String> = undefined_field_diags
            .iter()
            .filter_map(|diag| extract_name(&diag.message))
            .collect();

        assert!(
            !undefined_field_names
                .iter()
                .any(|name| matches!(name.as_str(), "Normalize" | "Length" | "Cross" | "x" | "y")),
            "unexpected vector undefined-field diagnostics after arithmetic chain: {undefined_field_diags:?}"
        );
    }

    #[test]
    fn test_dynamic_field_from_typed_variable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        // Mimics the real pattern: function param with @type re-annotation
        let file_id = ws.def(
            r#"
                ---@class base_glide_car
                local ENT = {}

                ---@return table
                function ENT:GetTable() return {} end

                function ENT:OnPostThink(dt, selfTbl)
                    ---@type base_glide_car
                    selfTbl = selfTbl or self:GetTable()

                    selfTbl.throttleRamp = 0.5
                    local x = selfTbl.throttleRamp * selfTbl.throttleRamp * selfTbl.throttleRamp
                end
            "#,
        );

        let result = ws.analysis.diagnose_file(file_id, CancellationToken::new());
        let diags: Vec<_> = result
            .unwrap_or_default()
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        "undefined-field".to_string(),
                    ))
            })
            .collect();

        let field_names: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();

        assert!(
            !field_names.iter().any(|m| m.contains("throttleRamp")),
            "throttleRamp should not trigger undefined-field after dynamic assignment: {field_names:?}"
        );
    }

    #[test]
    fn test_dynamic_field_multifile() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        // Class defined in file A
        ws.def_file(
            "lua/entities/base_glide_car/shared.lua",
            r#"
                ---@class base_glide_car
                local ENT = {}

                ---@return table
                function ENT:GetTable() return {} end
            "#,
        );

        // Dynamic field used in file B
        let file_id = ws.def_file(
            "lua/entities/base_glide_car/init.lua",
            r#"
                ---@class base_glide_car
                local ENT = {}

                local getTable = FindMetaTable("Entity").GetTable

                function ENT:OnPostThink(dt, selfTbl)
                    ---@type base_glide_car
                    selfTbl = selfTbl or getTable(self)

                    selfTbl.throttleRamp = 0.5
                    local x = selfTbl.throttleRamp * selfTbl.throttleRamp * selfTbl.throttleRamp
                end
            "#,
        );

        let result = ws.analysis.diagnose_file(file_id, CancellationToken::new());
        let diags: Vec<_> = result
            .unwrap_or_default()
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        "undefined-field".to_string(),
                    ))
            })
            .collect();

        let field_names: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();

        assert!(
            !field_names.iter().any(|m| m.contains("throttleRamp")),
            "throttleRamp should not trigger undefined-field in multi-file setup: {field_names:?}"
        );
    }

    #[test]
    fn test_dynamic_field_table_typed_in_class_file() {
        // When selfTbl is typed as `table` (e.g. from Entity:GetTable()),
        // dynamic field assignments in gmod class files should still be indexed
        // under the class type.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file(
            "lua/entities/base_glide_car/shared.lua",
            r#"
                ---@class base_glide_car : Entity
                local ENT = {}
            "#,
        );

        // selfTbl is typed as `table` (no @type annotation)
        let file_id = ws.def_file(
            "lua/entities/base_glide_car/sv_braking.lua",
            r#"
                ---@class base_glide_car
                local ENT = {}

                ---@return table
                local function getTable(e) return {} end

                function ENT:BrakeInit()
                    local selfTbl = getTable(self)
                    selfTbl.frontBrake = 0
                    selfTbl.rearBrake = 0
                end

                function ENT:GetBrakes()
                    return self.frontBrake + self.rearBrake
                end
            "#,
        );

        let result = ws.analysis.diagnose_file(file_id, CancellationToken::new());
        let diags: Vec<_> = result
            .unwrap_or_default()
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        "undefined-field".to_string(),
                    ))
            })
            .collect();

        let field_names: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();

        assert!(
            !field_names
                .iter()
                .any(|m| m.contains("frontBrake") || m.contains("rearBrake")),
            "frontBrake/rearBrake should not trigger undefined-field when assigned via table-typed selfTbl in class file: {field_names:?}"
        );
    }

    #[gtest]
    fn test_numeric_for_index_expr_on_inferred_ent_weapons_table() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file(
            "lua/entities/base_glide/shared.lua",
            r#"
                ---@class base_glide : Entity
                local ENT = {}
            "#,
        );

        let file_id = ws.def_file(
            "lua/entities/base_glide/sv_weapons.lua",
            r#"
                ---@class base_glide
                local ENT = {}

                local registry = {
                    base = {
                        Initialize = function(self) end,
                        OnRemove = function(self) end,
                    }
                }

                local function CreateVehicleWeapon(className, data)
                    local class = registry[className]
                    return setmetatable(data or {}, { __index = class })
                end

                function ENT:CreateWeapon(className, data)
                    local weapon = CreateVehicleWeapon(className, data)
                    local index = (self.weaponCount or 0) + 1

                    self.weaponCount = index
                    self.weapons = self.weapons or {}
                    self.weapons[index] = weapon
                    weapon:Initialize()
                end

                function ENT:ClearWeapons()
                    local myWeapons = self.weapons
                    if not myWeapons then return end

                    for i = #myWeapons, 1, -1 do
                        myWeapons[i]:OnRemove()
                        myWeapons[i] = nil
                    end
                end
            "#,
        );

        let field_names: Vec<String> = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("undefined-field".to_string())))
            .map(|d| d.message)
            .collect();

        assert!(
            !field_names.iter().any(|m| m.contains("`[i]`")),
            "numeric for index into inferred ENT weapons table should not trigger undefined-field: {field_names:?}"
        );
    }

    /// Verify that a method defined in two entity files (init.lua + shared.lua CLIENT block)
    /// is stored as Many with both member IDs in the member index.
    #[gtest]
    fn test_dual_realm_ent_method_stores_both_members() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/entities/dual_realm_ent/init.lua",
            r#"
            function ENT:GetFuelAmountUnits()
                return self.fuelAmount or 0
            end
        "#,
        );

        ws.def_file(
            "lua/entities/dual_realm_ent/shared.lua",
            r#"
            if CLIENT then
                function ENT:GetFuelAmountUnits()
                    return self:GetNWFloat("fuel", 0)
                end
            end
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("dual_realm_ent");
        let owner = LuaMemberOwner::Type(class_id.clone());

        // Get all members of the class
        let members = db.get_member_index().get_members(&owner);
        let fuel_members: Vec<_> = members
            .unwrap_or_default()
            .into_iter()
            .filter(|m| {
                m.get_key()
                    .get_name()
                    .is_some_and(|n| n == "GetFuelAmountUnits")
            })
            .collect();

        // We expect BOTH definitions (init.lua server + shared.lua client)
        assert!(
            fuel_members.len() >= 2,
            "Expected at least 2 member definitions for GetFuelAmountUnits on dual_realm_ent, got {} from files {:?}",
            fuel_members.len(),
            fuel_members
                .iter()
                .map(|m| m.get_file_id())
                .collect::<Vec<_>>()
        );
    }

    #[gtest]
    fn test_derma_define_control_registers_global() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/vgui/test_derma_panel.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
            end

            function PANEL:Paint(w, h)
            end

            derma.DefineControl("TestDermaPanel", "Description", PANEL, "DFrame")

            -- TestPanel should be recognized as a valid global
            local x = TestDermaPanel  -- should not report undefined global
        "#,
        );

        let db = ws.get_db_mut();

        // Verify the class type was created
        let class_id = LuaTypeDeclId::global("TestDermaPanel");
        let class_decl = db.get_type_index().get_type_decl(&class_id);
        assert!(
            class_decl.is_some(),
            "TestDermaPanel class should be created"
        );

        // Verify the global was registered
        let global_decl_ids = db.get_global_index().get_global_decl_ids("TestDermaPanel");
        assert!(
            global_decl_ids.is_some(),
            "TestDermaPanel should be registered as a global"
        );
        assert!(
            !global_decl_ids.unwrap().is_empty(),
            "TestDermaPanel global decl should exist"
        );
    }

    /// Regression test: SWEP field assignments in scope files (without an explicit `local SWEP
    /// = {}` declaration) should be owned by the per-entity class type, not by the shared
    /// `GlobalPath("SWEP")` owner. Before the fix, all weapon files that assigned the same field
    /// via `SWEP.Field = …` would accumulate that field on the global SWEP path, causing
    /// cross-contamination between different weapons' member types (e.g. `number|IMaterial`).
    #[gtest]
    fn test_swep_members_scoped_to_weapon_class_not_global_swep() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_files(vec![
            (
                "lua/weapons/weapon_a/shared.lua",
                r#"
                SWEP.UniqueFieldA = 1
                "#,
            ),
            (
                "lua/weapons/weapon_b/shared.lua",
                r#"
                SWEP.UniqueFieldB = 2
                "#,
            ),
        ]);

        let db = ws.get_db_mut();

        // Each weapon's class should own only its own field
        let weapon_a_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(LuaTypeDeclId::global("weapon_a")))
            .expect("weapon_a should have members");
        let weapon_a_names: Vec<_> = weapon_a_members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            weapon_a_names.contains(&"UniqueFieldA".to_string()),
            "weapon_a should have UniqueFieldA, got {weapon_a_names:?}"
        );
        assert!(
            !weapon_a_names.contains(&"UniqueFieldB".to_string()),
            "weapon_a should NOT contain UniqueFieldB from weapon_b, got {weapon_a_names:?}"
        );

        let weapon_b_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(LuaTypeDeclId::global("weapon_b")))
            .expect("weapon_b should have members");
        let weapon_b_names: Vec<_> = weapon_b_members
            .iter()
            .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
            .collect();

        assert!(
            weapon_b_names.contains(&"UniqueFieldB".to_string()),
            "weapon_b should have UniqueFieldB, got {weapon_b_names:?}"
        );
        assert!(
            !weapon_b_names.contains(&"UniqueFieldA".to_string()),
            "weapon_b should NOT contain UniqueFieldA from weapon_a, got {weapon_b_names:?}"
        );

        // Neither field should be left on the global SWEP path after migration
        let global_swep_members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::GlobalPath(GlobalId::new("SWEP")));
        if let Some(global_members) = global_swep_members {
            let global_names: Vec<_> = global_members
                .iter()
                .filter_map(|m| m.get_key().get_name().map(|n| n.to_string()))
                .collect();
            assert!(
                !global_names.contains(&"UniqueFieldA".to_string()),
                "UniqueFieldA should NOT remain on GlobalPath(SWEP), got {global_names:?}"
            );
            assert!(
                !global_names.contains(&"UniqueFieldB".to_string()),
                "UniqueFieldB should NOT remain on GlobalPath(SWEP), got {global_names:?}"
            );
        }
    }

    #[gtest]
    fn test_swep_wepselecticon_does_not_union_with_global_annotation_on_first_analysis() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::ParamTypeMismatch);

        let file_id = ws.def_file(
            "lua/weapons/weapon_mad_deagle/shared.lua",
            r#"
            if ( SERVER ) then return end

            SWEP.WepSelectIcon = Material("swepicons/cityrp_deagle.png")

            function SWEP:DrawWeaponSelection( x, y, w, h, a )
                surface.SetMaterial( self.WepSelectIcon )
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let param_type_mismatch_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        let mismatch_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == param_type_mismatch_code)
            .collect();
        assert!(
            mismatch_diags.is_empty(),
            "unexpected param-type-mismatch diagnostics: {mismatch_diags:?}"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let swep_name_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|name_expr| {
                name_expr
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "SWEP")
            })
            .expect("expected SWEP name expression");
        let swep_token = swep_name_expr
            .get_name_token()
            .expect("expected SWEP token");
        let swep_semantic = semantic_model
            .get_semantic_info(swep_token.syntax().clone().into())
            .expect("expected semantic info for SWEP");
        assert_eq!(
            swep_semantic.typ,
            LuaType::Def(LuaTypeDeclId::global("weapon_mad_deagle")),
            "expected SWEP to resolve to the scoped weapon class"
        );
    }

    #[gtest]
    fn test_scripted_class_cross_file_assignments_survive_single_file_reindex() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let shared_path = "lua/weapons/weapon_mad_deagle/shared.lua";
        let shared_text = r#"
            SWEP.WepSelectIcon = "material"
        "#;

        ws.def_files(vec![
            (shared_path, shared_text),
            (
                "lua/weapons/weapon_mad_deagle/cl_init.lua",
                r#"
                include("shared.lua")
                SWEP.WepSelectIcon = 1
                "#,
            ),
        ]);

        let shared_uri = ws.virtual_url_generator.new_uri(shared_path);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("weapon_mad_deagle"));
        let key = LuaMemberKey::Name("WepSelectIcon".into());

        let member_item_before = ws
            .analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_member_item(&owner, &key)
            .expect("expected WepSelectIcon member item before edit");
        let member_count_before = match member_item_before {
            crate::LuaMemberIndexItem::One(_) => 1,
            crate::LuaMemberIndexItem::Many(ids) => ids.len(),
        };
        assert_eq!(
            member_count_before, 1,
            "expected latest assignment to replace previous assignment"
        );

        ws.analysis
            .update_file_by_uri(&shared_uri, Some(format!("\n{shared_text}")))
            .expect("shared file should update");

        let member_item_after = ws
            .analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_member_item(&owner, &key)
            .expect("expected WepSelectIcon member item after edit");
        let member_count_after = match member_item_after {
            crate::LuaMemberIndexItem::One(_) => 1,
            crate::LuaMemberIndexItem::Many(ids) => ids.len(),
        };
        assert_eq!(
            member_count_after, 1,
            "single-file reindex should retain the latest assignment"
        );
    }
}
