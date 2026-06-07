#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use crate::{
        DiagnosticCode, LuaMemberKey, LuaMergedTableType, LuaObjectType, LuaType, VirtualWorkspace,
    };

    use super::super::check_type_compact;

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
    fn test_merged_table_types_check_structurally() {
        let db = crate::DbIndex::new();
        let source = merged_table_with_field("name", LuaType::String);
        let compact = merged_table_with_field("name", LuaType::String);

        assert!(check_type_compact(&db, &source, &compact).is_ok());
    }

    #[test]
    fn test_merged_table_type_check_preserves_index_access() {
        let db = crate::DbIndex::new();
        let source = merged_table_with_field_and_index_access(
            "name",
            LuaType::String,
            LuaType::String,
            LuaType::Number,
        );
        let compact = merged_table_with_field_and_index_access(
            "name",
            LuaType::String,
            LuaType::String,
            LuaType::String,
        );

        assert!(check_type_compact(&db, &source, &compact).is_err());
    }

    #[test]
    fn test_object_type_index_access_accepts_matching_explicit_fields() {
        let db = crate::DbIndex::new();
        let source = object_with_index_access(LuaType::String, LuaType::String);
        let compact = object_with_field("name", LuaType::String);

        assert!(check_type_compact(&db, &source, &compact).is_ok());
    }

    #[test]
    fn test_object_type_index_access_rejects_mismatched_explicit_fields() {
        let db = crate::DbIndex::new();
        let source = object_with_index_access(LuaType::String, LuaType::String);
        let compact = object_with_field_and_index_access(
            "count",
            LuaType::Number,
            LuaType::String,
            LuaType::String,
        );

        assert!(check_type_compact(&db, &source, &compact).is_err());
    }

    #[test]
    fn test_object_type_literal_index_access_ignores_non_matching_explicit_fields() {
        let db = crate::DbIndex::new();
        let source = object_with_index_access(
            LuaType::StringConst(smol_str::SmolStr::new("name").into()),
            LuaType::String,
        );
        let compact = object_with_field("count", LuaType::Number);

        assert!(check_type_compact(&db, &source, &compact).is_ok());
    }

    #[test]
    fn test_object_type_index_access_checks_all_matching_index_signatures() {
        let db = crate::DbIndex::new();
        let source = object_with_index_access(LuaType::String, LuaType::String);
        let compact = object_with_index_accesses(vec![
            (LuaType::String, LuaType::String),
            (LuaType::String, LuaType::Number),
        ]);

        assert!(check_type_compact(&db, &source, &compact).is_err());
    }

    #[test]
    fn test_pure_index_access_merged_tables_check_structurally() {
        let db = crate::DbIndex::new();
        let source = merged_table_with_index_access(LuaType::String, LuaType::String);
        let compact = merged_table_with_index_access(LuaType::String, LuaType::String);

        assert!(check_type_compact(&db, &source, &compact).is_ok());
    }

    #[test]
    fn test_table_generic_accepts_merged_table_argument() {
        let db = crate::DbIndex::new();
        let source = LuaType::TableGeneric(vec![LuaType::Any, LuaType::Any].into());
        let compact = LuaMergedTableType::new(vec![LuaType::Table]).into();

        assert!(check_type_compact(&db, &source, &compact).is_ok());
    }

    fn merged_table_with_field(name: &str, typ: LuaType) -> LuaType {
        merged_table_from_object(object_with_field(name, typ))
    }

    fn merged_table_with_index_access(
        index_key_type: LuaType,
        index_value_type: LuaType,
    ) -> LuaType {
        merged_table_from_object(object_with_index_access(index_key_type, index_value_type))
    }

    fn merged_table_from_object(object: LuaType) -> LuaType {
        LuaMergedTableType::new(vec![object]).into()
    }

    fn object_with_field(name: &str, typ: LuaType) -> LuaType {
        let mut fields = HashMap::new();
        fields.insert(LuaMemberKey::Name(name.into()), typ);
        LuaObjectType::new_with_fields(fields, Vec::new()).into()
    }

    fn object_with_index_access(index_key_type: LuaType, index_value_type: LuaType) -> LuaType {
        object_with_index_accesses(vec![(index_key_type, index_value_type)])
    }

    fn object_with_index_accesses(index_access: Vec<(LuaType, LuaType)>) -> LuaType {
        LuaObjectType::new_with_fields(HashMap::new(), index_access).into()
    }

    fn merged_table_with_field_and_index_access(
        name: &str,
        field_type: LuaType,
        index_key_type: LuaType,
        index_value_type: LuaType,
    ) -> LuaType {
        merged_table_from_object(object_with_field_and_index_access(
            name,
            field_type,
            index_key_type,
            index_value_type,
        ))
    }

    fn object_with_field_and_index_access(
        name: &str,
        field_type: LuaType,
        index_key_type: LuaType,
        index_value_type: LuaType,
    ) -> LuaType {
        let mut fields = HashMap::new();
        fields.insert(LuaMemberKey::Name(name.into()), field_type);
        LuaObjectType::new_with_fields(fields, vec![(index_key_type, index_value_type)]).into()
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

    /// Entity should be accepted where a child type (base_glide) is expected in GMod.
    #[test]
    fn test_entity_to_child_subtype() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            ---@class base_glide : Entity
            "#,
        );

        let entity_ty = ws.ty("Entity");
        let base_glide_ty = ws.ty("base_glide");

        // base_glide can be used where Entity is expected (Liskov).
        // check_type(source=Entity, compact=base_glide) → YES
        assert!(ws.check_type(&entity_ty, &base_glide_ty));

        // Entity can be used where base_glide is expected in GMod mode.
        // check_type(source=base_glide, compact=Entity) → YES (GMod pragmatic)
        assert!(ws.check_type(&base_glide_ty, &entity_ty));
    }

    /// Without GMod mode, supertype → subtype should still be rejected.
    #[test]
    fn test_supertype_to_subtype_rejected_without_gmod() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class A
            ---@class B : A
            "#,
        );

        let a_ty = ws.ty("A");
        let b_ty = ws.ty("B");

        // B can be used where A is expected (Liskov: B is subtype of A).
        // check_type(source=A, compact=B) = "can B be used where A is expected?" → YES
        assert!(ws.check_type(&a_ty, &b_ty));

        // A cannot be used where B is expected without GMod (strict OOP).
        // check_type(source=B, compact=A) = "can A be used where B is expected?" → NO
        assert!(!ws.check_type(&b_ty, &a_ty));
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

    #[test]
    fn test_defaulted_class_field_is_not_required_for_table_compatibility() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class TraceLike
            ---@field Hit boolean=false
            ---@field HitPos number
            "#,
        );

        let target_ty = ws.ty("TraceLike");
        let table_ty = ws.expr_ty("{ HitPos = 1 }");

        assert!(ws.check_type(&target_ty, &table_ty));
    }

    #[test]
    fn test_defaulted_generic_field_is_not_required_for_table_compatibility() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class TraceBox<T>
            ---@field Value T
            ---@field Hit boolean=false
            "#,
        );

        let target_ty = ws.ty("TraceBox<number>");
        let table_ty = ws.expr_ty("{ Value = 1 }");

        assert!(ws.check_type(&target_ty, &table_ty));
    }
}
