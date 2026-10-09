#[cfg(test)]
mod test {
    use crate::db_index::derivation::{DerivationKind, FactKey};
    use crate::{Emmyrc, Schedule, VirtualWorkspace};

    /// A reader ordered before its producer, cold-built under `schedule`:
    /// how often its statement ran, and what the workspace settled to.
    fn scheduled(schedule: Schedule) -> (usize, Vec<String>) {
        let _serial = crate::compilation::analyzer::serial_engine();
        let mut ws = VirtualWorkspace::new();
        ws.analysis.compilation.set_schedule(schedule);
        ws.def_files(vec![
            ("lua/autorun/a_reader.lua", "local value = Shared.Value\n"),
            (
                "lua/autorun/b_producer.lua",
                "Shared = {}\nShared.Value = 1\n",
            ),
        ]);
        let evaluations = ws
            .analysis
            .compilation
            .last_stats()
            .evaluations(DerivationKind::LocalStat);
        (evaluations, ws.semantic_digest())
    }

    #[test]
    fn a_reversed_schedule_runs_the_producer_first_and_settles_the_same() {
        let (canonical_evaluations, canonical) = scheduled(Schedule::Canonical);
        let (reversed_evaluations, reversed) = scheduled(Schedule::Reversed);

        assert_eq!(reversed, canonical);
        assert_eq!((canonical_evaluations, reversed_evaluations), (2, 1));
    }

    /// The passes that infer across a whole file, and the dynamic-field
    /// writes, of which the probe has one.
    const WHOLE_FILE_PASSES: [DerivationKind; 3] = [
        DerivationKind::DynamicFieldWrite,
        DerivationKind::CallSiteParams,
        DerivationKind::LocalFallbacks,
    ];

    /// The passes of each call and function body, which run once for every
    /// call of the probe and every closure with a body.
    const PER_SITE_PASSES: [DerivationKind; 4] = [
        DerivationKind::CallTarget,
        DerivationKind::NetCall,
        DerivationKind::NetHelper,
        DerivationKind::NetFlows,
    ];

    /// The whole-file passes run once the statements have settled, so a read
    /// that settles only at the last stage runs none of them again, nor the
    /// derivation of any of the file's five calls or one closure.
    #[test]
    fn whole_file_passes_run_once_when_a_read_settles_at_the_last_stage() {
        let mut ws = gmod_workspace();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "addons/probe/lua/autorun/server/sv_probe.lua",
            r#"Lib = {}
Lib.Undeclared()
local function greet(name) return name end
greet(Lib.Missing)
local handler = Lib.Handler or greet
handler("x")
local ent = Lib.Other
ent.health = 1
local count = ent.health
net.Start(Lib.Message)
net.Broadcast()
"#,
        );

        let stats = ws.analysis.compilation.last_stats();
        let evaluations = WHOLE_FILE_PASSES.map(|kind| (kind, stats.evaluations(kind)));
        assert_eq!(evaluations, WHOLE_FILE_PASSES.map(|kind| (kind, 1)));
        let reader_graph = ws.analysis.compilation.get_db().reader_graph();
        for kind in PER_SITE_PASSES {
            let sites = reader_graph
                .derivations()
                .filter(|id| id.kind == kind)
                .count();
            assert_eq!(stats.evaluations(kind), sites, "{kind:?}");
        }
        assert_eq!(stats.evaluations(DerivationKind::CallTarget), 5);
        assert_eq!(stats.evaluations(DerivationKind::NetFlows), 5);
        assert_eq!(stats.evaluations(DerivationKind::NetHelper), 1);
        assert_eq!(stats.evaluations(DerivationKind::CallSiteParam), 5);
    }

    /// The helper scan reads what a file's signatures declare, so a return
    /// that inference resolves after the scan does not run it again.
    #[test]
    fn a_resolved_return_does_not_re_run_the_helper_scan() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "addons/probe/lua/autorun/sh_helpers.lua",
            "function SendScore(score)
    net.Start(\"Score\")
    net.WriteInt(score, 16)
    net.Broadcast()
    return score + 1
end
",
        );

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::GmodHelperScan), 1);
    }

    /// A local typed from its uses, whose statement runs again and keeps its
    /// value when another file's function changes shape: the fallback typing
    /// the local stands, and only the edited file's pass runs.
    #[test]
    fn a_statement_that_keeps_its_value_does_not_run_its_files_local_fallbacks_again() {
        let player = (
            "player.lua",
            "---@class Player\n---@param ply Player\nfunction Kick(ply) end\n",
        );
        let lib = (
            "lib.lua",
            "Lib = {}\nfunction Lib.Get(a) return Mystery end\n",
        );
        let fallbacks_after_the_edit = |files: Vec<(&str, &str)>| {
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files);
            ws.def_file(
                "lib.lua",
                "Lib = {}\nfunction Lib.Get(a, b) return Mystery end\n",
            );
            let evaluations = ws
                .analysis
                .compilation
                .last_stats()
                .evaluations(DerivationKind::LocalFallbacks);
            (ws, evaluations)
        };
        let (ws, with_user) = fallbacks_after_the_edit(vec![
            player,
            lib,
            ("user.lua", "local target = Lib.Get(1)\nKick(target)\n"),
        ]);
        let (_, without_user) = fallbacks_after_the_edit(vec![player, lib]);

        assert!(
            ws.settled_digest()
                .iter()
                .any(|line| line.ends_with("target = Player"))
        );
        assert_eq!(with_user, without_user);
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
    fn a_structural_check_against_a_class_reads_its_fields_not_its_methods() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_ids = ws.def_files(vec![
            (
                "lib.lua",
                r#"
                ---@class Widget
                ---@field size integer
                Widget = {}

                ---@param w Widget
                ---@return integer
                ---@overload fun(w: string): string
                function use(w) end
                "#,
            ),
            (
                "methods.lua",
                "function Widget:Extra() end
",
            ),
            (
                "user.lua",
                "local r = use({ size = 1 })
",
            ),
        ]);
        let graph = ws.analysis.compilation.get_db().reader_graph();
        let widget = crate::LuaMemberOwner::Type(crate::LuaTypeDeclId::global("Widget"));
        let user_readers = |key: FactKey| {
            graph
                .derivations()
                .filter(|id| id.file_id == file_ids[2])
                .filter(|id| graph.reads_of(*id).any(|read| **read == key))
                .count()
        };

        assert!(user_readers(FactKey::MemberOwnerFields(widget.clone())) > 0);
        assert_eq!(user_readers(FactKey::MemberOwnerMap(widget)), 0);
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

        assert_eq!(ws.stale_derivations(), Vec::new());
    }

    fn helper_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws
    }

    const SETTER_HELPERS: &str =
        "Glide = {}\nfunction Glide.SetNumber(t, k, v)\n    t[k] = v\nend\n";
    const SETTER_CALLER: &str =
        "local data = {}\nGlide.SetNumber(data, \"buoyancy\", 1)\nlocal offset = data.buoyancy\n";

    fn has_dynamic_field(ws: &VirtualWorkspace, name: &str) -> bool {
        ws.analysis
            .compilation
            .get_db()
            .get_dynamic_field_index()
            .iter_all_field_definitions()
            .any(|(_, fields)| fields.keys().any(|field| field.as_str() == name))
    }

    #[test]
    fn a_callee_edit_that_keeps_its_setter_helpers_leaves_callers_dynamic_fields_settled() {
        let mut ws = helper_workspace();
        ws.def_file("lua/autorun/sh_helpers.lua", SETTER_HELPERS);
        ws.def_file("lua/autorun/sh_caller.lua", SETTER_CALLER);
        assert!(has_dynamic_field(&ws, "buoyancy"));

        ws.def_file(
            "lua/autorun/sh_helpers.lua",
            &format!("{SETTER_HELPERS}local unrelated = 1\n"),
        );

        // The edited file's two write sites run again; the caller's call
        // does not.
        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::DynamicFieldWrite), 2);
        assert!(has_dynamic_field(&ws, "buoyancy"));
    }

    #[test]
    fn a_callers_helper_fields_follow_edit_delete_and_reopen_of_the_helper_file() {
        let mut ws = helper_workspace();
        let helpers_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/sh_helpers.lua");
        ws.def_file("lua/autorun/sh_helpers.lua", SETTER_HELPERS);
        ws.def_file("lua/autorun/sh_caller.lua", SETTER_CALLER);
        assert!(has_dynamic_field(&ws, "buoyancy"));

        ws.def_file(
            "lua/autorun/sh_helpers.lua",
            "Glide = {}\nfunction Glide.SetNumber(t, k, v)\n    t.other = v\nend\n",
        );
        assert!(!has_dynamic_field(&ws, "buoyancy"));

        ws.def_file("lua/autorun/sh_helpers.lua", SETTER_HELPERS);
        assert!(has_dynamic_field(&ws, "buoyancy"));

        ws.analysis.remove_file_by_uri(&helpers_uri);
        assert!(!has_dynamic_field(&ws, "buoyancy"));

        ws.def_file("lua/autorun/sh_helpers.lua", SETTER_HELPERS);
        assert!(has_dynamic_field(&ws, "buoyancy"));
    }

    #[test]
    fn a_callers_call_site_params_read_the_callees_function_summaries_not_its_references() {
        let mut ws = VirtualWorkspace::new();
        let lib = ws.def_file(
            "lua/autorun/sh_lib.lua",
            "Lib = {}\nfunction Lib.Get(self)\n    return self\nend\n",
        );
        let caller = ws.def_file(
            "lua/autorun/sh_use.lua",
            "local obj = {}\nobj.Get = Lib.Get\nlocal v = obj:Get()\n",
        );

        let graph = ws.analysis.compilation.get_db().reader_graph();
        let id = graph
            .derivations_of_file(caller)
            .find(|id| id.kind == DerivationKind::CallSiteParam)
            .expect("caller call-site params");
        let reads = graph.reads_of(id).map(|key| key.key()).collect::<Vec<_>>();
        assert!(reads.contains(&&FactKey::FunctionSummaries(lib)));
        assert!(!reads.contains(&&FactKey::References(lib)));
    }

    /// Twenty-four handlers stored in a shared table under constants a later
    /// file defines, each followed by `after(index)`: each handler's key
    /// lands as its constant does.
    fn command_table_workspace(after: impl Fn(usize) -> String) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let handlers = (0..24)
            .map(|index| {
                format!(
                    "commands[Lib.CMD_{index}] = function()\n    local value = {index}\nend\n{}",
                    after(index)
                )
            })
            .collect::<String>();
        let handlers = format!("local commands = Lib.Commands\n{handlers}");
        let constants = (0..24)
            .map(|index| format!("Lib.CMD_{index} = {index}\n"))
            .collect::<String>();
        ws.def_files(vec![
            (
                "lua/autorun/a_lib.lua",
                "Lib = {}\nLib.Commands = {}\n---@type integer\nLib.Index = 0\n",
            ),
            ("lua/autorun/client/cl_commands.lua", &handlers),
            ("lua/autorun/sh_constants.lua", &constants),
        ]);
        ws
    }

    #[test]
    fn a_handler_under_a_constant_key_runs_a_few_times_however_many_siblings_land() {
        let _serial = crate::compilation::analyzer::serial_engine();
        let ws = command_table_workspace(|_| String::new());

        assert!(ws.analysis.compilation.last_stats().most_runs() <= 4);
    }

    /// A read under a key that is not a constant reads every handler, so
    /// each key that lands moves it; it is checked again once per round, not
    /// once per handler.
    #[test]
    fn a_read_of_every_handler_runs_a_few_times_however_many_siblings_land() {
        let _serial = crate::compilation::analyzer::serial_engine();
        let ws =
            command_table_workspace(|index| format!("local probe_{index} = commands[Lib.Index]\n"));

        assert!(ws.analysis.compilation.last_stats().most_runs() <= 4);
    }
}
