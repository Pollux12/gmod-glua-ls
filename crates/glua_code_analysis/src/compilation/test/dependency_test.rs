#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use crate::{Emmyrc, LuaDependencyKind, VirtualWorkspace};

    #[gtest]
    fn test_dependency_edge_kinds_for_require_include_and_addcsluafile() {
        let mut ws = VirtualWorkspace::new();

        ws.def_gmod_call_arg_builtins();
        let shared_id = ws.def_file("lua/entities/test/shared.lua", "return {}");
        let client_id = ws.def_file("lua/entities/test/cl_init.lua", "return {}");
        let required_id = ws.def_file("lua/dep.lua", "return {}");
        let init_id = ws.def_file(
            "lua/entities/test/init.lua",
            r#"
            include("shared.lua")
            AddCSLuaFile("cl_init.lua")
            local dep = require("dep")
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        let kinds = dependency_index
            .get_dependency_kinds(&init_id, &shared_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::Include)]
        );

        let kinds = dependency_index
            .get_dependency_kinds(&init_id, &client_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::AddCSLuaFile)]
        );

        let kinds = dependency_index
            .get_dependency_kinds(&init_id, &required_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::Require)]
        );
    }

    /// `include` takes a path relative to the including file before one
    /// relative to `lua/`, so every entity's `include("shared.lua")` reaches
    /// its own `shared.lua`.
    #[gtest]
    fn a_relative_include_resolves_next_to_the_including_file() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        let alpha_shared = ws.def_file("lua/entities/alpha/shared.lua", "return {}");
        let beta_shared = ws.def_file("lua/entities/beta/shared.lua", "return {}");
        ws.def_file("lua/entities/alpha/init.lua", "include(\"shared.lua\")\n");
        let beta_init = ws.def_file("lua/entities/beta/init.lua", "include(\"shared.lua\")\n");

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        expect_that!(
            dependency_index.get_dependency_kinds(&beta_init, &beta_shared),
            some(unordered_elements_are![eq(&LuaDependencyKind::Include)])
        );
        expect_that!(
            dependency_index.get_dependency_kinds(&beta_init, &alpha_shared),
            none()
        );
    }

    #[gtest]
    fn test_annotated_load_call_adds_dependency_edge() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let target_id = ws.def_file("lua/menu/mount/vgui/workshop.lua", "return {}");
        let source_id = ws.def_file(
            "lua/menu/mount/mount.lua",
            r#"
            vgui = vgui or {}

            ---@[call_arg("gmod.load", "include")]
            ---@param file string
            function vgui.RegisterFile(file) end

            vgui.RegisterFile("vgui/workshop.lua")
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        let kinds = dependency_index
            .get_dependency_kinds(&source_id, &target_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(
            kinds,
            unordered_elements_are![eq(&LuaDependencyKind::Include)]
        );
    }

    /// A load call the builtins also annotate as one is one dependency site,
    /// ranged over its path argument.
    #[test]
    fn an_annotated_include_is_one_site_over_its_path() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/autorun/helper.lua",
            "HelperValue = 1
",
        );
        let boot_id = ws.def_file(
            "lua/autorun/sv_boot.lua",
            "include(\"helper.lua\")
",
        );

        let ranges = ws
            .get_db_mut()
            .get_file_dependencies_index()
            .get_dependency_sites(&boot_id)
            .unwrap_or_default()
            .iter()
            .map(|site| (u32::from(site.range.start()), u32::from(site.range.end())))
            .collect::<Vec<_>>();
        assert_eq!(ranges, vec![(8, 20)]);
    }

    /// A method that shares a loader's name is not a loader.
    #[test]
    fn a_method_named_like_a_loader_loads_nothing() {
        let mut ws = gmod_workspace();
        ws.def_file("lua/mylib/target.lua", "return {}");
        let source_id = ws.def_file(
            "lua/autorun/sh_source.lua",
            r#"
            local builder = {}
            function builder:include(path) end
            function builder.AddCSLuaFile(path) end
            function builder.IncludeCS(path) end
            builder:include("mylib/target.lua")
            builder.AddCSLuaFile("mylib/target.lua")
            builder.IncludeCS("mylib/target.lua")
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert!(
            dependency_index
                .get_dependency_sites(&source_id)
                .is_none_or(<[_]>::is_empty)
        );
    }

    #[test]
    fn test_dynamic_include_path_does_not_add_dependency_edge() {
        let mut ws = VirtualWorkspace::new();

        ws.def_gmod_call_arg_builtins();
        let _shared_id = ws.def_file("lua/entities/test/shared.lua", "return {}");
        let init_id = ws.def_file(
            "lua/entities/test/init.lua",
            r#"
            local file_name = "shared.lua"
            include(file_name)
            "#,
        );

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert!(dependency_index.get_required_files(&init_id).is_none());
    }

    #[gtest]
    fn test_includecs_without_argument_does_not_add_dependency_edge() {
        let mut ws = VirtualWorkspace::new();

        ws.def_gmod_call_arg_builtins();
        let source_id = ws.def_file("lua/autorun/server/sv_boot.lua", "IncludeCS()");
        let dependency_index = ws.get_db_mut().get_file_dependencies_index();

        assert!(dependency_index.get_required_files(&source_id).is_none());
    }

    /// A file indexed in the same batch after its includer is still the
    /// include's target: a cold build must not depend on batch order.
    #[test]
    fn test_include_resolves_to_a_file_later_in_the_same_batch() {
        let mut ws = VirtualWorkspace::new();

        ws.def_gmod_call_arg_builtins();
        let ids = ws.def_files(vec![
            (
                "lua/autorun/includer.lua",
                r#"include("mixins/included.lua")"#,
            ),
            ("lua/mixins/included.lua", "return {}"),
        ]);

        let dependency_index = ws.get_db_mut().get_file_dependencies_index();
        assert!(
            dependency_index
                .get_required_files(&ids[0])
                .is_some_and(|files| files.contains(&ids[1]))
        );
    }

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws
    }

    const IDS_FILE: &str = "lua/mylib/ids.lua";
    const IDS: &str = "local IDS = { first = 1, second = \"two\" }\nreturn IDS\n";
    const CONSUMER_FILE: &str = "lua/autorun/sh_consumer.lua";
    const CONSUMER: &str = "local ids = include(\"mylib/ids.lua\")\nlocal first = ids.first\n";

    fn consumer_first(ws: &VirtualWorkspace) -> String {
        let consumer = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(CONSUMER_FILE))
            .expect("consumer file");
        ws.humanize_type(ws.local_ty(consumer, "first"))
    }

    /// `include` returns what the file it loads returns.
    #[test]
    fn an_include_returns_the_table_the_included_file_returns() {
        let mut ws = gmod_workspace();
        ws.def_file(IDS_FILE, IDS);
        ws.def_file(CONSUMER_FILE, CONSUMER);

        assert_eq!(consumer_first(&ws), "1");
    }

    /// Calling what `CompileFile` returns runs the file it compiled.
    #[test]
    fn a_compiled_file_returns_the_table_the_file_returns() {
        let mut ws = gmod_workspace();
        ws.def_file(IDS_FILE, IDS);
        ws.def_file(
            CONSUMER_FILE,
            "local ids = CompileFile(\"mylib/ids.lua\")()\nlocal first = ids.first\n",
        );

        assert_eq!(consumer_first(&ws), "1");
    }

    /// An included file that returns nothing gives its includer nothing.
    #[test]
    fn an_include_of_a_file_returning_nothing_is_nil() {
        let mut ws = gmod_workspace();
        ws.def_file(IDS_FILE, "IdsLoaded = true\n");
        let consumer = ws.def_file(CONSUMER_FILE, "local ids = include(\"mylib/ids.lua\")\n");

        let ids = ws.local_ty(consumer, "ids");
        assert_eq!(ws.humanize_type(ids), "nil");
    }

    /// A load function that documents its own return keeps it; only an
    /// `include`'s unspecified results are the file's.
    #[test]
    fn a_load_function_documenting_its_return_keeps_it() {
        let mut ws = gmod_workspace();
        ws.def_file(IDS_FILE, IDS);
        ws.def_file(
            "lua/autorun/sh_loader.lua",
            "---@[call_arg(\"gmod.load\", \"include\")]\n---@param file string\n---@return boolean\nfunction LoadChecked(file) end\n",
        );
        let consumer = ws.def_file(
            CONSUMER_FILE,
            "local loaded = LoadChecked(\"mylib/ids.lua\")\n",
        );

        let loaded = ws.local_ty(consumer, "loaded");
        assert_eq!(ws.humanize_type(loaded), "boolean");
    }

    /// Giving an included file a `return`, and taking it away again, moves
    /// what its includer reads as a cold build has it.
    #[test]
    fn an_included_file_gaining_and_losing_a_return_settles_as_a_cold_build() {
        const RETURNS_NOTHING: &str = "IdsLoaded = true\n";
        let cold = |ids: &str| {
            let mut ws = gmod_workspace();
            ws.def_file(IDS_FILE, ids);
            ws.def_file(CONSUMER_FILE, CONSUMER);
            consumer_first(&ws)
        };
        let mut warm = gmod_workspace();
        warm.def_file(IDS_FILE, RETURNS_NOTHING);
        warm.def_file(CONSUMER_FILE, CONSUMER);

        warm.def_file(IDS_FILE, IDS);
        let gained = consumer_first(&warm);
        warm.def_file(IDS_FILE, RETURNS_NOTHING);
        let lost = consumer_first(&warm);

        assert_eq!((gained, lost), (cold(IDS), cold(RETURNS_NOTHING)));
    }

    /// Editing, deleting and reopening the included file leaves what its
    /// includer reads as a cold build has it.
    #[test]
    fn an_included_file_edited_deleted_and_reopened_settles_as_a_cold_build() {
        const IDS_EDITED: &str = "local IDS = { first = \"one\" }\nreturn IDS\n";
        let cold = |ids: Option<&str>| {
            let mut ws = gmod_workspace();
            if let Some(ids) = ids {
                ws.def_file(IDS_FILE, ids);
            }
            ws.def_file(CONSUMER_FILE, CONSUMER);
            consumer_first(&ws)
        };
        let mut warm = gmod_workspace();
        warm.def_file(IDS_FILE, IDS);
        warm.def_file(CONSUMER_FILE, CONSUMER);
        let ids_uri = warm.virtual_url_generator.new_uri(IDS_FILE);

        warm.def_file(IDS_FILE, IDS_EDITED);
        let edited = consumer_first(&warm);
        warm.analysis
            .remove_file_by_uri(&ids_uri)
            .0
            .expect("included file must be removed");
        let deleted = consumer_first(&warm);
        warm.def_file(IDS_FILE, IDS);
        let reopened = consumer_first(&warm);

        assert_eq!(
            (edited.as_str(), deleted, reopened),
            ("\"one\"", cold(None), cold(Some(IDS)))
        );
        assert_eq!(cold(Some(IDS_EDITED)), "\"one\"");
    }

    const MODULE_FILE: &str = "lua/includes/modules/mylib_ids.lua";
    const REQUIRER_FILE: &str = "lua/autorun/sh_requirer.lua";

    fn requirer_first(ws: &mut VirtualWorkspace, requirer: &str) -> String {
        let file_id = ws.def_file(REQUIRER_FILE, requirer);
        ws.humanize_type(ws.local_ty(file_id, "first"))
    }

    /// A local function named `require` is not the loader: it loads nothing
    /// and its call has no module's value.
    #[test]
    fn a_local_named_require_loads_nothing() {
        let mut ws = gmod_workspace();
        ws.def_file(MODULE_FILE, IDS);
        let requirer = ws.def_file(
            REQUIRER_FILE,
            "local function require(name) end
local ids = require(\"mylib_ids\")
",
        );

        let sites = ws
            .get_db_mut()
            .get_file_dependencies_index()
            .get_dependency_sites(&requirer)
            .map(<[_]>::len);
        assert_eq!(sites, None);
    }

    /// A function the annotations make a `require` loader returns the module
    /// it names, as `require` does.
    #[test]
    fn an_annotated_require_wrapper_returns_the_module() {
        let mut ws = gmod_workspace();
        ws.def_file(MODULE_FILE, IDS);
        ws.def_file(
            LOADER_FILE,
            "---@[call_arg(\"gmod.load\", \"require\")]
---@param name string
function RequireModule(name) end
",
        );

        let first = requirer_first(
            &mut ws,
            "local ids = RequireModule(\"mylib_ids\")
local first = ids.first
",
        );
        assert_eq!(first, "1");
    }

    /// Tagging a wrapper as a `require` loader, and taking the tag away, moves
    /// what its callers read as a cold build has it.
    #[test]
    fn a_require_tag_added_and_removed_settles_as_a_cold_build() {
        const TAGGED: &str = "---@[call_arg(\"gmod.load\", \"require\")]
---@param name string
function RequireModule(name) end
";
        const UNTAGGED: &str = "---@param name string
function RequireModule(name) end
";
        const REQUIRER: &str = "local ids = RequireModule(\"mylib_ids\")
local first = ids.first
";
        let cold = |loader: &str| {
            let mut ws = gmod_workspace();
            ws.def_file(MODULE_FILE, IDS);
            ws.def_file(LOADER_FILE, loader);
            requirer_first(&mut ws, REQUIRER)
        };
        let mut warm = gmod_workspace();
        warm.def_file(MODULE_FILE, IDS);
        warm.def_file(LOADER_FILE, UNTAGGED);
        let untagged = requirer_first(&mut warm, REQUIRER);
        let requirer_uri = warm.virtual_url_generator.new_uri(REQUIRER_FILE);
        let read = |ws: &VirtualWorkspace| {
            let file_id = ws.analysis.get_file_id(&requirer_uri).expect("requirer");
            ws.humanize_type(ws.local_ty(file_id, "first"))
        };

        warm.def_file(LOADER_FILE, TAGGED);
        let tagged = read(&warm);
        warm.def_file(LOADER_FILE, UNTAGGED);
        let untagged_again = read(&warm);

        assert_eq!(
            (untagged.clone(), tagged.as_str(), untagged_again),
            (cold(UNTAGGED), "1", untagged)
        );
        assert_eq!(cold(TAGGED), "1");
    }

    /// Editing, deleting and reopening a required module leaves what its
    /// requirer reads as a cold build has it.
    #[test]
    fn a_required_module_edited_deleted_and_reopened_settles_as_a_cold_build() {
        const REQUIRER: &str = "local ids = require(\"mylib_ids\")
local first = ids.first
";
        const IDS_EDITED: &str = "local IDS = { first = \"one\" }
return IDS
";
        let cold = |ids: Option<&str>| {
            let mut ws = gmod_workspace();
            if let Some(ids) = ids {
                ws.def_file(MODULE_FILE, ids);
            }
            requirer_first(&mut ws, REQUIRER)
        };
        let mut warm = gmod_workspace();
        warm.def_file(MODULE_FILE, IDS);
        let first = requirer_first(&mut warm, REQUIRER);
        let module_uri = warm.virtual_url_generator.new_uri(MODULE_FILE);
        let requirer_uri = warm.virtual_url_generator.new_uri(REQUIRER_FILE);
        let read = |ws: &VirtualWorkspace| {
            let file_id = ws.analysis.get_file_id(&requirer_uri).expect("requirer");
            ws.humanize_type(ws.local_ty(file_id, "first"))
        };

        warm.def_file(MODULE_FILE, IDS_EDITED);
        let edited = read(&warm);
        warm.analysis
            .remove_file_by_uri(&module_uri)
            .0
            .expect("required module must be removed");
        let deleted = read(&warm);
        warm.def_file(MODULE_FILE, IDS);
        let reopened = read(&warm);

        assert_eq!(
            (first.as_str(), edited.as_str(), deleted, reopened),
            ("1", "\"one\"", cold(None), cold(Some(IDS)))
        );
    }

    const LOADER_FILE: &str = "lua/autorun/sh_loader.lua";
    const LOADER: &str = "function MyLoad(path)\n    return include(path)\nend\n";
    const THING_FILE: &str = "lua/mylib/thing.lua";
    const BOOT_FILE: &str = "lua/autorun/sh_boot.lua";

    fn loads_thing(ws: &mut VirtualWorkspace) -> bool {
        let uri = |path| ws.virtual_url_generator.new_uri(path);
        let (boot, thing) = (uri(BOOT_FILE), uri(THING_FILE));
        let file_id = |uri| ws.analysis.get_file_id(&uri).expect("file");
        let (boot, thing) = (file_id(boot), file_id(thing));
        ws.get_db_mut()
            .get_file_dependencies_index()
            .get_dependency_kinds(&boot, &thing)
            .is_some_and(|kinds| kinds.contains(&LuaDependencyKind::Include))
    }

    /// A function that passes its parameter on as the path of an `include`
    /// loads the file each call names, as `include` would.
    #[test]
    fn a_call_of_an_unannotated_include_wrapper_loads_the_file_it_names() {
        let mut ws = gmod_workspace();
        ws.def_file(THING_FILE, "return {}");
        ws.def_file(LOADER_FILE, LOADER);
        ws.def_file(BOOT_FILE, "MyLoad(\"mylib/thing.lua\")\n");

        assert!(loads_thing(&mut ws));
    }

    /// A loader in the shape of CityRP's: it picks `include` or `AddCSLuaFile`
    /// by its realm argument and the file's prefix.
    const REALM_LOADER: &str = r#"function MyLoad(path, realm)
    if (realm == "server" or path:find("sv_")) and SERVER then
        return include(path)
    elseif realm == "client" or path:find("cl_") then
        if SERVER then
            AddCSLuaFile(path)
        else
            return include(path)
        end
    end
end
"#;

    /// A loader that picks between `include` and `AddCSLuaFile`, or loads
    /// only under a condition, names the file its call gives without loading
    /// it in the caller's realm: which realm runs the file depends on its
    /// branches.
    #[test]
    fn a_call_of_a_dispatching_or_conditional_loader_only_names_the_file() {
        for loader in [
            "function MyLoad(path)\n    if SERVER then AddCSLuaFile(path) end\n    include(path)\nend\n",
            "function MyLoad(path)\n    if SERVER then include(path) end\nend\n",
            REALM_LOADER,
        ] {
            let mut ws = gmod_workspace();
            ws.def_file(THING_FILE, "return {}");
            ws.def_file(LOADER_FILE, loader);
            ws.def_file(BOOT_FILE, "MyLoad(\"mylib/thing.lua\")\n");

            let file_id = |ws: &VirtualWorkspace, path| {
                ws.analysis
                    .get_file_id(&ws.virtual_url_generator.new_uri(path))
                    .expect("file")
            };
            let (boot, thing) = (file_id(&ws, BOOT_FILE), file_id(&ws, THING_FILE));
            let kinds = ws
                .get_db_mut()
                .get_file_dependencies_index()
                .get_dependency_kinds(&boot, &thing)
                .cloned()
                .unwrap_or_default();
            assert_eq!(
                kinds.into_iter().collect::<Vec<_>>(),
                vec![LuaDependencyKind::Dispatched],
                "{loader}"
            );
        }
    }

    fn names_thing(ws: &mut VirtualWorkspace) -> bool {
        let uri = |path| ws.virtual_url_generator.new_uri(path);
        let (boot, thing) = (uri(BOOT_FILE), uri(THING_FILE));
        let file_id = |uri| ws.analysis.get_file_id(&uri).expect("file");
        let (boot, thing) = (file_id(boot), file_id(thing));
        ws.get_db_mut()
            .get_file_dependencies_index()
            .get_dependency_kinds(&boot, &thing)
            .is_some_and(|kinds| kinds.contains(&LuaDependencyKind::Dispatched))
    }

    /// A shift above a dispatching loader leaves what its calls name, so
    /// their files do not run again.
    #[test]
    fn a_shift_above_a_dispatching_loader_re_runs_nothing_in_its_callers() {
        let mut ws = gmod_workspace();
        ws.def_file(THING_FILE, "return {}");
        ws.def_file(LOADER_FILE, REALM_LOADER);
        ws.def_file(BOOT_FILE, "MyLoad(\"mylib/thing.lua\", \"server\")\n");

        let loader = ws.def_file(LOADER_FILE, &format!("\n{REALM_LOADER}"));

        assert_eq!(ws.analysis.compilation.last_evaluated_files(), vec![loader]);
    }

    /// Editing a dispatching loader so it loads nothing, deleting it, and
    /// putting it back each move what its calls name as a cold build has it.
    #[test]
    fn a_dispatching_loader_edited_deleted_and_reopened_moves_what_its_calls_name() {
        let mut ws = gmod_workspace();
        ws.def_file(THING_FILE, "return {}");
        ws.def_file(LOADER_FILE, REALM_LOADER);
        ws.def_file(BOOT_FILE, "MyLoad(\"mylib/thing.lua\", \"server\")\n");
        let loader_uri = ws.virtual_url_generator.new_uri(LOADER_FILE);

        ws.def_file(LOADER_FILE, "function MyLoad(path, realm)\nend\n");
        let edited = names_thing(&mut ws);
        ws.def_file(LOADER_FILE, REALM_LOADER);
        let restored = names_thing(&mut ws);
        ws.analysis
            .remove_file_by_uri(&loader_uri)
            .0
            .expect("loader must be removed");
        let deleted = names_thing(&mut ws);
        ws.def_file(LOADER_FILE, REALM_LOADER);
        let reopened = names_thing(&mut ws);

        assert_eq!(
            (edited, restored, deleted, reopened),
            (false, true, false, true)
        );
    }

    /// A wrapper returning its `include` returns the table the file it loads
    /// returns.
    #[test]
    fn an_include_wrapper_returns_the_table_the_file_returns() {
        let mut ws = gmod_workspace();
        ws.def_file(THING_FILE, "return { first = 1 }");
        ws.def_file(
            LOADER_FILE,
            "CNC = {}\nfunction CNC.Import(path)\n    return include(path)\nend\n",
        );
        let boot = ws.def_file(
            BOOT_FILE,
            "local thing = CNC.Import(\"mylib/thing.lua\")\nlocal first = thing.first\n",
        );

        let first = ws.local_ty(boot, "first");
        assert_eq!(ws.humanize_type(first), "1");
    }

    /// A wrapper of a wrapper loads the file too.
    #[test]
    fn a_call_of_a_wrapper_of_an_include_wrapper_loads_the_file_it_names() {
        let mut ws = gmod_workspace();
        ws.def_file(THING_FILE, "return {}");
        ws.def_file(
            LOADER_FILE,
            &format!("{LOADER}function LoadTwice(path)\n    MyLoad(path)\nend\n"),
        );
        ws.def_file(BOOT_FILE, "LoadTwice(\"mylib/thing.lua\")\n");

        assert!(loads_thing(&mut ws));
    }

    /// Editing the wrapper so it no longer includes its parameter, deleting
    /// it, and putting it back each move the load of its calls as a cold
    /// build has it.
    #[test]
    fn an_include_wrapper_edited_deleted_and_reopened_moves_its_calls_loads() {
        let mut ws = gmod_workspace();
        ws.def_file(THING_FILE, "return {}");
        ws.def_file(LOADER_FILE, LOADER);
        ws.def_file(BOOT_FILE, "MyLoad(\"mylib/thing.lua\")\n");
        let loader_uri = ws.virtual_url_generator.new_uri(LOADER_FILE);

        ws.def_file(LOADER_FILE, "function MyLoad(path)\nend\n");
        let edited = loads_thing(&mut ws);
        ws.def_file(LOADER_FILE, LOADER);
        let restored = loads_thing(&mut ws);
        ws.analysis
            .remove_file_by_uri(&loader_uri)
            .0
            .expect("loader must be removed");
        let deleted = loads_thing(&mut ws);
        ws.def_file(LOADER_FILE, LOADER);
        let reopened = loads_thing(&mut ws);

        assert_eq!(
            (edited, restored, deleted, reopened),
            (false, true, false, true)
        );
    }
}
