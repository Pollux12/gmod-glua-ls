#[cfg(test)]
mod test {
    use std::collections::HashSet;
    use std::sync::Arc;

    use internment::ArcIntern;
    use smol_str::SmolStr;

    use crate::{
        GenericTpl, GenericTplId, GlobalId, LuaMemberOwner, LuaType, LuaUnionType,
        VirtualWorkspace, compilation::analyzer::settled_assign_write_committable,
    };

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

    fn registry_files(reg: &str, source: &str) -> Vec<(String, String)> {
        vec![
            (
                format!("lua/autorun/sh_{reg}_registry.lua"),
                format!(
                    r#"
                    {reg} = {{}}
                    {reg}.alpha = {{}}
                    {reg}.beta = {{}}
                    {reg}.gamma = {{}}
                    "#
                ),
            ),
            (
                format!("lua/autorun/sh_{reg}_write.lua"),
                format!(
                    r#"
                    function {reg}_Write(keys)
                        for _, entry in pairs({source}) do
                            for _, key in pairs(keys) do
                                entry[key] = {{ door = 1 }}
                            end
                        end
                    end
                    "#
                ),
            ),
            (
                format!("lua/autorun/sh_{reg}_read.lua"),
                format!(
                    r#"
                    function {reg}_Read(name, key)
                        return {reg}[name][key]
                    end

                    function {reg}_ReadThroughIterVar(key)
                        for _, entry in pairs({reg}) do
                            return entry[key]
                        end
                    end
                    "#
                ),
            ),
        ]
    }

    fn load_registry(reg: &str, source: &str, reverse: bool) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut files = registry_files(reg, source);
        if reverse {
            files.reverse();
        }
        ws.def_files(
            files
                .iter()
                .map(|(path, source)| (path.as_str(), source.as_str()))
                .collect(),
        );
        ws
    }

    fn wildcard_bucket_keys(ws: &VirtualWorkspace, reg: &str) -> Vec<String> {
        let owner = LuaMemberOwner::GlobalPath(GlobalId::new(&format!("{reg}.[]")));
        ws.analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .iter()
            .map(|member| format!("{:?}", member.get_key()))
            .collect()
    }

    #[test]
    fn pairs_value_var_write_files_onto_the_registry_wildcard_bucket() {
        let ws = load_registry("REGWRITE", "REGWRITE", false);
        assert_eq!(
            wildcard_bucket_keys(&ws, "REGWRITE"),
            vec!["ExprType(Unknown)".to_string()]
        );
    }

    /// Without the wildcard bucket this read answers `any?`, which is what the
    /// `may be nil` reports on such registries come from.
    #[test]
    fn pairs_value_var_write_is_read_back_through_the_registry_path() {
        let mut ws = load_registry("REGREAD", "REGREAD", false);
        let ty = ws.expr_ty("REGREAD_Read('alpha', 'door')");
        assert_eq!(ws.humanize_type(ty), "table");
    }

    /// Same member, reached through a second loop's value variable. Nullable
    /// here because the write is in another file, where a computed-key write is
    /// evidence about the values a registry holds, not that this key is one.
    #[test]
    fn pairs_value_var_write_is_read_back_through_another_iter_var() {
        let mut ws = load_registry("REGITER", "REGITER", false);
        let ty = ws.expr_ty("REGITER_ReadThroughIterVar('door')");
        assert_eq!(ws.humanize_type(ty), "table?");
    }

    #[test]
    fn registry_wildcard_bucket_does_not_depend_on_file_order() {
        let forward = load_registry("REGORDER", "REGORDER", false);
        let reverse = load_registry("REGORDER", "REGORDER", true);
        assert_eq!(
            wildcard_bucket_keys(&forward, "REGORDER"),
            wildcard_bucket_keys(&reverse, "REGORDER")
        );
        assert_eq!(
            wildcard_bucket_keys(&reverse, "REGORDER"),
            vec!["ExprType(Unknown)".to_string()]
        );
    }

    #[test]
    fn pairs_over_a_call_result_files_nothing() {
        let ws = load_registry("REGCALL", "GetRegistry()", false);
        assert_eq!(wildcard_bucket_keys(&ws, "REGCALL"), Vec::<String>::new());
        assert_eq!(
            wildcard_bucket_keys(&ws, "GetRegistry"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn pairs_over_a_local_registry_files_nothing() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "lua/autorun/sh_local_registry.lua",
            r#"
            local reg = {}
            reg.alpha = {}

            function LocalRegistryWrite(keys)
                for _, entry in pairs(reg) do
                    for _, key in pairs(keys) do
                        entry[key] = { door = 1 }
                    end
                end
            end
            "#,
        );
        assert_eq!(wildcard_bucket_keys(&ws, "reg"), Vec::<String>::new());
    }

    /// A mutable local assigned from a loop variable must settle with it. The
    /// loop enumerates a value nothing determines (`keys` is never called), so
    /// the loop variable holds a raw template placeholder through the walk and
    /// the unresolve waves, and only the settled tail falls it back to `any`.
    /// The assignment was bound (then floored) before that, so without a
    /// settled replay the local keeps the placeholder-era answer while the
    /// loop variable it copies has moved on.
    #[test]
    fn settled_loop_var_move_rederives_mutable_local_assignment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_ids = ws.def_files(vec![(
            "lua/autorun/a_settled_assign_use.lua",
            r#"
            local selected
            local function f(keys)
                for key in pairs(keys) do
                    selected = key
                end
            end
            "#,
        )]);

        // Read the cold-build decl caches directly: a global copy of `selected`
        // can answer from flow facts, which would mask a stale lifetime type
        // on the declaration itself.
        let loop_file = file_ids[0];
        let db = ws.get_db_mut();
        let decl_tree = db
            .get_decl_index()
            .get_decl_tree(&loop_file)
            .expect("loop file decl tree");
        let decl_id_by_name = |name: &str| {
            decl_tree
                .get_decls()
                .values()
                .filter(|decl| decl.get_name() == name)
                .map(|decl| decl.get_id())
                .next()
                .unwrap_or_else(|| panic!("{name} decl"))
        };
        let cached_ty = |db: &crate::DbIndex, decl_id: crate::LuaDeclId| {
            db.get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|cache| cache.as_type().clone())
        };
        let key_ty = cached_ty(db, decl_id_by_name("key")).expect("key cache");
        let selected_ty = cached_ty(db, decl_id_by_name("selected"));
        let _ = db;

        // Guards against passing vacuously: `any` is the deliberate settled
        // answer for a loop nothing determines (a loop over an `any` value
        // answers the same way), not an unresolved placeholder.
        assert_eq!(key_ty, crate::LuaType::Any);
        assert_eq!(selected_ty, Some(key_ty));
    }

    /// Reads the cold-build lifetime type cached on a declaration directly. A
    /// global copy of the local could answer from flow facts, which would mask
    /// a stale lifetime type on the declaration itself.
    fn settled_assign_decl_cached_ty(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> Option<LuaType> {
        let db = ws.get_db_mut();
        let decl_tree = db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .expect("decl tree");
        let decl_id = decl_tree
            .get_decls()
            .values()
            .filter(|decl| decl.get_name() == name)
            .map(|decl| decl.get_id())
            .next()
            .unwrap_or_else(|| panic!("{name} decl"));
        db.get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|cache| cache.as_type().clone())
    }

    /// A widening replay must commit: `selected` is seeded with `string` up
    /// front, so the settled tail cannot take the loop's grown key union
    /// through the placeholder rules — only the structural widening gate (the
    /// settled union holding every cached arm) admits it. A supersedes-only
    /// gate keeps the stale `string` forever.
    #[test]
    fn settled_assign_replay_commits_union_widening() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_ids = ws.def_files(vec![(
            "lua/autorun/a_settled_assign_widen.lua",
            r#"
            local tb = { s = "b" }
            local selected = "init"
            local function f()
                for kb in pairs(tb) do
                    selected = kb
                end
            end
            tb[true] = 1
            "#,
        )]);
        let loop_file = file_ids[0];
        let kb_ty = settled_assign_decl_cached_ty(&mut ws, loop_file, "kb").expect("kb cache");
        let selected_ty =
            settled_assign_decl_cached_ty(&mut ws, loop_file, "selected").expect("selected cache");
        assert_eq!(selected_ty, kb_ty);
        let LuaType::Union(union) = selected_ty else {
            panic!("expected widened key union, got {:?}", kb_ty);
        };
        let arms = union.into_set();
        assert!(arms.contains(&LuaType::String), "string arm in {arms:?}");
        assert!(
            arms.iter()
                .any(|arm| matches!(arm, LuaType::BooleanConst(_))),
            "boolean arm in {arms:?}"
        );
    }

    /// The claim holder's sideways replace follows a multi-declaration global
    /// merge. The reader file walks between the two backing writes (batches
    /// walk in URI order), so `m` is bound from the first backing table only;
    /// the settled multi-decl pass force-completes it to the merged backing
    /// tables, a shape structurally unrelated to the subset. Only the holder
    /// — the `selected = m` write that owns the declaration — may replace its
    /// own contribution with it; `supersedes` never relates two informative
    /// tables.
    #[test]
    fn settled_assign_replay_holder_sideways_multi_decl_merge() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_ids = ws.def_files(vec![
            ("lua/autorun/smd_a_first.lua", "SMD_MERGE = { k = \"s\" }\n"),
            (
                "lua/autorun/smd_m_reader.lua",
                "local m = SMD_MERGE\nlocal selected\nselected = m\n",
            ),
            ("lua/autorun/smd_z_second.lua", "SMD_MERGE = { j = 1 }\n"),
        ]);
        let reader_file = file_ids[1];
        let m_ty = settled_assign_decl_cached_ty(&mut ws, reader_file, "m").expect("m cache");
        let selected_ty = settled_assign_decl_cached_ty(&mut ws, reader_file, "selected")
            .expect("selected cache");
        assert_eq!(selected_ty, m_ty);
        // Both backing tables are visible in the merged global: a walk-time
        // subset could never answer both members, so `m` must have moved in
        // the settled tail and `selected` must have tracked it there.
        assert!(matches!(ws.expr_ty("SMD_MERGE.k"), LuaType::StringConst(_)));
        assert!(matches!(
            ws.expr_ty("SMD_MERGE.j"),
            LuaType::IntegerConst(_)
        ));
    }

    /// Source order elects the winner between two moved right-hand sides. The
    /// initializer seeds `string` and never moves, so the holder is not even
    /// queued; both loop writes widen it, and exactly one — the earliest —
    /// may commit per round. A second replace of the same cache within the
    /// round would leave the later union behind instead.
    #[test]
    fn settled_assign_replay_source_order_winner_over_two_moved_rhs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_ids = ws.def_files(vec![(
            "lua/autorun/a_settled_assign_order.lua",
            r#"
            local ta = { s = "a" }
            local tb = { s = "b" }
            local selected = "init"
            local function f()
                for ka in pairs(ta) do
                    selected = ka
                end
                for kb in pairs(tb) do
                    selected = kb
                end
            end
            ta[false] = true
            tb[true] = 1
            "#,
        )]);
        let loop_file = file_ids[0];
        let ka_ty = settled_assign_decl_cached_ty(&mut ws, loop_file, "ka").expect("ka cache");
        let kb_ty = settled_assign_decl_cached_ty(&mut ws, loop_file, "kb").expect("kb cache");
        let selected_ty =
            settled_assign_decl_cached_ty(&mut ws, loop_file, "selected").expect("selected cache");
        assert_ne!(ka_ty, kb_ty);
        assert_eq!(selected_ty, ka_ty);
    }

    /// A trailing variadic right-hand side supplies one slot per extra target:
    /// `first, second = echo(key)` queues both, and the replay selects each
    /// target's own slot out of the settled answer. The generic call fails
    /// while the loop variable still holds its template placeholder, so the
    /// walk defers both targets and the force wave floors them to `unknown`;
    /// only the settled replay — the extra target solely through its
    /// `ret_idx = 1` queue entry — can complete them to the settled answer.
    /// Without the multi-target queue the second target keeps `unknown`: it
    /// reads no cache of its own, so no other pass re-derives it.
    #[test]
    fn settled_assign_replay_derives_all_multi_targets() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_ids = ws.def_files(vec![(
            "lua/autorun/a_settled_assign_multi.lua",
            r#"
            ---@generic T
            ---@param x T
            ---@return T, T
            local function echo(x) return x, x end

            local first
            local second
            local function f(keys)
                for key in pairs(keys) do
                    first, second = echo(key)
                end
            end
            "#,
        )]);
        let loop_file = file_ids[0];
        let key_ty = settled_assign_decl_cached_ty(&mut ws, loop_file, "key").expect("key cache");
        let first_ty =
            settled_assign_decl_cached_ty(&mut ws, loop_file, "first").expect("first cache");
        let second_ty =
            settled_assign_decl_cached_ty(&mut ws, loop_file, "second").expect("second cache");
        // Guards against passing vacuously, as in the single-target repro:
        // `any` is the deliberate settled answer for this loop.
        assert_eq!(key_ty, crate::LuaType::Any);
        assert_eq!(first_ty, key_ty);
        assert_eq!(second_ty, key_ty);
    }

    /// The force wave and the settled replay share one target and must agree:
    /// the call write floors while its callee is still unresolved, the loop
    /// write replays once its variable settles, and the tail terminates on
    /// the completed value instead of alternating the two contributions.
    #[test]
    fn settled_assign_replay_converges_with_force_on_shared_target() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_ids = ws.def_files(vec![(
            "lua/autorun/a_settled_assign_force.lua",
            r#"
            local selected
            local function f(keys, mk)
                for key in pairs(keys) do
                    selected = key
                end
                selected = mk()
            end
            "#,
        )]);
        let loop_file = file_ids[0];
        let key_ty = settled_assign_decl_cached_ty(&mut ws, loop_file, "key").expect("key cache");
        let selected_ty =
            settled_assign_decl_cached_ty(&mut ws, loop_file, "selected").expect("selected cache");
        assert_eq!(key_ty, crate::LuaType::Any);
        assert_eq!(selected_ty, key_ty);
    }

    /// The holder-sideways clause admits only forward moves: the holder may
    /// replace its own contribution with an informative settled answer, never
    /// trade it for one that says nothing. `any` is authoritative, not an
    /// unfinished placeholder, so `any -> nil` (and `any -> unknown`, and an
    /// informative answer collapsing to `nil`) must reject even from the
    /// claim holder.
    #[test]
    fn settled_assign_gate_rejects_holder_backward_moves() {
        assert!(!settled_assign_write_committable(
            true,
            &LuaType::Nil,
            Some(&LuaType::Any),
        ));
        assert!(!settled_assign_write_committable(
            true,
            &LuaType::Unknown,
            Some(&LuaType::Any),
        ));
        assert!(!settled_assign_write_committable(
            true,
            &LuaType::Nil,
            Some(&LuaType::String),
        ));
    }

    /// A leaked template parameter is not an answer, so any write — holder or
    /// not — may replace it with an informative, leak-free settled type. A
    /// floored placeholder behaves the same way for a non-holder.
    #[test]
    fn settled_assign_gate_accepts_non_holder_leak_and_placeholder_to_concrete() {
        let leaked = LuaType::TplRef(Arc::new(GenericTpl::new(
            GenericTplId::Func(0),
            ArcIntern::new(SmolStr::new("T")),
            None,
        )));
        assert!(settled_assign_write_committable(
            false,
            &LuaType::String,
            Some(&leaked),
        ));
        assert!(settled_assign_write_committable(
            false,
            &LuaType::String,
            Some(&LuaType::Unknown),
        ));
    }

    /// The rest of the gate is unchanged: the holder still takes a genuinely
    /// sideways informative answer (the multi-declaration merge shape), an
    /// informative settled type still displaces `any` from any write, a
    /// settled leak or a settled answer that says nothing still commits from
    /// nowhere, and a non-holder still cannot move one informative answer to
    /// another.
    #[test]
    fn settled_assign_gate_keeps_sideways_and_any_ranking() {
        assert!(settled_assign_write_committable(
            true,
            &LuaType::Integer,
            Some(&LuaType::String),
        ));
        assert!(settled_assign_write_committable(
            false,
            &LuaType::String,
            Some(&LuaType::Any),
        ));
        assert!(!settled_assign_write_committable(
            false,
            &LuaType::Integer,
            Some(&LuaType::String),
        ));
        assert!(!settled_assign_write_committable(
            false,
            &LuaType::Nil,
            Some(&LuaType::String),
        ));
        assert!(!settled_assign_write_committable(
            true,
            &LuaType::TplRef(Arc::new(GenericTpl::new(
                GenericTplId::Func(1),
                ArcIntern::new(SmolStr::new("U")),
                None,
            ))),
            Some(&LuaType::String),
        ));
    }

    /// The settled tail advances several independent parts of the boundary
    /// across successive rounds — a widening union, a multi-declaration merge
    /// tracked through the holder-sideways clause, a deferred multi-target,
    /// and a member map that must be rekeyed before its loop re-derives. No
    /// part may observe a partially settled loop break: every part must
    /// converge to its settled answer.
    ///
    /// The recurrence-vs-advance distinction itself — the same net delta
    /// recurring while another part advances must not confirm a cycle — is
    /// pinned at the fuse predicate level (`FixpointFuse` tests), where it is
    /// directly constructible: with monotone commit gates an end-to-end run
    /// cannot move an owner back to a previous answer, so a recurring net
    /// delta is unconstructible from Lua. This test pins the other half of
    /// the contract: multi-part advancement always runs to convergence.
    #[test]
    fn settled_tail_multi_part_boundary_converges_without_early_break() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        // `def_files` returns ids in URI-sorted order, so the numeric prefixes
        // below are the indexing: the reader still walks between the two
        // backing writes, as in the single-purpose merge test.
        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/mp_1_widen.lua",
                r#"
                local tb = { s = "b" }
                local selected = "init"
                local function f()
                    for kb in pairs(tb) do
                        selected = kb
                    end
                end
                tb[true] = 1
                "#,
            ),
            (
                "lua/autorun/mp_2_smd_a_first.lua",
                "MP_MERGE = { k = \"s\" }\n",
            ),
            (
                "lua/autorun/mp_3_smd_m_reader.lua",
                "local m = MP_MERGE\nlocal selected\nselected = m\n",
            ),
            (
                "lua/autorun/mp_4_smd_z_second.lua",
                "MP_MERGE = { j = 1 }\n",
            ),
            (
                "lua/autorun/mp_5_multi.lua",
                r#"
                ---@generic T
                ---@param x T
                ---@return T, T
                local function echo(x) return x, x end

                local first
                local second
                local function f(keys)
                    for key in pairs(keys) do
                        first, second = echo(key)
                    end
                end
                "#,
            ),
            (
                "lua/autorun/mp_6_exprkey.lua",
                r#"
                local key = tostring(1)
                local t = {
                    [key] = 1,
                }

                for k in pairs(t) do
                    mp_key_out = k
                end
                "#,
            ),
        ]);

        let widen_file = file_ids[0];
        let kb_ty = settled_assign_decl_cached_ty(&mut ws, widen_file, "kb").expect("kb cache");
        let widened_ty =
            settled_assign_decl_cached_ty(&mut ws, widen_file, "selected").expect("selected cache");
        assert_eq!(widened_ty, kb_ty);
        let LuaType::Union(union) = widened_ty else {
            panic!("expected widened key union, got {:?}", kb_ty);
        };
        let arms = union.into_set();
        assert!(arms.contains(&LuaType::String), "string arm in {arms:?}");
        assert!(
            arms.iter()
                .any(|arm| matches!(arm, LuaType::BooleanConst(_))),
            "boolean arm in {arms:?}"
        );

        let reader_file = file_ids[2];
        let m_ty = settled_assign_decl_cached_ty(&mut ws, reader_file, "m").expect("m cache");
        let tracked_ty = settled_assign_decl_cached_ty(&mut ws, reader_file, "selected")
            .expect("selected cache");
        assert_eq!(tracked_ty, m_ty);
        assert!(matches!(ws.expr_ty("MP_MERGE.k"), LuaType::StringConst(_)));
        assert!(matches!(ws.expr_ty("MP_MERGE.j"), LuaType::IntegerConst(_)));

        let multi_file = file_ids[4];
        let key_ty = settled_assign_decl_cached_ty(&mut ws, multi_file, "key").expect("key cache");
        let first_ty =
            settled_assign_decl_cached_ty(&mut ws, multi_file, "first").expect("first cache");
        let second_ty =
            settled_assign_decl_cached_ty(&mut ws, multi_file, "second").expect("second cache");
        assert_eq!(key_ty, crate::LuaType::Any);
        assert_eq!(first_ty, key_ty);
        assert_eq!(second_ty, key_ty);

        assert_eq!(ws.expr_ty("mp_key_out"), LuaType::String);
    }
}
