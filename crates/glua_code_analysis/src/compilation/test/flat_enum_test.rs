#[cfg(test)]
mod tests {
    use crate::{LuaMemberKey, LuaMemberOwner, LuaTypeDeclId, VirtualWorkspace};

    fn flat_enum_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            EF_BONEMERGE = 1
            EF_NODRAW = 32

            ---@enum EF
            ---| EF_BONEMERGE # Performs bone merge on client side
            ---| EF_NODRAW # Don't draw entity
            "#,
        );
        ws
    }

    #[test]
    fn flat_enum_registers_a_member_per_field() {
        let ws = flat_enum_workspace();
        let db = ws.analysis.compilation.get_db();

        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("EF"));
        let members = db
            .get_member_index()
            .get_members(&owner)
            .expect("flat enum must own members");

        let mut keys = members
            .iter()
            .filter_map(|member| match member.get_key() {
                LuaMemberKey::Name(name) => Some(name.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        keys.sort();

        assert_eq!(keys, vec!["EF_BONEMERGE", "EF_NODRAW"]);
    }

    #[test]
    fn flat_enum_member_values_resolve_from_globals() {
        let ws = flat_enum_workspace();
        let db = ws.analysis.compilation.get_db();

        let type_decl = db
            .get_type_index()
            .get_type_decl(&LuaTypeDeclId::global("EF"))
            .expect("EF must be declared");
        assert!(type_decl.is_flat_enum());

        let field_type = type_decl
            .get_enum_field_type(db)
            .expect("flat enum must expose field values");

        let rendered = ws.humanize_type(field_type);
        assert!(
            rendered.contains('1') && rendered.contains("32"),
            "expected both global values, got {rendered}"
        );
    }

    #[test]
    fn flat_enum_does_not_bind_the_following_statement() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            EF_BONEMERGE = 1

            ---@enum EF
            ---| EF_BONEMERGE # Performs bone merge on client side
            NotAnEnum = "not an enum"
            "#,
        );

        let after_type = ws.expr_ty("NotAnEnum");
        assert_eq!(ws.humanize_type(after_type), "\"not an enum\"");
    }

    #[test]
    fn table_enum_is_unchanged() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@enum TEXFILTER
            TEXFILTER = {
                NONE = 0,
                POINT = 1,
            }
            "#,
        );
        let db = ws.analysis.compilation.get_db();

        let type_decl = db
            .get_type_index()
            .get_type_decl(&LuaTypeDeclId::global("TEXFILTER"))
            .expect("TEXFILTER must be declared");
        assert!(!type_decl.is_flat_enum());

        let rendered = ws.humanize_type(
            type_decl
                .get_enum_field_type(db)
                .expect("table enum must expose field values"),
        );
        assert!(
            rendered.contains('0') && rendered.contains('1'),
            "expected table member values, got {rendered}"
        );
    }
}
