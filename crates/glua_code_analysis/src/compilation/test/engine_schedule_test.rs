#[cfg(test)]
mod test {
    use crate::db_index::derivation::DerivationKind;
    use crate::{Emmyrc, VirtualWorkspace};

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
}
