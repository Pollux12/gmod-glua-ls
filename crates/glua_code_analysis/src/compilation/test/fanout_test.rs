#[cfg(test)]
mod test {
    use crate::db_index::derivation::{DerivationKind, FactKey};
    use crate::{Emmyrc, FileId, VirtualWorkspace};

    const CONFIGURATION: &str = "gamemodes/cityrp/gamemode/core/sh_configuration.lua";
    const MONEY: &str = "gamemodes/cityrp/gamemode/core/sv_money.lua";
    const MOVEMENT: &str = "gamemodes/cityrp/gamemode/core/sv_movement.lua";
    const UNRELATED: &str = "gamemodes/cityrp/gamemode/core/sh_unrelated.lua";

    /// A configuration file in the shape of CityRP's: config-table values and
    /// a global function above a net helper, which puts the file in the
    /// workspace call roles.
    const CONFIGURATION_TEXT: &str = r#"cityrp = cityrp or {}
cityrp.configuration = cityrp.configuration or {}

cityrp.configuration["Default Money"] = 10000
cityrp.configuration["Walk Speed"] = 100

function cityrp.GetConfig(key)
    return cityrp.configuration[key]
end

function cityrp.SendConfig(name)
    net.Start(name)
    net.SendToServer()
end
"#;

    struct Workspace {
        ws: VirtualWorkspace,
        configuration: FileId,
        money: FileId,
    }

    fn cityrp_workspace() -> Workspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let configuration = ws.def_file(CONFIGURATION, CONFIGURATION_TEXT);
        let money = ws.def_file(
            MONEY,
            "local money = cityrp.configuration[\"Default Money\"]\nlocal doubled = money * 2\n",
        );
        ws.def_file(
            MOVEMENT,
            "local speed = cityrp.configuration[\"Walk Speed\"]\n",
        );
        ws.def_file(
            UNRELATED,
            "local x = 1\nlocal function twice(a) return a * 2 end\nlocal y = twice(x)\n",
        );
        Workspace {
            ws,
            configuration,
            money,
        }
    }

    impl Workspace {
        fn edit_configuration(&mut self, from: &str, to: &str) {
            assert!(CONFIGURATION_TEXT.contains(from));
            self.ws
                .def_file(CONFIGURATION, &CONFIGURATION_TEXT.replacen(from, to, 1));
        }

        fn evaluated_files(&self) -> Vec<FileId> {
            self.ws.analysis.compilation.last_evaluated_files().to_vec()
        }

        fn evaluations(&self, kind: DerivationKind) -> usize {
            self.ws.analysis.compilation.last_stats().evaluations(kind)
        }

        fn fingerprint(
            &self,
            key: crate::db_index::derivation::FactKey,
        ) -> crate::db_index::derivation::Fingerprint {
            self.ws.analysis.compilation.get_db().fact_fingerprint(&key)
        }
    }

    const SCORE: &str = "gamemodes/cityrp/gamemode/core/sh_score.lua";
    /// A net helper whose payload write sits in a branch.
    const SCORE_TEXT: &str = r#"function cityrp.SendScore(score)
    net.Start("cityrp_score")
    if score > 0 then
        net.WriteUInt(score, 8)
    end
    net.SendToServer()
end
"#;
    const SCORE_CALLER: &str = "gamemodes/cityrp/gamemode/core/cl_score.lua";
    const SCORE_CALLER_TEXT: &str = "cityrp.SendScore(3)\n";

    const PER_FILE_GMOD_PASSES: [DerivationKind; 3] = [
        DerivationKind::GmodPre,
        DerivationKind::GmodLoadCallSites,
        DerivationKind::GmodClassCallSites,
    ];

    /// The net derivations of each call and function body. The configuration
    /// file has two calls and two functions, so an edit of it, which re-runs
    /// its own sites alone, runs each kind at most twice.
    const PER_SITE_NET_PASSES: [DerivationKind; 3] = [
        DerivationKind::NetCall,
        DerivationKind::NetHelper,
        DerivationKind::NetFlows,
    ];

    /// Values keep their literal types, so a value's readers see it change;
    /// readers of the other values do not.
    #[test]
    fn a_config_value_change_re_runs_only_that_values_readers() {
        let mut workspace = cityrp_workspace();

        workspace.edit_configuration("= 10000", "= 20000");

        assert_eq!(
            workspace.evaluated_files(),
            vec![workspace.configuration, workspace.money]
        );
    }

    const VGUI_REGISTER: &str = "lua/aaa_annotations/vgui.lua";
    const VGUI_REGISTER_TEXT: &str = r#"---@[call_arg("gmod.vgui_panel", "define")]
---@param classname string
function vgui.Register(classname, panelTable, baseName) end
"#;
    const PANEL: &str = "gamemodes/cityrp/gamemode/core/derma/cl_character_money.lua";
    /// A registered panel whose field reads a config value: the registration
    /// moves the field onto the panel class.
    const PANEL_TEXT: &str = r#"local PANEL = {}
PANEL.Money = cityrp.configuration["Default Money"]

function PANEL:Init()
    self.amount = self.Money
end

vgui.Register("cityrp_Character_Money", PANEL, "DPanel")
"#;

    /// The panel field's own statement runs again when the value it reads
    /// changes; the registration that moved the field runs only for what it
    /// reads, so the edit runs as many registration passes as it does without
    /// the panel.
    #[test]
    fn a_config_value_change_re_runs_a_registered_panel_field_without_its_registration() {
        let gmod_post_runs = |with_panel: bool| {
            let mut workspace = cityrp_workspace();
            workspace.ws.def_file(VGUI_REGISTER, VGUI_REGISTER_TEXT);
            let panel = with_panel.then(|| workspace.ws.def_file(PANEL, PANEL_TEXT));

            workspace.edit_configuration("= 10000", "= 20000");

            if let Some(panel) = panel {
                assert!(workspace.evaluated_files().contains(&panel));
            }
            workspace.evaluations(DerivationKind::GmodPost)
        };

        assert_eq!(gmod_post_runs(true), gmod_post_runs(false));
    }

    /// The registration's move of a panel field is its own fact, so the
    /// field's member record has the field's statement as its only producer.
    #[test]
    fn a_registered_panel_field_is_produced_by_its_statement_alone() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(VGUI_REGISTER, VGUI_REGISTER_TEXT);
        let panel = workspace.ws.def_file(PANEL, PANEL_TEXT);
        let db = workspace.ws.analysis.compilation.get_db();
        let field = db
            .get_member_index()
            .get_file_members(panel)
            .into_iter()
            .find(|member| member.get_key().get_name() == Some("Money"))
            .expect("the panel field")
            .get_id();

        let producers = db
            .reader_graph()
            .producers(&crate::db_index::derivation::HashedKey::new(
                FactKey::Member(field),
            ))
            .map(|producer| producer.kind)
            .filter(|kind| *kind != DerivationKind::Source)
            .collect::<Vec<_>>();
        let transfers = db
            .reader_graph()
            .producers(&crate::db_index::derivation::HashedKey::new(
                FactKey::MemberTransfer(field),
            ))
            .map(|producer| producer.kind)
            .collect::<Vec<_>>();

        assert_eq!(
            (producers, transfers),
            (
                vec![DerivationKind::AssignStat],
                vec![DerivationKind::GmodPost]
            )
        );
    }

    /// The workspace's net helper names come from the statements around the
    /// net calls and their callers, so a config value written elsewhere in
    /// the same file does not re-run them.
    #[test]
    fn a_config_value_change_does_not_re_run_the_net_helper_names() {
        let mut workspace = cityrp_workspace();

        workspace.edit_configuration("= 10000", "= 20000");

        assert_eq!(workspace.evaluations(DerivationKind::NetHelperNames), 0);
    }

    /// A realm branch added to a shared file changes the realm only inside the
    /// branch, so a file whose reads fall outside it stands.
    #[test]
    fn a_realm_branch_added_to_a_shared_file_re_runs_nothing_in_its_readers() {
        let mut workspace = cityrp_workspace();

        workspace.edit_configuration(
            "cityrp.configuration[\"Walk Speed\"] = 100\n",
            "cityrp.configuration[\"Walk Speed\"] = 100\n\nif SERVER then\n    cityrp.realmProbe = 1\nend\n",
        );

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// Wrapping a value in a realm branch moves the realm its readers see, so
    /// they run again.
    #[test]
    fn wrapping_a_value_in_a_realm_branch_re_runs_its_reader() {
        let mut workspace = cityrp_workspace();
        let movement = workspace
            .ws
            .analysis
            .get_file_id(&workspace.ws.virtual_url_generator.new_uri(MOVEMENT));

        workspace.edit_configuration(
            "cityrp.configuration[\"Walk Speed\"] = 100
",
            "if SERVER then
    cityrp.configuration[\"Walk Speed\"] = 100
end
",
        );

        assert!(
            workspace
                .evaluated_files()
                .contains(&movement.expect("movement file"))
        );
    }

    /// The realm read at one offset follows the load graph: another file
    /// starting to send a server file to clients re-runs a client file that
    /// uses it, though neither of the two was edited.
    #[test]
    fn a_realm_the_load_graph_changes_re_runs_a_reader_at_that_offset() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/utils.lua",
            "Utils = Utils or {}\nfunction Utils.Ping() return 1 end\n",
        );
        ws.def_file("lua/sv_init.lua", "include(\"utils.lua\")\n");
        let reader = ws.def_file(
            "lua/autorun/client/cl_use.lua",
            "local pinged = Utils.Ping()\n",
        );

        ws.def_file(
            "lua/sv_init.lua",
            "include(\"utils.lua\")\nAddCSLuaFile(\"utils.lua\")\n",
        );

        assert!(
            ws.analysis
                .compilation
                .last_evaluated_files()
                .contains(&reader)
        );
    }

    #[test]
    fn a_config_type_change_re_runs_only_its_readers() {
        let mut workspace = cityrp_workspace();

        workspace.edit_configuration("= 10000", "= \"10000\"");

        assert_eq!(
            workspace.evaluated_files(),
            vec![workspace.configuration, workspace.money]
        );
        for kind in PER_FILE_GMOD_PASSES {
            assert_eq!(workspace.evaluations(kind), 1, "{kind:?}");
        }
        for kind in PER_SITE_NET_PASSES {
            assert!(workspace.evaluations(kind) <= 2, "{kind:?}");
        }
    }

    /// The load graph is folded from each file's load sites and load scan,
    /// which the file's own pass takes; the fold reads no file's text.
    #[test]
    fn the_load_graph_reads_no_file_text() {
        let workspace = cityrp_workspace();
        let graph = workspace.ws.analysis.compilation.get_db().reader_graph();

        let text_reads = graph
            .reads_of(crate::db_index::derivation::DerivationId::workspace(
                DerivationKind::GmodLoad,
            ))
            .filter(|key| matches!(key.key(), crate::db_index::derivation::FactKey::FileText(_)))
            .count();

        assert_eq!(text_reads, 0);
    }

    #[test]
    fn adding_a_parameter_re_runs_no_gmod_pass_in_files_that_never_mention_the_function() {
        let mut workspace = cityrp_workspace();

        workspace.edit_configuration(
            "function cityrp.GetConfig(key)",
            "function cityrp.GetConfig(key, fallback)",
        );

        for kind in PER_FILE_GMOD_PASSES {
            assert_eq!(workspace.evaluations(kind), 1, "{kind:?}");
        }
        for kind in PER_SITE_NET_PASSES {
            assert!(workspace.evaluations(kind) <= 2, "{kind:?}");
        }
    }

    /// A caller's network flows read the summaries of the helpers they
    /// expand, so a change to another function beside the helper leaves the
    /// caller standing.
    #[test]
    fn adding_a_parameter_beside_a_helper_re_runs_nothing_in_the_helpers_caller() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_config.lua",
            "cityrp.SendConfig(\"cityrp_config\")\n",
        );

        workspace.edit_configuration(
            "function cityrp.GetConfig(key)",
            "function cityrp.GetConfig(key, fallback)",
        );

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// A statement added inside a function's body changes neither its member,
    /// its signature nor the functions after it, so its callers stand.
    #[test]
    fn a_statement_added_inside_a_called_function_re_runs_nothing_in_its_caller() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/sv_speed.lua",
            "local speed = cityrp.GetConfig(\"Walk Speed\")\nlocal channel = cityrp.SendConfig(\"cityrp_speed\")\n",
        );

        workspace.edit_configuration(
            "    return cityrp.configuration[key]",
            "    local unused = 1\n    return cityrp.configuration[key]",
        );

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// Each entity's `include("shared.lua")` finds the module at its own full
    /// path, so removing one entity's `shared.lua` leaves another standing.
    #[test]
    fn deleting_one_entitys_shared_file_re_runs_nothing_in_another_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let files = ws.def_files(vec![
            ("lua/entities/alpha/shared.lua", "ENT.Type = \"anim\"\n"),
            ("lua/entities/alpha/init.lua", "include(\"shared.lua\")\n"),
            ("lua/entities/beta/shared.lua", "ENT.Type = \"anim\"\n"),
            ("lua/entities/beta/init.lua", "include(\"shared.lua\")\n"),
        ]);
        let alpha_shared = ws
            .virtual_url_generator
            .new_uri("lua/entities/alpha/shared.lua");

        ws.analysis.remove_file_by_uri(&alpha_shared);

        let evaluated = ws.analysis.compilation.last_evaluated_files();
        assert!(
            !evaluated.contains(&files[2]) && !evaluated.contains(&files[3]),
            "{evaluated:?}"
        );
    }

    /// Deleting the library a local's initializer calls re-runs the file's
    /// guessed locals, and a guess that settles as it was reaches no reader:
    /// the guesses are made once the run closes, not taken back before it.
    #[test]
    fn deleting_a_library_leaves_the_readers_of_an_unchanged_guess_alone() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let files = ws.def_files(vec![
            (
                "gamemodes/cityrp/gamemode/core/sh_classes.lua",
                "---@class Vector\n---@class Entity\n---@class Player: Entity\n---@field GetShootPos fun(self: Player): Vector\n",
            ),
            (
                "gamemodes/cityrp/gamemode/core/sh_data.lua",
                "cityrp = cityrp or {}\ncityrp.data = {}\nfunction cityrp.data.Get(key)\n    return {}\nend\n",
            ),
            (
                "gamemodes/cityrp/gamemode/core/sh_plugin.lua",
                "local unloaded = cityrp.data.Get(\"unloaded\")\nunloaded.first = true\n\n---@type Entity\nlocal owner\nlocal position = owner:GetShootPos()\nPluginOwner = owner\n",
            ),
            (
                "gamemodes/cityrp/gamemode/core/sv_reader.lua",
                "local owner = PluginOwner\n",
            ),
        ]);
        let data = ws
            .virtual_url_generator
            .new_uri("gamemodes/cityrp/gamemode/core/sh_data.lua");

        ws.analysis.remove_file_by_uri(&data);

        let evaluated = ws.analysis.compilation.last_evaluated_files();
        assert!(evaluated.contains(&files[2]), "{evaluated:?}");
        assert!(!evaluated.contains(&files[3]), "{evaluated:?}");
    }

    /// A helper's summary keeps the parameters its message and argument
    /// templates read, so a parameter nothing reads leaves its callers' network
    /// flows standing: only the flows of the helper's own three calls run.
    #[test]
    fn adding_an_unread_parameter_to_a_net_helper_re_runs_no_callers_network_flows() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(SCORE, SCORE_TEXT);
        workspace.ws.def_file(SCORE_CALLER, SCORE_CALLER_TEXT);

        workspace.ws.def_file(
            SCORE,
            &SCORE_TEXT.replace(
                "function cityrp.SendScore(score)",
                "function cityrp.SendScore(score, extra)",
            ),
        );

        assert_eq!(workspace.evaluations(DerivationKind::NetFlows), 3);
    }

    #[test]
    fn a_shift_in_a_file_that_defines_a_helper_re_runs_nothing_in_other_files() {
        let mut workspace = cityrp_workspace();

        workspace.edit_configuration(
            "cityrp.configuration[\"Default Money\"]",
            " cityrp.configuration[\"Default Money\"]",
        );

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// A read that held because the edit only moved positions reads the value
    /// as it stands after the edit, so the next edit compares against that.
    #[test]
    fn a_second_shift_in_a_file_that_defines_a_helper_re_runs_nothing_in_other_files() {
        let mut workspace = cityrp_workspace();
        let default_money = "cityrp.configuration[\"Default Money\"]";
        workspace.edit_configuration(default_money, &format!(" {default_money}"));

        workspace.edit_configuration(default_money, &format!("  {default_money}"));

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// A call into another file's net helper reads the statement that defines
    /// the helper, so a statement added elsewhere in that file leaves the
    /// caller standing.
    #[test]
    fn a_statement_added_after_a_helper_another_file_calls_re_runs_nothing_in_the_caller() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_config_menu.lua",
            "cityrp.SendConfig(\"cityrp_config\")\n",
        );

        workspace.ws.def_file(
            CONFIGURATION,
            &format!("{CONFIGURATION_TEXT}local unused = 1\n"),
        );

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// A caller in another file whose stored call target the shift moved is
    /// re-run once the edited file settles, so it names the helper's new place
    /// as a caller analysed after the shift does.
    #[test]
    fn a_shift_above_a_helper_moves_the_call_target_another_file_stored() {
        const CALLER: &str = "gamemodes/cityrp/gamemode/core/cl_config_menu.lua";
        const CALLER_TEXT: &str = "cityrp.SendConfig(\"cityrp_config\")\n";
        let shifted = format!("\n{CONFIGURATION_TEXT}");
        let mut edited = cityrp_workspace();
        let caller = edited.ws.def_file(CALLER, CALLER_TEXT);
        edited.ws.def_file(CONFIGURATION, &shifted);
        let mut analysed_after = cityrp_workspace();
        analysed_after.ws.def_file(CONFIGURATION, &shifted);
        analysed_after.ws.def_file(CALLER, CALLER_TEXT);

        let call = rowan::TextRange::up_to(rowan::TextSize::of(CALLER_TEXT.trim_end()));
        let key = crate::db_index::derivation::FactKey::NetCall(crate::InFiled::new(caller, call));
        assert_eq!(
            edited.fingerprint(key.clone()),
            analysed_after.fingerprint(key)
        );
    }

    /// The payload a call composes from a helper in another file names the
    /// helper's positions, which follow a shift there as a caller analysed
    /// after the shift names them.
    #[test]
    fn a_shift_above_a_payload_helper_moves_the_ops_another_file_composed() {
        let shifted = format!("\n{SCORE_TEXT}");
        let mut edited = cityrp_workspace();
        edited.ws.def_file(SCORE, SCORE_TEXT);
        let caller = edited.ws.def_file(SCORE_CALLER, SCORE_CALLER_TEXT);
        edited.ws.def_file(SCORE, &shifted);
        let mut analysed_after = cityrp_workspace();
        analysed_after.ws.def_file(SCORE, &shifted);
        analysed_after.ws.def_file(SCORE_CALLER, SCORE_CALLER_TEXT);

        let key = crate::db_index::derivation::FactKey::NetworkFile(caller);
        assert_eq!(
            edited.fingerprint(key.clone()),
            analysed_after.fingerprint(key)
        );
    }

    #[test]
    fn a_shift_above_a_payload_helper_another_file_calls_re_runs_nothing_in_the_caller() {
        let mut workspace = cityrp_workspace();
        let score = workspace.ws.def_file(SCORE, SCORE_TEXT);
        workspace.ws.def_file(SCORE_CALLER, SCORE_CALLER_TEXT);

        workspace.ws.def_file(SCORE, &format!("\n{SCORE_TEXT}"));

        assert_eq!(workspace.evaluated_files(), vec![score]);
    }

    /// The edit remap carries the call targets other files stored into the
    /// shifted file, so their net helpers stand.
    #[test]
    fn a_shift_above_a_helper_another_file_calls_re_runs_nothing_in_the_caller() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_config_menu.lua",
            "cityrp.SendConfig(\"cityrp_config\")\n",
        );

        workspace
            .ws
            .def_file(CONFIGURATION, &format!("\n{CONFIGURATION_TEXT}"));

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// A second shift compares the caller's reads against the first shift's
    /// result, not against the text before it.
    #[test]
    fn a_second_shift_above_a_helper_another_file_calls_re_runs_nothing_in_the_caller() {
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_config_menu.lua",
            "cityrp.SendConfig(\"cityrp_config\")\n",
        );
        workspace
            .ws
            .def_file(CONFIGURATION, &format!("\n{CONFIGURATION_TEXT}"));

        workspace
            .ws
            .def_file(CONFIGURATION, &format!("\n\n{CONFIGURATION_TEXT}"));

        assert_eq!(workspace.evaluated_files(), vec![workspace.configuration]);
    }

    /// Whether another file's write bootstraps a shared table is read off the
    /// statement that writes it, so a statement added elsewhere in that file
    /// leaves the other files' bootstraps standing.
    #[test]
    fn a_statement_added_after_a_shared_table_bootstrap_re_runs_nothing_in_other_bootstraps() {
        const MENU: &str = "gamemodes/cityrp/gamemode/core/derma/cl_menu.lua";
        const MENU_TEXT: &str = "cityrp.menu = cityrp.menu or {}\n";
        let mut workspace = cityrp_workspace();
        let menu = workspace.ws.def_file(MENU, MENU_TEXT);
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/derma/cl_interaction.lua",
            MENU_TEXT,
        );

        workspace
            .ws
            .def_file(MENU, &format!("{MENU_TEXT}local unused = 1\n"));

        assert_eq!(workspace.evaluated_files(), vec![menu]);
    }

    /// The call-site index keeps the evidence other files give a re-indexed
    /// file's parameters, so the callers do not gather it again.
    #[test]
    fn a_statement_added_to_a_file_whose_function_another_file_calls_re_runs_nothing_in_the_caller()
    {
        const TRACK: &str = "gamemodes/cityrp/gamemode/core/sv_track.lua";
        const TRACK_TEXT: &str =
            "function cityrp.TrackVehicle(vehicle)\n    return vehicle.speed\nend\n";
        let mut workspace = cityrp_workspace();
        let track = workspace.ws.def_file(TRACK, TRACK_TEXT);
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/sv_vehicle.lua",
            "---@class CityVehicle\n---@field speed number\nlocal Vehicle = {}\nfunction Vehicle:Spawn()\n    cityrp.TrackVehicle(self)\nend\n",
        );

        workspace
            .ws
            .def_file(TRACK, &format!("{TRACK_TEXT}local unused = 1\n"));

        assert_eq!(workspace.evaluated_files(), vec![track]);
    }

    /// A write into a table another file bootstraps reads positions a shift
    /// there moves, but what it writes names none of them, so it stands.
    #[test]
    fn a_shift_in_a_file_that_bootstraps_a_table_re_runs_nothing_in_a_panel_writing_into_it() {
        const MENU: &str = "gamemodes/cityrp/gamemode/core/derma/cl_menu.lua";
        const MENU_TEXT: &str = "cityrp.menu = cityrp.menu or {}\n";
        let mut workspace = cityrp_workspace();
        let menu = workspace.ws.def_file(MENU, MENU_TEXT);
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/derma/cl_interaction.lua",
            "local PANEL = {}\nfunction PANEL:Init()\n    cityrp.menu.interaction = self\nend\nvgui.Register(\"cityrp_interaction\", PANEL, \"DPanel\")\n",
        );

        workspace.ws.def_file(MENU, &format!("\n{MENU_TEXT}"));

        assert_eq!(workspace.evaluated_files(), vec![menu]);
    }

    /// A caller infers a registered panel method's receiver from the panel
    /// table's last write before the registration, which the assignment
    /// around the registration decides; a comment appended to the panel's
    /// file leaves the caller standing.
    #[test]
    fn a_comment_appended_to_a_panel_whose_method_another_file_calls_re_runs_nothing_in_it() {
        const CARD: &str = "gamemodes/cityrp/gamemode/core/derma/cl_card.lua";
        const CARD_TEXT: &str = "local PANEL = {}\nfunction PANEL:SetTitle(title)\n    self.title = title\nend\nvgui.Register(\"cityrp_card\", PANEL, \"DPanel\")\n";
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(CARD, CARD_TEXT);
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/derma/cl_hud.lua",
            "local card = vgui.Create(\"cityrp_card\")\ncard:SetTitle(\"x\")\n",
        );

        let card = workspace
            .ws
            .def_file(CARD, &format!("{CARD_TEXT}\n-- touch\n"));

        assert_eq!(workspace.evaluated_files(), vec![card]);
    }

    /// The panel table's writes a caller consults compare equal with a shift
    /// in the panel's file mapped back, so the caller stands.
    #[test]
    fn a_shift_in_a_panel_whose_method_another_file_calls_re_runs_nothing_in_it() {
        const CARD: &str = "gamemodes/cityrp/gamemode/core/derma/cl_card.lua";
        const CARD_TEXT: &str = "local PANEL = {}\nfunction PANEL:SetTitle(title)\n    self.title = title\nend\nvgui.Register(\"cityrp_card\", PANEL, \"DPanel\")\n";
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(CARD, CARD_TEXT);
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/derma/cl_hud.lua",
            "local card = vgui.Create(\"cityrp_card\")\ncard:SetTitle(\"x\")\n",
        );

        let card = workspace.ws.def_file(CARD, &format!("\n{CARD_TEXT}"));

        assert_eq!(workspace.evaluated_files(), vec![card]);
    }

    /// A new call role reaches only the files whose syntax names its path.
    #[test]
    fn annotating_a_net_payload_helper_re_runs_only_files_that_call_it() {
        const WRAPPER: &str = "gamemodes/cityrp/gamemode/core/sh_netwrap.lua";
        const WRAPPER_TEXT: &str = "function WriteCityName(value) end\n";
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(WRAPPER, WRAPPER_TEXT);
        let caller = workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_city.lua",
            "net.Start(\"cityrp_city\")\nWriteCityName(\"x\")\nnet.SendToServer()\n",
        );

        let wrapper = workspace.ws.def_file(
            WRAPPER,
            &format!("---@[net_payload(\"write\", \"string\")]\n{WRAPPER_TEXT}"),
        );

        assert_eq!(workspace.evaluated_files(), vec![wrapper, caller]);
    }

    /// A new net helper name reaches only the files that call a function by
    /// that name.
    #[test]
    fn a_function_becoming_a_net_helper_re_runs_only_files_that_call_it() {
        const PING: &str = "gamemodes/cityrp/gamemode/core/sh_ping.lua";
        const PING_TEXT: &str = "function cityrp.Ping()\nend\n";
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(PING, PING_TEXT);
        let caller = workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_ping.lua",
            "cityrp.Ping()\n",
        );

        let ping = workspace.ws.def_file(
            PING,
            &PING_TEXT.replace(
                "\nend",
                "\n    net.Start(\"cityrp_ping\")\n    net.SendToServer()\nend",
            ),
        );

        assert_eq!(workspace.evaluated_files(), vec![ping, caller]);
    }

    /// Each file's valid-guard scan asks about the names of its own globals,
    /// so a function becoming a valid guard re-runs nothing in a file that
    /// declares none of that name.
    #[test]
    fn a_function_becoming_a_valid_guard_re_runs_nothing_in_files_without_that_global() {
        const GUARD: &str = "gamemodes/cityrp/gamemode/core/sh_guard.lua";
        const GUARD_TEXT: &str =
            "---@param value any\n---@return TypeGuard<any>\nfunction IsValidThing(value) end\n";
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (GUARD, GUARD_TEXT),
            (
                "gamemodes/cityrp/gamemode/core/sh_other.lua",
                "function IsOtherThing(value) return value ~= nil end\n",
            ),
        ]);

        let guard = ws.def_file(
            GUARD,
            &GUARD_TEXT.replace("function", "---@[valid_guard]\nfunction"),
        );

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![guard]);
    }

    /// Per-file passes look call roles up by path; only the workspace folds
    /// read the whole registry.
    #[test]
    fn no_per_file_derivation_reads_the_whole_call_role_registry() {
        let workspace = cityrp_workspace();
        let graph = workspace.ws.analysis.compilation.get_db().reader_graph();
        let registry = crate::db_index::derivation::HashedKey::new(
            crate::db_index::derivation::FactKey::HelperRegistry,
        );

        let per_file_readers = graph
            .stale_readers(&registry, |_| false)
            .filter(|reader| reader.file_id != FileId::VIRTUAL)
            .map(|reader| reader.kind)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(per_file_readers, std::collections::BTreeSet::new());
    }

    /// Whether a table literal is array-shaped is read off the statement that
    /// holds it, so a statement added elsewhere in its file leaves the files
    /// that index it standing.
    #[test]
    fn a_statement_added_after_a_shaped_table_another_file_indexes_re_runs_nothing_in_it() {
        const PALETTE: &str = "gamemodes/cityrp/gamemode/core/sh_palette.lua";
        const PALETTE_TEXT: &str = "Palette = { { 255, 0, 0 }, { 0, 255, 0 } }\n";
        let mut workspace = cityrp_workspace();
        let palette = workspace.ws.def_file(PALETTE, PALETTE_TEXT);
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/cl_paint.lua",
            "for i = 1, 2 do local color = Palette[i] end\n",
        );

        workspace
            .ws
            .def_file(PALETTE, &format!("{PALETTE_TEXT}local unused = 1\n"));

        assert_eq!(workspace.evaluated_files(), vec![palette]);
    }

    #[test]
    fn a_shift_in_a_file_that_defines_a_net_payload_wrapper_re_runs_nothing_in_other_files() {
        const WRAPPER: &str = "gamemodes/cityrp/gamemode/core/sh_netwrap.lua";
        const WRAPPER_TEXT: &str = "---@[net_payload(\"write\", \"cityrp_name\")]
function WriteCityName(value) end
";
        let mut workspace = cityrp_workspace();
        let wrapper = workspace.ws.def_file(WRAPPER, WRAPPER_TEXT);

        workspace.ws.def_file(
            WRAPPER,
            &format!(
                "
{WRAPPER_TEXT}"
            ),
        );

        assert_eq!(workspace.evaluated_files(), vec![wrapper]);
    }

    #[test]
    fn a_shift_in_a_file_that_bootstraps_a_shared_global_re_runs_nothing_in_other_files() {
        const PLUGIN: &str = "gamemodes/cityrp/gamemode/core/sh_plugin.lua";
        const PLUGIN_TEXT: &str = "cityrp = cityrp or {}
";
        let mut workspace = cityrp_workspace();
        let plugin = workspace.ws.def_file(PLUGIN, PLUGIN_TEXT);

        workspace.ws.def_file(
            PLUGIN,
            &format!(
                "
{PLUGIN_TEXT}"
            ),
        );

        assert_eq!(workspace.evaluated_files(), vec![plugin]);
    }

    /// A file documented early reads a class that a later file's `---@class`
    /// gives a supertype. Resolving the name only needs the declaration, so
    /// the supertype landing afterwards leaves the early file's source layer
    /// standing.
    #[test]
    fn a_supertype_documented_later_rebuilds_no_source_layer() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            ("lua/includes/entity.lua", "---@class Entity\n"),
            (
                "gamemodes/cityrp/gamemode/core/a_uses_player.lua",
                "---@param ply? Player\nfunction UsePlayer(ply) end\n",
            ),
            (
                "gamemodes/cityrp/gamemode/core/z_player.lua",
                "---@class Player : Entity\n",
            ),
        ]);

        assert_eq!(ws.analysis.compilation.last_stats().rebuilt_sources, 0);
    }

    /// The standard library, loaded before the workspace, binds `string` to
    /// its class; a workspace file initialising `string` again adds a
    /// definition site. Where the path's members live is the member index's
    /// own placement, so the library's source layer stands.
    #[test]
    fn a_workspace_file_initialising_a_library_global_rebuilds_no_library_source() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "gamemodes/cityrp/gamemode/core/sh_string.lua",
            "string = {}\nfunction string.Explode(separator, str) end\n",
        );

        assert_eq!(ws.analysis.compilation.last_stats().rebuilt_sources, 0);
    }

    /// `---@class Glide` moves every `Glide.*` member onto the class,
    /// including the ones another file writes. Re-indexing that other file
    /// files its members onto the class again by itself, so the class's file
    /// is not re-analysed.
    #[test]
    fn re_indexing_a_file_that_writes_a_class_bound_global_re_runs_only_that_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let files = ws.def_files(vec![
            (
                "lua/autorun/sh_glide.lua",
                "---@class Glide\nGlide = Glide or {}\nGlide.VEHICLE_TYPE = {}\n",
            ),
            (
                "lua/glide/sh_lighting_api.lua",
                "Glide.Lighting = Glide.Lighting or {}\nlocal adapters = Glide.Lighting\n",
            ),
        ]);
        let lighting = files[1];

        ws.analysis.reindex_files(vec![lighting]);

        assert_eq!(ws.analysis.compilation.last_stats().rebuilt_sources, 0);
        assert_eq!(
            ws.analysis.compilation.last_evaluated_files(),
            vec![lighting]
        );
    }

    /// A path whose class a derivation binds keeps nothing under its own key,
    /// whichever order its writers were filed in, so re-indexing another
    /// file that writes through the path leaves the binding file alone.
    #[test]
    fn re_indexing_a_writer_of_a_derivation_bound_global_re_runs_only_that_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let files = ws.def_files(vec![
            (
                "lua/glide/server/fuel.lua",
                "local A = X.Y\n\nfunction X.Y.f() end\n",
            ),
            (
                "lua/glide/sh_fuel.lua",
                "X = X or {}\nX.Y = X.Y or {}\n\n---@class C\nlocal A = X.Y\n\nfunction A.g() end\n\nX.Y = A\n",
            ),
        ]);
        let writer = files[0];
        let path = crate::GlobalId::new("X.Y");
        let class = crate::LuaMemberOwner::Type(crate::LuaTypeDeclId::global("C"));
        {
            let index = ws.analysis.compilation.get_db().get_member_index();
            assert!(
                !index.sorted_global_path_owners().contains(&path),
                "{:?}",
                index.sorted_global_path_owners()
            );
            let homed = index
                .get_members(&class)
                .unwrap_or_default()
                .iter()
                .any(|member| member.get_id().file_id == writer);
            assert!(homed, "the writer's member is homed on the bound class");
        }

        ws.analysis.reindex_files(vec![writer]);

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![writer]);
        assert!(
            !ws.analysis
                .compilation
                .get_db()
                .get_member_index()
                .sorted_global_path_owners()
                .contains(&path)
        );
    }

    /// The class's file re-homed the other file's members, but where those
    /// members sit in their file is that file's own business: editing it
    /// leaves the class's file standing.
    #[test]
    fn editing_a_file_that_writes_a_class_bound_global_rebuilds_no_other_source() {
        const LIGHTING: &str = "lua/glide/sh_lighting_api.lua";
        const LIGHTING_TEXT: &str = "Glide.Lighting = Glide.Lighting or {}
local adapters = Glide.Lighting
";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (
                "lua/autorun/sh_glide.lua",
                "---@class Glide
Glide = Glide or {}
Glide.VEHICLE_TYPE = {}
",
            ),
            (LIGHTING, LIGHTING_TEXT),
        ]);

        ws.def_file(
            LIGHTING,
            &format!(
                "{LIGHTING_TEXT}local count = 1
"
            ),
        );

        assert_eq!(ws.analysis.compilation.last_stats().rebuilt_sources, 0);
    }

    /// A class's declaration sits at a position in its file; a shift there
    /// moves it without changing the class, so its readers elsewhere stand.
    #[test]
    fn a_shift_in_a_file_that_declares_a_class_re_runs_nothing_in_other_files() {
        const CAR: &str = "gamemodes/cityrp/gamemode/core/sh_car.lua";
        const CAR_TEXT: &str = "---@class Car
---@field speed number
Car = {}
";
        let mut ws = VirtualWorkspace::new();
        let files = ws.def_files(vec![
            (CAR, CAR_TEXT),
            (
                "gamemodes/cityrp/gamemode/core/sv_drive.lua",
                "---@param car Car
function Drive(car) local speed = car.speed end
",
            ),
        ]);
        let car = files[0];

        ws.def_file(
            CAR,
            &format!(
                "
{CAR_TEXT}"
            ),
        );

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![car]);
    }

    /// A field a class does not declare is looked up on the local tables bound
    /// to it; an edit elsewhere in their file leaves that lookup standing.
    #[test]
    fn a_comment_appended_to_a_file_with_a_local_class_table_re_runs_nothing_in_other_files() {
        const BAG: &str = "gamemodes/cityrp/gamemode/core/sh_bag.lua";
        const BAG_TEXT: &str = "---@class Bag
local Bag = {}
Bag.size = 1
";
        let mut ws = VirtualWorkspace::new();
        let files = ws.def_files(vec![
            (BAG, BAG_TEXT),
            (
                "gamemodes/cityrp/gamemode/core/sv_carry.lua",
                "---@param bag Bag
function Carry(bag) local weight = bag.weight end
",
            ),
        ]);
        let bag = files[0];

        ws.def_file(
            BAG,
            &format!(
                "{BAG_TEXT}-- appended
"
            ),
        );

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![bag]);
    }

    /// Each file declares the class it names by string through its own
    /// contribution, so a shift in one leaves the other's declaration
    /// standing.
    #[test]
    fn a_shift_in_one_of_two_files_that_name_a_class_by_string_re_runs_only_that_file() {
        const LIGHTING: &str = "lua/glide/sh_lighting_api.lua";
        const LIGHTING_TEXT: &str =
            "for _, controller in ipairs(ents.FindByClass(\"photon_controller\")) do end\n";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (
                "lua/includes/ents.lua",
                "---@class Entity\nents = {}\n---@generic T: Entity\n---@param class `T`\n---@return T[]\nfunction ents.FindByClass(class) end\n",
            ),
            (LIGHTING, LIGHTING_TEXT),
            (
                "lua/glide/client/lighting_photon.lua",
                "local controllers = ents.FindByClass(\"photon_controller\")\nlocal first = controllers[1]\n",
            ),
        ]);

        let lighting = ws.def_file(LIGHTING, &format!("\n{LIGHTING_TEXT}"));

        assert_eq!(
            ws.analysis.compilation.last_evaluated_files(),
            vec![lighting]
        );
    }

    /// A field a class does not declare is looked up on the local tables bound
    /// to it, which are read one declaration at a time: a new declaration
    /// elsewhere in their file leaves that lookup standing.
    #[test]
    fn a_local_added_to_a_file_with_a_local_class_table_re_runs_nothing_in_other_files() {
        const BAG: &str = "gamemodes/cityrp/gamemode/core/sh_bag.lua";
        const BAG_TEXT: &str = "---@class Bag\nlocal Bag = {}\nBag.size = 1\n";
        let mut ws = VirtualWorkspace::new();
        let bag = ws.def_file(BAG, BAG_TEXT);
        ws.def_file(
            "gamemodes/cityrp/gamemode/core/sv_carry.lua",
            "---@param bag Bag\nfunction Carry(bag) local weight = bag.weight end\n",
        );

        ws.def_file(BAG, &format!("{BAG_TEXT}local unused = 1\n"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![bag]);
    }

    /// Registered net messages, console commands and convars are read by name,
    /// so renaming a net message leaves a convar lookup elsewhere standing.
    #[test]
    fn renaming_a_net_message_re_runs_nothing_in_a_file_that_looks_up_a_convar() {
        const NET: &str = "gamemodes/cityrp/gamemode/core/sv_net.lua";
        const NET_TEXT: &str = "util.AddNetworkString(\"cityrp_ping\")\n";
        let mut workspace = cityrp_workspace();
        workspace.ws.def_file(
            "lua/includes/convar.lua",
            "---@class ConVar\n---@[call_arg(\"gmod.convar\", \"reference\")]\n---@param name string\n---@return ConVar?\nfunction GetConVar(name) end\n",
        );
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/sh_convars.lua",
            "CreateConVar(\"cityrp_speed\", \"1\")\n",
        );
        workspace.ws.def_file(
            "gamemodes/cityrp/gamemode/core/sh_speed.lua",
            "local speed = GetConVar(\"cityrp_speed\")\n",
        );
        let net = workspace.ws.def_file(NET, NET_TEXT);

        workspace
            .ws
            .def_file(NET, &NET_TEXT.replace("cityrp_ping", "cityrp_pong"));

        assert_eq!(workspace.evaluated_files(), vec![net]);
    }

    /// A class named by string is declared by the calls that name it, once
    /// every registration has run. Re-indexing one such file rebuilds its
    /// declaration before any other file is re-checked, so the class other
    /// files read never looks half-declared to them.
    #[test]
    fn re_indexing_a_file_that_names_a_class_by_string_re_runs_only_that_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let files = ws.def_files(vec![
            (
                "lua/includes/ents.lua",
                "---@class Entity
ents = {}
---@generic T: Entity
---@param class `T`
---@return T[]
function ents.FindByClass(class) end
",
            ),
            (
                "lua/glide/sh_lighting_api.lua",
                "for _, controller in ipairs(ents.FindByClass(\"photon_controller\")) do end
",
            ),
            (
                "lua/glide/client/lighting_photon.lua",
                "local controllers = ents.FindByClass(\"photon_controller\")
local first = controllers[1]
",
            ),
        ]);
        let lighting = files[1];

        ws.analysis.reindex_files(vec![lighting]);

        assert_eq!(
            ws.analysis.compilation.last_evaluated_files(),
            vec![lighting]
        );
    }

    /// A method's receiver is checked against the file that defines it, one
    /// statement of it at a time: a shift there leaves the calling file
    /// standing.
    #[test]
    fn a_shift_in_a_file_whose_method_other_files_call_re_runs_nothing_in_them() {
        const SHARED: &str = "lua/entities/shift_car/shared.lua";
        const SHARED_TEXT: &str =
            "ENT.Type = \"anim\"\nfunction ENT:OnSimulatePhysics(phys, dt) end\n";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (SHARED, SHARED_TEXT),
            (
                "lua/entities/shift_car/init.lua",
                "function ENT:PhysicsSimulate()\n    self:OnSimulatePhysics(1, 0.5)\nend\n",
            ),
        ]);

        let shared = ws.def_file(SHARED, &format!("\n{SHARED_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![shared]);
    }

    const LIB: &str = "lua/autorun/sh_lib.lua";
    const LIB_TEXT: &str = "Lib = Lib or {}
Lib.first = function() end
Lib.second = function() end
Lib.third = function() end
";
    const LIB_CALLER: &str = "lua/autorun/client/cl_lib_caller.lua";
    const LIB_CALLER_TEXT: &str = "Lib.first()\nLib.second()\nLib.third()\n";

    fn gmod(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
    }

    /// A callee the edit destroys re-runs what computed the caller's facts
    /// that named it, not every derivation of the caller's file: the caller's
    /// source layer and other passes run exactly as often as without the
    /// caller, and its other calls as often as without them.
    #[test]
    fn destroying_a_callee_re_runs_only_what_named_it_in_the_caller() {
        let destroyed =
            LIB_TEXT.replace("Lib.second = function() end", "Lib.second = nil           ");
        assert_eq!(destroyed.len(), LIB_TEXT.len());
        let edits = [(LIB, destroyed.as_str())];
        let with_caller = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[(LIB, LIB_TEXT), (LIB_CALLER, LIB_CALLER_TEXT)],
            &edits,
        );
        let with_one_call = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[(LIB, LIB_TEXT), (LIB_CALLER, "Lib.second()\n")],
            &edits,
        );
        let alone = VirtualWorkspace::assert_warm_matches_cold(gmod, &[(LIB, LIB_TEXT)], &edits);

        assert_eq!(
            with_caller
                .analysis
                .compilation
                .last_stats()
                .rebuilt_sources,
            alone.analysis.compilation.last_stats().rebuilt_sources
        );
        let evaluations = |ws: &VirtualWorkspace, kind: DerivationKind| {
            ws.analysis.compilation.last_stats().evaluations(kind)
        };
        assert_eq!(
            evaluations(&with_caller, DerivationKind::Call),
            evaluations(&with_one_call, DerivationKind::Call)
        );
        for kind in [
            DerivationKind::GmodHelperScan,
            DerivationKind::GmodLoadCallSites,
            DerivationKind::GmodPre,
            DerivationKind::ChunkReturn,
            DerivationKind::GmodClassCallSites,
            DerivationKind::GmodPost,
            DerivationKind::AccessorFunc,
            DerivationKind::DynamicFieldWrite,
            DerivationKind::LocalFallbacks,
        ] {
            assert_eq!(
                evaluations(&with_caller, kind),
                evaluations(&alone, kind),
                "{kind:?}"
            );
        }
    }

    const RANKS: &str = "gamemodes/cityrp/gamemode/core/sh_configuration.lua";
    /// A table literal of CityRP's ranks with a method that reads its fields.
    const RANKS_TEXT: &str = r#"cityrp = cityrp or {}
cityrp.configuration = cityrp.configuration or {}
cityrp.configuration.ranks = {
    remap = { mod = "admin" },
    user = { name = "User" },
    admin = { name = "Admin" },
}

function cityrp.configuration.ranks:Get(rank)
    rank = self.remap[rank] or rank
    return self[rank] or { noset = true }
end
"#;
    const RANK_READER_TEXT: &str = r#"local PLAYER = FindMetaTable("Player")
function PLAYER:GetRankData()
    local r = "user"
    if self:IsAdmin() then
        r = "admin"
    end
    return cityrp.configuration.ranks:Get(r) or cityrp.configuration.ranks:Get("streaming")
end
"#;

    const REGISTRY: &str = "gamemodes/cityrp/gamemode/core/sh_registry.lua";
    /// A member written inside a function before the table is generated
    /// again at the top level: whether it survives turns on the two writes'
    /// function scopes.
    const REGISTRY_TEXT: &str = "function SetupRegistry()
    Registry.Value = 1
end

Registry = {}
";

    /// The registry file and a file reading its member.
    fn registry_workspace() -> (VirtualWorkspace, FileId) {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(REGISTRY, REGISTRY_TEXT);
        let reader = ws.def_file(
            "gamemodes/cityrp/gamemode/core/sh_registry_reader.lua",
            "local value = Registry.Value
",
        );
        (ws, reader)
    }

    /// A reader of a table member that survives the table being generated
    /// again reads the function scopes around the two writes, not the text of
    /// the file holding them.
    #[test]
    fn a_reader_of_a_member_written_before_its_table_is_generated_reads_no_file_text() {
        let (ws, reader) = registry_workspace();
        let graph = ws.analysis.compilation.get_db().reader_graph();

        let reads = graph
            .derivations_of_file(reader)
            .flat_map(|id| graph.reads_of(id))
            .map(|key| key.key().clone())
            .collect::<Vec<_>>();

        assert!(
            reads
                .iter()
                .any(|key| matches!(key, crate::db_index::derivation::FactKey::FunctionScope(_)))
        );
        assert!(
            !reads
                .iter()
                .any(|key| matches!(key, crate::db_index::derivation::FactKey::FileText(_)))
        );
    }

    /// A comment appended to the registry file leaves the function scopes
    /// the reader read as they were.
    #[test]
    fn a_comment_appended_to_a_file_whose_member_survives_a_table_generation_re_runs_nothing_in_its_reader()
     {
        let (mut ws, _) = registry_workspace();

        let registry = ws.def_file(
            REGISTRY,
            &format!(
                "{REGISTRY_TEXT}
-- touch
"
            ),
        );

        assert_eq!(
            ws.analysis.compilation.last_evaluated_files(),
            vec![registry]
        );
    }

    /// The ranks file, a plugin that also bootstraps the configuration table,
    /// and a file calling a ranks method, which is returned.
    fn ranks_workspace() -> (VirtualWorkspace, FileId) {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(RANKS, RANKS_TEXT);
        ws.def_file(
            "gamemodes/cityrp/plugins/farming/sh_configuration.lua",
            "cityrp.configuration = cityrp.configuration or {}\ncityrp.configuration.farmTime = 60\n",
        );
        let reader = ws.def_file(
            "gamemodes/cityrp/gamemode/core/metatables/sh_player.lua",
            RANK_READER_TEXT,
        );
        (ws, reader)
    }

    /// Whether a same-file write of a table is visible inside a method of that
    /// file turns on the statement that wrote it, so a caller inferring the
    /// method's receiver reads that statement, not the whole file.
    #[test]
    fn a_caller_inferring_another_files_table_method_reads_no_file_text() {
        let (ws, reader) = ranks_workspace();
        let db = ws.analysis.compilation.get_db();
        let graph = db.reader_graph();

        let text_reads = graph
            .derivations_of_file(reader)
            .flat_map(|id| graph.reads_of(id))
            .filter(|key| matches!(key.key(), crate::db_index::derivation::FactKey::FileText(_)))
            .count();

        assert_eq!(text_reads, 0);
    }

    /// A comment appended to the ranks file leaves every statement the caller
    /// read as it was.
    #[test]
    fn a_comment_appended_to_a_file_whose_table_method_another_file_calls_re_runs_nothing_in_it() {
        let (mut ws, _) = ranks_workspace();

        let ranks = ws.def_file(RANKS, &format!("{RANKS_TEXT}\n-- touch\n"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![ranks]);
    }

    /// The caller infers the method through the flow of the file defining it;
    /// that flow compares equal with the shift mapped back.
    #[test]
    fn a_shift_in_a_file_whose_table_method_another_file_calls_re_runs_nothing_in_it() {
        let (mut ws, _) = ranks_workspace();

        let ranks = ws.def_file(RANKS, &format!("\n{RANKS_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![ranks]);
    }

    /// A type naming the entity a call creates names the call's expression,
    /// which a shift above it carries to its new place, so a file holding that
    /// type stands.
    #[test]
    fn a_shift_above_a_call_whose_instance_another_file_holds_re_runs_nothing_in_it() {
        const INIT: &str = "lua/entities/shift_car/init.lua";
        const INIT_TEXT: &str = r#"include("sv_seats.lua")

function ENT:CreateSeat()
    self.seat = ents.Create("prop_vehicle_prisoner_pod")
end
"#;
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/includes/extensions/ents.lua",
                "---@class Entity\nlocal Entity = {}\n---@class prop_vehicle_prisoner_pod : Entity\nents = {}\n---@generic T : Entity\n---@param class `T`\n---@return (instance) T\nfunction ents.Create(class) end\n",
            ),
            ("lua/entities/shift_car/shared.lua", "ENT.Type = \"anim\"\n"),
            (INIT, INIT_TEXT),
            (
                "lua/entities/shift_car/sv_seats.lua",
                "function ENT:KeepSeat()\n    self.backupSeat = self.seat\nend\n",
            ),
        ]);

        let init = ws.def_file(INIT, &format!("\n{INIT_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![init]);
    }

    /// A member another file files under a table literal keeps its place, and
    /// its share of the literal's file, when that file is re-indexed, so the
    /// writing file stands.
    #[test]
    fn a_comment_appended_to_a_file_whose_table_another_file_fills_re_runs_nothing_in_it() {
        const INIT: &str = "lua/entities/shift_car/init.lua";
        const INIT_TEXT: &str = "include(\"sv_input.lua\")\n\nfunction ENT:Initialize()\n    self.holdInputActions = {}\nend\n";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            ("lua/entities/shift_car/shared.lua", "ENT.Type = \"anim\"\n"),
            (INIT, INIT_TEXT),
            (
                "lua/entities/shift_car/sv_input.lua",
                "function ENT:RegisterHoldAction(action, data)\n    self.holdInputActions[action] = data\nend\n",
            ),
        ]);

        let init = ws.def_file(INIT, &format!("{INIT_TEXT}\n-- touch\n"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![init]);
    }

    /// The edit remap carries another file's owner share of a shifted file's
    /// member fingerprint to the literal's new place, so the writing file
    /// stands.
    #[test]
    fn a_shift_in_a_file_whose_table_another_file_fills_re_runs_nothing_in_it() {
        const INIT: &str = "lua/entities/shift_car/init.lua";
        const INIT_TEXT: &str = "include(\"sv_input.lua\")\n\nfunction ENT:Initialize()\n    self.holdInputActions = {}\nend\n";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            ("lua/entities/shift_car/shared.lua", "ENT.Type = \"anim\"\n"),
            (INIT, INIT_TEXT),
            (
                "lua/entities/shift_car/sv_input.lua",
                "function ENT:RegisterHoldAction(action, data)\n    self.holdInputActions[action] = data\nend\n",
            ),
        ]);

        let init = ws.def_file(INIT, &format!("\n{INIT_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![init]);
    }

    /// A member filed under a key naming another file's table literals is
    /// re-keyed in place when that file shifts, so its writer stands.
    #[test]
    fn a_shift_in_a_file_whose_tables_another_file_keys_members_by_re_runs_nothing_in_it() {
        const CONFIG: &str = "lua/cfg.lua";
        const CONFIG_TEXT: &str = "cfg = {}\ncfg.models = { { model = \"a\", skin = 1, groups = { torso = 1 } }, { model = \"b\", skin = 2, groups = { torso = 2 } } }\n";
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_files(vec![
            (CONFIG, CONFIG_TEXT),
            (
                "lua/user.lua",
                "optimised = {}\nfor _, data in pairs(cfg.models) do\n    optimised[data.skin > 0 and data or data.model] = data\nend\n",
            ),
        ]);

        let config = ws.def_file(CONFIG, &format!("\n{CONFIG_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![config]);
    }

    /// A file's accessor members come from its own accessor calls, so a shift
    /// in another file calling an accessor function re-runs nothing here.
    #[test]
    fn a_shift_in_a_file_that_calls_an_accessor_function_re_runs_nothing_in_other_callers() {
        const OTHER: &str = "lua/items/other_item.lua";
        const OTHER_TEXT: &str = "---@class other_item\nlocal OTHER = {}\n\n---@accessorfunc\nfunction OTHER:AutoFunction(name, key)\nend\n\nOTHER:AutoFunction(\"Colour\", \"colour\")\n";
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                "lua/items/base_item.lua",
                "---@class base_item\nlocal ITEM = {}\n\n---@accessorfunc\nfunction ITEM:AutoFunction(name, key)\nend\n\nITEM:AutoFunction(\"SpecialValue\", \"internal_flag\")\n",
            ),
            (OTHER, OTHER_TEXT),
        ]);

        let other = ws.def_file(OTHER, &format!("\n{OTHER_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![other]);
    }

    /// Another file's field written onto a table literal is kept by the member
    /// index when the literal's file is re-indexed, and the edit carries it to
    /// the literal's new position, so the writing file stands.
    #[test]
    fn a_shift_in_a_file_whose_table_another_file_writes_re_runs_nothing_in_it() {
        const SHARED: &str = "lua/entities/shift_car/shared.lua";
        const SHARED_TEXT: &str = "ENT.Type = \"anim\"\nENT.WaterBuoyancy = { depth = 1 }\n";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (SHARED, SHARED_TEXT),
            (
                "lua/weapons/gmod_tool/stools/amphibious.lua",
                "---@param veh shift_car\nfunction MakeAmphibious(veh)\n    veh.WaterBuoyancy.speed = 2\nend\n",
            ),
        ]);

        let shared = ws.def_file(SHARED, &format!("\n{SHARED_TEXT}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![shared]);
    }

    const FOO: &str = "lua/autorun/sh_foo.lua";
    const MORE_FOO: &str = "lua/autorun/sh_more_foo.lua";

    /// Declaring a class again in another file moves where it is declared,
    /// not what it is: a file that only names the class is not re-analysed.
    #[test]
    fn a_partial_class_in_another_file_re_analyses_no_file_that_only_names_the_class() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                FOO,
                "---@class Foo
Foo = {}
",
            ),
            (
                "lua/autorun/sh_reader.lua",
                "---@param foo Foo
function UseFoo(foo) end
",
            ),
        ]);

        let more = ws.def_file(
            MORE_FOO,
            "---@class Foo
local FOO = {}
",
        );

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![more]);
    }

    /// Where a class is declared is a fact of its own: a reader of the
    /// declaring files re-runs when they move, though the declaration does
    /// not.
    #[test]
    fn reading_where_a_class_is_declared_records_the_locations() {
        use crate::db_index::derivation::{self, DerivationId, FactKey};

        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            FOO,
            "---@class Foo
Foo = {}
",
        );
        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("the file is parsed")
            .get_red_root();
        let foo = crate::LuaTypeDeclId::global("Foo");

        let _recording = derivation::begin(DerivationId::new(
            DerivationKind::Diagnostics,
            file_id,
            glua_parser::LuaSyntaxId::from_node(&root),
        ));
        let declared_in = db
            .get_type_index()
            .get_type_decl(&foo)
            .map(|decl| decl.get_locations().len());
        let (_, reads, _) = derivation::end().into_parts(|key| db.fact_fingerprint(key));

        assert_eq!(declared_in, Some(1));
        assert!(
            reads
                .iter()
                .any(|(key, _)| *key.key() == FactKey::TypeDeclLocations(foo.clone()))
        );
    }

    /// A file's hook registrations are not its realm evidence: renaming the
    /// hook a file adds to re-runs no load-graph fold.
    #[test]
    fn renaming_a_hook_re_runs_no_load_fold() {
        const HOOKS: &str = "lua/autorun/server/sv_hooks.lua";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            HOOKS,
            "hook.Add(\"Think\", \"probe\", function() end)
",
        );

        ws.def_file(
            HOOKS,
            "hook.Add(\"Tick\", \"probe\", function() end)
",
        );

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::GmodLoad), 0);
    }

    /// The load fold resolves an included file by path, not by what it
    /// exports, so the export landing later does not run it again.
    #[test]
    fn a_module_export_landing_does_not_re_run_the_load_fold() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def_files(vec![
            (
                "lua/autorun/sh_loader.lua",
                "include(\"sh_lib.lua\")
",
            ),
            (
                "lua/autorun/sh_lib.lua",
                "local M = {}
function M.Value() return 1 end
return M
",
            ),
        ]);

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::GmodLoad), 1);
    }

    /// A condition calling a function on one variable cannot narrow another,
    /// so a reader of the other does not run again when the function changes.
    #[test]
    fn a_guard_call_on_one_variable_leaves_readers_of_another_alone() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/shared/check.lua",
            "function Check(value) return value ~= nil end",
        );
        ws.def_file(
            "lua/autorun/shared/use.lua",
            r#"
                local a, b = 1, 2
                if Check(a) then
                    local c = b
                end
            "#,
        );

        ws.def_file(
            "lua/autorun/shared/check.lua",
            "function Check(value) return value ~= nil, 1 end",
        );

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::LocalStat), 0);
    }

    /// A call passed to a guard names no local, so a reader of a local does
    /// not run again when that call's callee changes.
    #[test]
    fn a_guard_on_a_call_result_leaves_readers_of_a_local_alone() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/shared/pick.lua",
            "function Pick() return 1 end",
        );
        ws.def_file(
            "lua/autorun/shared/use.lua",
            r#"
                local b = 2
                if Check(Pick()) then
                    local c = b
                end
            "#,
        );

        ws.def_file(
            "lua/autorun/shared/pick.lua",
            "function Pick() return 1, 2 end",
        );

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::LocalStat), 0);
    }

    /// A gamemode file's whole-file class declaration is the support of the
    /// facts inferred from it, so a shift of a file that starts with a blank
    /// CRLF line, which a diff can align as an edit of that line, leaves the
    /// readers of those facts in other files alone.
    #[test]
    fn a_shift_of_a_gamemode_file_with_a_leading_crlf_line_re_runs_nothing_in_other_files() {
        const HUD: &str = "gamemodes/helix/gamemode/cl_hud.lua";
        const HUD_TEXT: &str = "\r\nfunction GM:HUDPaint()\r\n\tlocal x = 1\r\nend\r\n";
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let files = ws.def_files(vec![
            (HUD, HUD_TEXT),
            (
                "lua/annotations/gm.lua",
                "---@class GM
---@field BaseClass GM
GM = {}
",
            ),
            (
                "gamemodes/helix/gamemode/sh_other.lua",
                "function GM:Other()\n\tlocal y = 2\nend\n",
            ),
            (
                "gamemodes/helix/gamemode/sh_hooks.lua",
                "function GM:CanTool(client, trace, tool)\n\treturn self.BaseClass:CanTool(client, trace, tool)\nend\n",
            ),
        ]);

        ws.def_file(HUD, &format!("\n{HUD_TEXT}"));

        assert_eq!(
            ws.analysis.compilation.last_evaluated_files(),
            vec![files[0]]
        );
    }

    const WEAPON_BASE: &str = "entities/weapons/cw_base";

    fn weapon_base_workspace(
        base_text: &str,
        reader_text: &str,
    ) -> (VirtualWorkspace, FileId, FileId) {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/annotations/swep.lua",
            "---@class Entity
---@class WEAPON : Entity
---@class Player : Entity
---@class SWEP : WEAPON
SWEP = {}
---@return Player
function LocalPlayer() end
---@return WEAPON
function Player:GetActiveWeapon() end
---@return boolean
function IsValid(obj) end
",
        );
        let base = ws.def_file(&format!("{WEAPON_BASE}/sh_attachments.lua"), base_text);
        let reader = ws.def_file(&format!("{WEAPON_BASE}/cl_hooks.lua"), reader_text);
        (ws, base, reader)
    }

    fn files_reading_the_text_of(ws: &VirtualWorkspace, file_id: FileId) -> Vec<FileId> {
        let graph = ws.analysis.compilation.get_db().reader_graph();
        let mut files = graph
            .derivations()
            .filter(|id| id.file_id != file_id)
            .filter(|id| {
                graph
                    .reads_of(*id)
                    .any(|key| matches!(key.key(), FactKey::FileText(read) if *read == file_id))
            })
            .map(|id| id.file_id)
            .collect::<Vec<_>>();
        files.sort();
        files.dedup();
        files
    }

    /// A call through a method assigned from a local function inspects the
    /// statement that declares the function, not the whole file.
    #[test]
    fn calling_a_method_assigned_from_a_local_function_reads_no_file_text_of_its_file() {
        let base_text = "local proneFunction = function(self)
	return self.Owner:IsProne()
end

function SWEP:Initialize()
	self.isPlayerProne = proneFunction
end
";
        let reader_text = "function SWEP:Think()
	if self:isPlayerProne() then
		return 1
	end
end
";
        let (ws, base, _) = weapon_base_workspace(base_text, reader_text);

        assert_eq!(files_reading_the_text_of(&ws, base), Vec::<FileId>::new());
    }

    /// Reading a dynamic field written from a local's member inspects only
    /// the statement that declares the local.
    #[test]
    fn reading_a_dynamic_field_written_from_a_local_reads_no_file_text_of_its_file() {
        let base_text = "function SWEP:_attach(inherit)
	local att = inherit
	self.renderTargetFunc = att.drawRenderTarget
end
";
        let reader_text = "local function Render()
	local wep = LocalPlayer():GetActiveWeapon()
	if IsValid(wep) and wep.CW20Weapon then
		if wep.renderTargetFunc then
			wep.renderTargetFunc(wep)
		end
	end
end
";
        let (ws, base, _) = weapon_base_workspace(base_text, reader_text);

        assert_eq!(files_reading_the_text_of(&ws, base), Vec::<FileId>::new());
    }

    /// Calling a function through a global another file aliases from a local
    /// inspects the statement that declares the local, not the whole file.
    #[test]
    fn calling_a_function_another_file_aliases_from_a_local_reads_no_file_text_of_its_file() {
        let base_text = "local function helper(a) return a end
local alias = helper
GlobalAlias = alias
";
        let reader_text = "function SWEP:Think()
	return GlobalAlias(1)
end
";
        let (ws, base, _) = weapon_base_workspace(base_text, reader_text);

        assert_eq!(files_reading_the_text_of(&ws, base), Vec::<FileId>::new());
    }

    /// Calling a special-call function through a global another file aliases
    /// from a local reads only the statement that declares the local.
    #[test]
    fn calling_a_special_call_another_file_aliases_from_a_local_reads_no_file_text_of_its_file() {
        let base_text = "local find = ents.FindByClass
FindEntities = find
";
        let reader_text = "function SWEP:Think()
	FindEntities(\"prop_physics\")
end
";
        let (ws, base, _) = weapon_base_workspace(base_text, reader_text);

        assert_eq!(files_reading_the_text_of(&ws, base), Vec::<FileId>::new());
    }

    /// Reading an enum another file exposes through a global inspects the
    /// statement that declares the enum local, not the whole file.
    #[test]
    fn reading_an_enum_another_file_exposes_from_a_local_reads_no_file_text_of_its_file() {
        let base_text = "---@enum Mode
local Mode = { Fast = 1 }
GlobalMode = Mode
";
        let reader_text = "function SWEP:Think()
	return GlobalMode.Fast
end
";
        let (ws, base, _) = weapon_base_workspace(base_text, reader_text);

        assert_eq!(files_reading_the_text_of(&ws, base), Vec::<FileId>::new());
    }
}
