#[cfg(test)]
mod test {
    use glua_parser::{LuaAssignStat, LuaAstNode, LuaSyntaxId, LuaVarExpr};

    use crate::db_index::derivation::{DerivationId, DerivationKind, FactKey};
    use crate::{Emmyrc, FileId, LuaMemberId, VirtualWorkspace};

    const HUB: &str = "gamemodes/cityrp/gamemode/core/libraries/sh_hub.lua";
    const HUB_TEXT: &str = "cityrp = cityrp or {}\n";
    const FIRST: &str = "gamemodes/cityrp/gamemode/core/libraries/sh_first.lua";
    const SECOND: &str = "gamemodes/cityrp/gamemode/core/libraries/sh_second.lua";
    const THIRD: &str = "gamemodes/cityrp/gamemode/core/libraries/sh_third.lua";
    const READER: &str = "gamemodes/cityrp/gamemode/core/libraries/sh_reader.lua";
    const READER_TEXT: &str =
        "local present = cityrp.first\nlocal absent = cityrp.missing\nprint(present, absent)\n";

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws
    }

    /// The only assignment of `file_id`, and the member its target names.
    fn assignment(ws: &VirtualWorkspace, file_id: FileId) -> (DerivationId, LuaMemberId) {
        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree");
        let assign = tree
            .get_chunk_node()
            .descendants::<LuaAssignStat>()
            .next()
            .expect("assignment");
        let Some(LuaVarExpr::IndexExpr(target)) = assign.get_var_and_expr_list().0.first().cloned()
        else {
            panic!("an indexed target");
        };
        (
            DerivationId::new(
                DerivationKind::AssignStat,
                file_id,
                LuaSyntaxId::from_node(assign.syntax()),
            ),
            LuaMemberId::new(target.get_syntax_id(), file_id),
        )
    }

    /// Filing a member under a table that already has members changes which
    /// members it has, not whether it has any: the assignment produces the
    /// table's member map but not its presence, so a missing-key lookup on
    /// the table does not read from every assignment that writes it.
    #[test]
    fn a_write_into_a_table_with_members_does_not_produce_its_presence() {
        let mut ws = gmod_workspace();
        ws.def_file(HUB, HUB_TEXT);
        ws.def_file(FIRST, "cityrp.first = 1\n");
        ws.def_file(SECOND, "cityrp.second = 2\n");
        let third = ws.def_file(THIRD, "cityrp.third = 3\n");

        ws.def_file(THIRD, "cityrp.third = 4\n");

        let (statement, member) = assignment(&ws, third);
        let db = ws.analysis.compilation.get_db();
        let owner = db
            .get_member_index()
            .get_current_owner(&member)
            .cloned()
            .expect("the member is homed");
        let outputs = db
            .reader_graph()
            .outputs_of(statement)
            .map(|key| key.key().clone())
            .collect::<Vec<_>>();
        assert_eq!(
            (
                outputs.contains(&FactKey::MemberOwnerMap(owner.clone())),
                outputs.contains(&FactKey::MemberOwnerPresence(owner)),
            ),
            (true, false)
        );
    }

    /// The writes that make a table's member map appear and go are the ones
    /// a missing-key lookup on it re-checks: removing the table's only
    /// writer and adding it back settles as cold builds of both states do.
    #[test]
    fn a_member_map_that_goes_and_comes_back_settles_as_a_cold_build() {
        let only_writer = "cityrp.first = 1\n";
        let cold_without = {
            let mut ws = gmod_workspace();
            ws.def_file(HUB, HUB_TEXT);
            ws.def_file(FIRST, "");
            ws.def_file(READER, READER_TEXT);
            ws.settled_digest()
        };
        let cold_with = {
            let mut ws = gmod_workspace();
            ws.def_file(HUB, HUB_TEXT);
            ws.def_file(FIRST, only_writer);
            ws.def_file(READER, READER_TEXT);
            ws.settled_digest()
        };

        let mut warm = gmod_workspace();
        warm.def_file(HUB, HUB_TEXT);
        warm.def_file(FIRST, only_writer);
        warm.def_file(READER, READER_TEXT);
        warm.def_file(FIRST, "");
        let without = (warm.settled_digest(), warm.stale_derivations());
        warm.def_file(FIRST, only_writer);
        let with = (warm.settled_digest(), warm.stale_derivations());

        assert_eq!(
            (without, with),
            ((cold_without, Vec::new()), (cold_with, Vec::new()))
        );
    }

    const LOGGING: &str = "gamemodes/cityrp/gamemode/core/libraries/sh_logging.lua";
    const LOGGING_TEXT: &str = r#"cityrp = cityrp or {}

---@class Logging
cityrp.logging = cityrp.logging or setmetatable({
    stored = {},
}, { __index = cityrp })

cityrp.logging.Suppress = cityrp.logging.Suppress or false
"#;

    /// A library in the shape of CityRP's: its own table on the shared
    /// global, fields written into it, and a lookup through the logging
    /// table that falls back to the shared global.
    fn library(name: &str) -> String {
        format!(
            "cityrp.{name} = cityrp.{name} or {{}}\n\
             cityrp.{name}.settings = {{ enabled = true }}\n\
             cityrp.{name}.count = 0\n\
             local fallback = cityrp.logging.{name}Absent\n\
             function cityrp.{name}.Get() return cityrp.{name}.count, fallback end\n"
        )
    }

    /// Editing the logging hub and undoing it clears no loop across the
    /// libraries that write the shared global, and settles as a cold build.
    #[test]
    fn a_hub_edit_and_undo_clears_no_loop_across_libraries_writing_one_global() {
        let names = ["inventory", "vehicles", "property"];
        let path = |name: &str| format!("gamemodes/cityrp/gamemode/core/libraries/sh_{name}.lua");
        let build = |ws: &mut VirtualWorkspace| {
            ws.def_file(LOGGING, LOGGING_TEXT);
            for name in names {
                ws.def_file(&path(name), &library(name));
            }
            ws.def_file(READER, READER_TEXT);
        };
        let mut cold = gmod_workspace();
        build(&mut cold);
        let mut warm = gmod_workspace();
        build(&mut warm);

        warm.def_file(LOGGING, &format!("{LOGGING_TEXT}local edited = 1\n"));
        warm.def_file(LOGGING, LOGGING_TEXT);

        let loops_cleared = warm.analysis.compilation.last_stats().loops_cleared;
        assert_eq!(
            (
                loops_cleared,
                warm.stale_derivations(),
                warm.settled_digest()
            ),
            (0, Vec::new(), cold.settled_digest())
        );
    }
}
