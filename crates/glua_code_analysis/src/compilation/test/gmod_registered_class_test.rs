#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{Emmyrc, LuaMemberOwner, LuaType, LuaTypeDeclId, VirtualWorkspace};

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        setup(&mut ws);
        ws
    }

    fn setup(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
    }

    fn is_registered(ws: &mut VirtualWorkspace, class_name: &str) -> bool {
        ws.get_db_mut()
            .get_type_index()
            .get_type_decl(&LuaTypeDeclId::global(class_name))
            .is_some()
    }

    fn super_types(ws: &mut VirtualWorkspace, class_name: &str) -> Vec<LuaType> {
        ws.get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global(class_name))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default()
    }

    fn has_member(ws: &mut VirtualWorkspace, class_name: &str, member: &str) -> bool {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global(class_name));
        ws.get_db_mut()
            .get_member_index()
            .get_members(&owner)
            .is_some_and(|members| {
                members
                    .iter()
                    .any(|candidate| candidate.get_key().get_name() == Some(member))
            })
    }

    const PROJECTILES_PATH: &str = "addons/pac3/lua/pac3/extra/shared/projectiles.lua";
    const PROJECTILES: &str = r#"
        do
            local ENT = {}
            ENT.Type = "anim"
            ENT.ClassName = "pac_projectile"

            function ENT:Initialize() end

            scripted_ents.Register(ENT, ENT.ClassName)
        end
    "#;

    #[gtest]
    fn scripted_ents_register_takes_the_class_name_the_registered_table_holds() {
        let mut ws = gmod_workspace();
        ws.def_file(PROJECTILES_PATH, PROJECTILES);

        expect_that!(
            super_types(&mut ws, "pac_projectile"),
            contains(eq(&LuaType::Ref(LuaTypeDeclId::global("base_anim"))))
        );
        expect_true!(has_member(&mut ws, "pac_projectile", "Initialize"));
    }

    #[gtest]
    fn scripted_ents_register_leaves_a_class_name_that_is_not_one_string_unregistered() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "addons/pac3/lua/pac3/extra/shared/dynamic.lua",
            r#"
            local ENT = {}
            ENT.Type = "anim"
            ENT.ClassName = math.random() > 0.5 and "pac_first" or "pac_second"

            scripted_ents.Register(ENT, ENT.ClassName)
            "#,
        );

        expect_false!(is_registered(&mut ws, "pac_first"));
        expect_false!(is_registered(&mut ws, "pac_second"));
    }

    #[gtest]
    fn editing_the_class_name_field_moves_the_registration() {
        let renamed = PROJECTILES.replace("pac_projectile", "pac_renamed");
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[(PROJECTILES_PATH, PROJECTILES)],
            &[(PROJECTILES_PATH, renamed.as_str())],
        );

        expect_true!(is_registered(&mut ws, "pac_renamed"));
        expect_true!(has_member(&mut ws, "pac_renamed", "Initialize"));
        expect_false!(has_member(&mut ws, "pac_projectile", "Initialize"));
    }

    #[gtest]
    fn deleting_and_reopening_the_registering_file_drops_and_restores_the_class() {
        let mut ws = gmod_workspace();
        let uri = ws.virtual_url_generator.new_uri(PROJECTILES_PATH);
        ws.def_file(PROJECTILES_PATH, PROJECTILES);
        expect_true!(has_member(&mut ws, "pac_projectile", "Initialize"));

        ws.analysis.remove_file_by_uri(&uri);
        expect_false!(has_member(&mut ws, "pac_projectile", "Initialize"));

        ws.def_file(PROJECTILES_PATH, PROJECTILES);
        expect_true!(has_member(&mut ws, "pac_projectile", "Initialize"));
    }

    const PANEL_CLASSES_PATH: &str = "lua/includes/panels.lua";
    const PANEL_CLASSES: &str = r#"
        ---@meta
        ---@class Panel
        function Panel:Dock() end
        ---@class DPanel : Panel
        ---@class DButton : DPanel
    "#;
    const WRAPPER_PATH: &str = "addons/pac3/lua/pac3/editor/client/panels.lua";
    const WRAPPER: &str = r#"
        pace = pace or {}
        pace.RegisteredPanels = {}

        function pace.RegisterPanel(PANEL)
            pace.RegisteredPanels[PANEL.ClassName] = PANEL
            vgui.Register("pace_" .. PANEL.ClassName, PANEL, PANEL.Base)
        end
    "#;
    const BROWSER_PATH: &str = "addons/pac3/lua/pac3/editor/client/panels/browser.lua";
    const BROWSER: &str = r#"
        local PANEL = {}

        PANEL.ClassName = "browser"
        PANEL.Base = "DButton"

        function PANEL:Init()
            self:Dock()
            self:NotAPanelMethod()
        end

        pace.RegisterPanel(PANEL)
    "#;

    fn panel_workspace(files: &[(&str, &str)]) -> VirtualWorkspace {
        let mut ws = gmod_workspace();
        ws.def_file(PANEL_CLASSES_PATH, PANEL_CLASSES);
        ws.def_files(files.to_vec());
        ws
    }

    fn undefined_methods(ws: &mut VirtualWorkspace, path: &str) -> Vec<String> {
        let file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(path))
            .expect("file");
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String("undefined-method".to_string()))
            })
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[gtest]
    fn a_function_that_registers_the_panel_it_is_given_binds_the_panel_to_its_class() {
        let mut ws = panel_workspace(&[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, BROWSER)]);

        expect_that!(
            super_types(&mut ws, "pace_browser"),
            contains(eq(&LuaType::Ref(LuaTypeDeclId::global("DButton"))))
        );
        expect_true!(has_member(&mut ws, "pace_browser", "Init"));
        expect_that!(
            undefined_methods(&mut ws, BROWSER_PATH),
            elements_are![contains_substring("NotAPanelMethod")]
        );
    }

    #[gtest]
    fn a_wrapper_given_a_panel_without_a_class_name_leaves_it_unregistered() {
        let nameless = BROWSER.replace(
            "PANEL.ClassName = \"browser\"",
            "PANEL.ClassName = pace.Name()",
        );
        let mut ws = panel_workspace(&[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, nameless.as_str())]);

        expect_false!(is_registered(&mut ws, "pace_browser"));
        expect_false!(has_member(&mut ws, "pace_browser", "Init"));
    }

    #[gtest]
    fn editing_the_wrapper_or_the_panel_moves_the_registration() {
        let renamed_panel = BROWSER.replace("\"browser\"", "\"renamed\"");
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            |ws| {
                setup(ws);
                ws.def_file(PANEL_CLASSES_PATH, PANEL_CLASSES);
            },
            &[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, BROWSER)],
            &[(BROWSER_PATH, renamed_panel.as_str())],
        );
        expect_true!(has_member(&mut ws, "pace_renamed", "Init"));
        expect_false!(has_member(&mut ws, "pace_browser", "Init"));

        let prefixed = WRAPPER.replace("\"pace_\"", "\"ui_\"");
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            |ws| {
                setup(ws);
                ws.def_file(PANEL_CLASSES_PATH, PANEL_CLASSES);
            },
            &[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, BROWSER)],
            &[(WRAPPER_PATH, prefixed.as_str())],
        );
        expect_true!(has_member(&mut ws, "ui_browser", "Init"));
        expect_false!(has_member(&mut ws, "pace_browser", "Init"));

        let unwrapped = "pace = pace or {}
function pace.RegisterPanel(PANEL) end
";
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            |ws| {
                setup(ws);
                ws.def_file(PANEL_CLASSES_PATH, PANEL_CLASSES);
            },
            &[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, BROWSER)],
            &[(WRAPPER_PATH, unwrapped)],
        );
        expect_false!(has_member(&mut ws, "pace_browser", "Init"));
    }

    #[gtest]
    fn deleting_and_reopening_the_wrapper_unbinds_and_rebinds_its_panels() {
        let mut ws = panel_workspace(&[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, BROWSER)]);
        let wrapper_uri = ws.virtual_url_generator.new_uri(WRAPPER_PATH);
        expect_true!(has_member(&mut ws, "pace_browser", "Init"));

        ws.analysis.remove_file_by_uri(&wrapper_uri);
        expect_false!(has_member(&mut ws, "pace_browser", "Init"));

        ws.def_file(WRAPPER_PATH, WRAPPER);
        expect_true!(has_member(&mut ws, "pace_browser", "Init"));
    }

    #[gtest]
    fn a_function_wrapping_a_wrapper_registers_the_same_way() {
        let outer = r#"
            function pace.RegisterEditorPanel(P)
                pace.RegisterPanel(P)
            end
        "#;
        let user = BROWSER.replace(
            "pace.RegisterPanel(PANEL)",
            "pace.RegisterEditorPanel(PANEL)",
        );
        let mut ws = panel_workspace(&[
            (WRAPPER_PATH, WRAPPER),
            (
                "addons/pac3/lua/pac3/editor/client/editor_panels.lua",
                outer,
            ),
            (BROWSER_PATH, user.as_str()),
        ]);

        expect_true!(has_member(&mut ws, "pace_browser", "Init"));
    }

    const WEAPON_CLASSES_PATH: &str = "lua/includes/weapon_classes.lua";
    const WEAPON_CLASSES: &str = r#"
        ---@meta
        ---@class Entity
        function Entity:DrawShadow(draw) end
        ---@class Weapon : Entity
        ---@class weapon_base : Weapon
        ---@class weapon_hl2 : Weapon
    "#;
    const HANDS_PATH: &str = "addons/pac3/lua/pac3/extra/shared/hands.lua";
    const HANDS: &str = r#"
        local SWEP = { Primary = {} }

        function SWEP:Initialize()
            self:DrawShadow(false)
            self:NotAWeaponMethod()
        end

        weapons.Register(SWEP, "none", true)
    "#;

    fn weapon_workspace(files: &[(&str, &str)]) -> VirtualWorkspace {
        let mut ws = gmod_workspace();
        ws.def_file(WEAPON_CLASSES_PATH, WEAPON_CLASSES);
        ws.def_files(files.to_vec());
        ws
    }

    #[gtest]
    fn weapons_register_binds_the_swep_table_to_the_weapon_base_class() {
        let mut ws = weapon_workspace(&[(HANDS_PATH, HANDS)]);

        expect_that!(
            super_types(&mut ws, "none"),
            contains(eq(&LuaType::Ref(LuaTypeDeclId::global("weapon_base"))))
        );
        expect_true!(has_member(&mut ws, "none", "Initialize"));
        expect_that!(
            undefined_methods(&mut ws, HANDS_PATH),
            elements_are![contains_substring("NotAWeaponMethod")]
        );
    }

    #[gtest]
    fn weapons_register_prefers_the_base_the_swep_table_names() {
        let named = HANDS.replace(
            "local SWEP = { Primary = {} }",
            "local SWEP = { Primary = {} }
        SWEP.Base = \"weapon_hl2\"",
        );
        let mut ws = weapon_workspace(&[(HANDS_PATH, named.as_str())]);

        expect_that!(
            super_types(&mut ws, "none"),
            contains(eq(&LuaType::Ref(LuaTypeDeclId::global("weapon_hl2"))))
        );
    }

    #[gtest]
    fn a_wrapper_annotated_with_the_weapon_role_registers_the_same_way() {
        let wrapper = r#"
            ---@[call_arg("gmod.scripted_class", "weapon_table")]
            ---@param tbl table
            ---@[call_arg("gmod.scripted_class", "define")]
            ---@param name string
            function RegisterMyWeapon(tbl, name) end
        "#;
        let user = HANDS.replace(
            "weapons.Register(SWEP, \"none\", true)",
            "RegisterMyWeapon(SWEP, \"mine\")",
        );
        let mut ws = weapon_workspace(&[
            ("addons/pac3/lua/pac3/extra/shared/register.lua", wrapper),
            (HANDS_PATH, user.as_str()),
        ]);

        expect_true!(has_member(&mut ws, "mine", "Initialize"));
    }

    #[gtest]
    fn editing_the_weapon_class_name_moves_the_registration() {
        let renamed = HANDS.replace("\"none\"", "\"renamed\"");
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            |ws| {
                setup(ws);
                ws.def_file(WEAPON_CLASSES_PATH, WEAPON_CLASSES);
            },
            &[(HANDS_PATH, HANDS)],
            &[(HANDS_PATH, renamed.as_str())],
        );

        expect_true!(has_member(&mut ws, "renamed", "Initialize"));
        expect_false!(has_member(&mut ws, "none", "Initialize"));
    }

    #[gtest]
    fn deleting_and_reopening_the_weapon_file_drops_and_restores_the_class() {
        let mut ws = weapon_workspace(&[(HANDS_PATH, HANDS)]);
        let uri = ws.virtual_url_generator.new_uri(HANDS_PATH);
        expect_true!(has_member(&mut ws, "none", "Initialize"));

        ws.analysis.remove_file_by_uri(&uri);
        expect_false!(has_member(&mut ws, "none", "Initialize"));

        ws.def_file(HANDS_PATH, HANDS);
        expect_true!(has_member(&mut ws, "none", "Initialize"));
    }

    #[gtest]
    fn a_panel_registered_under_a_runtime_name_takes_members_from_its_base() {
        let dynamic = BROWSER.replace(
            "PANEL.ClassName = \"browser\"",
            "PANEL.ClassName = \"properties_\" .. pace.Kind()",
        );
        let mut ws = panel_workspace(&[(WRAPPER_PATH, WRAPPER), (BROWSER_PATH, dynamic.as_str())]);

        expect_false!(is_registered(&mut ws, "pace_browser"));
        expect_that!(undefined_methods(&mut ws, BROWSER_PATH), is_empty());
    }

    #[gtest]
    fn a_table_never_registered_as_a_panel_still_reports_missing_methods() {
        let mut ws = panel_workspace(&[(
            BROWSER_PATH,
            "local PANEL = {}
function PANEL:Init() self:Missing() end
",
        )]);

        expect_that!(
            undefined_methods(&mut ws, BROWSER_PATH),
            elements_are![contains_substring("Missing")]
        );
    }

    #[gtest]
    fn registering_a_swep_table_raises_no_parameter_mismatch_on_the_call() {
        let ws = weapon_workspace(&[(HANDS_PATH, HANDS)]);
        let file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(HANDS_PATH))
            .expect("file");
        let mismatches = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String("param-type-mismatch".to_string()))
            })
            .count();

        expect_that!(mismatches, eq(0));
    }
}
