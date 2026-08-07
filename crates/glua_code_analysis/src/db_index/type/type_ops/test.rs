#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, FileId, InFiled, LuaType, LuaUnionType, TypeOps, VirtualWorkspace};
    use internment::ArcIntern;
    use rowan::TextRange;
    use smol_str::SmolStr;

    /// `LuaType::from_vec` and repeated `TypeOps::Union` must agree on the
    /// same member set, in any assembly order.
    #[test]
    fn union_normal_form_is_assembly_order_free() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();

        let sets: Vec<Vec<LuaType>> = vec![
            // absorption families the structural path used to miss
            vec![LuaType::Number, LuaType::IntegerConst(1)],
            vec![LuaType::Integer, LuaType::IntegerConst(7)],
            vec![LuaType::String, LuaType::StringConst(ArcIntern::from(SmolStr::new("x")))],
            vec![LuaType::Boolean, LuaType::BooleanConst(true)],
            vec![LuaType::BooleanConst(true), LuaType::BooleanConst(false)],
            vec![LuaType::Table, LuaType::TableConst(InFiled::new(FileId { id: 1 }, TextRange::default()))],
            vec![LuaType::Never, LuaType::String],
            vec![LuaType::Unknown, LuaType::String],
            // sets that must stay untouched
            vec![LuaType::String, LuaType::Nil],
            vec![LuaType::String, LuaType::Integer, LuaType::Boolean],
            // three members, so order matters to the fold as well as the set
            vec![LuaType::Number, LuaType::IntegerConst(1), LuaType::Nil],
            vec![LuaType::String, LuaType::StringConst(ArcIntern::from(SmolStr::new("a"))), LuaType::Boolean],
        ];

        for set in sets {
            let folded = set
                .iter()
                .fold(LuaType::Never, |acc, ty| TypeOps::Union.apply(db, &acc, ty));
            assert_eq!(
                LuaType::from_vec(set.clone()),
                folded,
                "from_vec disagreed with pairwise TypeOps::Union for {set:?}"
            );

            // ...and the answer must not depend on the order the members arrived in.
            let mut reversed = set.clone();
            reversed.reverse();
            assert_eq!(
                LuaType::from_vec(set.clone()),
                LuaType::from_vec(reversed),
                "from_vec was assembly-order dependent for {set:?}"
            );
        }
    }

    #[test]
    fn test_custom_ops() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class a
        ---@class b
        "#,
        );
        {
            let type_a = ws.ty("a");
            let type_b = ws.ty("b");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_a, &type_b),
                ws.ty("a | b")
            );
        }
        {
            let type_ab = ws.ty("a | b");
            let type_string = ws.ty("string");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_ab, &type_string),
                ws.ty("a | b | string")
            );
        }
        {
            let type_ab = ws.ty("a | b");
            let type_a = ws.ty("a");
            assert_eq!(
                TypeOps::Remove.apply(ws.get_db_mut(), &type_ab, &type_a),
                ws.ty("b")
            );
        }
        {
            let type_a_opt = ws.ty("a?");
            let type_nil = ws.ty("nil");
            assert_eq!(
                TypeOps::Remove.apply(ws.get_db_mut(), &type_a_opt, &type_nil),
                ws.ty("a")
            );
        }
        {
            let type_a_nil = ws.ty("a | nil");
            let type_nil = ws.ty("nil");
            assert_eq!(
                TypeOps::Remove.apply(ws.get_db_mut(), &type_a_nil, &type_nil),
                ws.ty("a")
            );
        }
        // {
        //     let type_ab = ws.ty("a | b");
        //     let type_a = ws.ty("a");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_ab, &type_a),
        //         ws.ty("a")
        //     );
        // }
        // {
        //     let type_a_opt = ws.ty("a?");
        //     let type_a = ws.ty("a");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_a_opt, &type_a),
        //         ws.ty("a")
        //     );
        // }
        // {
        //     let type_ab = ws.ty("a | b");
        //     let type_ab2 = ws.ty("a | b");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_ab, &type_ab2),
        //         ws.ty("a | b")
        //     );
        // }
    }

    #[test]
    fn test_basic() {
        let mut ws = VirtualWorkspace::new();

        {
            let type_string = ws.ty("string");
            let type_literal = ws.ty("'ssss'");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_string, &type_literal),
                ws.ty("string")
            );
        }
        {
            let type_string = ws.ty("string");
            let type_number = ws.ty("number");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_string, &type_number),
                ws.ty("string | number")
            );
        }
        {
            let type_any = ws.ty("any");
            let type_nil = ws.ty("nil");
            let result = TypeOps::Union.apply(ws.get_db_mut(), &type_any, &type_nil);
            assert_eq!(result, ws.ty("any | nil"));
            assert!(result.is_nullable());
        }
        {
            let type_any = ws.ty("any");
            let type_string_nil = ws.ty("string | nil");
            let result = TypeOps::Union.apply(ws.get_db_mut(), &type_any, &type_string_nil);
            assert_eq!(result, ws.ty("any | nil"));
            assert!(result.is_nullable());
        }
        {
            let type_number = ws.ty("number");
            let type_integer = ws.ty("integer");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_number, &type_integer),
                ws.ty("number")
            );
        }
        {
            let type_integer = ws.ty("integer");
            let type_one = ws.ty("1");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_integer, &type_one),
                ws.ty("integer")
            );
        }
        {
            let type_one = ws.ty("1");
            let type_two = ws.ty("2");
            assert_eq!(
                TypeOps::Union.apply(ws.get_db_mut(), &type_one, &type_two),
                ws.ty("1|2")
            );
        }
        {
            assert_eq!(
                LuaType::from_vec(vec![LuaType::Unknown, LuaType::String]),
                LuaType::String
            );
        }
        {
            let union = LuaUnionType::from_vec(vec![LuaType::Unknown, LuaType::String]);
            assert_eq!(union.into_vec(), vec![LuaType::String]);
        }
        {
            let type_string_number = ws.ty("string | number");
            let type_string = ws.ty("string");
            assert_eq!(
                TypeOps::Remove.apply(ws.get_db_mut(), &type_string_number, &type_string),
                ws.ty("number")
            );
        }
        // {
        //     let type_string_number = ws.ty("string | number");
        //     let type_string = ws.ty("string");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_string_number, &type_string),
        //         ws.ty("string")
        //     );
        // }
        // {
        //     let type_string_number = ws.ty("string | number");
        //     let type_number = ws.ty("number");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_string_number, &type_number),
        //         ws.ty("number")
        //     );
        // }
        // {
        //     let type_string_nil = ws.ty("string | nil");
        //     let type_string = ws.ty("string");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_string_nil, &type_string),
        //         ws.ty("string")
        //     );
        // }
        // {
        //     let type_number_nil = ws.ty("number | nil");
        //     let type_number = ws.ty("number");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_number_nil, &type_number),
        //         ws.ty("number")
        //     );
        // }
        // {
        //     let type_one_nil = ws.ty("1 | nil");
        //     let type_integer = ws.ty("integer");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_one_nil, &type_integer),
        //         ws.ty("1")
        //     );
        // }
        // {
        //     let type_string_array_opt = ws.ty("string[]?");
        //     let type_empty_table = ws.expr_ty("{}");
        //     assert_eq!(
        //         TypeOps::Narrow.apply(ws.get_db_mut(), &type_string_array_opt, &type_empty_table),
        //         ws.ty("string[]")
        //     );
        // }
    }

    #[test]
    fn test_remove_type() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@return string[]
            function test()
                ---@type string[]|false
                local ids
                if ids == false then
                    return {}
                end
                return ids
            end
        "#
        ));
    }
}
