#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_string() {
        let mut ws = VirtualWorkspace::new();

        let string_ty = ws.ty("string");

        let right_ty = ws.ty("'ssss'");
        assert!(ws.check_type(&string_ty, &right_ty));

        let right_ty = ws.ty("number");
        assert!(!ws.check_type(&string_ty, &right_ty));

        let right_ty = ws.ty("string | number");
        assert!(!ws.check_type(&string_ty, &right_ty));

        let right_ty = ws.ty("'a' | 'b' | 'c'");
        assert!(ws.check_type(&string_ty, &right_ty));
    }

    #[test]
    fn test_number_types() {
        let mut ws = VirtualWorkspace::new();

        let number_ty = ws.ty("number");
        let integer_ty = ws.ty("integer");

        let number_expr1 = ws.expr_ty("1");
        assert!(ws.check_type(&number_ty, &number_expr1));
        let number_expr2 = ws.expr_ty("1.5");
        assert!(ws.check_type(&number_ty, &number_expr2));

        assert!(ws.check_type(&number_ty, &integer_ty));
        assert!(ws.check_type(&integer_ty, &number_ty));

        let number_union = ws.ty("1 | 2 | 3");
        assert!(ws.check_type(&number_ty, &number_union));
        assert!(ws.check_type(&integer_ty, &number_union));
    }

    #[test]
    fn test_union_types() {
        let mut ws = VirtualWorkspace::new();

        let ty_union = ws.ty("number | string");
        let ty_number = ws.ty("number");
        let ty_string = ws.ty("string");
        let ty_boolean = ws.ty("boolean");

        assert!(ws.check_type(&ty_union, &ty_number));
        assert!(ws.check_type(&ty_union, &ty_string));
        assert!(!ws.check_type(&ty_union, &ty_boolean));
        assert!(ws.check_type(&ty_union, &ty_union));

        let ty_union2 = ws.ty("number | string | boolean");
        assert!(ws.check_type(&ty_union2, &ty_number));
        assert!(ws.check_type(&ty_union2, &ty_string));
        assert!(ws.check_type(&ty_union2, &ty_union));
        assert!(ws.check_type(&ty_union2, &ty_union2));

        let ty_union3 = ws.ty("1 | 2 | 3");
        let ty_union4 = ws.ty("1 | 2");

        assert!(ws.check_type(&ty_union3, &ty_union4));
        assert!(!ws.check_type(&ty_union4, &ty_union3));
        assert!(ws.check_type(&ty_union3, &ty_union3));
    }

    #[test]
    fn test_object_types() {
        let mut ws = VirtualWorkspace::new();

        // case 1
        {
            let object_ty = ws.ty("{ x: number, y: string }");
            let matched_object_ty2 = ws.ty("{ x: 1, y: 'test' }");
            let mismatch_object_ty2 = ws.ty("{ x: 2, y: 3 }");
            let matched_table_ty = ws.expr_ty("{ x = 1, y = 'test' }");
            let mismatch_table_ty = ws.expr_ty("{ x = 2, y = 3 }");

            assert!(ws.check_type(&object_ty, &matched_object_ty2));
            assert!(!ws.check_type(&object_ty, &mismatch_object_ty2));
            assert!(ws.check_type(&object_ty, &matched_table_ty));
            assert!(!ws.check_type(&object_ty, &mismatch_table_ty));
        }

        // case for tuple, object, and table
        {
            let object_ty = ws.ty("{ [1]: string, [2]: number }");
            let matched_tulple_ty = ws.ty("[string, number");
            let matched_object_ty = ws.ty("{ [1]: 'test', [2]: 1 }");

            assert!(ws.check_type(&object_ty, &matched_tulple_ty));
            assert!(ws.check_type(&object_ty, &matched_object_ty));
            let mismatch_tulple_ty = ws.ty("[number, string]");
            assert!(!ws.check_type(&object_ty, &mismatch_tulple_ty));

            let matched_table_ty = ws.expr_ty("{ [1] = 'test', [2] = 1 }");
            assert!(ws.check_type(&object_ty, &matched_table_ty));
        }

        // issue #69
        {
            let object_ty = ws.ty("{ [1]: number, [2]: integer }?");

            assert!(ws.check_type(&object_ty, &object_ty));
        }
    }

    #[test]
    fn test_bare_table_does_not_satisfy_required_structural_members() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class Empty

        ---@class (exact) Person
        ---@field name string
        "#,
        );

        let table_ty = ws.ty("table");

        let empty_ty = ws.ty("Empty");
        assert!(ws.check_type(&empty_ty, &table_ty));

        let person_ty = ws.ty("Person");
        assert!(!ws.check_type(&person_ty, &table_ty));

        let object_ty = ws.ty("{ name: string }");
        assert!(!ws.check_type(&object_ty, &table_ty));

        let intersection_ty = ws.ty("{ name: string } & { age: integer }");
        assert!(!ws.check_type(&intersection_ty, &table_ty));
    }

    #[test]
    fn test_fresh_table_literal_reports_excess_structural_members() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class (exact) Person
        ---@field name string
        "#,
        );

        let named_object_ty = ws.ty("{ name: string }");
        let widened_extra_object_ty = ws.ty("{ name: string, age: integer }");
        assert!(ws.check_type(&named_object_ty, &widened_extra_object_ty));

        let extra_literal_ty = ws.expr_ty("{ name = 'Ada', age = 1 }");
        assert!(!ws.check_type(&named_object_ty, &extra_literal_ty));

        let empty_object_ty = ws.ty("{}");
        assert!(ws.check_type(&empty_object_ty, &extra_literal_ty));

        let person_ty = ws.ty("Person");
        assert!(!ws.check_type(&person_ty, &extra_literal_ty));
    }

    #[test]
    fn test_fresh_table_literal_excess_uses_union_target_properties() {
        let mut ws = VirtualWorkspace::new();

        let union_ty = ws.ty("{ a: integer } | { b: integer }");
        let both_literal_ty = ws.expr_ty("{ a = 1, b = 2 }");
        assert!(ws.check_type(&union_ty, &both_literal_ty));

        let extra_literal_ty = ws.expr_ty("{ a = 1, c = 3 }");
        assert!(!ws.check_type(&union_ty, &extra_literal_ty));

        let discriminated_union_ty = ws.ty("{ kind: 'a', a: integer } | { kind: 'b', b: integer }");
        let matched_discriminant_literal_ty = ws.expr_ty("{ kind = 'a', a = 1, b = 2 }");
        assert!(!ws.check_type(&discriminated_union_ty, &matched_discriminant_literal_ty));
    }

    #[test]
    fn test_array_types() {
        let mut ws = VirtualWorkspace::new();

        let array_ty = ws.ty("number[]");
        let matched_tuple_ty = ws.ty("[1, 2, 3]");
        let mismatch_array_ty = ws.ty("['a', 'b', 'c']");

        assert!(ws.check_type(&array_ty, &matched_tuple_ty));
        assert!(!ws.check_type(&array_ty, &mismatch_array_ty));

        let array_ty2 = ws.ty("integer[]");
        assert!(ws.check_type(&array_ty, &array_ty2));
        assert!(ws.check_type(&array_ty2, &array_ty));
    }

    #[test]
    fn test_tuple_types() {
        let mut ws = VirtualWorkspace::new();

        let tuple_ty = ws.ty("[number, string]");
        let matched_tuple_ty = ws.ty("[1, 'test']");
        let mismatch_tuple_ty = ws.ty("['a', 1]");

        assert!(ws.check_type(&tuple_ty, &matched_tuple_ty));
        assert!(!ws.check_type(&tuple_ty, &mismatch_tuple_ty));

        let tuple_ty2 = ws.ty("[integer, string]");
        assert!(ws.check_type(&tuple_ty, &tuple_ty2));
        assert!(ws.check_type(&tuple_ty2, &tuple_ty));
    }

    #[test]
    fn test_issue_86() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let ty = ws.ty("string?");
        let ty2 = ws.expr_ty("(\"hello\"):match(\".*\")");
        assert!(ws.check_type(&ty, &ty2));
    }

    #[test]
    fn test_issue_634() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            --- @class A
            --- @field a integer

            --- @param x table<integer,string>
            local function foo(x) end

            local y --- @type A
            foo(y) -- should error
        "#
        ));
    }

    #[test]
    fn test_nominal_subtyping_is_directional() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class Base

        ---@class Derived: Base

        ---@param value Base
        function takes_base(value) end

        ---@param value Derived
        function takes_derived(value) end
        "#,
        );

        let base = ws.ty("Base");
        let derived = ws.ty("Derived");
        assert!(ws.check_type(&base, &derived));
        assert!(!ws.check_type(&derived, &base));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Derived
            local derived
            takes_base(derived)
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Base
            local base
            takes_derived(base)
        "#
        ));
    }

    #[test]
    fn test_primitive_subtyping_is_directional() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class UserId: string

        ---@class UnitKey: integer
        "#,
        );

        let string_ty = ws.ty("string");
        let user_id_ty = ws.ty("UserId");
        assert!(ws.check_type(&string_ty, &user_id_ty));
        assert!(!ws.check_type(&user_id_ty, &string_ty));

        let integer_ty = ws.ty("integer");
        let unit_key_ty = ws.ty("UnitKey");
        assert!(ws.check_type(&integer_ty, &unit_key_ty));
        assert!(!ws.check_type(&unit_key_ty, &integer_ty));
    }

    #[test]
    fn test_issue_790() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class Holder<T>

        ---@class StringHolder: Holder<string>

        ---@class NumberHolder: Holder<number>

        ---@class StringHolderWith<T>: Holder<string>

        ---@generic T
        ---@param a T
        ---@param b T
        function test(a, b) end
        "#,
        );

        let holder_string = ws.ty("Holder<string>");
        let string_holder_with_table = ws.ty("StringHolderWith<table>");
        let number_holder = ws.ty("NumberHolder");
        assert!(ws.check_type(&holder_string, &string_holder_with_table));
        assert!(!ws.check_type(&string_holder_with_table, &holder_string));
        assert!(!ws.check_type(&holder_string, &number_holder));
        assert!(!ws.check_type(&number_holder, &holder_string));

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Holder<string>, NumberHolder
            local a, b
            test(a, b)
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Holder<string>, StringHolderWith<table>
            local a, b
            test(a, b)
        "#
        ));
    }
}
