#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr};

    use crate::db_index::QueryStats;
    use crate::db_index::derivation::DerivationKind;
    use crate::{Emmyrc, FileId, LuaType, Schedule, VirtualWorkspace};

    const RUNNER: &str = "lua/autorun/sh_runner.lua";
    const RUNNER_TEXT: &str = "function RunWith(cb)\n    cb({ ready = true })\nend\n";
    const CALLERS: [&str; 3] = [
        "lua/autorun/sh_caller_a.lua",
        "lua/autorun/sh_caller_b.lua",
        "lua/autorun/sh_caller_c.lua",
    ];
    const CALLER_TEXT: &str = "RunWith(function(state)\n    local seen = state.ready\nend)\n";

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws
    }

    /// A runner and three files passing it a callback.
    fn runner_workspace() -> (VirtualWorkspace, Vec<FileId>) {
        let mut ws = workspace();
        ws.def_file(RUNNER, RUNNER_TEXT);
        let callers = CALLERS
            .iter()
            .map(|path| ws.def_file(path, CALLER_TEXT))
            .collect();
        (ws, callers)
    }

    fn callback_stats(ws: &VirtualWorkspace) -> QueryStats {
        ws.analysis
            .compilation
            .get_db()
            .queries()
            .callback_params
            .stats()
    }

    fn call_site_param_runs(ws: &VirtualWorkspace) -> usize {
        ws.analysis
            .compilation
            .last_stats()
            .evaluations(DerivationKind::CallSiteParam)
    }

    fn index_expr_type(ws: &VirtualWorkspace, file_id: FileId, text: &str) -> Option<LuaType> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let index_expr = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|expr| expr.syntax().text() == text)
            .expect("index expression");
        semantic_model
            .infer_expr(LuaExpr::IndexExpr(index_expr))
            .ok()
    }

    fn is_string(typ: Option<LuaType>) -> bool {
        matches!(typ, Some(LuaType::String | LuaType::StringConst(_)))
    }

    #[test]
    fn a_callees_callback_args_are_computed_once_for_every_caller_file() {
        let (ws, callers) = runner_workspace();

        assert_eq!(callback_stats(&ws).computed, 1);
        for caller in callers {
            assert_eq!(
                index_expr_type(&ws, caller, "state.ready"),
                Some(LuaType::BooleanConst(true))
            );
        }
    }

    #[test]
    fn a_callee_edit_that_keeps_its_callback_args_reruns_none_of_its_callers() {
        let (mut ws, _) = runner_workspace();
        let before = callback_stats(&ws);

        ws.def_file(RUNNER, &format!("-- moved down a line\n{RUNNER_TEXT}"));

        assert_eq!(call_site_param_runs(&ws), 1, "only the runner's own call");
        assert!(
            callback_stats(&ws).computed + callback_stats(&ws).recomputed
                > before.computed + before.recomputed,
            "the runner's callback args are computed again"
        );
    }

    #[test]
    fn a_callee_edit_that_changes_its_callback_args_reruns_its_callers() {
        let (mut ws, callers) = runner_workspace();

        ws.def_file(
            RUNNER,
            "function RunWith(cb)
    cb({ ready = 7 })
end
",
        );

        assert_eq!(call_site_param_runs(&ws), 1 + CALLERS.len());
        for caller in callers {
            assert_eq!(
                index_expr_type(&ws, caller, "state.ready"),
                Some(LuaType::IntegerConst(7))
            );
        }
    }

    #[test]
    fn an_edit_of_an_unrelated_file_computes_no_callback_args_again() {
        let (mut ws, _) = runner_workspace();
        ws.def_file("lua/autorun/sh_other.lua", "Other = 1\n");
        let before = callback_stats(&ws);

        ws.def_file("lua/autorun/sh_other.lua", "Other = 2\n");

        let after = callback_stats(&ws);
        assert_eq!(
            (after.computed, after.recomputed),
            (before.computed, before.recomputed)
        );
    }

    #[test]
    fn a_callback_arg_read_from_another_file_follows_that_file() {
        let mut ws = workspace();
        ws.def_file(
            "lua/autorun/sh_defaults.lua",
            "Defaults = { ready = true }\n",
        );
        ws.def_file(RUNNER, "function RunWith(cb)\n    cb(Defaults)\nend\n");
        let caller = ws.def_file(
            CALLERS[0],
            "RunWith(function(state)\n    local seen = state.label\nend)\n",
        );
        assert!(!is_string(index_expr_type(&ws, caller, "state.label")));

        ws.def_file(
            "lua/autorun/sh_defaults.lua",
            "Defaults = { ready = true, label = \"x\" }\n",
        );

        assert!(is_string(index_expr_type(&ws, caller, "state.label")));
    }

    /// Under the deferred schedule a query re-queued by a moved read waits for
    /// the next round, while a reader already queued in this one demands it:
    /// the reader computes it afresh rather than taking the stale memo and
    /// running again once the query moves.
    #[test]
    fn a_reader_demanding_a_stale_query_computes_it_afresh() {
        let _serial = crate::compilation::analyzer::serial_engine();
        let mut ws = workspace();
        ws.analysis.compilation.set_schedule(Schedule::Deferred);
        ws.def_file(
            "lua/autorun/sh_defaults.lua",
            "Defaults = { ready = true }
",
        );
        ws.def_file(
            RUNNER,
            "function RunWith(cb)
    cb(Defaults)
end
",
        );
        ws.def_file(
            CALLERS[0],
            "RunWith(function(state)
    local seen = state.label
end)
",
        );

        let files = ws.def_files(vec![
            (
                "lua/autorun/sh_defaults.lua",
                "Defaults = { ready = true, label = \"x\" }
",
            ),
            (
                CALLERS[0],
                "RunWith(function(state)
    local label = state.label
end)
",
            ),
        ]);

        assert_eq!(
            call_site_param_runs(&ws),
            1,
            "the edited caller's call once"
        );
        assert!(is_string(index_expr_type(&ws, files[1], "state.label")));
    }
}
