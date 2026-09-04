#[cfg(test)]
mod test {
    use internment::ArcIntern;
    use smol_str::SmolStr;

    use crate::{
        DiagnosticCode, FileId, GenericTpl, GenericTplId, LuaMergedTableType, LuaType, RenderLevel,
        TypeSubstitutor, VirtualWorkspace, humanize_type, instantiate_type_generic,
    };

    fn local_type(ws: &VirtualWorkspace, file_id: FileId, name: &str) -> LuaType {
        let db = ws.analysis.compilation.get_db();
        let decl_id = db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .and_then(|tree| {
                tree.get_decls()
                    .values()
                    .find(|decl| decl.get_name() == name)
                    .map(|decl| decl.get_id())
            })
            .unwrap_or_else(|| panic!("no declaration named {name}"));
        db.get_type_index()
            .get_type_cache(&decl_id.into())
            .unwrap_or_else(|| panic!("no cached type for {name}"))
            .as_type()
            .clone()
    }

    /// `pairs` is declared `---@generic K, V, I` over
    /// `table<K, V> | V[] | {[K]: V}` (`resources/std/global.lua:244-247`), so
    /// `for k, v in pairs(t)` binds `k` and `v` through the iterator's `K` and
    /// `V`. When `t`'s members are known they bind to real types.
    ///
    /// When they are not — an empty literal, or the `x or {}` fallback that is
    /// idiomatic Lua — there is nothing to bind them to, and what must not be
    /// left behind is `pairs`'s own declared type parameter. A `TplRef` is an
    /// internal placeholder, and the cache it lands in is the one hover and
    /// completion read, so it escapes to the user as `K` and `V`.
    ///
    /// Normal inference retains the unbound template as a retry marker for
    /// unresolve. Once settled, loop-variable finalization consumes recorded
    /// call-binding provenance to replace proven unbound callee generics with
    /// their constraint or fallback, avoiding leaks without disturbing legitimate
    /// enclosing caller generics.
    #[test]
    fn pairs_over_a_shapeless_table_does_not_leak_its_type_parameters() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            local known = { alpha = 1 }
            for knownKey, knownValue in pairs(known) do end

            local empty = {}
            for emptyKey, emptyValue in pairs(empty) do end

            local function opaque() end
            local fallback = opaque() or {}
            for fallbackKey, fallbackValue in pairs(fallback) do end
            "#,
        );

        // Control: a table whose members are known still binds them. This is the
        // behaviour a fix must not trade away.
        assert!(
            matches!(local_type(&ws, file_id, "knownKey"), LuaType::StringConst(ref key) if key.as_str() == "alpha"),
            "knownKey was {:?}",
            local_type(&ws, file_id, "knownKey")
        );
        assert!(
            matches!(
                local_type(&ws, file_id, "knownValue"),
                LuaType::IntegerConst(1)
            ),
            "knownValue was {:?}",
            local_type(&ws, file_id, "knownValue")
        );

        for name in ["emptyKey", "emptyValue", "fallbackKey", "fallbackValue"] {
            let ty = local_type(&ws, file_id, name);
            assert!(
                !matches!(ty, LuaType::TplRef(_)),
                "{name} kept pairs's own declared type parameter: {ty:?}"
            );
            // `Any` is this analyzer's conservative "no information available".
            // A `TplRef` only looks more structured; it carries nothing.
            assert_eq!(ty, LuaType::Any, "{name} settled on an unexpected type");
        }
    }

    #[test]
    fn pairs_over_shapeless_table_inside_generic_function_does_not_leak_or_collide() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@generic K, V
            local function shapelessInsideGeneric()
                local empty = {}
                for k, v in pairs(empty) do end
            end
            "#,
        );

        let k_ty = local_type(&ws, file_id, "k");
        let v_ty = local_type(&ws, file_id, "v");
        assert_eq!(k_ty, LuaType::Any, "k should settle to Any");
        assert_eq!(v_ty, LuaType::Any, "v should settle to Any");
    }

    #[test]
    fn pairs_over_legitimate_caller_generic_table() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@generic K, V
            ---@param t table<K, V>
            local function genericTable(t)
                for k, v in pairs(t) do end
            end
            "#,
        );

        let k_ty = local_type(&ws, file_id, "k");
        let v_ty = local_type(&ws, file_id, "v");
        assert_ne!(k_ty, LuaType::Any, "k should not be Any");
        assert_ne!(v_ty, LuaType::Any, "v should not be Any");
        assert!(
            matches!(k_ty, LuaType::TplRef(_)),
            "k should be enclosing TplRef, got {:?}",
            k_ty
        );
        assert!(
            matches!(v_ty, LuaType::TplRef(_)),
            "v should be enclosing TplRef, got {:?}",
            v_ty
        );
        let k_debug = format!("{k_ty:?}");
        let v_debug = format!("{v_ty:?}");
        assert!(
            k_debug.contains("\"K\""),
            "k TplRef should be K, got {k_debug}"
        );
        assert!(
            v_debug.contains("\"V\""),
            "v TplRef should be V, got {v_debug}"
        );
    }

    #[test]
    fn pairs_over_caller_bound_composite_generic_table() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@generic K, V
            ---@param t table<K, V[]>
            local function genericTableComposite(t)
                for k, v in pairs(t) do end
            end
            "#,
        );

        let k_ty = local_type(&ws, file_id, "k");
        let v_ty = local_type(&ws, file_id, "v");
        assert_ne!(k_ty, LuaType::Any, "k should not be Any");
        assert_ne!(v_ty, LuaType::Any, "v should not be Any");
        assert!(
            matches!(k_ty, LuaType::TplRef(_)),
            "k should be enclosing TplRef, got {:?}",
            k_ty
        );
        let k_debug = format!("{k_ty:?}");
        assert!(
            k_debug.contains("\"K\""),
            "k TplRef should be K, got {k_debug}"
        );
        let v_debug = format!("{v_ty:?}");
        assert!(
            matches!(v_ty, LuaType::Array(_)) && v_debug.contains("\"V\""),
            "v should be Array of enclosing V, got {v_debug}"
        );
    }

    #[test]
    fn test_variadic_func() {
        let mut ws = crate::VirtualWorkspace::new();
        ws.def(
            r#"
        ---@generic T, R
        ---@param call async fun(...: T...): R...
        ---@return async fun(...: T...): R...
        function async_create(call)

        end


        ---@param a number
        ---@param b string
        ---@param c boolean
        ---@return number
        function locaf(a, b, c)

        end
        "#,
        );

        let ty = ws.expr_ty("async_create(locaf)");
        let expected = ws.ty("async fun(a: number, b: string, c:boolean): number...");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_merged_table_instantiates_generic_components() {
        let ws = crate::VirtualWorkspace::new();
        let mut substitutor = TypeSubstitutor::new();
        substitutor.insert_type(GenericTplId::Type(0), LuaType::String, true);

        let generic_type = LuaType::TplRef(
            GenericTpl::new(
                GenericTplId::Type(0),
                ArcIntern::new(SmolStr::new("T")),
                None,
            )
            .into(),
        );
        let merged_type = LuaMergedTableType::new(vec![generic_type, LuaType::Table]).into();

        let instantiated =
            instantiate_type_generic(ws.analysis.compilation.get_db(), &merged_type, &substitutor);

        let LuaType::MergedTable(merged) = instantiated else {
            panic!("expected merged table after generic instantiation");
        };
        assert_eq!(merged.get_types()[0], LuaType::String);
        assert_eq!(merged.get_types()[1], LuaType::Table);
    }

    #[test]
    fn test_select_type() {
        let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
        ---@param ... string
        function ffff(...)
            a, b, c = select(2, ...)
        end
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let b_ty = ws.expr_ty("b");
        let c_ty = ws.expr_ty("c");
        let expected = ws.ty("string");
        assert_eq!(a_ty, expected);
        assert_eq!(b_ty, expected);
        assert_eq!(c_ty, expected);

        ws.def(
            r#"
        e, f = select(2, "a", "b", "c")
        "#,
        );

        let e = ws.expr_ty("e");
        let expected = LuaType::String;
        let f = ws.expr_ty("f");
        let expected_f = LuaType::String;
        assert_eq!(e, expected);
        assert_eq!(f, expected_f);

        ws.def(
            r#"
        h = select('#', "a", "b")
        "#,
        );

        let h = ws.expr_ty("h");
        let expected = LuaType::IntegerConst(2);
        assert_eq!(h, expected);

        // select(n, func()) where func() returns multiple values
        ws.def(
            r#"
        ---@return integer, string
        function multi_ret() end
        g = select(2, multi_ret())
        "#,
        );

        let g = ws.expr_ty("g");
        let expected_g = ws.ty("string");
        assert_eq!(g, expected_g);
    }

    #[test]
    fn test_unpack() {
        let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        local h ---@type number[]
        a, b, c = table.unpack(h)
        "#,
        );

        let a = ws.expr_ty("a");
        let expected = ws.ty("number?");
        let b = ws.expr_ty("b");
        let expected_b = ws.ty("number?");
        let c = ws.expr_ty("c");
        let expected_c = ws.ty("number?");
        assert_eq!(a, expected);
        assert_eq!(b, expected_b);
        assert_eq!(c, expected_c);
    }

    #[test]
    fn test_return() {
        let mut ws = crate::VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class ab
                ---@field a number
                local A

                ---@generic T
                ---@param a T
                ---@return T
                local function name(a)
                    return a
                end

                local a = name(A)
                a.b = 1
                R = A.b
        "#,
        );

        let a = ws.expr_ty("R");
        // `name` returns its parameter unchanged, so `a` aliases `A` and the
        // subsequent field write is visible through both names.
        let expected = LuaType::IntegerConst(1);
        assert_eq!(a, expected);
    }

    #[test]
    fn test_issue_797() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
---@class Holder<T>

---@class C_StringHolder : Holder<string>

---@class C_StringHolderExt : C_StringHolder

---@class C_StringHolderWith<T> : Holder<string>

---@class C_StringHolderWithExt<T> : C_StringHolderWith<T>

---@alias A_StringHolder Holder<string>

---@alias A_StringHolderExt A_StringHolder

---@alias A_StringHolderWith<T> Holder<string>

---@alias A_StringHolderWithExt<T> A_StringHolderWith<T>

---@generic T
---@param v Holder<T>
---@return T
local function extract_holder(v) return v end

local direct ---@type Holder<string>

local class_a ---@type C_StringHolder
local class_b ---@type C_StringHolderExt
local class_c ---@type C_StringHolderWith<table>
local class_d ---@type C_StringHolderWithExt<table>

local alias_a ---@type A_StringHolder
local alias_b ---@type A_StringHolderExt
local alias_c ---@type A_StringHolderWith<table>
local alias_d ---@type A_StringHolderWithExt<table>

result = {
    direct = extract_holder(direct),

    class_a = extract_holder(class_a),
    class_b = extract_holder(class_b),
    class_c = extract_holder(class_c),
    class_d = extract_holder(class_d),

    alias_a = extract_holder(alias_a),
    alias_b = extract_holder(alias_b),
    alias_c = extract_holder(alias_c),
    alias_d = extract_holder(alias_d),
}
        "#,
        );

        let a = ws.expr_ty("result");
        let a_desc = humanize_type(
            ws.analysis.compilation.get_db(),
            &a,
            RenderLevel::DetailedCount(12),
        );
        let expected = r#"{
    direct: string,
    class_a: string,
    class_b: string,
    class_c: string,
    class_d: string,
    alias_a: string,
    alias_b: string,
    alias_c: string,
    alias_d: string,
}"#;
        assert_eq!(a_desc, expected);
    }

    #[test]
    fn test_call_generic() {
        let mut ws = crate::VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias Warp<T> T

            ---@generic T
            ---@param ... Warp<T>
            function test(...)
            end
        "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Warp<number>, Warp<string>
            local a, b
            test(a, b)
        "#,
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Warp<number>, Warp<string>
            local a, b
            test--[[@<number | string>]](a, b)
        "#,
        ));
    }

    #[test]
    fn test_generic_alias_instantiation() {
        let mut ws = crate::VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias Arrayable<T> T | T[]

            ---@class Suite

            ---@generic T
            ---@param value Arrayable<T>
            ---@return T[]
            function toArray(value)
            end
        "#,
        );

        ws.def(
            r#"
            ---@type Arrayable<Suite>
            local suite

            arraySuites = toArray(suite)
        "#,
        );

        let a = ws.expr_ty("arraySuites");
        let expected = ws.ty("Suite[]");
        assert_eq!(a, expected);
    }

    #[test]
    fn test_generic_alias_instantiation2() {
        let mut ws = crate::VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias Arrayable<T> T | T[]

            ---@class Suite

            ---@param value Arrayable<Suite>
            function toArray(value)

            end
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"

            ---@type Suite
            local suite

            local arraySuites = toArray(suite)
            "#
        ));
    }
}
