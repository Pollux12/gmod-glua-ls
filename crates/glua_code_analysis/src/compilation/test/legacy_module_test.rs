#[cfg(test)]
mod test {
    use glua_parser::{LuaAst, LuaAstNode, LuaAstToken, LuaLocalName};

    use crate::{Emmyrc, EmmyrcLuaVersion, LuaType, VirtualWorkspace};

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
}
