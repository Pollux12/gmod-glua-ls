#[cfg(test)]
mod test {
    use glua_parser::{LuaAst, LuaAstNode, LuaIndexKey, LuaVarExpr};
    use googletest::prelude::*;
    use smol_str::SmolStr;

    use crate::{DiagnosticCode, Emmyrc, LuaType, LuaUnionType, VirtualWorkspace};

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
            .clone()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr)
                    if index_expr.syntax().text().to_string() == "FuelModule.GetProfile" =>
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
            .clone()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaNameExpr(name_expr)
                    if name_expr.syntax().text().to_string() == "FuelModule" =>
                {
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
            .clone()
            .descendants::<LuaAst>()
            .filter_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr)
                    if index_expr.syntax().text().to_string() == "FuelModule.GetProfile" =>
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
}
