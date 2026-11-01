#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, EmmyrcLuaVersion, LuaType, VirtualWorkspace};

    #[test]
    fn test_unpack() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        a, b = table.unpack({ 1, 2, 3 })

        ---@type string[]
        local ddd

        e = table.unpack(ddd)
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let a_expected = ws.expr_ty("1");
        assert_eq!(a_ty, a_expected);

        let b_ty = ws.expr_ty("b");
        let b_expected = ws.expr_ty("2");
        assert_eq!(b_ty, b_expected);

        let e_ty = ws.expr_ty("e");
        let e_expected = ws.ty("string?");
        assert_eq!(e_ty, e_expected);
    }

    #[test]
    fn test_unpack_unresolved_trailing_call_keeps_the_known_prefix() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            local packed = {123, missing_callee()}
            a = table.unpack(packed)
        "#,
        );

        let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
            panic!("expected a tuple summary");
        };
        let types = tuple.get_types();
        assert_eq!(types.len(), 2);
        assert_eq!(types[0], LuaType::IntegerConst(123));
        assert!(matches!(types[1], LuaType::Variadic(_)));
        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(123));
    }

    #[test]
    fn test_unpack_declared_unknown_return_is_one_position() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@type fun(): unknown
            local one

            local packed = {123, one()}
            a, b, c = table.unpack(packed)
        "#,
        );

        let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
            panic!("expected a tuple summary");
        };
        let types = tuple.get_types();
        assert_eq!(types.len(), 2);
        assert!(!matches!(types[1], LuaType::Variadic(_)));
        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(123));
        assert!(ws.expr_ty("c").is_nil());
    }

    #[test]
    fn test_unpack_broad_callable_has_an_open_return_tail() {
        for annotation in ["function", "any"] {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let file_id = ws.def(&format!(
                r#"
                ---@param producer {annotation}
                local function pack(producer)
                    local packed = {{123, producer()}}
                    local first, second, third = table.unpack(packed)
                    return first, third
                end
                "#,
            ));
            let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
                panic!("expected a tuple summary for {annotation}");
            };
            let types = tuple.get_types();
            assert_eq!(types.len(), 2, "{annotation}");
            assert_eq!(types[0], LuaType::IntegerConst(123), "{annotation}");
            assert!(
                matches!(&types[1], LuaType::Variadic(tail)
                    if matches!(tail.as_ref(), crate::VariadicType::Base(ty) if ty.is_unknown())),
                "{annotation}"
            );
            assert_eq!(ws.local_ty(file_id, "first"), LuaType::IntegerConst(123));
            assert!(ws.local_ty(file_id, "third").is_unknown(), "{annotation}");
        }
    }

    #[test]
    fn test_parenthesized_broad_callable_has_one_return_position() {
        for annotation in ["function", "any"] {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let file_id = ws.def(&format!(
                r#"
                ---@param producer {annotation}
                local function pack(producer)
                    local packed = {{123, (producer())}}
                    local first, second, third = table.unpack(packed)
                    return first, third
                end
                "#,
            ));
            let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
                panic!("expected a tuple summary for {annotation}");
            };
            assert_eq!(tuple.get_types().len(), 2);
            assert!(!matches!(tuple.get_types()[1], LuaType::Variadic(_)));
            assert!(ws.local_ty(file_id, "third").is_nil(), "{annotation}");
        }
    }

    #[test]
    fn test_unpack_alias_and_union_return_contracts() {
        for (annotation, parenthesized, open_tail) in [
            ("OpaqueProducer", false, true),
            ("any|fun(): number", false, true),
            ("function|fun(): number", false, true),
            ("any|fun(): number...", false, true),
            ("function|fun(): number...", false, true),
            ("fun(): any", false, false),
            ("fun(): unknown", false, false),
            ("OpaqueProducer", true, false),
            ("any|fun(): number", true, false),
            ("function|fun(): number", true, false),
        ] {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let call = if parenthesized {
                "(producer())"
            } else {
                "producer()"
            };
            let file_id = ws.def(&format!(
                r#"
                ---@alias OpaqueProducer any

                ---@param producer {annotation}
                local function pack(producer)
                    local packed = {{123, {call}}}
                    local first, second, third = table.unpack(packed)
                    return first, third
                end
                "#,
            ));
            let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
                panic!("expected a tuple summary for {annotation}, {call}");
            };
            let types = tuple.get_types();
            assert_eq!(types.len(), 2, "{annotation}, {call}");
            assert_eq!(types[0], LuaType::IntegerConst(123), "{annotation}, {call}");
            assert_eq!(ws.local_ty(file_id, "first"), LuaType::IntegerConst(123));
            let third = ws.local_ty(file_id, "third");
            if open_tail {
                assert!(
                    matches!(&types[1], LuaType::Variadic(tail)
                        if matches!(tail.as_ref(), crate::VariadicType::Base(ty) if ty.is_unknown())),
                    "{annotation}, {call}"
                );
                assert!(!third.is_nil(), "{annotation}, {call}");
                assert!(third.is_unknown(), "{annotation}, {call}");
            } else {
                assert!(
                    !matches!(types[1], LuaType::Variadic(_)),
                    "{annotation}, {call}"
                );
                assert!(third.is_nil(), "{annotation}, {call}");
            }
        }
    }

    #[test]
    fn test_unpack_generic_constraint_return_contracts() {
        for (generics, annotation, open_tail) in [
            ("T: T|fun(): number", "T", true),
            ("T: fun(): number", "T", false),
            ("T: (fun(): number), U: T", "T|U|fun(): number", false),
        ] {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let file_id = ws.def(&format!(
                r#"
                ---@generic {generics}
                ---@param producer {annotation}
                local function pack(producer)
                    local packed = {{123, producer()}}
                    local first, second, third = table.unpack(packed)
                    return first, third
                end
                "#,
            ));
            let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
                panic!("expected a tuple summary for {generics}, {annotation}");
            };
            let types = tuple.get_types();
            assert_eq!(types.len(), 2, "{generics}, {annotation}");
            assert_eq!(
                types[0],
                LuaType::IntegerConst(123),
                "{generics}, {annotation}"
            );
            assert_eq!(ws.local_ty(file_id, "first"), LuaType::IntegerConst(123));
            let third = ws.local_ty(file_id, "third");
            if open_tail {
                assert!(
                    matches!(&types[1], LuaType::Variadic(tail)
                        if matches!(tail.as_ref(), crate::VariadicType::Base(ty) if ty.is_unknown())),
                    "{generics}, {annotation}"
                );
                assert!(!third.is_nil(), "{generics}, {annotation}");
                assert!(third.is_unknown(), "{generics}, {annotation}");
            } else {
                assert_eq!(types[1], LuaType::Number, "{generics}, {annotation}");
                assert_eq!(ws.local_ty(file_id, "second"), LuaType::Number);
                assert!(third.is_nil(), "{generics}, {annotation}");
            }
        }
    }

    #[test]
    fn test_unpack_unresolved_wrapped_call_keeps_an_open_tail() {
        for setup in [
            r#"
                local target = {}
                local wrapper = setmetatable({}, { __call = target })
            "#,
            r#"
                local wrapper, other
                wrapper = setmetatable({}, { __call = other })
                other = setmetatable({}, { __call = wrapper })
            "#,
        ] {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let file_id = ws.def(&format!(
                r#"
                {setup}
                local packed = {{123, wrapper()}}
                local first, second, third = table.unpack(packed)
                "#,
            ));
            let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
                panic!("expected a tuple summary for {setup}");
            };
            let types = tuple.get_types();
            assert_eq!(types.len(), 2, "{setup}");
            assert_eq!(types[0], LuaType::IntegerConst(123), "{setup}");
            assert!(
                matches!(&types[1], LuaType::Variadic(tail)
                    if matches!(tail.as_ref(), crate::VariadicType::Base(ty) if ty.is_unknown())),
                "{setup}"
            );
            assert_eq!(ws.local_ty(file_id, "first"), LuaType::IntegerConst(123));
            let third = ws.local_ty(file_id, "third");
            assert!(!third.is_nil(), "{setup}");
            assert!(third.is_unknown(), "{setup}");
        }
    }

    #[test]
    fn test_unpack_declared_callable_table_has_a_finite_return_contract() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@type fun(self: table): number
            local target
            local wrapper = setmetatable({}, { __call = target })
            local packed = {123, wrapper()}
            local first, second, third = table.unpack(packed)
            "#,
        );
        let LuaType::Tuple(tuple) = ws.local_ty(file_id, "packed") else {
            panic!("expected a tuple summary");
        };
        assert_eq!(
            tuple.get_types(),
            &[LuaType::IntegerConst(123), LuaType::Number]
        );
        assert_eq!(ws.local_ty(file_id, "first"), LuaType::IntegerConst(123));
        assert_eq!(ws.local_ty(file_id, "second"), LuaType::Number);
        assert!(ws.local_ty(file_id, "third").is_nil());
    }

    #[test]
    fn test_unpack_declared_variadic_return_keeps_the_typed_array() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@param producer fun(): number...
            local function pack(producer)
                local packed = {123, producer()}
                local first, second, third = table.unpack(packed)
                return first, third
            end
            "#,
        );
        let LuaType::Array(array) = ws.local_ty(file_id, "packed") else {
            panic!("expected a typed variadic array summary");
        };
        assert_eq!(array.get_base(), &LuaType::Number);
        let expected = ws.ty("number?");
        assert_eq!(ws.local_ty(file_id, "first"), expected);
        assert_eq!(ws.local_ty(file_id, "third"), expected);
    }

    #[test]
    fn test_unpack_alias_call_union() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@overload fun<T>(t: T): std.Unpack<T>
            ---@overload fun(t: number): number
            local function f(t)
            end

            a, b, c = f({ 1, 2, 3 })
        "#,
        );

        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(1));
        assert_eq!(ws.expr_ty("b"), LuaType::IntegerConst(2));
        assert_eq!(ws.expr_ty("c"), LuaType::IntegerConst(3));
    }

    #[test]
    fn test_unpack_member_array_literal_preserves_positional_evidence() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            local holder = {}
            holder.args = { 1, 2 }

            a, b = table.unpack(holder.args)
        "#,
        );

        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(1));
        assert_eq!(ws.expr_ty("b"), LuaType::IntegerConst(2));
    }

    #[test]
    fn test_unpack_class_member_array_literal_preserves_positional_evidence() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class TestEntity
            local TestEntity = {}

            ---@type TestEntity
            local ent

            function TestEntity:Init()
                self.args = { 1, 2 }
            end

            a, b = table.unpack(ent.args)
        "#,
        );

        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(1));
        assert_eq!(ws.expr_ty("b"), LuaType::IntegerConst(2));
    }

    #[test]
    fn test_unpack_alias_call_colon_mismatch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Obj
            ---@field unpack (fun<T>(self: Obj, t: T): std.Unpack<T>) | (fun(self: Obj, t: number): number)
            local Obj = {}

            a, b = Obj:unpack({ 1, 2 })
        "#,
        );

        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(1));
        assert_eq!(ws.expr_ty("b"), LuaType::IntegerConst(2));
    }

    #[test]
    fn test_unpack_method_dot_call_with_instance() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Obj
            local Obj = {}

            ---@generic T
            ---@param t T
            ---@return std.Unpack<T>
            function Obj:unpack(t)
            end

            ---@type Obj
            obj = {}

            a, b = Obj.unpack(obj, { 1, 2 })
        "#,
        );

        assert_eq!(ws.expr_ty("a"), LuaType::IntegerConst(1));
        assert_eq!(ws.expr_ty("b"), LuaType::IntegerConst(2));
    }

    #[test]
    fn test_unpack_alias_call_self_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@return std.Unpack<[self, self]>
            function table:dup()
            end

            a, b = table:dup()
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let b_ty = ws.expr_ty("b");
        assert_eq!(ws.humanize_type(a_ty), "tablelib");
        assert_eq!(ws.humanize_type(b_ty), "tablelib");
    }

    #[test]
    fn test_unpack_alias_call_explicit_generic_list() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@generic T
            ---@return std.Unpack<T>
            function f()
            end

            a, b = f--[[@<[string, number]>]]()
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let b_ty = ws.expr_ty("b");
        assert_eq!(ws.humanize_type(a_ty), "string");
        assert_eq!(ws.humanize_type(b_ty), "number");
    }

    #[test]
    fn test_issue_484() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        --- @type integer,integer,integer
        local _a, _b, _c = unpack({ 1, 2, 3 })
        "#,
        ));
    }

    #[test]
    fn test_issue_594() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.analysis.update_config(emmyrc.into());
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        --- @type string[]
        local s = {}

        --- @type string[]
        local s2 = { 'a', unpack(s) }
        "#,
        ));
    }

    #[test]
    fn test_unpack_passthrough_vararg() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            local function passthrough(...)
                local args = { ... }
                return unpack(args)
            end
            n1, n2 = passthrough(1, 2)
            "#,
        );
        assert!(!ws.expr_ty("n1").is_optional());
        assert!(!ws.expr_ty("n2").is_optional());
    }
}
