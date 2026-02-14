#[cfg(test)]
mod test {
    use emmylua_parser::{LuaAstNode, LuaAstToken, LuaLocalName, LuaNameExpr};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, Emmyrc, GmodClassCallLiteral, LuaMemberOwner, LuaType, LuaTypeDeclId,
        VirtualWorkspace,
    };

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
    fn test_scripted_class_scope_filters_metadata_collection() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["lua/entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let allowed_file_id = ws.def_file(
            "lua/entities/entities_test.lua",
            r#"DEFINE_BASECLASS("base_anim")"#,
        );
        let denied_file_id = ws.def_file(
            "lua/autorun/ignored.lua",
            r#"DEFINE_BASECLASS("base_anim")"#,
        );

        assert!(
            ws.get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&allowed_file_id)
                .is_some()
        );
        assert!(
            ws.get_db_mut()
                .get_gmod_class_metadata_index()
                .get_file_metadata(&denied_file_id)
                .is_none()
        );
    }

    #[test]
    fn test_scripted_class_scope_matches_nested_entities_folder_anywhere() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
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

    #[test]
    fn test_plugin_scope_binds_plugin_decl_to_scoped_class() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["plugins/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/plugins/vehicles/sh_plugin.lua",
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

    #[test]
    fn test_plugin_scope_binds_plugin_decl_with_self_reference_initializer() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["plugins/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/plugins/vehicles/sh_plugin.lua",
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

    #[test]
    fn test_plugin_scope_binding_respects_scope_filters() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/plugins/vehicles/sh_plugin.lua",
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

    #[test]
    fn test_entity_scope_infers_ent_reference_to_scoped_class_without_local_decl() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/entities/entities/cityrp_money/sh_init.lua",
            r#"
            ENT.Type = "anim"
            ENT.Base = "base_gmodentity"
        "#,
        );

        let name_expr = ws.get_node::<LuaNameExpr>(file_id);
        let token = name_expr
            .get_name_token()
            .expect("expected ENT name token");
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

    #[test]
    fn test_entity_scope_creates_class_decl_without_ent_base_assignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/entities/entities/cityrp_inventory/init.lua",
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
        let token = name_expr
            .get_name_token()
            .expect("expected ENT name token");
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

    #[test]
    fn test_entity_method_self_infers_scoped_class_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/entities/entities/cityrp_inventory/init.lua",
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

    #[test]
    fn test_accessor_func_synthesizes_get_set_members() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/entities/my_entity/init.lua",
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

    #[test]
    fn test_accessor_func_force_type_resolves_correctly() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/entities/typed_ent/init.lua",
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

        let getter_type = db
            .get_type_index()
            .get_type_cache(&getter.get_id().into());
        assert!(getter_type.is_some(), "GetPosition should have a type bound");
    }

    #[test]
    fn test_network_var_synthesizes_get_set_members() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/entities/nw_entity/init.lua",
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

    #[test]
    fn test_define_baseclass_sets_super_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/entities/derived_ent/init.lua",
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

    #[test]
    fn test_ent_base_from_shared_file_sets_folder_class_super_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/cityrp/gamemode/entities/entities/cityrp_money/sh_init.lua",
            r#"
            ENT.Base = "cityrp_base"
        "#,
        );

        let file_id = ws.def_file(
            "addons/cityrp/gamemode/entities/entities/cityrp_money/init.lua",
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
        let token = name_expr
            .get_name_token()
            .expect("expected ENT name token");
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

    #[test]
    fn test_ent_base_known_gmod_base_maps_to_ent_super_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/entities/mapped_ent/shared.lua",
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

    #[test]
    fn test_doc_param_resolves_scoped_entity_type_without_type_not_found() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::TypeNotFound);

        ws.def_files(vec![
            (
                "addons/test/lua/entities/base_glide_car/shared.lua",
                r#"
                function ENT:Initialize()
                end
            "#,
            ),
            (
                "addons/test/lua/autorun/use_base.lua",
                r#"
                ---@param ent base_glide_car
                local function consume(ent)
                end
            "#,
            ),
        ]);

        let param_uri = ws
            .virtual_url_generator
            .new_uri("addons/test/lua/autorun/use_base.lua");
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
            diagnostics.iter().all(|diag| diag.code != type_not_found_code),
            "unexpected type-not-found diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_vgui_register_creates_panel_class() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/vgui/my_panel.lua",
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

    #[test]
    fn test_vgui_register_not_captured_when_gmod_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "addons/test/lua/vgui/disabled_panel.lua",
            r#"
            local PANEL = {}
            vgui.Register("DisabledPanel", PANEL, "DPanel")
        "#,
        );

        let db = ws.get_db_mut();
        let class_id = LuaTypeDeclId::global("DisabledPanel");
        let decl = db.get_type_index().get_type_decl(&class_id);
        assert!(decl.is_none(), "Panel class should not be created when gmod disabled");
    }
}
