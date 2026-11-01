//! A derivation that re-runs without producing a fact it produced before
//! must leave the index as a cold build of the same texts would. Each
//! scenario edits only a file the derivation reads, so the derivation re-runs
//! in place instead of being rebuilt with its own file.
#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaIndexExpr, LuaNameExpr};

    use crate::{Emmyrc, FileId, LuaMemberOwner, LuaTypeDeclId, VirtualWorkspace};

    fn gmod(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
    }

    fn gmod_with_std(ws: &mut VirtualWorkspace) {
        ws.analysis.init_std_lib();
        gmod(ws);
    }

    fn file_id(ws: &VirtualWorkspace, path: &str) -> FileId {
        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis.get_file_id(&uri).expect("file id")
    }

    fn last_name_type(ws: &VirtualWorkspace, path: &str, name: &str) -> String {
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id(ws, path))
            .expect("semantic model");
        let expr = model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .last()
            .expect("name expression");
        let info = model
            .get_semantic_info(expr.syntax().clone().into())
            .expect("semantic info");
        ws.humanize_type(info.display_typ().clone())
    }

    fn index_type(ws: &VirtualWorkspace, path: &str, text: &str) -> String {
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id(ws, path))
            .expect("semantic model");
        let expr = model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|expr| expr.syntax().text() == text)
            .expect("index expression");
        let info = model
            .get_semantic_info(expr.syntax().clone().into())
            .expect("semantic info");
        ws.humanize_type(info.display_typ().clone())
    }

    fn class_member_names(ws: &VirtualWorkspace, class: &str) -> Vec<String> {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global(class));
        let mut names = ws
            .analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .iter()
            .filter_map(|member| member.get_key().get_name().map(str::to_string))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn inferred_string_default_is_not_duplicated_by_a_rerun() {
        let panels = "lua/autorun/sh_panels.lua";
        let menu = "lua/autorun/client/cl_menu.lua";
        let panels_text = |ret: &str| {
            format!(
                "---@class Panel\n---@class DScrollPanel: Panel\n\n---@generic T: Panel\n---@param classname `T`\n---@return T\nfunction create_panel(classname) end\n\n---@return {ret}\nfunction GetPanelClass() end\n"
            )
        };
        let before = panels_text("string?");
        let after = panels_text("string|false|nil");
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (panels, &before),
                (
                    menu,
                    "local panelClass = GetPanelClass()\npanelClass = panelClass or \"DScrollPanel\"\nlocal p = create_panel(panelClass)\nMenuPanel = p\n",
                ),
            ],
            &[(panels, &after)],
        );

        assert_eq!(last_name_type(&warm, menu, "p"), "DScrollPanel");
    }

    #[test]
    fn accessor_call_is_dropped_when_the_accessor_annotation_goes() {
        let helper = "lua/autorun/sh_accessor.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (
                    helper,
                    "---@class AccessorHost\nlocal Host = {}\n\n---@accessorfunc\nfunction Host:AutoFunction(name, key) end\n",
                ),
                (
                    "lua/items/base_item.lua",
                    "---@class base_item\nlocal ITEM = {}\n\nITEM:AutoFunction(\"Speed\", \"speed_key\")\n",
                ),
            ],
            &[(
                helper,
                "---@class AccessorHost\nlocal Host = {}\n\nfunction Host:AutoFunction(name, key) end\n",
            )],
        );

        let members = class_member_names(&warm, "base_item");
        assert!(
            !members.iter().any(|name| name == "GetSpeed"),
            "stale accessor members: {members:?}"
        );
    }

    #[test]
    fn load_site_is_dropped_when_the_call_stops_being_a_load_call() {
        let loader = "lua/autorun/sh_loader.lua";
        let boot = "lua/autorun/sh_boot.lua";
        let target = "lua/myaddon/sh_target.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (
                    loader,
                    "---@[call_arg(\"gmod.load\", \"include\")]\n---@param path string\nfunction LoadAddonFile(path) end\n",
                ),
                (target, "return true"),
                (boot, "LoadAddonFile(\"myaddon/sh_target.lua\")\n"),
            ],
            &[(
                loader,
                "---@param path string\nfunction LoadAddonFile(path) end\n",
            )],
        );

        let db = warm.analysis.compilation.get_db();
        let required = db
            .get_file_dependencies_index()
            .get_required_files(&file_id(&warm, boot))
            .cloned()
            .unwrap_or_default();
        assert!(
            !required.contains(&file_id(&warm, target)),
            "stale load edge: {required:?}"
        );
    }

    #[test]
    fn setmetatable_binding_is_dropped_when_the_metatable_stops_resolving() {
        let shared = "lua/autorun/sh_shared.lua";
        let obj = "lua/autorun/sh_obj.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod_with_std,
            &[
                (shared, "Shared = {}\n"),
                (
                    obj,
                    "Shared.Meta = { __index = { Greet = function() return 1 end } }\nlocal obj = {}\nsetmetatable(obj, Shared.Meta)\nObjGreet = obj.Greet\n",
                ),
            ],
            &[(shared, "-- Shared moved elsewhere\n")],
        );

        assert_eq!(index_type(&warm, obj, "obj.Greet"), "nil");
    }

    #[test]
    fn setmetatable_factory_binding_is_dropped_when_the_metatable_stops_resolving() {
        let lib = "lua/autorun/sh_lib.lua";
        let factory = "lua/autorun/sh_anim_factory.lua";
        let factory_text = "local AnimationTable = {}\nAnimationTable.__index = AnimationTable\nLib.Animation = AnimationTable\nlocal Animation = Lib.Animation\n\nfunction MakeAnimation()\n    local anim = {}\n    anim.Func = function() end\n    return setmetatable(anim, Animation)\nend\n";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod_with_std,
            &[(lib, "Lib = {}\n"), (factory, factory_text)],
            &[(lib, "-- Lib moved elsewhere\n")],
        );

        let animation_table = rowan::TextRange::new(23.into(), 25.into());
        let owner = LuaMemberOwner::Element(crate::InFiled::new(
            file_id(&warm, factory),
            animation_table,
        ));
        let members = warm
            .analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .iter()
            .filter_map(|member| member.get_key().get_name().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(
            !members.iter().any(|name| name == "Func"),
            "stale factory transfer: {members:?}"
        );
    }

    #[test]
    fn global_path_class_is_withdrawn_when_the_assignment_stops_proposing_it() {
        let fuel = "lua/autorun/sh_fuel.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (
                    fuel,
                    "---@class Fuel\nlocal FuelDef = {}\nFuelClass = FuelDef\n",
                ),
                (
                    "lua/autorun/sh_glide.lua",
                    "Glide = {}\nGlide.Fuel = FuelClass\nfunction Glide.Fuel.Refill() end\n",
                ),
            ],
            &[(fuel, "---@class Fuel\nlocal FuelDef = {}\nFuelClass = {}\n")],
        );

        let members = class_member_names(&warm, "Fuel");
        assert!(
            !members.iter().any(|name| name == "Refill"),
            "member homed on a withdrawn path class: {members:?}"
        );
    }

    #[test]
    fn special_call_effect_is_dropped_when_the_outparam_goes() {
        let layout = "lua/autorun/sh_layout.lua";
        let hud = "lua/autorun/client/cl_hud.lua";
        let before = stack_layout_text("---@outparam out HUDStackLayout\n");
        let after = stack_layout_text("");
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (layout, &before),
                (
                    hud,
                    "local layout = {}\nglide.GetHUDStackLayout(layout)\nlocal rowHeight = layout.rowHeight\n",
                ),
            ],
            &[(layout, &after)],
        );

        assert_ne!(index_type(&warm, hud, "layout.rowHeight"), "integer");
    }

    fn stack_layout_text(outparam: &str) -> String {
        format!(
            "---@class HUDStackLayout\n---@field rowHeight integer\n\nglide = {{}}\n\n{outparam}---@param out table\nfunction glide.GetHUDStackLayout(out) end\n"
        )
    }

    #[test]
    fn an_outparam_narrows_only_its_own_variable_and_only_after_the_call() {
        let layout = "lua/autorun/sh_layout.lua";
        let hud = "lua/autorun/client/cl_hud.lua";
        let mut ws = VirtualWorkspace::new();
        gmod(&mut ws);
        ws.def_files(vec![
            (
                layout,
                &stack_layout_text(
                    "---@outparam out HUDStackLayout
",
                ),
            ),
            (
                hud,
                "local layout = {}
local other = {}
local early = layout.rowHeight
glide.GetHUDStackLayout(layout)
local late = layout.rowHeight
local untouched = other.rowHeight
print(early, late, untouched)
",
            ),
        ]);

        assert_eq!(last_name_type(&ws, hud, "late"), "integer");
        assert_ne!(last_name_type(&ws, hud, "early"), "integer");
        assert_ne!(last_name_type(&ws, hud, "untouched"), "integer");
    }

    #[test]
    fn an_outparam_appearing_reaches_a_caller_in_another_file() {
        let layout = "lua/autorun/sh_layout.lua";
        let hud = "lua/autorun/client/cl_hud.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (layout, &stack_layout_text("")),
                (
                    hud,
                    "local layout = {}
local early = layout.rowHeight
glide.GetHUDStackLayout(layout)
local late = layout.rowHeight
print(early, late)
",
                ),
            ],
            &[(
                layout,
                &stack_layout_text(
                    "---@outparam out HUDStackLayout
",
                ),
            )],
        );

        assert_eq!(last_name_type(&warm, hud, "late"), "integer");
        assert_ne!(last_name_type(&warm, hud, "early"), "integer");
    }

    #[test]
    fn an_outparam_appearing_on_another_variable_leaves_a_reader_unnarrowed() {
        let layout = "lua/autorun/sh_layout.lua";
        let hud = "lua/autorun/client/cl_hud.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (layout, &stack_layout_text("")),
                (
                    hud,
                    "local layout = {}
local other = {}
glide.GetHUDStackLayout(other)
local mine = layout.rowHeight
local theirs = other.rowHeight
print(mine, theirs)
",
                ),
            ],
            &[(
                layout,
                &stack_layout_text(
                    "---@outparam out HUDStackLayout
",
                ),
            )],
        );

        assert_eq!(last_name_type(&warm, hud, "theirs"), "integer");
        assert_ne!(last_name_type(&warm, hud, "mine"), "integer");
    }

    #[test]
    fn default_constructor_operator_is_retracted_when_the_constructor_attribute_goes() {
        let meta = "lua/autorun/sh_class_meta.lua";
        let class = "lua/autorun/sh_myclass.lua";
        let meta_text = |attribute: &str| {
            format!(
                "---@attribute constructor(name: string, root_class: string?, strip_self: boolean?, return_self: boolean?)\n\n---@generic T\n{attribute}---@param name `T`\n---@return T\nfunction meta(name) end\n"
            )
        };
        let before = meta_text("---@[constructor(\"__init\")]\n");
        let after = meta_text("");
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (meta, &before),
                (
                    class,
                    "---@class MyClass\nlocal M = meta(\"MyClass\")\n\nfunction M:__init() end\n\nMyObject = M()\n",
                ),
            ],
            &[(meta, &after)],
        );

        assert_ne!(last_name_type(&warm, class, "MyObject"), "MyClass");
    }

    #[test]
    fn hook_sites_are_replaced_not_appended_when_call_roles_change() {
        let helpers = "lua/autorun/sh_hook_helpers.lua";
        let hooks = "lua/autorun/sh_hooks.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (
                    helpers,
                    "---@[call_arg(\"gmod.hook\", \"add\")]\n---@param eventName string\n---@param identifier any\n---@[call_arg(\"gmod.hook\", \"callback\")]\n---@param func function\nfunction AddHook(eventName, identifier, func) end\n",
                ),
                (
                    hooks,
                    "hook.Add(\"Think\", \"a\", function() end)\nAddHook(\"Tick\", \"b\", function() end)\n",
                ),
            ],
            &[(
                helpers,
                "---@param eventName string\n---@param identifier any\n---@param func function\nfunction AddHook(eventName, identifier, func) end\n",
            )],
        );

        let db = warm.analysis.compilation.get_db();
        let names = db
            .get_gmod_infer_index()
            .get_hook_file_metadata(&file_id(&warm, hooks))
            .map(|metadata| {
                metadata
                    .sites
                    .iter()
                    .map(|site| site.hook_name.clone().unwrap_or_default())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(names, vec!["Think".to_string()]);
    }

    #[test]
    fn network_flows_are_dropped_when_they_become_empty() {
        let alias = "lua/autorun/server/sv_alias.lua";
        let sender = "lua/autorun/server/sv_send.lua";
        let warm = VirtualWorkspace::assert_warm_matches_cold(
            gmod,
            &[
                (alias, "StartMessage = net.Start\n"),
                (
                    sender,
                    "StartMessage(\"AliasMessage\")\nnet.WriteString(\"x\")\nnet.Broadcast()\n",
                ),
            ],
            &[(alias, "StartMessage = function(name) end\n")],
        );

        let db = warm.analysis.compilation.get_db();
        let flows = db
            .get_gmod_network_index()
            .get_file_data(file_id(&warm, sender))
            .map_or(0, |data| data.send_flows.len() + data.receive_flows.len());
        assert_eq!(flows, 0);
    }
}
