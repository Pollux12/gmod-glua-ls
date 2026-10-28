#[cfg(test)]
mod test {
    use googletest::prelude::*;

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
}
