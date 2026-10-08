#[cfg(test)]
mod test {
    use crate::db_index::derivation::DerivationKind;
    use crate::{
        Emmyrc, EmmyrcGmodScriptedClassScopeEntry, FileId, GmodRealm, LuaSemanticDeclId, LuaType,
        LuaTypeDeclId, VirtualWorkspace,
    };
    use tokio_util::sync::CancellationToken;

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

    /// Moving a file another file reads a global of re-runs only the moved
    /// file: where it belongs does not change, so neither do its readers.
    #[test]
    fn moving_a_file_whose_global_is_read_re_runs_only_the_file() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![
            (
                "lua/autorun/sh_values.lua",
                "SharedValue = 1
",
            ),
            (
                "lua/autorun/sh_reader.lua",
                "local value = SharedValue + 1
",
            ),
        ]);
        let moved = file_id(&ws, "lua/autorun/sh_values.lua").expect("the file has an id");

        ws.analysis.apply_file_system_changes(vec![
            (
                ws.virtual_url_generator
                    .new_uri("lua/autorun/sh_values.lua"),
                None,
            ),
            (
                ws.virtual_url_generator
                    .new_uri("lua/autorun/sh_values_moved.lua"),
                Some(
                    "SharedValue = 1
"
                    .to_string(),
                ),
            ),
        ]);

        assert_eq!(file_id(&ws, "lua/autorun/sh_values_moved.lua"), Some(moved));
        let evaluated = ws
            .analysis
            .compilation
            .last_evaluated_files()
            .iter()
            .copied()
            .filter(|file_id| *file_id != FileId::VIRTUAL)
            .collect::<Vec<_>>();
        assert_eq!(evaluated, vec![moved]);
    }

    /// A moved file's diagnostics are shown at its new path, so the move
    /// reports them affected even where nothing else about them changed.
    #[test]
    fn moving_a_file_reports_its_diagnostics_affected() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![(
            "lua/autorun/sh_unused.lua",
            "local unused = 1\nUnusedValue = unused\n",
        )]);
        let moved = file_id(&ws, "lua/autorun/sh_unused.lua").expect("the file has an id");
        ws.analysis
            .diagnose_file(moved, CancellationToken::new())
            .expect("the file is diagnosed");

        let affected = ws.analysis.apply_file_system_changes(vec![
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
        assert!(affected.contains(&moved), "{affected:?}");
    }

    const HUD: &str = "gamemode/cl_hud.lua";
    const HUD_TEXT: &str = "function GM:HUDPaint() end\n";
    const PLUGIN: &str = "plugins/foo/sh_main.lua";
    const PLUGIN_TEXT: &str = "local paint = GAMEMODE.HUDPaint\n";
    const ENTRY: &str = "gamemode/init.lua";
    const ENTRY_TEXT: &str = "GM.Name = \"Root\"\n";
    const BOOT: &str = "gamemode/core/sv_boot.lua";

    /// A workspace holding `files` whose root may be a gamemode folder.
    fn root_gamemode_workspace(files: &[(&str, &str)]) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_files(files.to_vec());
        ws
    }

    /// Applies `events` to `warm` in one batch and asserts it settles as a
    /// workspace built with `final_files`.
    fn assert_events_match_cold(
        warm: &mut VirtualWorkspace,
        events: &[(&str, Option<&str>)],
        final_files: &[(&str, &str)],
    ) {
        let events = events
            .iter()
            .map(|(path, text)| {
                (
                    warm.virtual_url_generator.new_uri(path),
                    text.map(str::to_string),
                )
            })
            .collect();
        warm.analysis.apply_file_system_changes(events);
        assert_eq!(warm.stale_derivations(), Vec::new());
        assert_eq!(digest(warm), digest(&root_gamemode_workspace(final_files)));
    }

    /// Whether the file at `path` authors a scripted class.
    fn is_scoped(ws: &VirtualWorkspace, path: &str) -> bool {
        file_id(ws, path).is_some_and(|file_id| {
            ws.analysis
                .compilation
                .get_db()
                .get_gmod_infer_index()
                .get_scoped_class_info(&file_id)
                .is_some()
        })
    }

    /// Creating a gamemode entry file at the workspace root makes the root a
    /// gamemode folder, as a cold build with the file finds it.
    #[test]
    fn creating_a_gamemode_entry_file_makes_the_root_a_gamemode() {
        let mut ws = root_gamemode_workspace(&[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT)]);
        assert!(!is_scoped(&ws, HUD));
        assert_events_match_cold(
            &mut ws,
            &[(ENTRY, Some(ENTRY_TEXT))],
            &[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (ENTRY, ENTRY_TEXT)],
        );
        assert!(is_scoped(&ws, HUD));
    }

    /// Deleting the only gamemode entry file leaves the root a plain folder.
    #[test]
    fn deleting_the_gamemode_entry_file_leaves_the_root_plain() {
        let mut ws =
            root_gamemode_workspace(&[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (ENTRY, ENTRY_TEXT)]);
        assert!(is_scoped(&ws, HUD));
        assert_events_match_cold(
            &mut ws,
            &[(ENTRY, None)],
            &[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT)],
        );
        assert!(!is_scoped(&ws, HUD));
    }

    /// Moving the only gamemode entry file away vacates its path: the root is
    /// a plain folder, though the moved file keeps its id.
    #[test]
    fn moving_the_gamemode_entry_file_away_leaves_the_root_plain() {
        let mut ws =
            root_gamemode_workspace(&[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (ENTRY, ENTRY_TEXT)]);
        let entry = file_id(&ws, ENTRY).expect("the entry file has an id");
        assert_events_match_cold(
            &mut ws,
            &[(ENTRY, None), (BOOT, Some(ENTRY_TEXT))],
            &[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (BOOT, ENTRY_TEXT)],
        );
        assert_eq!(file_id(&ws, BOOT), Some(entry));
        assert!(!is_scoped(&ws, HUD));
    }

    /// Moving a file onto a gamemode entry path makes the root a gamemode
    /// folder.
    #[test]
    fn moving_a_file_onto_the_gamemode_entry_path_makes_the_root_a_gamemode() {
        let mut ws =
            root_gamemode_workspace(&[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (BOOT, ENTRY_TEXT)]);
        let boot = file_id(&ws, BOOT).expect("the boot file has an id");
        assert!(!is_scoped(&ws, HUD));
        assert_events_match_cold(
            &mut ws,
            &[(BOOT, None), (ENTRY, Some(ENTRY_TEXT))],
            &[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (ENTRY, ENTRY_TEXT)],
        );
        assert_eq!(file_id(&ws, ENTRY), Some(boot));
        assert!(is_scoped(&ws, HUD));
    }

    /// Creating a file at a path no analysis probed re-runs nothing outside
    /// the file, though files read the gamemode entry paths.
    #[test]
    fn creating_a_file_at_an_unprobed_path_re_runs_only_the_file() {
        let mut ws =
            root_gamemode_workspace(&[(HUD, HUD_TEXT), (PLUGIN, PLUGIN_TEXT), (ENTRY, ENTRY_TEXT)]);
        ws.analysis.apply_file_system_changes(vec![(
            ws.virtual_url_generator
                .new_uri("lua/autorun/sh_unrelated.lua"),
            Some("local unused = 1\n".to_string()),
        )]);

        let created = file_id(&ws, "lua/autorun/sh_unrelated.lua").expect("the file has an id");
        let evaluated = ws
            .analysis
            .compilation
            .last_evaluated_files()
            .iter()
            .copied()
            .filter(|file_id| *file_id != FileId::VIRTUAL)
            .collect::<Vec<_>>();
        assert_eq!(evaluated, vec![created]);
        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.loops_cleared, 0);
        assert_eq!(stats.rebuilt_sources, 0);
    }

    /// The path of `file_id` in `ws`, relative to the workspace.
    fn relative_path(ws: &VirtualWorkspace, file_id: FileId) -> String {
        ws.analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_path(&file_id)
            .and_then(|path| path.strip_prefix(&ws.virtual_url_generator.base).ok())
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }

    /// Moving a directory whose files keep their scope and workspace re-runs
    /// none of their statements: none of them reads where its file is.
    #[test]
    fn moving_a_directory_re_runs_none_of_its_statements() {
        let warm = assert_move_matches_cold(
            &[
                (
                    "lua/mylib/sh_first.lua",
                    "local a = 1\nlocal b = a + 1\nMyLibFirst = b\n",
                ),
                (
                    "lua/mylib/sh_second.lua",
                    "local c = 2\nlocal d = c * 2\nMyLibSecond = d\n",
                ),
                (
                    "lua/autorun/sh_reader.lua",
                    "local sum = MyLibFirst + MyLibSecond\n",
                ),
            ],
            &[
                ("lua/mylib/sh_first.lua", "lua/moved/sh_first.lua"),
                ("lua/mylib/sh_second.lua", "lua/moved/sh_second.lua"),
            ],
        );

        let stats = warm.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::LocalStat), 0);
        assert_eq!(stats.rebuilt_sources, 0);
    }

    /// Two files declare one global, listed in the order of their paths:
    /// renaming one lists them as a cold build of the renamed layout does.
    #[test]
    fn renaming_a_file_re_orders_the_declarations_of_its_globals() {
        let warm = assert_move_matches_cold(
            &[
                ("lua/autorun/sh_first.lua", "SharedGlobal = 1\n"),
                ("lua/autorun/sh_second.lua", "SharedGlobal = \"text\"\n"),
                ("lua/autorun/sh_reader.lua", "local value = SharedGlobal\n"),
            ],
            &[("lua/autorun/sh_first.lua", "lua/autorun/sh_third.lua")],
        );

        let decl_paths = warm
            .analysis
            .compilation
            .get_db()
            .get_global_index()
            .get_global_decl_ids("SharedGlobal")
            .into_iter()
            .flatten()
            .map(|decl_id| relative_path(&warm, decl_id.file_id))
            .collect::<Vec<_>>();
        assert_eq!(
            decl_paths,
            vec!["lua/autorun/sh_second.lua", "lua/autorun/sh_third.lua"]
        );
    }

    /// Two files declare one alias, and the one first by path gives its
    /// origin: renaming one selects the origin a cold build selects.
    #[test]
    fn renaming_a_file_re_selects_the_origin_of_an_alias_it_declares() {
        let warm = assert_move_matches_cold(
            &[
                (
                    "lua/autorun/sh_first.lua",
                    "---@alias SharedAlias integer\n",
                ),
                (
                    "lua/autorun/sh_second.lua",
                    "---@alias SharedAlias string\n",
                ),
            ],
            &[("lua/autorun/sh_first.lua", "lua/autorun/sh_third.lua")],
        );

        let origin = warm
            .analysis
            .compilation
            .get_db()
            .get_type_index()
            .get_type_decl(&LuaTypeDeclId::global("SharedAlias"))
            .and_then(|decl| decl.get_alias_ref().cloned());
        assert_eq!(origin, Some(LuaType::String));
    }

    /// Two files document one class, and the first by path gives its
    /// description: renaming one merges the docs as a cold build does.
    #[test]
    fn renaming_a_file_re_orders_the_docs_of_a_class_it_documents() {
        const FIRST: &str = "--- First description\n---@class SharedDoc\nlocal first = {}\n";
        const SECOND: &str = "--- Second description\n---@class SharedDoc\nlocal second = {}\n";
        let description = |ws: &VirtualWorkspace| {
            ws.analysis
                .compilation
                .get_db()
                .get_property_index()
                .get_property(&LuaSemanticDeclId::TypeDecl(LuaTypeDeclId::global(
                    "SharedDoc",
                )))
                .and_then(|property| property.description().cloned())
        };
        let warm = assert_move_matches_cold(
            &[
                ("lua/autorun/sh_first.lua", FIRST),
                ("lua/autorun/sh_second.lua", SECOND),
            ],
            &[("lua/autorun/sh_first.lua", "lua/autorun/sh_third.lua")],
        );
        let mut cold = gmod_workspace();
        cold.def_files(vec![
            ("lua/autorun/sh_second.lua", SECOND),
            ("lua/autorun/sh_third.lua", FIRST),
        ]);

        assert_eq!(description(&warm), description(&cold));
        assert!(
            description(&warm).is_some_and(|text| text.contains("Second")),
            "{:?}",
            description(&warm)
        );
    }

    /// A moved file that loads another by a path relative to it loads the
    /// file next to where it now is, as a cold build of the moved layout does.
    #[test]
    fn a_moved_file_loads_by_path_from_where_it_now_is() {
        let warm = assert_move_matches_cold(
            &[
                (
                    "lua/first/sv_boot.lua",
                    "include(\"helper.lua\")\nlocal value = HelperValue\n",
                ),
                ("lua/first/helper.lua", "HelperValue = 1\n"),
                ("lua/second/helper.lua", "HelperValue = \"text\"\n"),
            ],
            &[("lua/first/sv_boot.lua", "lua/second/sv_boot.lua")],
        );

        let boot = file_id(&warm, "lua/second/sv_boot.lua").expect("the loader has an id");
        let targets = warm
            .analysis
            .compilation
            .get_db()
            .get_file_dependencies_index()
            .get_dependency_sites(&boot)
            .unwrap_or_default()
            .iter()
            .filter_map(|site| site.target_file_id)
            .map(|target| relative_path(&warm, target))
            .collect::<Vec<_>>();
        assert_eq!(targets, vec!["lua/second/helper.lua"]);
    }
}
