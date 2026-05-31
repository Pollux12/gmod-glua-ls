#[cfg(test)]
mod test {
    use glua_parser::{LuaAst, LuaAstNode, LuaAstToken, LuaIndexKey, LuaLocalName, LuaVarExpr};
    use googletest::prelude::*;
    use lsp_types::{NumberOrString, Uri};
    use smol_str::SmolStr;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, Emmyrc, LuaMemberId, LuaMemberOwner, LuaType, LuaUnionType,
        VirtualWorkspace,
    };

    fn file_has_diagnostic(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        diagnostic_code: DiagnosticCode,
    ) -> bool {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            diagnostic_code.get_name().to_string(),
        ));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    fn local_name_type(ws: &mut VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let local_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == name)
            })
            .expect("expected local name");
        let token = local_name
            .get_name_token()
            .expect("expected local name token");

        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .map(|info| info.typ)
            .expect("expected semantic info for local name")
    }

    fn index_expr_type(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .find_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr) if index_expr.syntax().text() == expr_text => {
                    semantic_model
                        .get_semantic_info(index_expr.syntax().clone().into())
                        .map(|info| info.typ)
                }
                _ => None,
            })
            .expect("expected semantic info for index expr")
    }

    fn first_index_expr_member_owner(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> LuaMemberOwner {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let index_expr = semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .find_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr) if index_expr.syntax().text() == expr_text => {
                    Some(index_expr)
                }
                _ => None,
            })
            .expect("expected index expr");
        let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
        ws.analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_member_owner(&member_id)
            .cloned()
            .expect("expected member owner")
    }

    #[test]
    fn test_issue_318() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        local map = {
            a = 'a',
            b = 'b',
            c = 'c',
        }
        local key      --- @type string
        c = map[key]   -- type should be ('a'|'b'|'c'|nil)

        "#,
        );

        let c_ty = ws.expr_ty("c");

        let union_type = LuaType::Union(
            LuaUnionType::from_vec(vec![
                LuaType::StringConst(SmolStr::new("a").into()),
                LuaType::StringConst(SmolStr::new("b").into()),
                LuaType::StringConst(SmolStr::new("c").into()),
                LuaType::Nil,
            ])
            .into(),
        );

        assert_eq!(c_ty, union_type);
    }

    #[test]
    fn test_issue_314_generic_inheritance() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        ---@class foo<T>: T
        local foo_mt = {}

        ---@type foo<{a: string}>
        local bar = { a = 'test' }

        c = bar.a -- should be string

        ---@class buz<T>: foo<T>
        local buz_mt = {}

        ---@type buz<{a: integer}>
        local qux = { a = 5 }

        d = qux.a -- should be integer
        "#,
        );

        let c_ty = ws.expr_ty("c");
        let d_ty = ws.expr_ty("d");

        assert_eq!(c_ty, LuaType::String);
        assert_eq!(d_ty, LuaType::Integer);
    }

    #[test]
    fn test_issue_397() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        --- @class A
        --- @field field? integer

        --- @class B : A
        --- @field field integer

        --- @type B
        local b = { field = 1 }

        local key1 --- @type 'field'
        local key2 = 'field'

        a = b.field -- type is integer - correct
        d = b['field'] -- type is integer - correct
        e = b[key1] -- type is integer? - wrong
        f = b[key2] -- type is integer? - wrong
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let d_ty = ws.expr_ty("d");
        let e_ty = ws.expr_ty("e");
        let f_ty = ws.expr_ty("f");

        assert_eq!(a_ty, LuaType::Integer);
        assert_eq!(d_ty, LuaType::Integer);
        assert_eq!(e_ty, LuaType::Integer);
        assert_eq!(f_ty, LuaType::Integer);
    }

    #[test]
    fn test_keyof() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class SuiteHooks
        ---@field beforeAll string
        ---@field afterAll number

        ---@type SuiteHooks
        local hooks = {}

        ---@type keyof SuiteHooks
        local name = "beforeAll"

        A = hooks[name]
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "(number|string)");
    }

    #[gtest]
    fn test_flow_fallback_for_class_typed_dynamic_field() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class TestEntity
        local TestEntity = {}

        ---@return TestEntity
        local function create_entity()
        end

        local ent = create_entity()
        ent.testVar = true
        A = ent.testVar
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_that!(ws.check_type(&ty, &LuaType::Boolean), eq(true));
    }

    #[gtest]
    fn test_flow_fallback_table_literal_regression_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local tbl = {}
        tbl.testVar = true
        A = tbl.testVar
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_that!(ws.check_type(&ty, &LuaType::Boolean), eq(true));
    }

    #[gtest]
    fn test_dynamic_field_value_inference_is_position_aware() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
        util = {}

        ---@return table?
        function util.JSONToTable(s) end

        ---@return table
        local function read_table()
            return util.JSONToTable("") or {}
        end

        ---@param value string
        ---@return string
        local function translate(value)
            return value
        end

        local data = read_table()
        A = data.text
        data.text = translate(data.text)
        B = data.text
        "#,
        );

        let before_ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(before_ty), "any?");

        let after_ty = ws.expr_ty("B");
        assert_that!(ws.check_type(&after_ty, &LuaType::String), eq(true));
    }

    #[gtest]
    fn test_incremental_reindex_keeps_dynamic_field_value_inference_position_aware() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let file_name = "lua/glide/client/network.lua";
        let source = r#"
        util = {}

        ---@return table?
        function util.JSONToTable(s) end

        ---@return table
        local function read_table()
            return util.JSONToTable("") or {}
        end

        ---@param value string
        ---@return string
        local function translate(value)
            return value
        end

        local data = read_table()
        local before = data.text
        data.text = translate(data.text)
        local after = data.text
        "#;

        let file_id = ws.def_file(file_name, source);

        let before_ty = local_name_type(&mut ws, file_id, "before");
        assert_eq!(ws.humanize_type(before_ty), "any?");

        let uri = ws.virtual_url_generator.new_uri(file_name);
        ws.analysis
            .update_file_text_only(&uri, format!("{source}\n"));
        ws.analysis.reindex_files(vec![file_id]);

        let after_reindex_before_ty = local_name_type(&mut ws, file_id, "before");
        assert_eq!(ws.humanize_type(after_reindex_before_ty), "any?");

        let after_reindex_after_ty = local_name_type(&mut ws, file_id, "after");
        assert_that!(
            ws.check_type(&after_reindex_after_ty, &LuaType::String),
            eq(true)
        );
    }

    #[gtest]
    fn test_reindex_keeps_later_dynamic_assignment_out_of_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let consumer_path = "lua/glide/client/network.lua";
        let consumer_source = r#"
        ---@param value string
        local function use_string(value)
        end

        local data = Glide.ReadTable()
        local before = data.text
        use_string(data.text)
        data.text = "translated"
        local after = data.text
        "#;

        let consumer_file = ws.def_file(consumer_path, consumer_source);

        ws.def_file(
            "lua/glide/sh_network.lua",
            r#"
            Glide = Glide or {}

            ---@return any
            function Glide.FromJSON()
            end

            function Glide.ReadTable()
                if net.ReadBool() then
                    return {}
                end

                return Glide.FromJSON()
            end
            "#,
        );

        let initial_before_ty = local_name_type(&mut ws, consumer_file, "before");
        assert_eq!(ws.humanize_type(initial_before_ty), "any");

        let uri = ws.virtual_url_generator.new_uri(consumer_path);
        ws.analysis
            .update_file_text_only(&uri, format!("{consumer_source}\n"));
        ws.analysis.reindex_files(vec![consumer_file]);

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::ParamTypeMismatch),
            eq(false),
            "reindexing only the consumer must not introduce a param mismatch for the earlier read"
        );

        let after_reindex_before_ty = local_name_type(&mut ws, consumer_file, "before");
        assert_eq!(ws.humanize_type(after_reindex_before_ty), "any");

        let after_reindex_after_ty = local_name_type(&mut ws, consumer_file, "after");
        assert_that!(
            ws.check_type(&after_reindex_after_ty, &LuaType::String),
            eq(true)
        );
    }

    #[gtest]
    fn test_missing_plain_table_field_is_nil() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local test = {}
        A = test.meow
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "nil");
    }

    #[gtest]
    fn test_unknown_dynamic_key_does_not_match_exact_field_lookup() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local test = {}

        local function assign(key)
            test[key] = true
            B = test[key]
        end

        A = test.meow
        "#,
        );

        let exact_lookup_ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(exact_lookup_ty), "nil");

        let dynamic_lookup_ty = ws.expr_ty("B");
        assert_that!(
            ws.check_type(&dynamic_lookup_ty, &LuaType::Boolean),
            eq(true)
        );
    }

    #[gtest]
    fn test_inferred_collection_integer_index_returns_element_union() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        local keys = {}

        for key in pairs({"a", "b", "c", "d"}) do
            keys[#keys + 1] = key
        end

        A = keys[1]
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = LuaType::Union(
            LuaUnionType::from_vec(vec![
                LuaType::IntegerConst(1),
                LuaType::IntegerConst(2),
                LuaType::IntegerConst(3),
                LuaType::IntegerConst(4),
            ])
            .into(),
        );
        assert_that!(ws.check_type(&ty, &expected), eq(true));
    }

    #[gtest]
    fn test_member_collection_append_preserves_prefix_flow_type() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
        local holder = {}
        holder.items = {}

        holder.items[#holder.items + 1] = "created"

        local after = holder.items
        "#,
        );

        let ty = local_name_type(&mut ws, file_id, "after");
        let expected = ws.ty("string[]");
        assert_that!(
            ws.check_type(&ty, &expected),
            eq(true),
            "expected holder.items to include appended string array, got {}",
            ws.humanize_type(ty)
        );
    }

    #[gtest]
    fn test_flow_fallback_prefers_latest_dynamic_field_assignment() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class TestEntity
        local TestEntity = {}

        ---@return TestEntity
        local function create_entity()
        end

        local ent = create_entity()
        ent.testVar = true
        ent.testVar = 42
        A = ent.testVar
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_that!(ws.check_type(&ty, &LuaType::Integer), eq(true));
    }

    #[gtest]
    fn test_class_field_reassignment_across_methods_is_optional_boolean() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class TestClass
        local TestClass = {}

        ---@type TestClass
        local obj

        function TestClass:MethodOne()
            self._testVar = true
        end

        function TestClass:MethodTwo()
            self._testVar = nil
        end

        A = obj._testVar
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "boolean?");
    }

    #[gtest]
    fn test_annotated_class_field_overrides_repeated_inferred_writes() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class TestClass
        ---@field _testVar integer
        local TestClass = {}

        ---@type TestClass
        local obj

        function TestClass:SetValue()
            self._testVar = 1
        end

        function TestClass:ResetWrong()
            self._testVar = nil
        end

        function TestClass:SetWrong()
            self._testVar = true
        end

        A = obj._testVar
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "integer");
    }

    #[gtest]
    fn test_assignment_side_dynamic_field_type_for_class_typed_variables() {
        let mut ws = VirtualWorkspace::new();

        let source = r#"
        ---@class Entity
        ---@class prop_physics: Entity
        ---@class Player
        ---@class Panel
        ---@class DPanel: Panel

        ---@generic T: Entity
        ---@param class `T`
        ---@return T
        local function ents_Create(class)
        end

        ---@generic T: Panel
        ---@param class `T`
        ---@return T
        local function vgui_Create(class)
        end

        ---@param idx integer
        ---@return Player
        local function Player_func(idx)
        end

        ---@class TEST
        local TEST = {}

        function TEST:Function()
            self.testVar = true

            if self.testVar then
                return
            end

            local tbl = {}
            tbl.testVar = true

            if tbl.testVar then
                return
            end

            local ent = ents_Create("prop_physics")
            ent.testVar = true

            if ent.testVar then
                return
            end

            local row = vgui_Create("DPanel")
            row.testVar = true

            if row.testVar then
                return
            end

            local ply = Player_func(1)
            ply.testVar = true

            if ply.testVar then
                return
            end
        end
        "#;

        assert_that!(
            ws.check_code_for(DiagnosticCode::UndefinedField, source),
            eq(true)
        );
        assert_that!(
            ws.check_code_for(DiagnosticCode::InjectField, source),
            eq(true)
        );

        let file_id = ws.def(source);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let mut assignment_types = Vec::new();
        for node in semantic_model.get_root().clone().descendants::<LuaAst>() {
            let LuaAst::LuaAssignStat(assign) = node else {
                continue;
            };

            let (vars, _) = assign.get_var_and_expr_list();
            for var in vars.iter() {
                let LuaVarExpr::IndexExpr(index_expr) = var else {
                    continue;
                };

                let Some(index_key) = index_expr.get_index_key() else {
                    continue;
                };
                let is_test_var = match index_key {
                    LuaIndexKey::Name(name) => name.get_name_text() == "testVar",
                    LuaIndexKey::String(str_token) => str_token.get_value() == "testVar",
                    _ => false,
                };
                if !is_test_var {
                    continue;
                }

                let semantic_info = semantic_model
                    .get_semantic_info(index_expr.syntax().clone().into())
                    .expect("expected semantic info for assignment field");
                assignment_types.push((index_expr.syntax().text().to_string(), semantic_info.typ));
            }
        }

        assert_eq!(assignment_types.len(), 5);
        for (assignment_expr, typ) in assignment_types {
            assert!(
                !typ.is_unknown(),
                "assignment `{assignment_expr}` inferred as unknown"
            );
            assert_that!(ws.check_type(&typ, &LuaType::Boolean), eq(true));
        }
    }

    #[test]
    fn test_class_annotated_local_alias_propagates_members_to_global_alias_in_server_consumer() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/glide/sh_fuel.lua",
            r#"
            Glide = Glide or {}
            Glide.Fuel = Glide.Fuel or {}

            ---@class Fuel
            local Fuel = Glide.Fuel

            function Fuel.GetProfile(id)
                return id
            end

            if SERVER then
                function Fuel.ServerOnly()
                    return true
                end
            end

            Glide.Fuel = Fuel
            "#,
        );

        let consumer_file = ws.def_file(
            "lua/entities/base_glide_car/init.lua",
            r#"
            local FuelModule = Glide.Fuel
            local getter = FuelModule.GetProfile
            "#,
        );

        let client_consumer_file = ws.def_file(
            "lua/entities/base_glide_car/cl_init.lua",
            r#"
            local FuelModule = Glide.Fuel
            local getter = FuelModule.GetProfile
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file)
            .expect("expected semantic model");

        let get_profile_type = semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr)
                    if index_expr.syntax().text() == "FuelModule.GetProfile" =>
                {
                    semantic_model
                        .get_semantic_info(index_expr.syntax().clone().into())
                        .map(|info| info.typ)
                }
                _ => None,
            })
            .next()
            .expect("expected semantic info for FuelModule.GetProfile");

        let fuel_module_type = semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaNameExpr(name_expr) if name_expr.syntax().text() == "FuelModule" => {
                    semantic_model
                        .get_semantic_info(name_expr.syntax().clone().into())
                        .map(|info| info.typ)
                }
                _ => None,
            })
            .next()
            .expect("expected semantic info for FuelModule");

        assert!(
            !fuel_module_type.is_unknown(),
            "FuelModule should not infer as unknown, got {fuel_module_type:?}"
        );
        let fuel_module_humanized = ws.humanize_type(fuel_module_type.clone());
        assert_that!(
            fuel_module_humanized.as_str(),
            not(contains_substring("table"))
        );

        assert!(
            !get_profile_type.is_unknown(),
            "FuelModule.GetProfile should not infer as unknown, got {get_profile_type:?}"
        );

        let client_semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(client_consumer_file)
            .expect("expected semantic model");
        let client_get_profile_type = client_semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr)
                    if index_expr.syntax().text() == "FuelModule.GetProfile" =>
                {
                    client_semantic_model
                        .get_semantic_info(index_expr.syntax().clone().into())
                        .map(|info| info.typ)
                }
                _ => None,
            })
            .next()
            .expect("expected semantic info for client FuelModule.GetProfile");
        assert!(
            !client_get_profile_type.is_unknown(),
            "client FuelModule.GetProfile should not infer as unknown, got {client_get_profile_type:?}"
        );
    }

    #[gtest]
    fn test_incremental_edit_in_server_fuel_file_keeps_global_alias_member_visible() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let sh_fuel_path = "lua/glide/sh_fuel.lua";
        let fuel_server_path = "lua/glide/server/fuel.lua";

        let sh_fuel_source = r#"
        Glide = Glide or {}

        ---@class Fuel
        local Fuel = Glide.Fuel or {}

        function Fuel.GetProfile(id)
            return id
        end

        Glide.Fuel = Fuel
        "#;

        let fuel_server_source = r#"
        Glide.Fuel = Glide.Fuel or {}

        --- @param nozzle Entity
        --- @param reason number?
        function Glide.Fuel.EndSessionByNozzle(nozzle, reason)
        end
        "#;

        ws.def_file(fuel_server_path, fuel_server_source);
        ws.def_file(sh_fuel_path, sh_fuel_source);

        let consumer_file = ws.def_file(
            "lua/entities/glide_fuel_nozzle/init.lua",
            r#"
            local fuelModule = Glide.Fuel
            local endSession = Glide.Fuel.EndSessionByNozzle
            "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::UndefinedField),
            eq(false),
            "baseline analysis should resolve Glide.Fuel.EndSessionByNozzle"
        );
        let baseline_fuel_module_type = local_name_type(&mut ws, consumer_file, "fuelModule");
        assert_that!(
            ws.humanize_type(baseline_fuel_module_type).as_str(),
            not(contains_substring("table"))
        );

        let fuel_server_uri = ws.virtual_url_generator.new_uri(fuel_server_path);
        ws.analysis
            .update_file_by_uri(&fuel_server_uri, Some(format!("{fuel_server_source}\n")));

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::UndefinedField),
            eq(false),
            "editing fuel.lua should not hide EndSessionByNozzle from Glide.Fuel"
        );
        let post_edit_fuel_module_type = local_name_type(&mut ws, consumer_file, "fuelModule");
        assert_that!(
            ws.humanize_type(post_edit_fuel_module_type).as_str(),
            not(contains_substring("table"))
        );
    }

    #[gtest]
    fn test_global_class_annotation_allows_cross_file_extension_without_reedit() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let sh_repair_path = "lua/glide/sh_repair.lua";
        let sv_repair_path = "lua/glide/server/repair.lua";

        let sh_repair_source = r#"
        Glide = Glide or {}

        --- @class Repair
        Glide.Repair = Glide.Repair or {}
        "#;

        let sv_repair_source = r#"
        Glide.Repair = Glide.Repair or {}

        --- @param nozzle Entity
        --- @param reason number?
        function Glide.Repair.EndSessionByNozzle(nozzle, reason)
        end
        "#;

        ws.def_file(sh_repair_path, sh_repair_source);
        ws.def_file(sv_repair_path, sv_repair_source);

        let consumer_file = ws.def_file(
            "lua/entities/glide_repair_tool/init.lua",
            r#"
            local repairModule = Glide.Repair
            local endSession = Glide.Repair.EndSessionByNozzle
            "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::UndefinedField),
            eq(false),
            "cross-file Repair extensions should be visible without editing repair.lua after startup"
        );
        let repair_module_type = local_name_type(&mut ws, consumer_file, "repairModule");
        assert_that!(
            ws.humanize_type(repair_module_type).as_str(),
            not(contains_substring("table"))
        );
    }

    #[gtest]
    fn test_global_class_annotation_cross_file_extension_survives_batch_startup_index() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let sh_repair_path = ws.virtual_url_generator.new_path("lua/glide/sh_repair.lua");
        let sv_repair_path = ws
            .virtual_url_generator
            .new_path("lua/glide/server/repair.lua");
        let consumer_path = ws
            .virtual_url_generator
            .new_path("lua/entities/glide_repair_tool/init.lua");

        let sh_repair_source = r#"
        Glide = Glide or {}

        --- @class Repair
        Glide.Repair = Glide.Repair or {}
        "#;

        let sv_repair_source = r#"
        Glide.Repair = Glide.Repair or {}

        --- @param nozzle Entity
        --- @param reason number?
        function Glide.Repair.EndSessionByNozzle(nozzle, reason)
        end
        "#;

        let consumer_source = r#"
            local repairModule = Glide.Repair
            local endSession = Glide.Repair.EndSessionByNozzle
            "#;

        ws.analysis.update_files_by_path(vec![
            (sh_repair_path.clone(), Some(sh_repair_source.to_string())),
            (sv_repair_path.clone(), Some(sv_repair_source.to_string())),
            (consumer_path.clone(), Some(consumer_source.to_string())),
        ]);

        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/entities/glide_repair_tool/init.lua");
        let consumer_file = ws
            .analysis
            .get_file_id(&consumer_uri)
            .expect("expected consumer file id");

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::UndefinedField),
            eq(false),
            "batch startup index should preserve cross-file class extension members"
        );
        let repair_module_type = local_name_type(&mut ws, consumer_file, "repairModule");
        assert_that!(
            ws.humanize_type(repair_module_type).as_str(),
            not(contains_substring("table"))
        );
    }

    #[gtest]
    fn test_global_class_annotation_keeps_server_local_alias_methods_visible() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let sh_repair_path = "lua/glide/sh_repair.lua";
        let sv_repair_path = "lua/glide/server/repair.lua";

        let sh_repair_source = r#"
        Glide = Glide or {}

        --- @class Repair
        Glide.Repair = Glide.Repair or {}
        "#;

        let sv_repair_source = r#"
        Glide.Repair = Glide.Repair or {}
        local Repair = Glide.Repair

        --- @param nozzle Entity
        --- @param reason number?
        function Repair.EndSessionByNozzle(nozzle, reason)
        end

        Glide.Repair = Repair
        "#;

        ws.def_file(sh_repair_path, sh_repair_source);
        ws.def_file(sv_repair_path, sv_repair_source);

        let consumer_file = ws.def_file(
            "lua/entities/glide_repair_tool/init.lua",
            r#"
            local repairModule = Glide.Repair
            local endSession = Glide.Repair.EndSessionByNozzle
            "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::UndefinedField),
            eq(false),
            "server local alias methods should remain visible on class-annotated Glide.Repair"
        );
        let repair_module_type = local_name_type(&mut ws, consumer_file, "repairModule");
        assert_that!(
            ws.humanize_type(repair_module_type).as_str(),
            not(contains_substring("table"))
        );
    }

    #[gtest]
    fn test_global_class_annotation_cross_file_extensions_are_stable_across_load_orders() {
        let sh_repair_path = "lua/glide/sh_repair.lua";
        let sv_repair_path = "lua/glide/server/repair.lua";
        let consumer_path = "lua/entities/glide_repair_tool/init.lua";

        let sh_repair_source = r#"
        Glide = Glide or {}

        --- @class Repair
        Glide.Repair = Glide.Repair or {}
        "#;

        let sv_repair_source = r#"
        Glide.Repair = Glide.Repair or {}

        --- @param nozzle Entity
        --- @param reason number?
        function Glide.Repair.EndSessionByNozzle(nozzle, reason)
        end
        "#;

        let consumer_source = r#"
            local repairModule = Glide.Repair
            local endSession = Glide.Repair.EndSessionByNozzle
            "#;

        let mut failures = Vec::new();

        for scenario in [
            "sequential_sh_then_sv",
            "sequential_sv_then_sh",
            "batch_startup",
            "batch_then_full_reindex",
        ] {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);

            let server_file_id = match scenario {
                "sequential_sh_then_sv" => {
                    ws.def_file(sh_repair_path, sh_repair_source);
                    ws.def_file(sv_repair_path, sv_repair_source)
                }
                "sequential_sv_then_sh" => {
                    let server_file_id = ws.def_file(sv_repair_path, sv_repair_source);
                    ws.def_file(sh_repair_path, sh_repair_source);
                    server_file_id
                }
                "batch_startup" | "batch_then_full_reindex" => {
                    let sh_path = ws.virtual_url_generator.new_path(sh_repair_path);
                    let sv_path = ws.virtual_url_generator.new_path(sv_repair_path);
                    let consumer_batch_path = ws.virtual_url_generator.new_path(consumer_path);
                    ws.analysis.update_files_by_path(vec![
                        (sh_path, Some(sh_repair_source.to_string())),
                        (sv_path, Some(sv_repair_source.to_string())),
                        (consumer_batch_path, Some(consumer_source.to_string())),
                    ]);

                    let server_uri = ws.virtual_url_generator.new_uri(sv_repair_path);
                    ws.analysis
                        .get_file_id(&server_uri)
                        .expect("expected server file id")
                }
                _ => unreachable!(),
            };

            let consumer_file_id =
                if matches!(scenario, "batch_startup" | "batch_then_full_reindex") {
                    let consumer_uri = ws.virtual_url_generator.new_uri(consumer_path);
                    ws.analysis
                        .get_file_id(&consumer_uri)
                        .expect("expected consumer file id")
                } else {
                    ws.def_file(consumer_path, consumer_source)
                };

            if scenario == "batch_then_full_reindex" {
                ws.analysis.reindex();
            }

            let method_type =
                index_expr_type(&mut ws, server_file_id, "Glide.Repair.EndSessionByNozzle");
            if method_type.is_unknown() {
                failures.push(format!(
                    "{scenario}: server definition type resolved as unknown"
                ));
            }

            if file_has_diagnostic(&mut ws, consumer_file_id, DiagnosticCode::UndefinedField) {
                failures.push(format!(
                    "{scenario}: consumer has UndefinedField for Glide.Repair.EndSessionByNozzle"
                ));
            }
        }

        assert_that!(
            failures,
            is_empty(),
            "cross-file Repair extension should be stable across load orders: {failures:?}"
        );
    }

    #[test]
    fn test_gmod_string_numeric_index() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
        local str = "XX"
        local var = str[2]
        "#,
        );

        let index_ty = index_expr_type(&mut ws, file_id, "str[2]");
        assert_eq!(
            index_ty,
            LuaType::String,
            "str[2] index expression should be string, got {:?}",
            index_ty
        );

        let var_ty = local_name_type(&mut ws, file_id, "var");
        assert_eq!(
            var_ty,
            LuaType::String,
            "local var assigned from str[2] should be string, got {:?}",
            var_ty
        );
    }

    #[test]
    fn test_table_expr_key_string() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        local key = tostring(1)
        local t = { [key] = 1 }
        value = t[key]
        "#,
        );

        let value_ty = ws.expr_ty("value");
        assert!(
            matches!(value_ty, LuaType::Integer | LuaType::IntegerConst(_)),
            "expected integer type, got {:?}",
            value_ty
        );
    }

    #[test]
    fn test_table_expr_key_doc_const() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        ---@type 'field'
        local key = "field"
        local t = { [key] = 1 }
        value = t[key]
        "#,
        );

        let value_ty = ws.expr_ty("value");
        assert!(
            matches!(value_ty, LuaType::Integer | LuaType::IntegerConst(_)),
            "expected integer type, got {:?}",
            value_ty
        );
    }

    #[gtest]
    fn test_bootstrap_table_literal_preserves_methods() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
local var = Glide.TestVar or {}
function var:TestMethod() end
Glide.TestVar = var

var:TestMethod()
Glide.TestVar:TestMethod()
"#,
        );

        let has_diag = file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedField);
        assert_that!(has_diag, eq(false));
    }

    #[gtest]
    fn test_bootstrap_table_cross_file() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
local var = Glide.TestVar or {}
function var:TestMethod() end
Glide.TestVar = var
"#,
        );
        let file_id2 = ws.def(
            r#"
Glide.TestVar:TestMethod()
"#,
        );

        let has_diag = file_has_diagnostic(&mut ws, file_id2, DiagnosticCode::UndefinedField);
        assert_that!(has_diag, eq(false));
    }

    #[gtest]
    fn test_guarded_global_field_table_persists_methods_across_files() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_a = ws.def_file(
            "file_a.lua",
            r#"
Glide = Glide or {}
---@class GlideEditor
local Editor = Glide.VehicleLayoutEditor or {}
function Editor:TestMethod() end
Glide.VehicleLayoutEditor = Editor
"#,
        );
        let file_b = ws.def_file(
            "file_b.lua",
            r#"
local Editor = Glide.VehicleLayoutEditor
Editor:TestMethod()
"#,
        );
        let file_c = ws.def_file(
            "file_c.lua",
            r#"
local Editor = Glide.VehicleLayoutEditor
Editor:MissingMethod()
"#,
        );

        let editor_type = local_name_type(&mut ws, file_b, "Editor");
        assert_that!(
            editor_type.is_unknown(),
            eq(false),
            "Editor should not be unknown. Actually: {:?}",
            editor_type
        );

        let has_undef = file_has_diagnostic(&mut ws, file_b, DiagnosticCode::UndefinedField);
        assert_that!(
            has_undef,
            eq(false),
            "Method defined through guarded local alias should persist across files"
        );

        let has_need_check_nil = file_has_diagnostic(&mut ws, file_b, DiagnosticCode::NeedCheckNil);
        assert_that!(
            has_need_check_nil,
            eq(false),
            "Method call should not require a nil check if correctly inferred"
        );

        let has_undef_missing =
            file_has_diagnostic(&mut ws, file_c, DiagnosticCode::UndefinedField);
        assert_that!(
            has_undef_missing,
            eq(true),
            "Method missing from Editor should trigger UndefinedField"
        );

        let editor_type_a = local_name_type(&mut ws, file_a, "Editor");
        assert_that!(
            &editor_type,
            eq(&editor_type_a),
            "Editor in file_b should have the same concrete type as Editor in file_a"
        );
    }

    #[gtest]
    fn test_global_self_assignment_preserves_existing_table_shape() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "marauth/gamemode/src/boot/boot.lua",
            r#"
marauth = {}
marauth.util = {}
"#,
        );
        let file_id = ws.def_file(
            "marauth-hl2rp/gamemode/sh_init.lua",
            r#"
marauth = marauth

local observed = marauth
local util = marauth.util
"#,
        );

        let observed_type = local_name_type(&mut ws, file_id, "observed");
        assert_that!(
            observed_type.is_unknown(),
            eq(false),
            "self-assignment should not make marauth unknown"
        );

        let util_type = local_name_type(&mut ws, file_id, "util");
        assert_that!(
            util_type.is_unknown() || util_type.is_nil(),
            eq(false),
            "self-assignment should not erase the known marauth.util table"
        );
    }

    #[gtest]
    fn test_global_self_assignment_does_not_shadow_lower_priority_table() {
        let mut ws = VirtualWorkspace::new();

        let library_root = ws.virtual_url_generator.new_path("__test_library_marauth");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri = Uri::parse_from_file_path(&library_root.join("marauth.lua"))
            .expect("valid library uri");
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
marauth = {}
marauth.util = {}
"#
                .to_string(),
            ),
        );

        let file_id = ws.def(
            r#"
marauth = marauth

local observed = marauth
local util = marauth.util
"#,
        );

        let observed_type = local_name_type(&mut ws, file_id, "observed");
        assert_that!(
            observed_type.is_table(),
            eq(true),
            "self-assignment should not shadow a visible lower-priority table with unknown"
        );

        let util_type = local_name_type(&mut ws, file_id, "util");
        assert_that!(
            util_type.is_table(),
            eq(true),
            "self-assignment should preserve fields from the visible lower-priority table"
        );
    }

    #[gtest]
    fn test_plain_global_table_prefers_global_path_member_over_nullable_any() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
---@type table
marauth = {}

marauth.util = {}

function marauth.util:TestFunction()
end

local util = marauth.util
local testFunction = marauth.util.TestFunction
"#,
        );

        let util_type = local_name_type(&mut ws, file_id, "util");
        assert_that!(
            matches!(util_type, LuaType::TableConst(_) | LuaType::MergedTable(_)),
            eq(true),
            "plain global table should resolve the concrete global-path util member, got {util_type:?}"
        );

        let test_function_type = local_name_type(&mut ws, file_id, "testFunction");
        assert_that!(
            test_function_type.is_function(),
            eq(true),
            "plain global table should not turn nested global-path methods into nullable any, got {test_function_type:?}"
        );
    }

    #[gtest]
    fn test_bare_table_fragment_does_not_pollute_merged_global_member() {
        let mut ws = VirtualWorkspace::new();

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_marauth_bare_table");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            Uri::parse_from_file_path(&library_root.join("marauth.lua")).expect("valid uri");
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
marauth = {}
marauth.util = {}

function marauth.util:TestFunction()
end
"#
                .to_string(),
            ),
        );

        let file_id = ws.def(
            r#"
---@type table
marauth = marauth or {}

local testFunction = marauth.util.TestFunction
"#,
        );

        let test_function_type = local_name_type(&mut ws, file_id, "testFunction");
        assert_that!(
            test_function_type.is_function(),
            eq(true),
            "bare table fragments in a merged global should not pollute known members with nullable any, got {test_function_type:?}"
        );
    }

    #[gtest]
    fn test_resolved_current_workspace_globals_do_not_fall_back_to_lower_priority_table() {
        let mut ws = VirtualWorkspace::new();

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_shadowed_global");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri = Uri::parse_from_file_path(&library_root.join("shadowed.lua"))
            .expect("valid library uri");
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
shadowed = {}
shadowed.util = {}
"#
                .to_string(),
            ),
        );

        let file_id = ws.def(
            r#"
shadowed = 1
shadowed = 2

local value = shadowed
"#,
        );

        let value_type = local_name_type(&mut ws, file_id, "value");
        assert_that!(
            matches!(value_type, LuaType::Integer | LuaType::IntegerConst(_)),
            eq(true),
            "a resolved current-workspace primitive global must remain a primitive, got {:?}",
            value_type
        );
    }

    #[gtest]
    fn test_guarded_global_assignment_keeps_lower_priority_fields_without_field_guard() {
        let mut ws = VirtualWorkspace::new();

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_marauth_guard");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri = Uri::parse_from_file_path(&library_root.join("marauth.lua"))
            .expect("valid library uri");
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
marauth = {}
marauth.util = {}

function marauth.util:TestFunction()
end
"#
                .to_string(),
            ),
        );

        let file_id = ws.def(
            r#"
marauth = marauth or {}

local util = marauth.util
local testFunction = marauth.util.TestFunction
"#,
        );

        let util_type = local_name_type(&mut ws, file_id, "util");
        assert_that!(
            util_type.is_table(),
            eq(true),
            "root guard should not require repeating `marauth.util = marauth.util or {{}}` in the child file"
        );

        let function_type = local_name_type(&mut ws, file_id, "testFunction");
        assert_that!(
            function_type.is_function(),
            eq(true),
            "fields from the lower-priority table should remain visible through the guarded root"
        );
    }

    #[gtest]
    fn test_same_priority_guarded_global_tables_merge_fields_independent_of_index_order() {
        let mut ws = VirtualWorkspace::new();

        let child_file = ws.def_file(
            "gamemodes/test/gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}

marauth.util:TestFunction()
local util = marauth.util
local testFunction = marauth.util.TestFunction
"#,
        );
        ws.def_file(
            "gamemodes/test_base/gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}
marauth.util = marauth.util or {}

function marauth.util:TestFunction()
end
"#,
        );

        let util_type = local_name_type(&mut ws, child_file, "util");
        assert_that!(
            util_type.is_table(),
            eq(true),
            "same-priority guarded table pieces should merge instead of depending on file order"
        );

        let function_type = local_name_type(&mut ws, child_file, "testFunction");
        assert_that!(
            function_type.is_function(),
            eq(true),
            "same-priority guarded table merge should preserve method fields"
        );

        assert_that!(
            file_has_diagnostic(&mut ws, child_file, DiagnosticCode::NeedCheckNil),
            eq(false),
            "same-priority guarded table merge should not make known fields nullable"
        );
    }

    #[gtest]
    fn test_cross_workspace_guarded_global_tables_merge_fields_with_isolation_disabled() {
        let mut ws = VirtualWorkspace::new();

        let base_root = ws
            .virtual_url_generator
            .new_path("__test_base_gamemode_root");
        ws.analysis.add_main_workspace(base_root.clone());
        let base_uri = Uri::parse_from_file_path(&base_root.join("gamemode/sh_test1.lua"))
            .expect("valid base uri");
        ws.analysis.update_file_by_uri(
            &base_uri,
            Some(
                r#"
marauth = marauth or {}
marauth.util = marauth.util or {}

function marauth.util:TestFunction()
end
"#
                .to_string(),
            ),
        );

        let child_file = ws.def_file(
            "gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}

marauth.util:TestFunction()
local util = marauth.util
local testFunction = marauth.util.TestFunction
"#,
        );

        let util_type = local_name_type(&mut ws, child_file, "util");
        assert_that!(
            util_type.is_table(),
            eq(true),
            "guarded table fields from another visible main workspace should stay non-nil, got {:?}",
            util_type
        );

        let function_type = local_name_type(&mut ws, child_file, "testFunction");
        assert_that!(
            function_type.is_function(),
            eq(true),
            "guarded table methods from another visible main workspace should stay visible, got {:?}",
            function_type
        );

        assert_that!(
            file_has_diagnostic(&mut ws, child_file, DiagnosticCode::NeedCheckNil),
            eq(false),
            "visible guarded table fields should not require nil checks"
        );
    }

    #[gtest]
    fn test_cross_workspace_guarded_global_tables_merge_disjoint_and_nested_fields() {
        let mut ws = VirtualWorkspace::new();

        let base_root = ws
            .virtual_url_generator
            .new_path("__test_base_gamemode_merge_root");
        ws.analysis.add_main_workspace(base_root.clone());
        let base_uri = Uri::parse_from_file_path(&base_root.join("gamemode/sh_test1.lua"))
            .expect("valid base uri");
        ws.analysis.update_file_by_uri(
            &base_uri,
            Some(
                r#"
marauth = marauth or {}
marauth.util = marauth.util or {}

function marauth.util:BaseFunction()
end
"#
                .to_string(),
            ),
        );

        let child_file = ws.def_file(
            "gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}
marauth.character = marauth.character or {}
marauth.util = marauth.util or {}

function marauth.character:Create()
end

function marauth.util:ChildFunction()
end

local util = marauth.util
local baseFunction = marauth.util.BaseFunction
local childFunction = marauth.util.ChildFunction
local character = marauth.character
local create = marauth.character.Create
"#,
        );

        let util_type = local_name_type(&mut ws, child_file, "util");
        assert_that!(
            util_type.is_table(),
            eq(true),
            "same global table field contributions should merge into a definite table"
        );

        let base_function_type = local_name_type(&mut ws, child_file, "baseFunction");
        assert_that!(
            base_function_type.is_function(),
            eq(true),
            "nested table merge should preserve base workspace methods"
        );

        let child_function_type = local_name_type(&mut ws, child_file, "childFunction");
        assert_that!(
            child_function_type.is_function(),
            eq(true),
            "nested table merge should preserve child workspace methods"
        );

        let character_type = local_name_type(&mut ws, child_file, "character");
        assert_that!(
            character_type.is_table(),
            eq(true),
            "disjoint root fields should remain visible through the merged global table"
        );

        let create_type = local_name_type(&mut ws, child_file, "create");
        assert_that!(
            create_type.is_function(),
            eq(true),
            "disjoint nested methods should remain visible through the merged global table"
        );
    }

    #[gtest]
    fn test_three_file_nested_guarded_global_table_chain_keeps_all_methods() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "gamemodes/test_base/gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}
marauth.util = marauth.util or {}

function marauth.util:BaseFunction()
end
"#,
        );
        ws.def_file(
            "gamemodes/test/gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}
marauth.util = marauth.util or {}

function marauth.util:FirstChildFunction()
end
"#,
        );
        let consumer_file = ws.def_file(
            "gamemodes/test/gamemode/sh_test2.lua",
            r#"
marauth = marauth or {}
marauth.util = marauth.util or {}

function marauth.util:SecondChildFunction()
end

local util = marauth.util
local baseFunction = marauth.util.BaseFunction
local firstChildFunction = marauth.util.FirstChildFunction
local secondChildFunction = marauth.util.SecondChildFunction
"#,
        );

        let util_type = local_name_type(&mut ws, consumer_file, "util");
        assert_that!(
            util_type.is_table(),
            eq(true),
            "nested guarded table chain should remain a definite table, got {:?}",
            util_type
        );

        for name in ["baseFunction", "firstChildFunction", "secondChildFunction"] {
            let function_type = local_name_type(&mut ws, consumer_file, name);
            assert_that!(
                function_type.is_function(),
                eq(true),
                "nested guarded table chain should preserve {name}, got {:?}",
                function_type
            );
        }

        assert_that!(
            file_has_diagnostic(&mut ws, consumer_file, DiagnosticCode::NeedCheckNil),
            eq(false),
            "nested guarded table chain should not make known methods nullable"
        );
    }

    #[gtest]
    fn test_guarded_global_child_field_owner_prefers_current_file_table() {
        let mut ws = VirtualWorkspace::new();

        let base_root = ws
            .virtual_url_generator
            .new_path("__test_base_gamemode_owner_root");
        ws.analysis.add_main_workspace(base_root.clone());
        let base_uri = Uri::parse_from_file_path(&base_root.join("gamemode/sh_test1.lua"))
            .expect("valid base uri");
        ws.analysis.update_file_by_uri(
            &base_uri,
            Some(
                r#"
marauth = marauth or {}
marauth.util = marauth.util or {}
"#
                .to_string(),
            ),
        );

        let child_file = ws.def_file(
            "gamemode/sh_test1.lua",
            r#"
marauth = marauth or {}
marauth.character = marauth.character or {}
"#,
        );

        let owner = first_index_expr_member_owner(&ws, child_file, "marauth.character");
        let LuaMemberOwner::Element(owner_range) = owner else {
            panic!("expected marauth.character to be owned by a table element, got {owner:?}");
        };
        assert_that!(
            owner_range.file_id,
            eq(child_file),
            "child guarded field assignment should attach to the current file's table owner"
        );
    }
}
