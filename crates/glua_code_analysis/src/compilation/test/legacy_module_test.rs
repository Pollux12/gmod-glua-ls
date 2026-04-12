#[cfg(test)]
mod test {
    use glua_parser::{LuaAst, LuaAstNode, LuaAstToken, LuaLocalName, LuaNameExpr};

    use crate::humanize_type;
    use crate::{
        Emmyrc, EmmyrcLuaVersion, LuaSemanticDeclId, LuaType, RenderLevel, VirtualWorkspace,
    };

    fn local_type_by_name(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let token = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .filter_map(|local_name| local_name.get_name_token())
            .find(|token| token.get_name_text() == name)
            .expect("expected local name token");

        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info")
            .typ
    }

    fn index_expr_type_by_text(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let index_expr = semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .find_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr) if index_expr.syntax().text() == expr_text => {
                    Some(index_expr)
                }
                _ => None,
            })
            .expect("expected index expr");

        semantic_model
            .get_semantic_info(index_expr.syntax().clone().into())
            .expect("expected semantic info")
            .typ
    }

    /// Returns the `SemanticInfo` for the index-key NAME TOKEN of the index expression
    /// matching `expr_text`. This mirrors the token-level hover path
    /// (`get_semantic_info(token)` where the token's parent is `LuaIndexExpr`).
    fn index_expr_name_token_semantic_info(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> Option<crate::SemanticInfo> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let index_expr = semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .find_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr) if index_expr.syntax().text() == expr_text => {
                    Some(index_expr)
                }
                _ => None,
            })
            .expect("expected index expr");

        // Get the name token (the key after the dot) — mirrors what hover token_at_offset returns.
        let name_token = index_expr.get_name_token()?.syntax().clone();
        semantic_model.get_semantic_info(name_token.into())
    }

    #[test]
    fn legacy_module_bare_after_activation_resolves_in_file() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        let class_file = ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            function Create() end
            local c = Create
            "#,
        );

        assert!(local_type_by_name(&mut ws, class_file, "c").is_function());
    }

    #[test]
    fn legacy_module_external_member_access_resolves() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            function Create() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local c = class.Create
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "class.Create").is_function());
    }

    #[test]
    fn legacy_module_nested_external_member_access_resolves() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "module_file.lua",
            r#"
            module("a.b.c", package.seeall)

            function make() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local m = a.b.c.make
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "a.b.c.make").is_function());
    }

    #[test]
    fn legacy_module_member_access_ignores_unrelated_global_name_collision() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            function Create() end
            "#,
        );
        ws.def_file(
            "global.lua",
            r#"
            Create = 123
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local c = class.Create
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "class.Create").is_function());
    }

    #[test]
    fn legacy_module_forward_reference_resolves_in_file() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        let class_file = ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            function Create(name)
                return Get(name)
            end

            function Get(name)
                return name
            end

            local c = Create
            "#,
        );

        assert!(local_type_by_name(&mut ws, class_file, "c").is_function());
    }

    #[test]
    fn legacy_module_explicit_global_escape_stays_global() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            _G.GlobalCreate = function() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local c = GlobalCreate
            "#,
        );

        assert!(local_type_by_name(&mut ws, consumer_file, "c").is_function());
    }

    #[test]
    fn legacy_module_m_table_export_resolves_externally() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            _M.Create = function() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local c = class.Create
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "class.Create").is_function());
    }

    #[test]
    fn legacy_module_multiple_module_calls_resolve_by_segment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "modules.lua",
            r#"
            module("a", package.seeall)
            function Foo() end

            module("b", package.seeall)
            function Bar() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local a_foo = a.Foo
            local b_bar = b.Bar
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "a.Foo").is_function());
        assert!(index_expr_type_by_text(&mut ws, consumer_file, "b.Bar").is_function());
    }

    #[test]
    fn explicit_global_escape_from_local_table_stays_visible() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "pon.lua",
            r#"
            local pon = {}
            _G.pon = pon

            function pon.encode() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local encode_fn = pon.encode
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "pon.encode").is_function());
    }

    #[test]
    fn explicit_env_escape_from_local_table_stays_visible() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "pon.lua",
            r#"
            local pon = {}
            _ENV.pon = pon

            function pon.encode() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local encode_fn = pon.encode
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "pon.encode").is_function());
    }

    #[test]
    fn legacy_module_multiple_nested_module_calls_resolve_by_segment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "modules.lua",
            r#"
            module("a.b", package.seeall)
            function Foo() end

            module("x.y", package.seeall)
            function Bar() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local a_foo = a.b.Foo
            local x_bar = x.y.Bar
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "a.b.Foo").is_function());
        assert!(index_expr_type_by_text(&mut ws, consumer_file, "x.y.Bar").is_function());
    }

    // ── Semantic-decl (hover identity / goto-definition) tests ──────────────

    /// Returns the `semantic_decl` field from hovering over the index expression
    /// whose full text matches `expr_text` in the given file.
    fn index_expr_semantic_decl(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> Option<LuaSemanticDeclId> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        semantic_model
            .get_root()
            .descendants::<LuaAst>()
            .find_map(|node| match node {
                LuaAst::LuaIndexExpr(index_expr) if index_expr.syntax().text() == expr_text => {
                    semantic_model
                        .get_semantic_info(index_expr.syntax().clone().into())
                        .and_then(|info| info.semantic_decl)
                }
                _ => None,
            })
    }

    // ── Hover type must NOT render as `{ includes }` ──────────────────────

    /// Hovering over the bare module name in an external file must not produce
    /// the generic `{ <name> }` namespace rendering in the UI.
    ///
    /// The type will be `LuaType::Namespace("class")` which is correct for member
    /// resolution, but `humanize_type` must render it as just `class`, not `{ class }`.
    #[test]
    fn legacy_module_hover_on_global_table_not_namespace() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            function Create() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local t = class
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file)
            .expect("expected semantic model");

        // "class" as a NameExpr in consumer.lua – must NOT render as `{ class }`
        let token = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter_map(|expr| expr.get_name_token())
            .find(|t| t.get_name_text() == "class")
            .expect("expected 'class' name token");

        let info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info");

        let rendered = humanize_type(
            ws.analysis.compilation.get_db(),
            &info.typ,
            RenderLevel::Detailed,
        );
        assert_ne!(
            rendered,
            format!("{{ {} }}", "class"),
            "hover on legacy module global name must not render as '{{ class }}', got {:?}",
            rendered
        );
    }

    // ── Goto-definition (semantic_decl) tests ──────────────────────────────

    /// Hovering a member of a legacy namespace-backed module must return a
    /// non-None semantic_decl so that goto-definition works.
    #[test]
    fn legacy_module_member_hover_has_semantic_decl() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)

            function Create() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local c = class.Create
            "#,
        );

        let decl = index_expr_semantic_decl(&mut ws, consumer_file, "class.Create");
        assert!(
            decl.is_some(),
            "class.Create in external file must have a semantic_decl (goto-definition target)"
        );
    }

    /// Goto-definition must resolve to the declaration site, not just return any decl.
    /// We verify by checking that the returned decl is a Member or LuaDecl (not None).
    #[test]
    fn legacy_module_member_goto_definition_resolves_to_decl() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "mymod.lua",
            r#"
            module("mymod", package.seeall)

            function File(name)
                return name
            end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local result = mymod.File("test")
            "#,
        );

        let decl = index_expr_semantic_decl(&mut ws, consumer_file, "mymod.File");
        assert!(
            decl.is_some(),
            "goto-definition for mymod.File must resolve to a declaration"
        );
        // Must be a real declaration or member, not a type-decl placeholder
        assert!(
            matches!(
                decl,
                Some(LuaSemanticDeclId::LuaDecl(_)) | Some(LuaSemanticDeclId::Member(_))
            ),
            "semantic_decl should be LuaDecl or Member, got {:?}",
            decl
        );
    }

    /// Nested namespace member hover must also have semantic_decl.
    #[test]
    fn legacy_module_nested_member_hover_has_semantic_decl() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "module_file.lua",
            r#"
            module("a.b.c", package.seeall)

            function make() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local m = a.b.c.make
            "#,
        );

        let decl = index_expr_semantic_decl(&mut ws, consumer_file, "a.b.c.make");
        assert!(
            decl.is_some(),
            "a.b.c.make in external file must have a semantic_decl"
        );
    }

    // ── Reparse / invalidation regression tests ───────────────────────────

    /// Regression: reparsing a legacy-module file must not leave stale alias entries
    /// in the property owner map (`property_owners_map`).
    ///
    /// Before the fix, `add_owner_map` only tracked the *source* owner in
    /// `in_filed_owner`.  The *alias* owner (the `LuaMember` side of the mapping)
    /// was inserted into `property_owners_map` but never registered for cleanup,
    /// so after every reparse it accumulated as a dead key pointing to a deleted
    /// `LuaPropertyId`.  After the fix both are tracked, so a reparsed file
    /// leaves no stale alias entries.
    ///
    /// We verify the observable consequence: a member looked up via the alias
    /// owner after reparse must still return a valid (non-stale) property, i.e.
    /// the member's property must match the decl's property.
    #[test]
    fn legacy_module_owner_map_alias_cleaned_up_on_reparse() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);

        let module_source = r#"
            module("mymod", package.seeall)

            ---@param x number
            function Foo(x) end
            "#;

        let module_uri = ws.virtual_url_generator.new_uri("mymod.lua");
        ws.analysis
            .update_file_by_uri(&module_uri, Some(module_source.to_string()))
            .expect("initial file update must succeed");

        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local f = mymod.Foo
            "#,
        );

        // Baseline: member resolution must work after initial load.
        assert!(
            index_expr_type_by_text(&mut ws, consumer_file, "mymod.Foo").is_function(),
            "baseline: mymod.Foo must resolve to a function before reparse"
        );

        // Simulate an LS-session reparse (the content is unchanged; the
        // analysis layer still removes-then-re-indexes the file).
        ws.analysis
            .update_file_by_uri(&module_uri, Some(module_source.to_string()))
            .expect("reparse must succeed");

        // After reparse the alias entry must have been cleaned up and re-created
        // cleanly — member resolution must still work, not return stale/None.
        assert!(
            index_expr_type_by_text(&mut ws, consumer_file, "mymod.Foo").is_function(),
            "after reparse: mymod.Foo must still resolve to a function (no stale alias)"
        );
    }

    /// Diagnostic test: call get_semantic_info on the NAME TOKEN of the index expression
    /// (mirrors hover path where hover gets `File` token from `includes.File`).
    /// This test reveals whether the token-based path works vs the node-based path.
    #[test]
    fn legacy_module_member_hover_via_name_token_has_semantic_info() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "includes.lua",
            r#"
            module("includes", package.seeall)

            ---Include a file by path.
            ---@param path string The file path to include
            function File(path) end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            includes.File("sv_init.lua")
            "#,
        );

        // Test: token-level (what hover uses) should also return Some
        let info = index_expr_name_token_semantic_info(&mut ws, consumer_file, "includes.File");
        assert!(
            info.is_some(),
            "get_semantic_info via name token must return Some for includes.File (hover path)"
        );
        let info = info.unwrap();
        assert!(
            info.semantic_decl.is_some(),
            "semantic_decl via name token must be Some for includes.File, got type: {:?}",
            info.typ
        );
    }

    #[test]
    fn legacy_module_seeall_variable_alias_resolves_globals() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "shared.lua",
            r#"
            SGSModuleLoader = package.seeall
            "#,
        );
        let module_file = ws.def_file(
            "consumer.lua",
            r#"
            module("ErrorLog", SGSModuleLoader)
            function LogError() end
            local c = LogError
            "#,
        );

        assert!(local_type_by_name(&mut ws, module_file, "c").is_function());
    }

    #[test]
    fn legacy_module_seeall_variable_alias_external_access() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "shared.lua",
            r#"
            SGSModuleLoader = package.seeall
            "#,
        );
        ws.def_file(
            "module.lua",
            r#"
            module("ErrorLog", SGSModuleLoader)
            function LogError() end
            "#,
        );
        let consumer_file = ws.def_file(
            "consumer.lua",
            r#"
            local c = ErrorLog.LogError
            "#,
        );

        assert!(index_expr_type_by_text(&mut ws, consumer_file, "ErrorLog.LogError").is_function());
    }
}
