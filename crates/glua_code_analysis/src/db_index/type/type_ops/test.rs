#[cfg(test)]
mod tests {
    use crate::{
        DiagnosticCode, FileId, InFiled, LuaMultiLineUnion, LuaType, LuaUnionType, TypeOps,
        VirtualWorkspace,
    };
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
            vec![
                LuaType::String,
                LuaType::StringConst(ArcIntern::from(SmolStr::new("x"))),
            ],
            vec![LuaType::Boolean, LuaType::BooleanConst(true)],
            vec![LuaType::BooleanConst(true), LuaType::BooleanConst(false)],
            vec![
                LuaType::Table,
                LuaType::TableConst(InFiled::new(FileId { id: 1 }, TextRange::default())),
            ],
            vec![LuaType::Never, LuaType::String],
            vec![LuaType::Unknown, LuaType::String],
            // sets that must stay untouched
            vec![LuaType::String, LuaType::Nil],
            vec![LuaType::String, LuaType::Integer, LuaType::Boolean],
            // three members, so order matters to the fold as well as the set
            vec![LuaType::Number, LuaType::IntegerConst(1), LuaType::Nil],
            vec![
                LuaType::String,
                LuaType::StringConst(ArcIntern::from(SmolStr::new("a"))),
                LuaType::Boolean,
            ],
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

    /// A declared `string|"a"|"b"` keeps every arm the author wrote, while an
    /// inferred literal still collapses into its primitive.
    #[test]
    fn declared_literal_arms_outlive_their_primitive() {
        let members = |types: Vec<LuaType>| match LuaType::from_vec(types) {
            LuaType::Union(union) => union.into_vec(),
            other => panic!("expected a union, got {other:?}"),
        };

        let doc_a = LuaType::DocStringConst(ArcIntern::from(SmolStr::new("a")));
        let doc_b = LuaType::DocStringConst(ArcIntern::from(SmolStr::new("b")));
        let declared = members(vec![LuaType::String, doc_a.clone(), doc_b.clone()]);
        assert_eq!(
            declared.len(),
            3,
            "declared arms were absorbed: {declared:?}"
        );
        for arm in [LuaType::String, doc_a, doc_b] {
            assert!(declared.contains(&arm), "{arm:?} missing from {declared:?}");
        }

        let declared_ints = members(vec![
            LuaType::Integer,
            LuaType::DocIntegerConst(1),
            LuaType::DocIntegerConst(2),
        ]);
        assert_eq!(
            declared_ints.len(),
            3,
            "declared arms were absorbed: {declared_ints:?}"
        );

        assert_eq!(
            LuaType::from_vec(vec![
                LuaType::String,
                LuaType::StringConst(ArcIntern::from(SmolStr::new("a"))),
            ]),
            LuaType::String
        );
        assert_eq!(
            LuaType::from_vec(vec![LuaType::Integer, LuaType::IntegerConst(1)]),
            LuaType::Integer
        );
    }

    /// A multi-line union answers the same whichever operand it arrives as.
    #[test]
    fn multi_line_union_membership_is_operand_order_free() {
        let mut ws = VirtualWorkspace::new();
        let db = ws.get_db_mut();

        let multi_line = LuaType::MultiLineUnion(
            LuaMultiLineUnion::new(vec![
                (
                    LuaType::DocStringConst(ArcIntern::from(SmolStr::new("a"))),
                    None,
                ),
                (
                    LuaType::DocStringConst(ArcIntern::from(SmolStr::new("b"))),
                    None,
                ),
            ])
            .into(),
        );

        for value in ["a", "z"] {
            let other = LuaType::StringConst(ArcIntern::from(SmolStr::new(value)));
            assert_eq!(
                TypeOps::Union.apply(db, &multi_line, &other),
                TypeOps::Union.apply(db, &other, &multi_line),
                "multi-line union depended on operand order for {value:?}"
            );
        }

        let member = LuaType::StringConst(ArcIntern::from(SmolStr::new("a")));
        assert_eq!(
            TypeOps::Union.apply(db, &member, &multi_line),
            multi_line,
            "an existing arm must not widen the multi-line union"
        );
    }

    /// Unioning an alias into an existing union must keep the *alias*, not
    /// the type it dereferences to.
    #[test]
    fn union_with_a_union_keeps_the_alias_member() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@alias Seconds string
        "#,
        );

        let alias = LuaType::Ref(crate::LuaTypeDeclId::global("Seconds"));
        let union = LuaType::from_vec(vec![LuaType::Integer, LuaType::Boolean]);
        let merged = TypeOps::Union.apply(ws.get_db_mut(), &alias, &union);

        let members = match &merged {
            LuaType::Union(union) => union.into_vec(),
            other => panic!("expected a union, got {other:?}"),
        };
        assert!(
            members.contains(&alias),
            "the alias must survive into the union, got {members:?}"
        );
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
            // An `unknown` arm is an unsettled type, not an absent one, so it
            // stays in the union instead of being dropped as noise.
            assert_eq!(
                LuaType::from_vec(vec![LuaType::Unknown, LuaType::String]),
                LuaType::from(LuaUnionType::Multi(vec![LuaType::String, LuaType::Unknown]))
            );
        }
        {
            let union = LuaUnionType::from_vec(vec![LuaType::Unknown, LuaType::String]);
            assert_eq!(union.into_vec(), vec![LuaType::String, LuaType::Unknown]);
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

    fn ordered_fold(db: &crate::DbIndex, types: Vec<LuaType>) -> LuaType {
        types.into_iter().fold(LuaType::Never, |acc, ty| {
            TypeOps::Union.apply(db, &acc, &ty)
        })
    }

    fn assert_union_all_matches_fold(ws: &mut VirtualWorkspace, types: Vec<LuaType>) {
        let db = ws.get_db_mut();
        let expected = ordered_fold(db, types.clone());
        let actual = TypeOps::union_all(db, types.clone());
        assert_eq!(
            actual, expected,
            "union_all disagreed with the ordered fold for {types:?}"
        );
    }

    #[test]
    fn union_all_matches_fold_for_empty_and_singletons() {
        let mut ws = VirtualWorkspace::new();
        assert_union_all_matches_fold(&mut ws, vec![]);
        assert_eq!(TypeOps::union_all(ws.get_db_mut(), vec![]), LuaType::Never);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Never]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::String]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Any]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Unknown]);
        let alias_single = LuaType::Ref(crate::LuaTypeDeclId::global("MissingAlias"));
        assert_union_all_matches_fold(&mut ws, vec![alias_single]);
    }

    #[test]
    fn union_all_matches_fold_for_never_and_any() {
        let mut ws = VirtualWorkspace::new();
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Never, LuaType::String]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::String, LuaType::Never]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Never, LuaType::Never]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Any, LuaType::String]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::String, LuaType::Any]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Any, LuaType::Nil]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Nil, LuaType::Any]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::Any, LuaType::String, LuaType::Integer],
        );
        assert_union_all_matches_fold(&mut ws, vec![LuaType::String, LuaType::Nil, LuaType::Any]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![
                LuaType::Unknown,
                LuaType::String,
                LuaType::Nil,
                LuaType::Any,
            ],
        );
    }

    #[test]
    fn union_all_matches_fold_for_primitive_collapse() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class FoldAlpha
            ---@class FoldBeta
            "#,
        );
        let alpha = LuaType::Ref(crate::LuaTypeDeclId::global("FoldAlpha"));
        let beta = LuaType::Ref(crate::LuaTypeDeclId::global("FoldBeta"));
        let string_a = LuaType::StringConst(ArcIntern::from(SmolStr::new("a")));

        assert_union_all_matches_fold(&mut ws, vec![LuaType::Integer, LuaType::IntegerConst(7)]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![
                LuaType::IntegerConst(1),
                LuaType::IntegerConst(2),
                LuaType::Integer,
            ],
        );
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Number, LuaType::IntegerConst(1)]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::Number, LuaType::Integer, LuaType::FloatConst(1.5)],
        );
        assert_union_all_matches_fold(&mut ws, vec![LuaType::String, string_a.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![string_a, LuaType::String]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Boolean, LuaType::BooleanConst(true)]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::BooleanConst(true), LuaType::BooleanConst(false)],
        );
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::BooleanConst(true), LuaType::BooleanConst(true)],
        );
        assert_union_all_matches_fold(
            &mut ws,
            vec![
                LuaType::Table,
                LuaType::TableConst(InFiled::new(FileId::new(1), TextRange::default())),
            ],
        );
        assert_union_all_matches_fold(&mut ws, vec![alpha.clone(), beta.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![alpha.clone(), beta.clone(), alpha.clone()]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::Unknown, LuaType::String, LuaType::Unknown],
        );
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Nil, LuaType::Nil]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::String, LuaType::Integer, LuaType::Boolean],
        );
    }

    #[test]
    fn union_all_matches_fold_for_doc_consts() {
        let mut ws = VirtualWorkspace::new();
        let doc_a = LuaType::DocStringConst(ArcIntern::from(SmolStr::new("a")));
        let doc_b = LuaType::DocStringConst(ArcIntern::from(SmolStr::new("b")));
        let doc_1 = LuaType::DocIntegerConst(1);
        let doc_2 = LuaType::DocIntegerConst(2);

        assert_union_all_matches_fold(&mut ws, vec![LuaType::String, doc_a.clone(), doc_b.clone()]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![LuaType::Integer, doc_1.clone(), doc_2.clone()],
        );
        assert_union_all_matches_fold(&mut ws, vec![doc_a.clone(), doc_b.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![doc_a.clone(), doc_a.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![doc_1.clone(), doc_2.clone()]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![
                LuaType::StringConst(ArcIntern::from(SmolStr::new("a"))),
                doc_a.clone(),
            ],
        );
    }

    #[test]
    fn union_all_matches_fold_for_nested_and_multiline_unions() {
        let mut ws = VirtualWorkspace::new();
        let nested = LuaType::from_vec_structural(vec![LuaType::String, LuaType::Integer]);
        assert_union_all_matches_fold(&mut ws, vec![nested.clone(), LuaType::Boolean]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Boolean, nested.clone()]);
        let nullable_string =
            LuaType::Union(LuaUnionType::from_vec(vec![LuaType::String, LuaType::Nil]).into());
        assert_union_all_matches_fold(&mut ws, vec![nullable_string.clone(), LuaType::Integer]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Integer, nullable_string.clone()]);

        let multi_line = LuaType::MultiLineUnion(
            LuaMultiLineUnion::new(vec![
                (
                    LuaType::DocStringConst(ArcIntern::from(SmolStr::new("a"))),
                    None,
                ),
                (
                    LuaType::DocStringConst(ArcIntern::from(SmolStr::new("b"))),
                    None,
                ),
            ])
            .into(),
        );
        let member_a = LuaType::StringConst(ArcIntern::from(SmolStr::new("a")));
        let member_z = LuaType::StringConst(ArcIntern::from(SmolStr::new("z")));
        assert_union_all_matches_fold(&mut ws, vec![multi_line.clone(), member_a.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![member_a.clone(), multi_line.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![multi_line.clone(), member_z.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![member_z.clone(), multi_line.clone()]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![multi_line.clone(), LuaType::String, LuaType::Integer],
        );
        assert_union_all_matches_fold(&mut ws, vec![nested, multi_line, LuaType::Boolean]);
    }

    #[test]
    fn union_all_matches_fold_for_alias_refs() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias FoldSeconds string
            ---@alias FoldMyInt integer
            ---@alias FoldChoice string|number
            ---@class FoldReal
            "#,
        );
        let seconds = LuaType::Ref(crate::LuaTypeDeclId::global("FoldSeconds"));
        let my_int = LuaType::Ref(crate::LuaTypeDeclId::global("FoldMyInt"));
        let choice = LuaType::Ref(crate::LuaTypeDeclId::global("FoldChoice"));
        let real = LuaType::Ref(crate::LuaTypeDeclId::global("FoldReal"));

        // Alias first: the dereferenced primitive absorbs a later literal.
        assert_union_all_matches_fold(
            &mut ws,
            vec![
                seconds.clone(),
                LuaType::StringConst(ArcIntern::from(SmolStr::new("x"))),
            ],
        );
        assert_union_all_matches_fold(
            &mut ws,
            vec![
                LuaType::StringConst(ArcIntern::from(SmolStr::new("x"))),
                seconds.clone(),
            ],
        );
        assert_union_all_matches_fold(&mut ws, vec![my_int.clone(), LuaType::IntegerConst(3)]);
        assert_union_all_matches_fold(&mut ws, vec![seconds.clone(), LuaType::String]);
        assert_union_all_matches_fold(&mut ws, vec![seconds.clone(), LuaType::Any]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![seconds.clone(), seconds.clone(), LuaType::String],
        );
        // Alias to a union splices its arms instead of surviving as a member.
        assert_union_all_matches_fold(&mut ws, vec![choice.clone(), LuaType::Boolean]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Boolean, choice.clone()]);
        // Non-alias refs keep identity on both paths.
        assert_union_all_matches_fold(&mut ws, vec![real.clone(), LuaType::String]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![real.clone(), seconds.clone(), LuaType::Integer],
        );
    }

    #[test]
    fn union_all_matches_fold_for_callables() {
        use std::sync::Arc;

        use rowan::TextSize;

        let mut ws = VirtualWorkspace::new();
        let sig_a =
            LuaType::Signature(crate::LuaSignatureId::new(FileId::new(1), TextSize::new(0)));
        let sig_b = LuaType::Signature(crate::LuaSignatureId::new(
            FileId::new(1),
            TextSize::new(40),
        ));
        let doc_func = LuaType::DocFunction(Arc::new(crate::LuaFunctionType::new(
            crate::AsyncState::Sync,
            false,
            false,
            vec![],
            LuaType::Nil,
        )));

        assert_union_all_matches_fold(&mut ws, vec![LuaType::Function, doc_func.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![doc_func.clone(), LuaType::Function]);
        assert_union_all_matches_fold(&mut ws, vec![LuaType::Function, sig_a.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![sig_a.clone(), sig_b.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![sig_a.clone(), sig_a.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![sig_a.clone(), LuaType::String, sig_b.clone()]);
        assert_union_all_matches_fold(
            &mut ws,
            vec![doc_func.clone(), sig_a.clone(), LuaType::Integer],
        );
        let tpl = LuaType::TplRef(Arc::new(crate::GenericTpl::new(
            crate::GenericTplId::Type(0),
            ArcIntern::new(SmolStr::new("T")),
            None,
        )));
        assert_union_all_matches_fold(&mut ws, vec![tpl.clone(), LuaType::String, sig_a.clone()]);
        assert_union_all_matches_fold(&mut ws, vec![tpl, sig_a]);
    }

    #[test]
    fn union_all_seeded_fuzz_matches_ordered_fold() {
        use std::sync::Arc;

        use rowan::TextSize;

        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias FuzzStr string
            ---@alias FuzzInt integer
            ---@alias FuzzChoice string|number
            ---@class FuzzAlpha
            ---@class FuzzBeta
            ---@class FuzzGamma
            "#,
        );

        fn sample(pick: u64) -> LuaType {
            match pick % 28 {
                0 => LuaType::Nil,
                1 => LuaType::Boolean,
                2 => LuaType::BooleanConst(pick % 32 < 16),
                3 => LuaType::Integer,
                4 => LuaType::IntegerConst((pick % 5) as i64),
                5 => LuaType::DocIntegerConst((pick % 5) as i64),
                6 => LuaType::Number,
                7 => LuaType::FloatConst((pick % 3) as f64),
                8 => LuaType::String,
                9 => LuaType::StringConst(ArcIntern::new(SmolStr::new(match pick % 4 {
                    0 => "a",
                    1 => "b",
                    2 => "c",
                    _ => "d",
                }))),
                10 => LuaType::DocStringConst(ArcIntern::new(SmolStr::new(match pick % 3 {
                    0 => "a",
                    1 => "b",
                    _ => "c",
                }))),
                11 => LuaType::Table,
                12 => LuaType::TableConst(InFiled::new(
                    FileId::new(1),
                    TextRange::new(
                        rowan::TextSize::new(((pick % 7) * 10) as u32),
                        rowan::TextSize::new(((pick % 7) * 10 + 5) as u32),
                    ),
                )),
                13 => LuaType::Function,
                14 => LuaType::Ref(crate::LuaTypeDeclId::global(match pick % 3 {
                    0 => "FuzzAlpha",
                    1 => "FuzzBeta",
                    _ => "FuzzGamma",
                })),
                15 => LuaType::Ref(crate::LuaTypeDeclId::global(match pick % 3 {
                    0 => "FuzzStr",
                    1 => "FuzzInt",
                    _ => "FuzzChoice",
                })),
                16 => LuaType::Unknown,
                17 => LuaType::Any,
                18 => LuaType::Never,
                19 => LuaType::Signature(crate::LuaSignatureId::new(
                    FileId::new(1),
                    TextSize::new(((pick % 5) * 40) as u32),
                )),
                20 => LuaType::DocFunction(Arc::new(crate::LuaFunctionType::new(
                    crate::AsyncState::Sync,
                    false,
                    false,
                    vec![],
                    LuaType::Nil,
                ))),
                21 => LuaType::TplRef(Arc::new(crate::GenericTpl::new(
                    crate::GenericTplId::Type((pick % 3) as u32),
                    ArcIntern::new(SmolStr::new("T")),
                    None,
                ))),
                22 => LuaType::from_vec_structural(vec![LuaType::String, LuaType::Integer]),
                23 => LuaType::Union(
                    LuaUnionType::from_vec(vec![LuaType::String, LuaType::Nil]).into(),
                ),
                24 => LuaType::MultiLineUnion(
                    LuaMultiLineUnion::new(vec![
                        (
                            LuaType::DocStringConst(ArcIntern::from(SmolStr::new("a"))),
                            None,
                        ),
                        (
                            LuaType::DocStringConst(ArcIntern::from(SmolStr::new("b"))),
                            None,
                        ),
                    ])
                    .into(),
                ),
                25 => LuaType::TypeGuard(Arc::new(LuaType::String)),
                26 => LuaType::TableOf(Box::new(LuaType::String)),
                _ => LuaType::Userdata,
            }
        }

        fn rng(seed: u64) -> impl FnMut() -> u64 {
            let mut state = seed;
            move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            }
        }

        let mut next = rng(0x51ab_3c7d_9e2f_1049);
        for _ in 0..3000 {
            let count = (next() % 10) as usize + 1;
            let types = (0..count).map(|_| sample(next())).collect::<Vec<_>>();
            let db = ws.get_db_mut();
            let expected = ordered_fold(db, types.clone());
            let actual = TypeOps::union_all(db, types.clone());
            assert_eq!(
                actual, expected,
                "union_all disagreed with the ordered fold for {types:?}"
            );
        }
    }

    #[test]
    fn union_all_large_tuple_dynamic_index_matches_fold_exactly() {
        use glua_parser::{LuaAstNode, LuaExpr, LuaNameExpr};

        const COUNT: usize = 300;
        let mut ws = VirtualWorkspace::new();
        let mut class_defs = String::new();
        for i in 0..COUNT {
            class_defs.push_str(&format!("---@class BigTupleCls{i}\n"));
        }
        ws.def(&class_defs);
        let tuple_repr = (0..COUNT)
            .map(|i| format!("BigTupleCls{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let tuple_ty = ws.ty(&format!("[{tuple_repr}]"));
        let tuple_members = match &tuple_ty {
            LuaType::Tuple(tuple) => tuple.get_types().to_vec(),
            other => panic!("expected a tuple type, got {other:?}"),
        };
        assert_eq!(tuple_members.len(), COUNT);
        assert_union_all_matches_fold(&mut ws, tuple_members.clone());

        let file_id = ws.def(&format!(
            r#"
            ---@type [{tuple_repr}]
            local big_tup

            ---@type integer
            local big_idx

            local big_val = big_tup[big_idx]
            print(big_val)
            "#
        ));
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model must exist");
        let target = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some("big_val"))
            .collect::<Vec<_>>()
            .pop()
            .expect("big_val must exist");
        let inferred = semantic_model
            .infer_expr(LuaExpr::NameExpr(target))
            .expect("big_val must infer");
        let db = ws.get_db_mut();
        let unioned = TypeOps::union_all(db, tuple_members);
        let expected = TypeOps::Union.apply(db, &unioned, &LuaType::Nil);
        assert_eq!(
            inferred, expected,
            "large tuple dynamic index must equal the batched union plus nil"
        );
        match &inferred {
            LuaType::Union(union) => {
                assert_eq!(union.len(), COUNT + 1, "unexpected arm count: {inferred:?}");
                assert!(union.is_nullable(), "large tuple index must stay nullable");
            }
            other => panic!("expected a union from the large tuple index, got {other:?}"),
        }
    }
}
