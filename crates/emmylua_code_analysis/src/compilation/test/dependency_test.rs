#[cfg(test)]
mod test {
    use crate::{LuaDependencyKind, VirtualWorkspace};

    #[test]
    fn test_dependency_edge_kinds_for_require_include_and_addcsluafile() {
        let mut ws = VirtualWorkspace::new();

        let shared_id = ws.def_file("entities/test/shared.lua", "return {}");
        let client_id = ws.def_file("entities/test/cl_init.lua", "return {}");
        let required_id = ws.def_file("dep.lua", "return {}");
        let init_id = ws.def_file(
            "entities/test/init.lua",
            r#"
            include("shared.lua")
            AddCSLuaFile("cl_init.lua")
            local dep = require("dep")
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert_eq!(
            dependency_index.get_dependency_kind(&init_id, &shared_id),
            Some(LuaDependencyKind::Include)
        );
        assert_eq!(
            dependency_index.get_dependency_kind(&init_id, &client_id),
            Some(LuaDependencyKind::AddCSLuaFile)
        );
        assert_eq!(
            dependency_index.get_dependency_kind(&init_id, &required_id),
            Some(LuaDependencyKind::Require)
        );
    }

    #[test]
    fn test_dynamic_include_path_does_not_add_dependency_edge() {
        let mut ws = VirtualWorkspace::new();

        let _shared_id = ws.def_file("entities/test/shared.lua", "return {}");
        let init_id = ws.def_file(
            "entities/test/init.lua",
            r#"
            local file_name = "shared.lua"
            include(file_name)
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert!(dependency_index.get_required_files(&init_id).is_none());
    }
}
