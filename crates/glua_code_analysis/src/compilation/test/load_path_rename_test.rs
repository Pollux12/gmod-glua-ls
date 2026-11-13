#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use glua_parser::{LuaAstNode, LuaLiteralExpr, LuaLiteralToken, LuaParser, ParserConfig};

    use crate::{FileRenames, VirtualWorkspace, load_path_rename_edits};

    /// Each edit as (file, the text it replaces, its new text).
    fn rename_edits(
        ws: &VirtualWorkspace,
        renames: &[(&str, &str)],
    ) -> Vec<(String, String, String)> {
        let base = &ws.virtual_url_generator.base;
        let renames = FileRenames::new(
            renames
                .iter()
                .map(|(old, new)| (base.join(old), base.join(new)))
                .collect(),
        );
        let db = ws.analysis.compilation.get_db();
        let vfs = db.get_vfs();
        load_path_rename_edits(db, &renames)
            .into_iter()
            .map(|edit| {
                let path = vfs
                    .get_file_path(&edit.file_id)
                    .expect("edited file has a path");
                let text = vfs
                    .get_file_content(&edit.file_id)
                    .expect("edited file has text");
                (
                    path.strip_prefix(base)
                        .expect("edited file is in the workspace")
                        .to_string_lossy()
                        .replace('\\', "/"),
                    text[edit.range].to_string(),
                    edit.new_text,
                )
            })
            .collect()
    }

    fn edit(file: &str, old: &str, new: &str) -> (String, String, String) {
        (file.to_string(), old.to_string(), new.to_string())
    }

    /// Renaming a file rewrites the `include`, `AddCSLuaFile` and `require`
    /// strings that load it, and no other load string.
    #[gtest]
    fn a_file_rename_rewrites_only_the_strings_loading_it() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("lua/myaddon/sh_config.lua", "return {}\n"),
            ("lua/myaddon/sh_unrelated.lua", "return {}\n"),
            ("lua/includes/modules/myutil.lua", "return {}\n"),
            (
                "lua/autorun/sh_loader.lua",
                "AddCSLuaFile(\"myaddon/sh_config.lua\")\ninclude(\"myaddon/sh_config.lua\")\ninclude(\"myaddon/sh_unrelated.lua\")\nlocal util = require(\"myutil\")\n",
            ),
        ]);

        let edits = rename_edits(
            &ws,
            &[(
                "lua/myaddon/sh_config.lua",
                "lua/myaddon/shared/sh_settings.lua",
            )],
        );

        assert_that!(
            edits,
            elements_are![
                eq(&edit(
                    "lua/autorun/sh_loader.lua",
                    "myaddon/sh_config.lua",
                    "myaddon/shared/sh_settings.lua"
                )),
                eq(&edit(
                    "lua/autorun/sh_loader.lua",
                    "myaddon/sh_config.lua",
                    "myaddon/shared/sh_settings.lua"
                )),
            ]
        );
    }

    #[gtest]
    fn a_module_rename_rewrites_its_require() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("lua/includes/modules/myutil.lua", "return {}\n"),
            (
                "lua/autorun/sh_loader.lua",
                "local util = require(\"myutil\")\n",
            ),
        ]);

        let edits = rename_edits(
            &ws,
            &[(
                "lua/includes/modules/myutil.lua",
                "lua/includes/modules/myhelpers.lua",
            )],
        );

        assert_that!(
            edits,
            elements_are![eq(&edit(
                "lua/autorun/sh_loader.lua",
                "myutil",
                "myhelpers"
            ))]
        );
    }

    /// Renaming a gamemode folder rewrites the paths into it from outside,
    /// and leaves the relative paths between files inside it as they are.
    #[gtest]
    fn a_folder_rename_rewrites_the_paths_into_it() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("gamemodes/mygm/gamemode/shared.lua", "return {}\n"),
            ("gamemodes/mygm/gamemode/modules/sh_round.lua", "return {}\n"),
            (
                "gamemodes/mygm/gamemode/modules/sv_round.lua",
                "include(\"sh_round.lua\")\n",
            ),
            (
                "gamemodes/mygm/gamemode/init.lua",
                "AddCSLuaFile(\"modules/sh_round.lua\")\ninclude(\"modules/sh_round.lua\")\ninclude(\"modules/sv_round.lua\")\ninclude(\"shared.lua\")\n",
            ),
        ]);

        let edits = rename_edits(
            &ws,
            &[(
                "gamemodes/mygm/gamemode/modules",
                "gamemodes/mygm/gamemode/systems",
            )],
        );

        assert_that!(
            edits,
            elements_are![
                eq(&edit(
                    "gamemodes/mygm/gamemode/init.lua",
                    "modules/sh_round.lua",
                    "systems/sh_round.lua"
                )),
                eq(&edit(
                    "gamemodes/mygm/gamemode/init.lua",
                    "modules/sh_round.lua",
                    "systems/sh_round.lua"
                )),
                eq(&edit(
                    "gamemodes/mygm/gamemode/init.lua",
                    "modules/sv_round.lua",
                    "systems/sv_round.lua"
                )),
            ]
        );
    }

    /// A path relative to the including file is rewritten from the `lua/`
    /// root when that file moves out of reach of what it loads.
    #[gtest]
    fn moving_the_including_file_rewrites_its_relative_paths() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("lua/autorun/server/sv_helpers.lua", "return {}\n"),
            (
                "lua/autorun/server/sv_boot.lua",
                "include(\"sv_helpers.lua\")\n",
            ),
        ]);

        let edits = rename_edits(
            &ws,
            &[("lua/autorun/server/sv_boot.lua", "lua/myaddon/sv_boot.lua")],
        );

        assert_that!(
            edits,
            elements_are![eq(&edit(
                "lua/autorun/server/sv_boot.lua",
                "sv_helpers.lua",
                "autorun/server/sv_helpers.lua"
            ))]
        );
    }

    /// `file`'s text with the edits `renames` makes applied, and the values
    /// of the string literals the result parses to.
    fn renamed_strings(ws: &VirtualWorkspace, file: &str, renames: &[(&str, &str)]) -> Vec<String> {
        let base = &ws.virtual_url_generator.base;
        let renames = FileRenames::new(
            renames
                .iter()
                .map(|(old, new)| (base.join(old), base.join(new)))
                .collect(),
        );
        let db = ws.analysis.compilation.get_db();
        let file_id = db
            .get_vfs()
            .get_file_id(&ws.virtual_url_generator.new_uri(file))
            .expect("the file is indexed");
        let mut text = db
            .get_vfs()
            .get_file_content(&file_id)
            .expect("the file has text")
            .clone();
        let mut edits = load_path_rename_edits(db, &renames)
            .into_iter()
            .filter(|edit| edit.file_id == file_id)
            .collect::<Vec<_>>();
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start()));
        for edit in edits {
            text.replace_range(std::ops::Range::<usize>::from(edit.range), &edit.new_text);
        }
        let tree = LuaParser::parse(&text, ParserConfig::default());
        assert!(tree.get_errors().is_empty(), "{text}");
        tree.get_chunk_node()
            .descendants::<LuaLiteralExpr>()
            .filter_map(|literal| match literal.get_literal()? {
                LuaLiteralToken::String(string) => Some(string.get_value()),
                _ => None,
            })
            .collect()
    }

    /// A new path holding a quote, a backslash or a long string's closing
    /// bracket is written so the literal still parses to exactly that path.
    #[test]
    fn a_new_path_is_written_for_the_literal_it_goes_into() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("lua/autorun/sh_old.lua", "return {}\n"),
            (
                "lua/autorun/sh_loader.lua",
                "include('sh_old.lua')\ninclude(\"sh_old.lua\")\ninclude([[sh_old.lua]])\ninclude([==[sh_old.lua]==])\n",
            ),
        ]);

        for new_name in [r#"sh_o'b"r.lua"#, "sh_tab\tname.lua", "sh_a]]b.lua"] {
            assert_eq!(
                renamed_strings(
                    &ws,
                    "lua/autorun/sh_loader.lua",
                    &[("lua/autorun/sh_old.lua", &format!("lua/autorun/{new_name}"))],
                ),
                [new_name; 4],
                "{new_name}"
            );
        }
    }

    /// A literal that spells its path with escapes, as a rename writes one, or
    /// with a long string's dropped newline is found by the path it parses to,
    /// so the next rename rewrites it too.
    #[test]
    fn a_path_spelled_with_escapes_is_rewritten_again() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("lua/autorun/sh_o'b.lua", "return {}\n"),
            (
                "lua/autorun/sh_loader.lua",
                "include('sh_o\\'b.lua')\ninclude(\"sh_o\\039b.lua\")\ninclude([[\nsh_o'b.lua]])\n",
            ),
        ]);

        assert_eq!(
            renamed_strings(
                &ws,
                "lua/autorun/sh_loader.lua",
                &[("lua/autorun/sh_o'b.lua", "lua/autorun/sh_new.lua")],
            ),
            ["sh_new.lua"; 3]
        );
    }

    /// A `require` names enough trailing segments of the module's new name
    /// that no other module answers to them once the renames are done.
    #[gtest]
    fn a_require_names_only_the_module_it_loaded() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            ("zlib/old.lua", "return {}\n"),
            ("new.lua", "return {}\n"),
            ("other/stale.lua", "return {}\n"),
            ("loader.lua", "local z = require(\"old\")\n"),
        ]);

        let renamed = [("zlib/old.lua", "zlib/new.lua")];
        let other_moves_in = [renamed[0], ("other/stale.lua", "other/new.lua")];
        let other_moves_away = [renamed[0], ("new.lua", "newer.lua")];
        let edits = [&renamed[..], &other_moves_in, &other_moves_away]
            .map(|renames| rename_edits(&ws, renames));

        assert_that!(
            edits,
            elements_are![
                elements_are![eq(&edit("loader.lua", "old", "zlib.new"))],
                elements_are![eq(&edit("loader.lua", "old", "zlib.new"))],
                elements_are![eq(&edit("loader.lua", "old", "new"))],
            ]
        );
    }

    /// A `require` names a module by the trailing segments of its path, in
    /// the caller's own separators, through the configured patterns.
    #[gtest]
    fn a_require_keeps_its_style_and_follows_the_configured_patterns() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.runtime.require_pattern = vec!["?.lua".to_string(), "?/main.lua".to_string()];
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            ("mylib/core/util/main.lua", "return {}\n"),
            (
                "loader.lua",
                "local a = require(\"core.util\")\nlocal b = require(\"core/util\")\n",
            ),
        ]);

        let edits = rename_edits(&ws, &[("mylib/core", "mylib/base")]);

        assert_that!(
            edits,
            elements_are![
                eq(&edit("loader.lua", "core.util", "base.util")),
                eq(&edit("loader.lua", "core/util", "base/util")),
            ]
        );
    }
}
