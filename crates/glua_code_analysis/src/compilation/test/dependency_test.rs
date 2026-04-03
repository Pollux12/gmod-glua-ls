#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use crate::{LuaDependencyKind, VirtualWorkspace};

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
}
