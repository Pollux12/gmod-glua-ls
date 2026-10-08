#[cfg(test)]
mod test {
    use crate::{
        Emmyrc, EmmyrcGmodScriptedClassScopeEntry, FileId, GmodRealm, LuaTypeDeclId,
        VirtualWorkspace,
    };

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.set_include(vec![
            EmmyrcGmodScriptedClassScopeEntry::LegacyGlob("entities/**".to_string()),
        ]);
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws
    }

    fn file_id(ws: &VirtualWorkspace, path: &str) -> Option<FileId> {
        ws.analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(path))
    }

    /// The semantic digest with every file's realm and what each of its load
    /// calls loads, every file named by its path in the workspace.
    fn digest(ws: &VirtualWorkspace) -> Vec<String> {
        let db = ws.analysis.compilation.get_db();
        let vfs = db.get_vfs();
        let path_of = |file_id: &FileId| {
            vfs.get_file_path(file_id)
                .and_then(|path| path.strip_prefix(&ws.virtual_url_generator.base).ok())
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default()
        };
        let mut digest = ws.semantic_digest();
        for file_id in db.get_module_index().get_main_workspace_file_ids() {
            let path = path_of(&file_id);
            let realm = db
                .get_gmod_infer_index()
                .get_realm_file_metadata(&file_id)
                .map(|metadata| metadata.inferred_realm);
            digest.push(format!("{path} realm {realm:?}"));
            let sites = db
                .get_file_dependencies_index()
                .get_dependency_sites(&file_id)
                .unwrap_or_default();
            for site in sites {
                digest.push(format!(
                    "{path} loads {:?} {:?} -> {:?}",
                    site.kind,
                    site.path,
                    site.target_file_id.as_ref().map(path_of)
                ));
            }
        }
        digest.sort();
        digest
    }

    /// Moves each `(from, to)` of `moves` in one batch of file-system events
    /// in a workspace holding `files`, and asserts the moved files keep their
    /// ids and the workspace settles as one built with the files where they
    /// were moved to. Returns the moved workspace.
    fn assert_move_matches_cold(
        files: &[(&str, &str)],
        moves: &[(&str, &str)],
    ) -> VirtualWorkspace {
        let text_of = |path: &str| {
            files
                .iter()
                .find(|(file, _)| *file == path)
                .map(|(_, text)| text.to_string())
                .expect("a moved file is among the files")
        };
        let mut warm = gmod_workspace();
        warm.def_files(files.to_vec());
        let mut moved_ids = moves
            .iter()
            .map(|(from, _)| file_id(&warm, from).expect("a moved file has an id"))
            .collect::<Vec<_>>();
        let events = moves
            .iter()
            .flat_map(|(from, to)| {
                [
                    (warm.virtual_url_generator.new_uri(from), None),
                    (warm.virtual_url_generator.new_uri(to), Some(text_of(from))),
                ]
            })
            .collect();
        warm.analysis.apply_file_system_changes(events);

        let mut ids_after = moves
            .iter()
            .map(|(_, to)| file_id(&warm, to).expect("a moved file has an id at its new path"))
            .collect::<Vec<_>>();
        moved_ids.sort_unstable();
        ids_after.sort_unstable();
        assert_eq!(ids_after, moved_ids, "moved files keep their ids");
        for (from, _) in moves {
            assert_eq!(file_id(&warm, from), None, "{from} names no file");
        }
        assert_eq!(warm.stale_derivations(), Vec::new());

        let final_files = files
            .iter()
            .map(|(path, text)| {
                let path = moves
                    .iter()
                    .find(|(from, _)| from == path)
                    .map_or(*path, |(_, to)| *to);
                (path, *text)
            })
            .collect::<Vec<_>>();
        let mut cold = gmod_workspace();
        cold.def_files(final_files);
        assert_eq!(digest(&warm), digest(&cold));
        warm
    }

    /// A file another includes by path is found at its new path once moved,
    /// as a cold build of the moved layout finds it.
    #[test]
    fn a_moved_file_is_loaded_from_its_new_path() {
        const BOOT: &str = "lua/sv_boot.lua";
        const BOOT_TEXT: &str =
            "include(\"helper.lua\")\ninclude(\"moved/helper.lua\")\nlocal value = HelperValue\n";
        const HELPER_TEXT: &str = "HelperValue = 1\n";
        let warm = assert_move_matches_cold(
            &[(BOOT, BOOT_TEXT), ("lua/helper.lua", HELPER_TEXT)],
            &[("lua/helper.lua", "lua/moved/helper.lua")],
        );
        let mut cold = gmod_workspace();
        cold.def_files(vec![
            (BOOT, BOOT_TEXT),
            ("lua/moved/helper.lua", HELPER_TEXT),
        ]);

        let include_sites = |ws: &VirtualWorkspace| {
            let db = ws.analysis.compilation.get_db();
            let boot = file_id(ws, BOOT).expect("the loader has an id");
            db.get_file_dependencies_index()
                .get_dependency_sites(&boot)
                .unwrap_or_default()
                .iter()
                .map(|site| {
                    let target = site
                        .target_file_id
                        .and_then(|target| db.get_vfs().get_file_path(&target).cloned());
                    (site.path.clone(), target)
                })
                .collect::<Vec<_>>()
        };
        let sites = include_sites(&warm);
        assert!(
            sites.iter().any(|(path, target)| {
                path.as_deref() == Some("moved/helper.lua") && target.is_some()
            }),
            "{sites:?}"
        );
        assert_eq!(sites, include_sites(&cold));
    }

    /// A file's realm follows its path: a client file moved under a server
    /// path is a server file.
    #[test]
    fn a_moved_file_takes_the_realm_of_its_new_path() {
        let warm = assert_move_matches_cold(
            &[
                ("lua/autorun/client/hud.lua", "HudScale = 1\n"),
                ("lua/cl_menu.lua", "MenuOpen = false\n"),
                (
                    "lua/autorun/sh_reader.lua",
                    "local scale = HudScale\nlocal open = MenuOpen\n",
                ),
            ],
            &[
                ("lua/autorun/client/hud.lua", "lua/autorun/server/hud.lua"),
                ("lua/cl_menu.lua", "lua/sv_menu.lua"),
            ],
        );

        let menu = file_id(&warm, "lua/sv_menu.lua").expect("the menu has an id");
        assert_eq!(
            warm.analysis
                .compilation
                .get_db()
                .get_gmod_infer_index()
                .get_realm_file_metadata(&menu)
                .map(|metadata| metadata.inferred_realm),
            Some(GmodRealm::Server)
        );
    }

    /// A scripted entity's file moved to another entity's folder authors that
    /// entity's class.
    #[test]
    fn a_moved_entity_file_authors_the_class_of_its_new_folder() {
        let warm = assert_move_matches_cold(
            &[
                (
                    "lua/entities/first_entity/shared.lua",
                    "ENT.Type = \"anim\"\nENT.PrintName = \"Crate\"\n",
                ),
                (
                    "lua/entities/first_entity/init.lua",
                    "include(\"shared.lua\")\nfunction ENT:Initialize() end\n",
                ),
            ],
            &[(
                "lua/entities/first_entity/shared.lua",
                "lua/entities/second_entity/shared.lua",
            )],
        );

        let types = warm.analysis.compilation.get_db().get_type_index();
        assert!(
            types
                .get_type_decl(&LuaTypeDeclId::global("second_entity"))
                .is_some()
        );
    }

    /// Entity folders hold files with the same text: moving two folders at
    /// once may pair one folder's file with the other's new path, and still
    /// settles as the workspace built with the moved layout.
    #[test]
    fn files_of_equal_text_moved_together_settle_whichever_way_they_pair() {
        const SHARED: &str = "ENT.Type = \"anim\"\n";
        assert_move_matches_cold(
            &[
                ("lua/entities/alpha/shared.lua", SHARED),
                (
                    "lua/entities/alpha/init.lua",
                    "include(\"shared.lua\")\nENT.PrintName = \"Alpha\"\n",
                ),
                ("lua/entities/beta/shared.lua", SHARED),
                (
                    "lua/entities/beta/init.lua",
                    "include(\"shared.lua\")\nENT.PrintName = \"Beta\"\n",
                ),
            ],
            &[
                (
                    "lua/entities/alpha/shared.lua",
                    "lua/entities/zeta/shared.lua",
                ),
                ("lua/entities/alpha/init.lua", "lua/entities/zeta/init.lua"),
                (
                    "lua/entities/beta/shared.lua",
                    "lua/entities/gamma/shared.lua",
                ),
                ("lua/entities/beta/init.lua", "lua/entities/gamma/init.lua"),
            ],
        );
    }

    /// Moving a file nothing loads by path re-runs nothing outside the file:
    /// what other files read of it does not depend on where it is.
    #[test]
    fn moving_a_file_nothing_loads_re_runs_only_the_file() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![
            ("lua/autorun/sh_values.lua", "SharedValue = 1\n"),
            ("lua/autorun/sh_reader.lua", "local value = SharedValue\n"),
            (
                "lua/autorun/sh_unused.lua",
                "local unused = 1\nUnusedValue = unused\n",
            ),
        ]);
        let moved = file_id(&ws, "lua/autorun/sh_unused.lua").expect("the file has an id");

        ws.analysis.apply_file_system_changes(vec![
            (
                ws.virtual_url_generator
                    .new_uri("lua/autorun/sh_unused.lua"),
                None,
            ),
            (
                ws.virtual_url_generator.new_uri("lua/autorun/sh_moved.lua"),
                Some("local unused = 1\nUnusedValue = unused\n".to_string()),
            ),
        ]);

        assert_eq!(file_id(&ws, "lua/autorun/sh_moved.lua"), Some(moved));
        let evaluated = ws
            .analysis
            .compilation
            .last_evaluated_files()
            .iter()
            .copied()
            .filter(|file_id| *file_id != FileId::VIRTUAL)
            .collect::<Vec<_>>();
        assert_eq!(evaluated, vec![moved]);
        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.loops_cleared, 0);
        assert_eq!(stats.rebuilt_sources, 0);
    }
}
