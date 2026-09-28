#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaCallExpr};

    use crate::compilation::analyzer::call_target::{pass_view, resolve_call_target};
    use crate::db_index::derivation::DerivationKind;
    use crate::profile::work::{Work, counted};
    use crate::{Emmyrc, FileId, LuaInferCache, VirtualWorkspace};

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws
    }

    /// The file's first call.
    fn first_call(ws: &VirtualWorkspace, file_id: FileId) -> LuaCallExpr {
        ws.analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("the file is parsed")
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .next()
            .expect("the file has a call")
    }

    /// The net helper pass reads each call's callee from the file's call
    /// targets instead of inferring it again, and still finds the net roles.
    #[test]
    fn net_helpers_read_the_files_call_targets() {
        let mut ws = gmod_workspace();
        let (file_id, counts) = counted(|| {
            ws.def_file(
                "addons/probe/lua/autorun/server/net_send.lua",
                "net.Start(\"MyMessage\")\nnet.WriteString(\"hello\")\nnet.Broadcast()\n",
            )
        });
        let inferred = counts
            .get(&(Work::Infer, Some(DerivationKind::NetHelpers)))
            .map_or(0, |(computed, _)| *computed);
        let flows = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .map_or(0, |data| data.send_flows.len());

        assert_eq!((inferred, flows), (0, 1));
    }

    /// The call-site parameter pass reads a callee another file declares from
    /// the file's call targets instead of inferring the call's prefix again.
    #[test]
    fn call_site_params_read_the_files_call_targets() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/sh_greet.lua",
            "function Greet(name)\n    return name\nend\n",
        );
        let (_, counts) =
            counted(|| ws.def_file("lua/autorun/sh_caller.lua", "Greet()\nGreet()\n"));
        let inferred = counts
            .get(&(Work::Infer, Some(DerivationKind::CallSiteParams)))
            .map_or(0, |(computed, _)| *computed);

        assert_eq!(inferred, 0);
    }

    /// A stored target naming a function in another file moves with it when
    /// that file shifts, as resolving it again would find.
    #[test]
    fn a_call_target_follows_its_callee_when_the_callee_file_shifts() {
        const LIB: &str = "lua/autorun/sh_greet.lua";
        const LIB_TEXT: &str = "function Greet(name)\n    return name\nend\n";
        let mut ws = VirtualWorkspace::new();
        ws.def_file(LIB, LIB_TEXT);
        let caller = ws.def_file("lua/autorun/sh_caller.lua", "Greet(\"x\")\n");
        ws.def_file(LIB, &format!("\n\n{LIB_TEXT}"));

        let call = first_call(&ws, caller);
        let db = ws.analysis.compilation.get_db();
        let stored = db
            .get_call_target_index()
            .file(caller)
            .and_then(|targets| targets.get(&call.get_range()).cloned())
            .expect("the call's target is stored");
        let fresh = resolve_call_target(db, &mut LuaInferCache::new(caller, pass_view()), &call);

        assert!(fresh.declared.is_some(), "the call resolves to Greet");
        assert_eq!(stored, fresh);
    }
}
