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

    /// The gmod call-site pass takes a panel method call's receiver from the
    /// file's call targets instead of inferring it before the statements, and
    /// still records the parent call.
    #[test]
    fn gmod_call_sites_read_a_receiver_method_from_the_call_targets() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "lua/vgui/panel.lua",
            "---@class Panel
Panel = {}
",
        );
        let (menu, counts) = counted(|| {
            ws.def_file(
                "lua/vgui/cl_menu.lua",
                "---@class MyMenu: Panel
local PANEL = {}
function PANEL:Init()
    self.Btn = self:Add(\"DButton\")
end
vgui.Register(\"MyMenu\", PANEL, \"Panel\")
",
            )
        });
        let inferred = counts
            .get(&(Work::Infer, Some(DerivationKind::GmodClassCallSites)))
            .map_or(0, |(computed, _)| *computed);
        let parent_calls = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index()
            .get_vgui_parent_calls(&menu)
            .len();

        assert_eq!((inferred, parent_calls), (0, 1));
    }

    /// A callee's callback arguments are inferred once for every call into
    /// it, not once per call: by the callee's query.
    #[test]
    fn call_site_params_infer_a_callee_files_callback_arguments_once() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "addons/probe/lua/autorun/sh_runner.lua",
            "function RunWith(cb)
    cb({ ready = true })
end
",
        );
        let (_, counts) = counted(|| {
            ws.def_file(
                "addons/probe/lua/autorun/sh_caller.lua",
                "RunWith(function(a) end)
RunWith(function(b) end)
",
            )
        });
        let (computed, distinct) = counts
            .get(&(Work::Infer, Some(DerivationKind::CallbackParams)))
            .copied()
            .unwrap_or_default();

        assert!(distinct > 0, "the pass inferred no callback argument");
        assert_eq!(computed, distinct as u64);
    }
}
