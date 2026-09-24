#[cfg(test)]
mod test {
    use crate::db_index::derivation::{DerivationId, DerivationKind};
    use crate::{Emmyrc, VirtualWorkspace};

    /// The derivations a read of which no longer holds: after an update the
    /// engine must leave none.
    fn stale_derivations(ws: &VirtualWorkspace) -> Vec<DerivationId> {
        let db = ws.analysis.compilation.get_db();
        let graph = db.reader_graph();
        let mut stale = graph
            .derivations()
            .filter(|id| id.kind != DerivationKind::Diagnostics)
            .filter(|id| graph.is_stale(*id, |key, seen| db.fact_fingerprint(key) == seen))
            .collect::<Vec<_>>();
        stale.sort();
        stale
    }

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws
    }

    #[test]
    fn a_batch_over_an_existing_load_fold_evaluates_each_statement_once() {
        let mut ws = gmod_workspace();
        ws.def_file("addons/a/lua/autorun/client/first.lua", "local a = 1\n");

        ws.def_file(
            "addons/a/lua/autorun/client/second.lua",
            "local x = 1\nlocal y = x\nlocal z = y\n",
        );

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::GmodLoad), 1);
        assert_eq!(stats.evaluations(DerivationKind::LocalStat), 3);
    }

    #[test]
    fn a_helper_scan_reads_a_method_definition_as_the_source_declares_it() {
        let mut ws = gmod_workspace();

        ws.def_file(
            "addons/a/lua/autorun/client/methods.lua",
            r#"
            Messages = {}

            ---@[call_arg("gmod.net_message", "start")]
            ---@param name string
            function Messages:Begin(name) end
            "#,
        );

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::GmodCallRoles), 1);
    }

    #[test]
    fn a_reset_that_rehomes_other_derivations_members_re_checks_their_readers() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/glide/sh_repair_zebra.lua",
            r#"
            Glide = Glide or {}

            --- @class Zebra
            Glide.Repair = Glide.Repair or {}

            function Glide.Repair.x() end
            "#,
        );
        ws.def_file(
            "lua/glide/sh_repair_apple.lua",
            r#"
            --- @class Apple
            Glide.Repair = Glide.Repair or {}
            "#,
        );
        ws.def_file(
            "lua/entities/glide_repair_tool/init.lua",
            "local x = Glide.Repair.x
",
        );

        assert_eq!(stale_derivations(&ws), Vec::new());
    }
}
