#[cfg(test)]
mod test {
    use std::collections::HashSet;
    use std::sync::Arc;

    use crate::{LuaType, LuaUnionType, VirtualWorkspace};

    #[test]
    fn test_closure_param_infer() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@alias foo (fun(tbl: any): (number, string))

        ---@type foo
        local b = {}

        for k3, v3 in b do
            k1 = k3
            v1 = v3
        end


        ---@class bar
        ---@overload fun(tbl: any): (number, string)

        ---@type bar
        local c = {}

        for k4, v4 in c do
            k2 = k4
            v2 = v4
        end
        "#,
        );

        assert_eq!(ws.expr_ty("k1"), LuaType::Number);
        assert_eq!(ws.expr_ty("v1"), LuaType::String);
        assert_eq!(ws.expr_ty("k2"), LuaType::Number);
        assert_eq!(ws.expr_ty("v2"), LuaType::String);
    }

    #[test]
    fn test_issue_227() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        local a --- @type any

        for k in pairs(a) do
            -- k should be any not integer
            d = k
        end
        "#,
        );

        assert_eq!(ws.expr_ty("d"), LuaType::Any);
    }

    #[test]
    fn explicit_table_type_overrides_implicit_class_type_for_pairs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            local Entity = {}

            ---@class RefundData
            ---@field units number
            ---@field cost number?
            ---@field reason number
            ---@field pumpType string|number
            ---@type table<Entity, RefundData>
            Registry = Registry or {}

            local registry = Registry
            for pump, refund_data in pairs(registry) do
                pump_out = pump
                refund_data_out = refund_data
            end
            "#,
        );

        assert_eq!(ws.expr_ty("pump_out"), ws.ty("Entity"));
        assert_eq!(ws.expr_ty("refund_data_out"), ws.ty("RefundData"));
    }

    #[test]
    fn implicit_class_type_still_binds_owner_without_explicit_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ImplicitData
            data = {}
            "#,
        );

        let data_type = ws.expr_ty("data");
        assert_eq!(ws.humanize_type(data_type), "ImplicitData");
    }

    #[test]
    fn explicit_type_overrides_implicit_enum_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@enum Flags
            ---@type table<string, integer>
            flags = {}
            "#,
        );

        assert_eq!(ws.expr_ty("flags"), ws.ty("table<string, integer>"));
    }

    #[test]
    fn schema_tag_skips_implicit_class_owner_type_before_resolution() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class SchemaShape
            ---@schema "not a URL"
            local value = {}
            "#,
        );

        assert_ne!(ws.expr_ty("value"), ws.ty("SchemaShape"));
    }

    #[test]
    fn test_issue_321() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        ---@return fun():string?
        local function test(...) end

        for k in test() do
            -- k can't be nil
            d = k
        end
        "#,
        );

        assert_eq!(ws.expr_ty("d"), LuaType::String);
    }

    #[test]
    fn test_issue_490() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@generic T: table, K, V
        ---@param t T
        ---@return fun(table: table<K, V>, index?: K):K, V
        ---@return T
        local function spairs(t) end

        --- @type table<string, integer>
        local t = { a = 1, b = 2, c = 3 }
        for name, value in spairs(t) do
            a = name
            b = value
        end
        "#,
        );

        let a = ws.expr_ty("a");
        let b = ws.expr_ty("b");
        assert_eq!(a, LuaType::String);
        assert_eq!(b, LuaType::Integer);
    }

    #[test]
    fn test_enum_key_pairs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            --- @enum Severity
            local severity = {
                ERROR = 1,
                WARN = 2,
                INFO = 3,
                HINT = 4,
            }

            local severities = {
                [severity.ERROR] = 1,
                [severity.WARN] = 2,
                [severity.INFO] = 3,
                [severity.HINT] = 4,
            }

            for k in pairs(severities) do
                key = k
            end
        "#,
        );

        let key_ty = ws.expr_ty("key");
        let LuaType::Union(union) = key_ty else {
            panic!("expected enum key union, got {:?}", key_ty);
        };
        let set = union.into_set();
        let expected: HashSet<_> = vec![
            LuaType::IntegerConst(1),
            LuaType::IntegerConst(2),
            LuaType::IntegerConst(3),
            LuaType::IntegerConst(4),
        ]
        .into_iter()
        .collect();
        assert_eq!(set, expected);
    }

    #[test]
    fn test_pairs_expr_key_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            local key = tostring(1)
            local t = {
                [key] = 1,
            }

            for k in pairs(t) do
                key_out = k
            end
        "#,
        );

        assert_eq!(ws.expr_ty("key_out"), LuaType::String);
    }

    #[test]
    fn pairs_over_table_projection_keeps_compact_key_and_symbolic_value_types() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field KnownField string
            ---@field KnownMethod fun(self: Entity)

            ---@return tableof<Entity>
            local function GetTable() end

            for key, value in pairs(GetTable()) do
                key_out = key
                value_out = value
            end
            "#,
        );

        let key_type = ws.expr_ty("key_out");
        let value_type = ws.expr_ty("value_out");
        assert_eq!(ws.humanize_type(key_type), "string");
        assert_eq!(ws.humanize_type(value_type), "index<Entity,string>");
    }

    #[test]
    fn pairs_over_table_projection_preserves_mixed_key_categories() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class MixedTable
            ---@field [integer] boolean
            ---@field named string

            ---@return tableof<MixedTable>
            local function GetTable() end

            for key, value in pairs(GetTable()) do
                key_out = key
                value_out = value
            end
            "#,
        );

        assert_eq!(
            ws.expr_ty("key_out"),
            LuaType::from_vec(vec![LuaType::Integer, LuaType::String])
        );
        let value_type = ws.expr_ty("value_out");
        assert_eq!(
            ws.humanize_type(value_type),
            "index<MixedTable,(integer|string)>"
        );
    }

    #[test]
    fn pairs_over_generic_table_projection_keeps_symbolic_generic_key() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@generic T
            ---@class GenericTable<T>
            ---@field [T] boolean

            ---@return tableof<GenericTable<string>>
            local function GetTable() end

            for key, value in pairs(GetTable()) do
                key_out = key
                value_out = value
            end
            "#,
        );

        let key_type = ws.expr_ty("key_out");
        let value_type = ws.expr_ty("value_out");
        assert_eq!(ws.humanize_type(key_type), "keyof<GenericTable<string>>");
        assert_eq!(
            ws.humanize_type(value_type),
            "index<GenericTable<string>,keyof<GenericTable<string>>>"
        );
    }

    #[test]
    fn test_issue_291() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            --- @class A
            --- @field [integer] string
            --- @field a boolean
            --- @field b number
            local a

            for _, v in ipairs(a) do
                d = v
            end
        "#,
        );

        assert_eq!(ws.expr_ty("d"), LuaType::String);
    }

    #[test]
    fn test_issue_291_2() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            --- @class A
            --- @field [1] string
            --- @field [2] number
            local a

            for _, v in ipairs(a) do
                d = v
            end
        "#,
        );

        assert_eq!(
            ws.expr_ty("d"),
            LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
                LuaType::String,
                LuaType::Number
            ]))),
        );
    }

    #[test]
    fn test_pairs_array_records_use_compact_object_value_shape() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            local records = {
                { pos = 1, name = "a" },
                { pos = 2, name = "b", optional = true },
            }

            for key, value in pairs(records) do
                key_out = key
                value_out = value
                pos_out = value.pos
                optional_out = value.optional
            end
        "#,
        );

        let LuaType::Union(key_union) = ws.expr_ty("key_out") else {
            panic!("expected exact small-table key union");
        };
        let key_set = key_union.into_set();
        assert!(key_set.contains(&LuaType::IntegerConst(1)));
        assert!(key_set.contains(&LuaType::IntegerConst(2)));
        assert!(matches!(ws.expr_ty("value_out"), LuaType::Object(_)));
        let LuaType::Union(pos_union) = ws.expr_ty("pos_out") else {
            panic!("expected exact small-table field union");
        };
        let pos_set = pos_union.into_set();
        assert!(pos_set.contains(&LuaType::IntegerConst(1)));
        assert!(pos_set.contains(&LuaType::IntegerConst(2)));
        assert_eq!(
            ws.expr_ty("optional_out"),
            LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
                LuaType::BooleanConst(true),
                LuaType::Nil,
            ]))),
        );
    }

    #[test]
    fn pairs_defers_iter_vars_when_generic_leaves_template_refs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_ids = ws.def_files(vec![
            (
                "a_loop.lua",
                r#"
                local packages = Store.packages or {}

                for pkg, data in pairs(packages) do
                    pkg_out = pkg
                    data_out = data
                end
                "#,
            ),
            (
                "b_writer.lua",
                r#"
                Store = Store or {}
                Store.packages = Store.packages or {}

                ---@class Package
                ---@field id number

                ---@param pkg Package
                function AddPackage(pkg)
                    Store.packages[pkg.id] = pkg
                end
                "#,
            ),
        ]);

        let loop_file = file_ids[0];
        let db = ws.get_db_mut();
        let decl_tree = db
            .get_decl_index()
            .get_decl_tree(&loop_file)
            .expect("loop file decl tree");
        let iter_decls = decl_tree
            .get_decls()
            .values()
            .filter(|decl| matches!(decl.get_name(), "pkg" | "data"))
            .map(|decl| (decl.get_name().to_string(), decl.get_id()))
            .collect::<Vec<_>>();
        assert_eq!(iter_decls.len(), 2, "expected both iterator var decls");

        for (name, decl_id) in iter_decls {
            let cached = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|cache| cache.as_type().clone());
            assert!(
                cached.as_ref().is_some_and(|typ| !typ.contain_tpl()),
                "iter var `{name}` froze as a raw template ref: {cached:?}"
            );
        }
    }

    #[test]
    fn test_pairs_nil_only_values_fall_back_to_unknown() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@type table<string, nil>
            local t = {
                a = nil,
                b = nil,
            }

            for _, v in pairs(t) do
                value_out = v
            end
        "#,
        );

        assert_eq!(ws.expr_ty("value_out"), LuaType::Unknown);
    }

    /// A loop variable that still holds a raw template ref is a placeholder, and
    /// facts derived from it inside the body are deferred rather than committed.
    /// The deferral must not swallow a body whose template really is bound by
    /// the enclosing generic: those types still have to be published.
    #[test]
    fn generic_iterator_body_still_publishes_its_loop_derived_types() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@generic K, V
            ---@param source table<K, V>
            local function walk(source)
                for key, value in pairs(source) do
                    local seen_key = key
                    local seen_value = value
                    return seen_key, seen_value
                end
            end

            key_out, value_out = walk({ [1] = "a" })
        "#,
        );

        assert!(!ws.expr_ty("key_out").contain_tpl());
        assert!(!ws.expr_ty("value_out").contain_tpl());
    }

    /// The container is declared in a file analysed *after* the loop, so the
    /// iteration variable starts as a placeholder and is repaired once the
    /// member map settles. A plain local copying it — taken after a conditional
    /// early exit, which is what splits the copy off into its own flow branch —
    /// has to be repaired with it. Copying the placeholder instead left the
    /// alias at `nil` while the variable it copied resolved correctly, so the
    /// same value held two incompatible types two lines apart.
    #[test]
    fn pairs_alias_after_early_exit_matches_the_loop_variable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_files(vec![
            (
                "a_use.lua",
                r#"
                for k, def in pairs(MCP.KNOBS) do
                    if k == "" then return end

                    local alias = def

                    def_out = def
                    alias_out = alias
                end
                "#,
            ),
            (
                "z_decl.lua",
                r#"
                MCP = {}

                ---@type table<string, { a: string }>
                MCP.KNOBS = {}
                "#,
            ),
        ]);

        let def_ty = ws.expr_ty("def_out");
        let alias_ty = ws.expr_ty("alias_out");

        // Guards against passing vacuously: if the container stopped resolving,
        // both sides would agree on `unknown` and the assertion below would hold
        // for the wrong reason.
        assert_eq!(ws.humanize_type(def_ty.clone()), "{ a: string }");
        assert_eq!(ws.humanize_type(alias_ty.clone()), "{ a: string }");
        assert_eq!(def_ty, alias_ty);
    }
}
