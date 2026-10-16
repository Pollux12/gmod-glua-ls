#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr};

    use crate::compilation::analyzer::call_target::{pass_view, resolve_call_target};
    use crate::db_index::derivation::DerivationKind;
    use crate::profile::work::{Work, counted};
    use crate::{Emmyrc, FileId, LuaInferCache, LuaSignatureId, VirtualWorkspace};

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

    /// What each call of `file_ids` targets, as text.
    fn call_targets(ws: &VirtualWorkspace, file_ids: &[FileId]) -> Vec<String> {
        let db = ws.analysis.compilation.get_db();
        let mut targets = Vec::new();
        for file_id in file_ids {
            let root = db
                .get_vfs()
                .get_syntax_tree(file_id)
                .expect("the file is parsed")
                .get_chunk_node();
            for call in root.descendants::<LuaCallExpr>() {
                targets.push(format!(
                    "{file_id:?} {:?} {:?}",
                    call.get_range(),
                    db.get_call_target_index()
                        .target(*file_id, call.get_range())
                ));
            }
        }
        targets
    }

    /// An edit leaves the call targets and diagnostics a cold build of the
    /// edited text has, for a callee that moved and one that went away.
    #[test]
    fn an_edit_leaves_the_call_targets_and_diagnostics_a_cold_build_has() {
        const LIB: &str = "lua/autorun/sh_lib.lua";
        const CALLER: &str = "lua/autorun/sh_caller.lua";
        const CALLER_TEXT: &str = "Greet(\"a\")\nWave(\"b\")\nGreet(\"c\")\n";
        const LIB_TEXT: &str =
            "function Greet(name) return name end\nfunction Wave(name) return name end\n";
        const EDITED_LIB_TEXT: &str =
            "-- lib\nfunction Greet(name) return name end\nfunction Hello(name) return name end\n";

        let mut warm = VirtualWorkspace::new();
        let file_ids = warm.def_files(vec![(LIB, LIB_TEXT), (CALLER, CALLER_TEXT)]);
        warm.def_file(LIB, EDITED_LIB_TEXT);
        let mut cold = VirtualWorkspace::new();
        cold.def_files(vec![(LIB, EDITED_LIB_TEXT), (CALLER, CALLER_TEXT)]);

        let targets = call_targets(&warm, &file_ids);
        assert!(
            targets
                .iter()
                .any(|target| target.contains("declared: Some")),
            "a call still resolves: {targets:?}"
        );
        assert_eq!(targets, call_targets(&cold, &file_ids));
        assert_eq!(warm.semantic_digest(), cold.semantic_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    /// Editing what one call targets re-runs that call's derivation alone,
    /// not the other calls of its file.
    #[test]
    fn editing_one_calls_callee_re_runs_only_that_calls_target() {
        const LIB: &str = "lua/autorun/sh_lib.lua";
        const CALLER: &str = "lua/autorun/sh_caller.lua";
        const LIB_TEXT: &str =
            "LibA = {}\nLibA.f = function() end\n\nLibB = {}\nLibB.f = function() end\n";
        const EDITED_LIB_TEXT: &str =
            "LibA = {}\nLibA.f = nil           \n\nLibB = {}\nLibB.f = function() end\n";
        // Each call reads whether the calls before it return, so the edited
        // callee is called last.
        const CALLER_TEXT: &str = "LibB.f()\nLibB.f()\nLibA.f()\n";

        let mut ws = VirtualWorkspace::new();
        let file_ids = ws.def_files(vec![(LIB, LIB_TEXT), (CALLER, CALLER_TEXT)]);
        ws.def_file(LIB, EDITED_LIB_TEXT);
        let evaluations = ws
            .analysis
            .compilation
            .last_stats()
            .evaluations(DerivationKind::CallTarget);
        let mut cold = VirtualWorkspace::new();
        cold.def_files(vec![(LIB, EDITED_LIB_TEXT), (CALLER, CALLER_TEXT)]);

        assert_eq!(evaluations, 1);
        assert_eq!(call_targets(&ws, &file_ids), call_targets(&cold, &file_ids));
    }

    /// The net passes read each call's callee from the file's call targets
    /// instead of inferring it again, and still find the net roles.
    #[test]
    fn net_helpers_read_the_files_call_targets() {
        let mut ws = gmod_workspace();
        let (file_id, counts) = counted(|| {
            ws.def_file(
                "addons/probe/lua/autorun/server/net_send.lua",
                "net.Start(\"MyMessage\")\nnet.WriteString(\"hello\")\nnet.Broadcast()\n",
            )
        });
        let inferred = [
            DerivationKind::NetCall,
            DerivationKind::NetHelper,
            DerivationKind::NetFlows,
        ]
        .into_iter()
        .map(|kind| {
            counts
                .get(&(Work::Infer, Some(kind)))
                .map_or(0, |(computed, _)| *computed)
        })
        .sum::<u64>();
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
            .get(&(Work::Infer, Some(DerivationKind::CallSiteParam)))
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
            .target(caller, call.get_range())
            .cloned()
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

    /// The call-site evidence of every file, rendered for an exact comparison.
    fn call_site_records(ws: &VirtualWorkspace) -> Vec<(FileId, Vec<String>)> {
        let mut records = ws
            .analysis
            .compilation
            .get_db()
            .get_call_site_param_index()
            .iter_file_contributions()
            .map(|(file_id, contributions)| {
                (
                    *file_id,
                    contributions
                        .into_iter()
                        .map(|contribution| format!("{contribution:?}"))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|(file_id, _)| *file_id);
        records
    }

    const PROBE: &str = "lua/autorun/sh_probe.lua";
    const PROBE_TEXT: &str = "function Probe() return 1 end\n";
    const EDITED_PROBE_TEXT: &str = "function Probe() return 2 end\n";
    const GREETER: &str = "lua/autorun/sh_greeter.lua";
    // Each call reads whether the calls before it return, so the call whose
    // argument depends on the edited file is the last.
    const GREETER_TEXT: &str = "function Greet(ply) return ply end
Greet(\"a\")
Greet(\"b\")
Greet(Probe())
";
    const LOCAL_GREETER_TEXT: &str = "local function Greet(ply) return ply end
Greet(\"a\")
Greet(\"b\")
Greet(Probe())
";

    /// An edit of what a call's argument depends on leaves the call-site
    /// evidence and diagnostics a cold build of the edited text has.
    #[test]
    fn an_edit_leaves_the_call_site_evidence_a_cold_build_has() {
        let mut warm = gmod_workspace();
        let file_ids = warm.def_files(vec![(PROBE, PROBE_TEXT), (GREETER, GREETER_TEXT)]);
        warm.def_file(PROBE, EDITED_PROBE_TEXT);
        let mut cold = gmod_workspace();
        let cold_ids = cold.def_files(vec![(PROBE, EDITED_PROBE_TEXT), (GREETER, GREETER_TEXT)]);

        let records = call_site_records(&warm);
        assert!(
            records
                .iter()
                .any(|(_, contributions)| !contributions.is_empty()),
            "the calls contribute evidence: {records:?}"
        );
        assert_eq!(file_ids, cold_ids);
        assert_eq!(records, call_site_records(&cold));
        assert_eq!(warm.semantic_digest(), cold.semantic_digest());
        assert!(warm.stale_derivations().is_empty());

        warm.def_file(PROBE, PROBE_TEXT);
        let mut reverted = gmod_workspace();
        reverted.def_files(vec![(PROBE, PROBE_TEXT), (GREETER, GREETER_TEXT)]);
        assert_eq!(call_site_records(&warm), call_site_records(&reverted));
        assert_eq!(warm.semantic_digest(), reverted.semantic_digest());
    }

    /// Editing what one call's argument depends on re-runs that call's
    /// call-site derivation alone, not the other calls of its file, including
    /// the calls of the callee whose parameter its evidence moves, and leaves
    /// what a cold build of the edit has. A local callee takes no evidence
    /// from its calls here, so none of them runs again.
    #[test]
    fn editing_one_calls_argument_re_runs_only_that_calls_call_site_params() {
        for (greeter_text, expected) in [(GREETER_TEXT, 1), (LOCAL_GREETER_TEXT, 0)] {
            let mut ws = gmod_workspace();
            ws.def_files(vec![(PROBE, PROBE_TEXT), (GREETER, greeter_text)]);
            ws.def_file(PROBE, EDITED_PROBE_TEXT);
            let mut cold = gmod_workspace();
            cold.def_files(vec![(PROBE, EDITED_PROBE_TEXT), (GREETER, greeter_text)]);

            let evaluations = ws
                .analysis
                .compilation
                .last_stats()
                .evaluations(DerivationKind::CallSiteParam);

            assert_eq!(
                call_site_records(&ws),
                call_site_records(&cold),
                "{greeter_text}"
            );
            assert_eq!(
                ws.semantic_digest(),
                cold.semantic_digest(),
                "{greeter_text}"
            );
            assert!(ws.stale_derivations().is_empty(), "{greeter_text}");
            assert_eq!(evaluations, expected, "{greeter_text}");
        }
    }
    const ICON_LIB: &str = "lua/autorun/sh_icon.lua";
    const ICON_LIB_TEXT: &str = "Glide = Glide or {}
function Glide.DrawIcon(angle) return angle end
";
    const ICON_CALLER: &str = "lua/autorun/sh_icon_caller.lua";

    /// The type the call-site evidence gives the parameter of the function the
    /// caller file's only call targets.
    fn call_site_param_of_only_call(ws: &VirtualWorkspace, caller: FileId) -> Option<String> {
        let db = ws.analysis.compilation.get_db();
        let call = first_call(ws, caller);
        let signature_id = db
            .get_call_target_index()
            .target(caller, call.get_range())
            .and_then(|target| target.signature_id())?;
        db.get_call_site_param_index()
            .get_inferred_param(&signature_id, 0)
            .map(|typ| format!("{typ:?}"))
    }

    fn icon_param(caller_text: &str) -> Option<String> {
        let mut ws = gmod_workspace();
        let ids = ws.def_files(vec![(ICON_LIB, ICON_LIB_TEXT), (ICON_CALLER, caller_text)]);
        call_site_param_of_only_call(&ws, ids[1])
    }

    /// A call through `local f = Lib.Fn` types the parameter from its argument,
    /// as the direct call does, so the parameter's name does not guess its type.
    #[test]
    fn a_call_through_an_unwritten_local_alias_gives_call_site_evidence() {
        let direct = icon_param(
            "Glide.DrawIcon(0)
",
        );
        let alias = icon_param(
            "local DrawIcon = Glide.DrawIcon
DrawIcon(0)
",
        );

        assert!(direct.is_some(), "the direct call contributes");
        assert_eq!(alias, direct);
    }

    /// A reassigned alias may hold another function, so its calls give nothing.
    #[test]
    fn a_call_through_a_reassigned_local_alias_gives_no_call_site_evidence() {
        let alias = icon_param(
            "local DrawIcon = Glide.DrawIcon
DrawIcon = print
DrawIcon(0)
",
        );

        assert_eq!(alias, None);
    }

    /// An alias of either of two functions may call any of them.
    #[test]
    fn a_call_through_a_local_alias_of_two_functions_gives_no_call_site_evidence() {
        let alias = icon_param(
            "local DrawIcon = Glide.DrawIcon or print
DrawIcon(0)
",
        );

        assert_eq!(alias, None);
    }

    /// Removing the alias call leaves the evidence a cold build of the edit has.
    #[test]
    fn removing_a_local_alias_call_re_derives_the_parameter() {
        const WITH_CALL: &str = "local DrawIcon = Glide.DrawIcon
DrawIcon(0)
";
        const WITHOUT_CALL: &str = "local DrawIcon = Glide.DrawIcon
";
        let mut warm = gmod_workspace();
        warm.def_files(vec![(ICON_LIB, ICON_LIB_TEXT), (ICON_CALLER, WITH_CALL)]);
        warm.def_file(ICON_CALLER, WITHOUT_CALL);
        let mut cold = gmod_workspace();
        cold.def_files(vec![(ICON_LIB, ICON_LIB_TEXT), (ICON_CALLER, WITHOUT_CALL)]);

        assert_eq!(call_site_records(&warm), call_site_records(&cold));
        assert_eq!(warm.semantic_digest(), cold.semantic_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    const TWO_ICON_LIB_TEXT: &str = "Glide = Glide or {}
function Glide.DrawIcon(angle) return angle end
Other = Other or {}
function Other.DrawIcon(angle) return angle end
";

    /// What the call-site index holds for the first parameter of each function `ICON_LIB`
    /// defines, once `caller_text` is analysed.
    fn lib_param_evidence(lib_text: &str, caller_text: &str) -> Vec<(bool, Option<String>)> {
        let mut ws = gmod_workspace();
        let ids = ws.def_files(vec![(ICON_LIB, lib_text), (ICON_CALLER, caller_text)]);
        let db = ws.analysis.compilation.get_db();
        let index = db.get_call_site_param_index();
        db.get_vfs()
            .get_syntax_tree(&ids[0])
            .expect("the library is parsed")
            .get_chunk_node()
            .descendants::<LuaClosureExpr>()
            .map(|closure| {
                let signature_id = LuaSignatureId::from_closure(ids[0], &closure);
                (
                    index.has_param_evidence(&signature_id, 0),
                    index
                        .get_inferred_param(&signature_id, 0)
                        .map(|typ| format!("{typ:?}")),
                )
            })
            .collect()
    }

    /// An alias holding one of two functions through another local may call either, so its
    /// calls give nothing to either.
    #[test]
    fn a_call_through_a_local_alias_of_a_local_holding_two_functions_gives_no_evidence() {
        let direct = lib_param_evidence(
            TWO_ICON_LIB_TEXT,
            "Glide.DrawIcon(0)
",
        );
        assert!(
            direct[0].0,
            "the direct call contributes to the first function"
        );

        let alias = lib_param_evidence(
            TWO_ICON_LIB_TEXT,
            "local flag = math.random() > 0.5
local source = flag and Glide.DrawIcon or Other.DrawIcon
local DrawIcon = source
DrawIcon(0)
",
        );

        assert_eq!(alias, vec![(false, None); 2]);
    }

    /// A closure that writes the alias may leave it holding another function.
    #[test]
    fn a_call_through_a_local_alias_written_by_a_closure_gives_no_evidence() {
        let alias = lib_param_evidence(
            ICON_LIB_TEXT,
            "local DrawIcon = Glide.DrawIcon
local function swap()
    DrawIcon = print
end
DrawIcon(0)
",
        );

        assert_eq!(alias, vec![(false, None)]);
    }
}
