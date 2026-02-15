#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, Emmyrc, FileId, LuaMemberOwner, LuaType, LuaTypeDeclId, VirtualWorkspace,
    };

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec!["entities/**".to_string()];
        ws.update_emmyrc(emmyrc);
    }

    fn type_decl_id_from_type(typ: &LuaType) -> Option<LuaTypeDeclId> {
        match typ {
            LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => Some(type_decl_id.clone()),
            LuaType::Instance(instance) => type_decl_id_from_type(instance.get_base()),
            LuaType::TypeGuard(inner) => type_decl_id_from_type(inner),
            _ => None,
        }
    }

    fn member_names_for_type(ws: &mut VirtualWorkspace, type_name: &str) -> Vec<String> {
        let typ = ws.ty(type_name);
        let type_decl_id = type_decl_id_from_type(&typ)
            .expect("expected class-like type from VirtualWorkspace::ty");
        let owner = LuaMemberOwner::Type(type_decl_id);

        ws.get_db_mut()
            .get_member_index()
            .get_members(&owner)
            .map(|members| {
                members
                    .iter()
                    .filter_map(|member| member.get_key().get_name().map(|name| name.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn assert_has_members(member_names: &[String], expected: &[&str]) {
        for name in expected {
            assert!(
                member_names.contains(&name.to_string()),
                "missing member `{name}` in {member_names:?}"
            );
        }
    }

    fn assert_missing_members(member_names: &[String], expected_absent: &[&str]) {
        for name in expected_absent {
            assert!(
                !member_names.contains(&name.to_string()),
                "unexpected member `{name}` in {member_names:?}"
            );
        }
    }

    fn undefined_field_messages_for_file(
        ws: &mut VirtualWorkspace,
        file_id: FileId,
    ) -> Vec<String> {
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic.code == undefined_field_code)
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[gtest]
    fn test_accessorfunc_synthesizes_get_set_members() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "lua/items/base_item.lua",
            r#"
            ---@class base_item
            local ITEM = {}

            ---@accessorfunc
            function ITEM:AutoFunction(name, key)
            end

            ITEM:AutoFunction("SpecialValue", "internal_flag")
        "#,
        );

        let member_names = member_names_for_type(&mut ws, "base_item");
        assert_has_members(&member_names, &["GetSpecialValue", "SetSpecialValue"]);
    }

    #[gtest]
    fn test_accessorfunc_multiple_calls_create_multiple_pairs() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "lua/items/multi_item.lua",
            r#"
            ---@class multi_item
            local ITEM = {}

            ---@accessorfunc
            function ITEM:AutoFunction(name, key)
            end

            ITEM:AutoFunction("Speed", "speed_key")
            ITEM:AutoFunction("Health", "health_key")
            ITEM:AutoFunction("Name", "name_key")
        "#,
        );

        let member_names = member_names_for_type(&mut ws, "multi_item");
        assert_has_members(
            &member_names,
            &[
                "GetSpeed",
                "SetSpeed",
                "GetHealth",
                "SetHealth",
                "GetName",
                "SetName",
            ],
        );
    }

    #[gtest]
    fn test_accessorfunc_custom_param_index_uses_description() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "lua/items/custom_idx.lua",
            r#"
            ---@class custom_idx
            local ITEM = {}

            ---@accessorfunc 2
            function ITEM:CreateAccessor(key, name)
            end

            ITEM:CreateAccessor("internal_key", "Power")
        "#,
        );

        let member_names = member_names_for_type(&mut ws, "custom_idx");
        assert_has_members(&member_names, &["GetPower", "SetPower"]);
    }

    #[gtest]
    fn test_accessorfunc_usage_has_no_undefined_field_diagnostics() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "lua/items/usable_item.lua",
            r#"
            ---@class usable_item
            local ITEM = {}

            ---@accessorfunc
            function ITEM:AutoFunction(name, key)
            end

            ITEM:AutoFunction("Value", "val_key")

            function ITEM:TestUsage()
                local v = self:GetValue()
                self:SetValue(42)
            end
        "#,
        );

        let undefined_fields = undefined_field_messages_for_file(&mut ws, file_id);
        assert!(
            undefined_fields.is_empty(),
            "unexpected undefined-field diagnostics: {undefined_fields:?}"
        );
    }

    #[gtest]
    fn test_accessorfunc_works_with_gmod_network_var_members() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        ws.def_file(
            "lua/entities/test_ent/shared.lua",
            r#"
            ---@class test_ent
            ENT = {}
            ENT.Type = "anim"
            ENT.Base = "base_anim"

            ---@accessorfunc
            function ENT:CustomAccessor(name)
            end

            function ENT:SetupDataTables()
                self:NetworkVar("Float", "Speed")
            end

            ENT:CustomAccessor("Ammo")
        "#,
        );

        let member_names = member_names_for_type(&mut ws, "test_ent");
        assert_has_members(
            &member_names,
            &["GetSpeed", "SetSpeed", "GetAmmo", "SetAmmo"],
        );
    }

    #[gtest]
    fn test_accessorfunc_non_string_argument_is_ignored() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "lua/items/safe_item.lua",
            r#"
            ---@class safe_item
            local ITEM = {}

            ---@accessorfunc
            function ITEM:MakeAccessor(name)
            end

            local varName = "Dynamic"
            ITEM:MakeAccessor(varName)
        "#,
        );

        let member_names = member_names_for_type(&mut ws, "safe_item");
        assert_has_members(&member_names, &["MakeAccessor"]);
        assert_missing_members(&member_names, &["GetDynamic", "SetDynamic"]);
    }

    #[gtest]
    fn test_accessorfunc_cross_file_annotation_and_call() {
        let mut ws = VirtualWorkspace::new();

        let file_ids = ws.def_files(vec![
            (
                "lua/items/cross_item_def.lua",
                r#"
                ---@class cross_item
                ITEM = {}

                ---@accessorfunc
                function ITEM:AddProp(name, key)
                end
            "#,
            ),
            (
                "lua/items/cross_item_use.lua",
                r#"
                ITEM:AddProp("Color", "color_key")

                function ITEM:UseColor()
                    local c = self:GetColor()
                end
            "#,
            ),
        ]);

        let member_names = member_names_for_type(&mut ws, "cross_item");
        assert_has_members(&member_names, &["GetColor", "SetColor"]);

        ws.enable_check(DiagnosticCode::UndefinedField);
        let undefined_fields = undefined_field_messages_for_file(&mut ws, file_ids[1]);
        assert!(
            undefined_fields.is_empty(),
            "unexpected undefined-field diagnostics in cross-file use: {undefined_fields:?}"
        );
    }
}
