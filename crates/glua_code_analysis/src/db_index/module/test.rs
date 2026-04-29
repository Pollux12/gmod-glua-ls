#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::{
        Emmyrc, FileId, WorkspaceId,
        db_index::{WorkspaceKind, module::LuaModuleIndex, traits::LuaIndex},
    };

    fn create_module() -> LuaModuleIndex {
        let mut m = LuaModuleIndex::new();
        m.set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        m
    }

    #[test]
    fn test_basic() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test.lua");
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.name, "test");
        assert_eq!(module_info.full_module_name, "test");
        assert_eq!(module_info.visible, true);

        let file_id = FileId { id: 2 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test2/init.lua");
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.name, "test2");
        assert_eq!(module_info.full_module_name, "test2");
        assert_eq!(module_info.visible, true);

        let file_id = FileId { id: 3 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test3/hhhhiii.lua");
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.name, "hhhhiii");
        assert_eq!(module_info.full_module_name, "test3.hhhhiii");
        assert_eq!(module_info.visible, true);
    }

    #[test]
    fn test_multi_workspace() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        m.add_workspace_root(
            Path::new("C:/Users/username/Downloads").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test.lua");
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.name, "test");
        assert_eq!(module_info.full_module_name, "test");
        assert_eq!(module_info.visible, true);

        let file_id = FileId { id: 2 };
        m.add_module_by_path(file_id, "C:/Users/username/Downloads/test2/init.lua");
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.name, "test2");
        assert_eq!(module_info.full_module_name, "test2");
        assert_eq!(module_info.visible, true);

        let file_id = FileId { id: 3 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test3/hhhhiii.lua");
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.name, "hhhhiii");
        assert_eq!(module_info.full_module_name, "test3.hhhhiii");
        assert_eq!(module_info.visible, true);
    }

    #[test]
    fn test_find_module() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test.lua");
        let module_info = m.find_module("test").unwrap();
        assert_eq!(module_info.name, "test");
        assert_eq!(module_info.full_module_name, "test");
        assert_eq!(module_info.visible, true);

        let file_id = FileId { id: 2 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test2/init.lua");
        let module_info = m.find_module("test2").unwrap();
        assert_eq!(module_info.name, "test2");
        assert_eq!(module_info.full_module_name, "test2");
        assert_eq!(module_info.visible, true);

        let file_id = FileId { id: 3 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test3/hhhhiii.lua");
        let module_info = m.find_module("test3.hhhhiii").unwrap();
        assert_eq!(module_info.name, "hhhhiii");
        assert_eq!(module_info.full_module_name, "test3.hhhhiii");
        assert_eq!(module_info.visible, true);

        let not_found = m.find_module("test3.hhhhiii.notfound");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_find_module_by_path() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(
            file_id,
            "C:/Users/username/Documents/entities/test/shared.lua",
        );
        let module_info = m
            .find_module_by_path(Path::new(
                "C:/Users/username/Documents/entities/test/shared.lua",
            ))
            .unwrap();
        assert_eq!(module_info.name, "shared");
        assert_eq!(module_info.full_module_name, "entities.test.shared");
    }

    #[test]
    fn test_find_module_node() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test.lua");
        let file_id = FileId { id: 2 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test/aaa.lua");
        let file_id = FileId { id: 3 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test/hhhhiii.lua");

        let module_node = m.find_module_node("test").unwrap();
        assert_eq!(module_node.children.len(), 2);
        let first_child = module_node.children.get("aaa");
        assert!(first_child.is_some());
        let second_child = module_node.children.get("hhhhiii");
        assert!(second_child.is_some());
    }

    #[test]
    fn test_set_module_visibility() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test.lua");
        m.set_module_visibility(file_id, false);
        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.visible, false);
    }

    #[test]
    fn test_remove_module() {
        let mut m = create_module();
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );
        let file_id = FileId { id: 1 };
        m.add_module_by_path(file_id, "C:/Users/username/Documents/test.lua");
        m.remove(file_id);
        let module_info = m.get_module(file_id);
        assert!(module_info.is_none());

        let file_id = FileId { id: 2 };
        m.add_module_by_path(
            file_id,
            "C:/Users/username/Documents/test2/aaa/bbb/cccc/dddd.lua",
        );
        m.remove(file_id);
        let module_info = m.get_module(file_id);
        assert!(module_info.is_none());
        let module_node = m.find_module_node("test2.aaa");
        assert!(module_node.is_none());
    }

    #[test]
    fn test_find_module_in_workspace_isolates_main_roots() {
        let mut m = create_module();
        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };

        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );

        let file_a = FileId { id: 10 };
        m.add_module_by_path(file_a, "C:/Users/username/ProjectA/shared.lua");

        let file_b = FileId { id: 11 };
        m.add_module_by_path(file_b, "C:/Users/username/ProjectB/shared.lua");

        let only_b = FileId { id: 12 };
        m.add_module_by_path(only_b, "C:/Users/username/ProjectB/only_b.lua");

        let shared_a = m.find_module_in_workspace("shared", workspace_a).unwrap();
        assert_eq!(shared_a.file_id, file_a);

        let shared_b = m.find_module_in_workspace("shared", workspace_b).unwrap();
        assert_eq!(shared_b.file_id, file_b);

        let hidden_from_a = m.find_module_in_workspace("only_b", workspace_a);
        assert!(hidden_from_a.is_none());

        let visible_from_b = m.find_module_in_workspace("only_b", workspace_b).unwrap();
        assert_eq!(visible_from_b.file_id, only_b);
    }

    #[test]
    fn test_find_module_in_workspace_allows_cross_main_roots_when_isolation_disabled() {
        let mut m = create_module();
        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };

        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );

        let mut emmyrc = Emmyrc::default();
        emmyrc.workspace.enable_isolation = false;
        m.update_config(Arc::new(emmyrc));

        let file_a = FileId { id: 20 };
        m.add_module_by_path(file_a, "C:/Users/username/ProjectA/shared.lua");

        let file_b = FileId { id: 21 };
        m.add_module_by_path(file_b, "C:/Users/username/ProjectB/shared.lua");

        let only_b = FileId { id: 22 };
        m.add_module_by_path(only_b, "C:/Users/username/ProjectB/only_b.lua");

        let shared_a = m.find_module_in_workspace("shared", workspace_a).unwrap();
        assert_eq!(shared_a.file_id, file_a);

        let shared_b = m.find_module_in_workspace("shared", workspace_b).unwrap();
        assert_eq!(shared_b.file_id, file_b);

        let visible_from_a = m.find_module_in_workspace("only_b", workspace_a).unwrap();
        assert_eq!(visible_from_a.file_id, only_b);
    }

    #[test]
    fn test_extract_module_path_with_mixed_separators_prefers_library_workspace() {
        let mut m = create_module();
        let workspace_main = WorkspaceId::MAIN;
        let workspace_lib = WorkspaceId { id: 3 };

        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/Project").into(),
            workspace_main,
            WorkspaceKind::Main,
        );
        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/Project/lua/lib").into(),
            workspace_lib,
            WorkspaceKind::Library,
        );

        let file_id = FileId { id: 50 };
        m.add_module_by_path(
            file_id,
            "C:\\Users\\username\\Project\\lua\\lib\\globals.lua",
        );

        let module_info = m.get_module(file_id).unwrap();
        assert_eq!(module_info.workspace_id, workspace_lib);
        assert_eq!(module_info.name, "globals");
    }

    #[test]
    fn test_get_main_workspace_roots_returns_only_main_workspaces() {
        let mut m = create_module();
        let main_a = PathBuf::from("C:/Users/username/ProjectA");
        let main_b = PathBuf::from("C:/Users/username/ProjectB");
        let library = PathBuf::from("C:/Users/username/Annotations");
        let std_root = PathBuf::from("C:/Users/username/Std");

        m.add_workspace_root_with_kind(main_a.clone(), WorkspaceId::MAIN, WorkspaceKind::Main);
        m.add_workspace_root_with_kind(main_b.clone(), WorkspaceId { id: 7 }, WorkspaceKind::Main);
        m.add_workspace_root_with_kind(library, WorkspaceId { id: 8 }, WorkspaceKind::Library);
        m.add_workspace_root_with_kind(std_root, WorkspaceId::STD, WorkspaceKind::Std);

        assert_eq!(m.get_main_workspace_roots(), vec![main_a, main_b]);
    }

    #[test]
    fn test_require_fuzzy_match_honors_segment_boundaries() {
        let mut m = LuaModuleIndex::new();
        m.update_config(Arc::new(Emmyrc::default()));
        m.add_workspace_root(
            Path::new("C:/Users/username/Documents").into(),
            WorkspaceId::MAIN,
        );

        let file_id = FileId { id: 1 };
        m.add_module_by_path(
            file_id,
            "C:/Users/username/Documents/nvim-cmp/lua/cmp/utils/event.lua",
        );

        assert!(m.find_module("pckr.event").is_none());
        let module_info = m.find_module("event").unwrap();
        assert_eq!(module_info.full_module_name, "nvim-cmp.lua.cmp.utils.event");
    }

    #[test]
    fn test_require_fuzzy_match_prefers_shortest_prefix_independent_of_insert_order() {
        const PLUGIN_ENTRY: &str = "C:/Users/username/Documents/plugin/treesitter-context.lua";
        const LUA_ENTRY: &str = "C:/Users/username/Documents/lua/treesitter-context.lua";

        // Validate both insertion orders to ensure lookup does not depend on indexing order.
        for paths in [[PLUGIN_ENTRY, LUA_ENTRY], [LUA_ENTRY, PLUGIN_ENTRY]] {
            let mut m = LuaModuleIndex::new();
            m.update_config(Arc::new(Emmyrc::default()));
            m.add_workspace_root(
                Path::new("C:/Users/username/Documents").into(),
                WorkspaceId::MAIN,
            );

            for (file_id, path) in [FileId { id: 1 }, FileId { id: 2 }].into_iter().zip(paths) {
                m.add_module_by_path(file_id, path);
            }

            let module_info = m.find_module("treesitter-context").unwrap();
            assert_eq!(module_info.full_module_name, "lua.treesitter-context");
        }
    }

    #[test]
    fn test_workspace_file_id_apis_are_sorted() {
        let mut m = create_module();
        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/Project").into(),
            WorkspaceId::MAIN,
            WorkspaceKind::Main,
        );
        m.add_workspace_root_with_kind(
            Path::new("C:/Users/username/Annotations").into(),
            WorkspaceId { id: 3 },
            WorkspaceKind::Library,
        );

        m.add_module_by_module_path(FileId { id: 30 }, "main_c".to_string(), WorkspaceId::MAIN);
        m.add_module_by_module_path(FileId { id: 10 }, "main_a".to_string(), WorkspaceId::MAIN);
        m.add_module_by_module_path(FileId { id: 20 }, "main_b".to_string(), WorkspaceId::MAIN);

        m.add_module_by_module_path(
            FileId { id: 50 },
            "lib_b".to_string(),
            WorkspaceId { id: 3 },
        );
        m.add_module_by_module_path(
            FileId { id: 40 },
            "lib_a".to_string(),
            WorkspaceId { id: 3 },
        );

        assert_eq!(
            m.get_main_workspace_file_ids(),
            vec![FileId { id: 10 }, FileId { id: 20 }, FileId { id: 30 }]
        );
        assert_eq!(
            m.get_lib_file_ids(),
            vec![FileId { id: 40 }, FileId { id: 50 }]
        );
    }

    #[test]
    fn test_find_module_duplicate_prefers_main_workspace_independent_of_insert_order() {
        for file_ids in [
            [FileId { id: 1 }, FileId { id: 2 }],
            [FileId { id: 2 }, FileId { id: 1 }],
        ] {
            let mut m = create_module();
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/Project").into(),
                WorkspaceId::MAIN,
                WorkspaceKind::Main,
            );
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/Annotations").into(),
                WorkspaceId { id: 3 },
                WorkspaceKind::Library,
            );

            m.add_module_by_module_path(file_ids[0], "shared".to_string(), WorkspaceId { id: 3 });
            m.add_module_by_module_path(file_ids[1], "shared".to_string(), WorkspaceId::MAIN);

            let resolved = m.find_module("shared").unwrap();
            assert_eq!(resolved.workspace_id, WorkspaceId::MAIN);
        }
    }

    #[test]
    fn test_find_module_in_workspace_duplicate_main_uses_workspace_order() {
        for paths in [
            [
                (
                    "C:/Users/username/ProjectB/shared.lua",
                    WorkspaceId { id: 3 },
                ),
                ("C:/Users/username/ProjectA/shared.lua", WorkspaceId::MAIN),
            ],
            [
                ("C:/Users/username/ProjectA/shared.lua", WorkspaceId::MAIN),
                (
                    "C:/Users/username/ProjectB/shared.lua",
                    WorkspaceId { id: 3 },
                ),
            ],
        ] {
            let mut m = create_module();
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/ProjectA").into(),
                WorkspaceId::MAIN,
                WorkspaceKind::Main,
            );
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/ProjectB").into(),
                WorkspaceId { id: 3 },
                WorkspaceKind::Main,
            );
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/ProjectC").into(),
                WorkspaceId { id: 4 },
                WorkspaceKind::Main,
            );

            let mut emmyrc = Emmyrc::default();
            emmyrc.workspace.enable_isolation = false;
            m.update_config(Arc::new(emmyrc));

            m.add_module_by_path(FileId { id: 1 }, paths[0].0);
            m.add_module_by_path(FileId { id: 2 }, paths[1].0);

            let resolved = m
                .find_module_in_workspace("shared", WorkspaceId { id: 4 })
                .unwrap();
            assert_eq!(resolved.workspace_id, WorkspaceId::MAIN);
        }
    }

    #[test]
    fn test_find_module_duplicate_same_workspace_uses_lexical_path_priority() {
        for insertion_order in [
            [
                (FileId { id: 20 }, "C:/Users/username/Project/a/shared.lua"),
                (FileId { id: 10 }, "C:/Users/username/Project/b/shared.lua"),
            ],
            [
                (FileId { id: 20 }, "C:/Users/username/Project/b/shared.lua"),
                (FileId { id: 10 }, "C:/Users/username/Project/a/shared.lua"),
            ],
        ] {
            let mut m = create_module();
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/Project").into(),
                WorkspaceId::MAIN,
                WorkspaceKind::Main,
            );
            m.set_module_replace_patterns(
                [("^([ab])\\.(.*)$".to_string(), "$2".to_string())]
                    .into_iter()
                    .collect(),
            );

            m.add_module_by_path(insertion_order[0].0, insertion_order[0].1);
            m.add_module_by_path(insertion_order[1].0, insertion_order[1].1);

            let expected_file_id = insertion_order
                .iter()
                .find(|(_, path)| path.contains("/a/"))
                .map(|(file_id, _)| *file_id)
                .unwrap();
            let resolved = m.find_module("shared").unwrap();
            assert_eq!(resolved.file_id, expected_file_id);
        }
    }

    #[test]
    fn test_fuzzy_find_module_duplicate_prefers_main_workspace_independent_of_insert_order() {
        for insertion_order in [
            [
                (
                    FileId { id: 1 },
                    WorkspaceId { id: 3 },
                    "C:/Users/username/Annotations/lua/shared.lua",
                ),
                (
                    FileId { id: 2 },
                    WorkspaceId::MAIN,
                    "C:/Users/username/Project/lua/shared.lua",
                ),
            ],
            [
                (
                    FileId { id: 2 },
                    WorkspaceId::MAIN,
                    "C:/Users/username/Project/lua/shared.lua",
                ),
                (
                    FileId { id: 1 },
                    WorkspaceId { id: 3 },
                    "C:/Users/username/Annotations/lua/shared.lua",
                ),
            ],
        ] {
            let mut m = LuaModuleIndex::new();
            m.update_config(Arc::new(Emmyrc::default()));
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/Project").into(),
                WorkspaceId::MAIN,
                WorkspaceKind::Main,
            );
            m.add_workspace_root_with_kind(
                Path::new("C:/Users/username/Annotations").into(),
                WorkspaceId { id: 3 },
                WorkspaceKind::Library,
            );

            m.add_module_by_path(insertion_order[0].0, insertion_order[0].2);
            m.add_module_by_path(insertion_order[1].0, insertion_order[1].2);

            let resolved = m.find_module("shared").unwrap();
            assert_eq!(resolved.full_module_name, "lua.shared");
            assert_eq!(resolved.workspace_id, WorkspaceId::MAIN);
        }
    }
}
