#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use smol_str::SmolStr;

    use crate::{LuaType, LuaUnionType, VirtualWorkspace};

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
}
