#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        ADDS_FIELD_ATTRIBUTE, DiagnosticCode, FileId, LuaSignatureId, LuaType, VirtualWorkspace,
    };

    const REGISTER_META_TABLE: (&str, &str) = (
        "lua/includes/register.lua",
        r#"
        ---@param metaName string
        ---@[adds_field("MetaID", "integer")]
        ---@[adds_field("MetaName", "string")]
        ---@param metaTable table
        function _G.RegisterMetaTable(metaName, metaTable) end
        "#,
    );

    fn undefined_field_messages(ws: &mut VirtualWorkspace, file_id: FileId) -> Vec<String> {
        let undefined_field = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == undefined_field)
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[gtest]
    fn adds_field_uses_are_kept_on_the_parameter_they_precede() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(REGISTER_META_TABLE.0, REGISTER_META_TABLE.1);

        let db = ws.get_db_mut();
        let signature_id = db
            .get_signature_index()
            .iter()
            .map(|(signature_id, _)| *signature_id)
            .find(|signature_id: &LuaSignatureId| signature_id.get_file_id() == file_id)
            .expect("RegisterMetaTable signature");
        let signature = db.get_signature_index().get(&signature_id).unwrap();
        let field_names = |param_idx: usize| {
            signature
                .get_param_info_by_id(param_idx)
                .map(|param| {
                    param
                        .iter_attributes_by_name(ADDS_FIELD_ATTRIBUTE)
                        .filter_map(|attribute| match attribute.get_param_by_name("name") {
                            Some(LuaType::DocStringConst(name)) => Some(name.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };

        expect_that!(field_names(0), is_empty());
        expect_that!(
            field_names(1),
            unordered_elements_are![eq("MetaID"), eq("MetaName")]
        );
    }

    #[gtest]
    fn adds_field_gives_a_class_table_its_typed_fields_and_keeps_its_members() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(REGISTER_META_TABLE.0, REGISTER_META_TABLE.1);
        let file_id = ws.def_file(
            "lua/autorun/meta.lua",
            r#"
            ---@class MyMeta
            local META = {}

            function META:Describe()
                return "meta"
            end

            RegisterMetaTable("MyMeta", META)

            local id = META.MetaID
            local name = META.MetaName
            local describe = META.Describe
            "#,
        );

        expect_eq!(ws.local_ty(file_id, "id"), LuaType::Integer);
        expect_eq!(ws.local_ty(file_id, "name"), LuaType::String);
        expect_that!(ws.local_ty(file_id, "describe"), not(eq(&LuaType::Unknown)));
        expect_that!(undefined_field_messages(&mut ws, file_id), is_empty());
    }

    #[gtest]
    fn adds_field_gives_a_table_literal_its_fields() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(REGISTER_META_TABLE.0, REGISTER_META_TABLE.1);
        let file_id = ws.def_file(
            "lua/autorun/meta.lua",
            r#"
            local meta = { Existing = true }
            RegisterMetaTable("Plain", meta)

            local id = meta.MetaID
            local existing = meta.Existing
            "#,
        );

        expect_eq!(ws.local_ty(file_id, "id"), LuaType::Integer);
        expect_eq!(
            ws.local_ty(file_id, "existing"),
            LuaType::BooleanConst(true)
        );
    }

    #[gtest]
    fn adds_field_reaches_classes_made_by_a_constructor_factory() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(REGISTER_META_TABLE.0, REGISTER_META_TABLE.1);
        let file_id = ws.def_file(
            "lua/battlemechs/sh_core.lua",
            r#"
            ---@class battlemechs.Base
            local Base = {}

            ---@generic T : battlemechs.Base
            ---@param name battlemechs.`T`
            ---@return (definition) T
            local function CreateMetaTable(name)
                ---@type (definition) battlemechs.Base
                local meta = setmetatable({}, { __index = Base })
                RegisterMetaTable("Battlemechs." .. name, meta)
                return meta
            end

            local walker = CreateMetaTable("Walker")
            local id = walker.MetaID
            "#,
        );

        expect_eq!(ws.local_ty(file_id, "id"), LuaType::Integer);
        expect_that!(undefined_field_messages(&mut ws, file_id), is_empty());
    }

    #[gtest]
    fn adds_field_type_defaults_to_any() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/tag.lua",
            r#"
            ---@[adds_field("Tagged")]
            ---@param t table
            local function tag(t) end

            local value = {}
            tag(value)
            local tagged = value.Tagged
            "#,
        );

        expect_eq!(ws.local_ty(file_id, "tagged"), LuaType::Any);
        expect_that!(undefined_field_messages(&mut ws, file_id), is_empty());
    }

    #[gtest]
    fn adds_field_on_a_table_function_resolves_an_optional_class_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/track.lua",
            r#"
            ---@class Player

            MyAddon = {}

            ---@[adds_field("Owner", "Player?")]
            ---@param ent table
            function MyAddon.Track(ent) end

            local tracked = {}
            MyAddon.Track(tracked)
            local owner = tracked.Owner
            "#,
        );

        expect_eq!(
            ws.humanize_type(ws.local_ty(file_id, "owner")),
            "Player?".to_string()
        );
    }

    #[gtest]
    fn adds_field_fields_leave_with_the_call_that_added_them() {
        let call_file = (
            "lua/autorun/meta.lua",
            r#"
            ---@class MyMeta
            local META = {}
            RegisterMetaTable("MyMeta", META)
            print(META.MetaID)
            "#,
        );
        let without_call = r#"
            ---@class MyMeta
            local META = {}
            print(META.MetaID)
            "#;

        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            |_| {},
            &[REGISTER_META_TABLE, call_file],
            &[(call_file.0, without_call)],
        );
        let file_id = ws.def_file(call_file.0, without_call);
        expect_that!(
            undefined_field_messages(&mut ws, file_id),
            elements_are![contains_substring("MetaID")]
        );
    }

    #[gtest]
    fn adds_field_fields_follow_an_edit_to_the_callee() {
        let call_file = (
            "lua/autorun/meta.lua",
            r#"
            ---@class MyMeta
            local META = {}
            RegisterMetaTable("MyMeta", META)
            print(META.MetaID)
            "#,
        );
        let renamed_field = r#"
            ---@param metaName string
            ---@[adds_field("MetaIndex", "integer")]
            ---@param metaTable table
            function _G.RegisterMetaTable(metaName, metaTable) end
            "#;

        VirtualWorkspace::assert_warm_matches_cold(
            |_| {},
            &[REGISTER_META_TABLE, call_file],
            &[(REGISTER_META_TABLE.0, renamed_field)],
        );
    }
}
