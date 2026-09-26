#[cfg(test)]
mod test {
    use crate::db_index::derivation::DerivationKind;
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

    const PER_FILE_GMOD_PASSES: [DerivationKind; 4] = [
        DerivationKind::GmodPre,
        DerivationKind::GmodLoadCallSites,
        DerivationKind::GmodClassCallSites,
        DerivationKind::NetHelpers,
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

        let key = crate::db_index::derivation::FactKey::NetHelpers(caller);
        assert_eq!(
            edited.fingerprint(key.clone()),
            analysed_after.fingerprint(key)
        );
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
}
