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
}
