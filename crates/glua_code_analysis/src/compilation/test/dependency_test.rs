#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use crate::{Emmyrc, LuaDependencyKind, VirtualWorkspace};

    #[gtest]
    fn test_dependency_edge_kinds_for_require_include_and_addcsluafile() {
        let mut ws = VirtualWorkspace::new();

        let shared_id = ws.def_file("lua/entities/test/shared.lua", "return {}");
        let client_id = ws.def_file("lua/entities/test/cl_init.lua", "return {}");
        let required_id = ws.def_file("lua/dep.lua", "return {}");
        let init_id = ws.def_file(
            "lua/entities/test/init.lua",
            r#"
            include("shared.lua")
            AddCSLuaFile("cl_init.lua")
            local dep = require("dep")
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        let kinds = dependency_index
            .get_dependency_kinds(&init_id, &shared_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::Include)]
        );

        let kinds = dependency_index
            .get_dependency_kinds(&init_id, &client_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::AddCSLuaFile)]
        );

        let kinds = dependency_index
            .get_dependency_kinds(&init_id, &required_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::Require)]
        );
    }

    /// `include` takes a path relative to the including file before one
    /// relative to `lua/`, so every entity's `include("shared.lua")` reaches
    /// its own `shared.lua`.
    #[gtest]
    fn a_relative_include_resolves_next_to_the_including_file() {
        let mut ws = VirtualWorkspace::new();
        let alpha_shared = ws.def_file("lua/entities/alpha/shared.lua", "return {}");
        let beta_shared = ws.def_file("lua/entities/beta/shared.lua", "return {}");
        ws.def_file("lua/entities/alpha/init.lua", "include(\"shared.lua\")\n");
        let beta_init = ws.def_file("lua/entities/beta/init.lua", "include(\"shared.lua\")\n");

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        expect_that!(
            dependency_index.get_dependency_kinds(&beta_init, &beta_shared),
            some(unordered_elements_are![eq(&LuaDependencyKind::Include)])
        );
        expect_that!(
            dependency_index.get_dependency_kinds(&beta_init, &alpha_shared),
            none()
        );
    }

    #[gtest]
    fn test_annotated_load_call_adds_dependency_edge() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let target_id = ws.def_file("lua/menu/mount/vgui/workshop.lua", "return {}");
        let source_id = ws.def_file(
            "lua/menu/mount/mount.lua",
            r#"
            vgui = vgui or {}

            ---@[call_arg("gmod.load", "include")]
            ---@param file string
            function vgui.RegisterFile(file) end

            vgui.RegisterFile("vgui/workshop.lua")
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        let kinds = dependency_index
            .get_dependency_kinds(&source_id, &target_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::Include)]
        );
    }

    /// A load call the builtins also annotate as one is one dependency site,
    /// ranged over its path argument.
    #[test]
    fn an_annotated_include_is_one_site_over_its_path() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/autorun/helper.lua",
            "HelperValue = 1
",
        );
        let boot_id = ws.def_file(
            "lua/autorun/sv_boot.lua",
            "include(\"helper.lua\")
",
        );

        let ranges = ws
            .get_db_mut()
            .get_file_dependencies_index()
            .get_dependency_sites(&boot_id)
            .unwrap_or_default()
            .iter()
            .map(|site| (u32::from(site.range.start()), u32::from(site.range.end())))
            .collect::<Vec<_>>();
        assert_eq!(ranges, vec![(8, 20)]);
    }

    #[test]
    fn test_dynamic_include_path_does_not_add_dependency_edge() {
        let mut ws = VirtualWorkspace::new();

        let _shared_id = ws.def_file("lua/entities/test/shared.lua", "return {}");
        let init_id = ws.def_file(
            "lua/entities/test/init.lua",
            r#"
            local file_name = "shared.lua"
            include(file_name)
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert!(dependency_index.get_required_files(&init_id).is_none());
    }

    #[gtest]
    fn test_includecs_without_argument_does_not_add_dependency_edge() {
        let mut ws = VirtualWorkspace::new();

        let source_id = ws.def_file("lua/autorun/server/sv_boot.lua", "IncludeCS()");
        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        assert!(dependency_index.get_required_files(&source_id).is_none());
    }

    /// A file indexed in the same batch after its includer is still the
    /// include's target: a cold build must not depend on batch order.
    #[test]
    fn test_include_resolves_to_a_file_later_in_the_same_batch() {
        let mut ws = VirtualWorkspace::new();

        let ids = ws.def_files(vec![
            (
                "lua/autorun/includer.lua",
                r#"include("mixins/included.lua")"#,
            ),
            ("lua/mixins/included.lua", "return {}"),
        ]);

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert!(
            dependency_index
                .get_required_files(&ids[0])
                .is_some_and(|files| files.contains(&ids[1]))
        );
    }
}
